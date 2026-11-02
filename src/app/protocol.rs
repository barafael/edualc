//! What the process says, taken into the state: the conversation's messages and stream
//! events, its own status, permission requests, and the answers to our control requests.

use serde::Deserialize;

use super::{App, Doing, LimitUse, Mode, Request, Sandboxing, TaskProgress};
use crate::chat::{Origin, UserText};
use crate::claude::Output;
use crate::external::wire::{
    self, Activity, CommandInfo, ContentBlock, ControlRequest, ControlResponse, IncomingRequest,
    Initialized, Message, Progress, RateLimits, ResultKind, StreamEvent, System, TurnResult,
    UserContent,
};
use crate::ipc::Generation;

impl App {
    /// Takes what process `generation` said; an earlier process's output is dropped.
    pub(super) fn take(&mut self, generation: Generation, output: Output) {
        if generation != self.generation {
            return;
        }
        match output {
            Output::Message(message) => {
                self.heard = std::time::Instant::now();
                if matches!(self.status.doing, Doing::Silent(_)) {
                    self.status.doing = Doing::Working;
                }
                self.message(message);
            }
            Output::Log(line) => {
                tracing::debug!(target: "claude", "{line}");
                self.log.push_back(line);
                if self.log.len() > super::LOG_LINES {
                    self.log.pop_front();
                }
            }
            Output::Started {
                pid,
                enforcement,
                turns,
            } => {
                if let Some(process) = self.process.as_mut() {
                    process.started(pid);
                }
                if let Some(enforcement) = enforcement {
                    self.status.sandboxing = Sandboxing::Landlock(enforcement);
                }
                // Attached in the middle of a turn: it runs, doing what isn't told yet. A
                // process just started counts none; the window's own prompt may be on its way.
                if turns > self.status.turns {
                    self.status.turns = turns;
                    self.status.doing = Doing::Working;
                    self.start_listening();
                }
            }
            Output::Failed(failure) => {
                self.process = None;
                self.forget_process();
                self.chat.error(failure.to_string());
            }
            Output::Ended(code) => self.ended(code),
            Output::Diverged(left) => self.chat.info(format!(
                "This conversation went two ways in and out of the sandbox. What only the other \
                 copy has is kept in {}",
                left.display()
            )),
        }
    }

    fn message(&mut self, message: Message) {
        match message {
            Message::StreamEvent {
                event,
                parent_tool_use_id,
            } => {
                self.chat.stream_event(&event, parent_tool_use_id.as_ref());
                if parent_tool_use_id.is_none() {
                    self.stream_activity(&event);
                }
            }
            Message::Assistant {
                message,
                parent_tool_use_id,
            } => {
                self.chat
                    .assistant_message(&message, parent_tool_use_id.as_ref(), Origin::Live);
                if parent_tool_use_id.is_none() {
                    let context = message.usage.context();
                    if context > 0 {
                        self.status.context_tokens = context;
                    }
                }
                let running = message.content.iter().rev().find_map(|block| match block {
                    ContentBlock::ToolUse { name, .. } => Some(name),
                    ContentBlock::Text { .. }
                    | ContentBlock::Thinking { .. }
                    | ContentBlock::ServerToolUse { .. }
                    | ContentBlock::ToolResult { .. }
                    | ContentBlock::Image { .. }
                    | ContentBlock::Other => None,
                });
                if let Some(name) = running {
                    self.status.doing = Doing::Running(name.clone());
                }
            }
            Message::User { message } => {
                if let UserContent::Text(text) = &message.content
                    && matches!(
                        UserText::of(text),
                        UserText::Shell(_) | UserText::ShellOutput { .. }
                    )
                {
                    self.shell_rows.push(text.clone());
                }
                self.chat.user_message(&message, Origin::Live);
            }
            Message::System(system) => self.system(system),
            Message::Result(result) => self.result(&result),
            Message::RateLimitEvent { rate_limit_info } => self.take_limits(&rate_limit_info),
            Message::ControlRequest {
                request_id,
                request,
            } => match request {
                IncomingRequest::CanUseTool {
                    tool_name,
                    input,
                    permission_suggestions,
                    description,
                    tool_use_id,
                } => self.ask(
                    request_id,
                    tool_name,
                    input,
                    permission_suggestions,
                    description,
                    tool_use_id,
                ),
                IncomingRequest::Other => {
                    self.write(|process| {
                        process.refuse(&request_id, "not supported by this client")
                    });
                }
            },
            Message::ControlResponse { response } => self.control_response(response),
            Message::ControlCancelRequest { request_id } => self.cancel(&request_id),
            Message::Other => {}
        }
    }

    fn stream_activity(&mut self, event: &StreamEvent) {
        let doing = match event {
            StreamEvent::MessageStart { .. } => Doing::Thinking,
            StreamEvent::ContentBlockStart { content_block, .. } => match content_block {
                ContentBlock::Text { .. } => Doing::Writing,
                ContentBlock::ToolUse { name, .. } | ContentBlock::ServerToolUse { name, .. } => {
                    Doing::Preparing(name.clone())
                }
                ContentBlock::Thinking { .. }
                | ContentBlock::ToolResult { .. }
                | ContentBlock::Image { .. }
                | ContentBlock::Other => return,
            },
            StreamEvent::ContentBlockDelta { .. }
            | StreamEvent::ContentBlockStop { .. }
            | StreamEvent::Other => return,
        };
        self.status.doing = doing;
    }

    fn system(&mut self, system: System) {
        match system {
            System::Init {
                session_id,
                model,
                permission_mode,
                cwd,
            } => {
                if let Some(worktree) = crate::sessions::linked_worktree(&cwd) {
                    self.status.worktree = Some(worktree);
                }
                if self.status.session_id.as_ref() != Some(&session_id) {
                    // The daemon lists it under this id, for a window to come back to.
                    if let Some(process) = &self.process
                        && let Err(error) = process.identify(&session_id)
                    {
                        tracing::warn!(%error, "couldn't name the session to the daemon");
                    }
                    self.status.session_id = Some(session_id);
                    self.redraw.sessions = true;
                }
                self.status.model = model;
                self.status.mode = Some(permission_mode);
                self.redraw.menus = true;
            }
            System::Status { status } => match status {
                Some(Activity::Requesting) => self.status.doing = Doing::Thinking,
                Some(Activity::Compacting) => self.status.doing = Doing::Compacting,
                Some(Activity::Other) | None => {}
            },
            System::ThinkingTokens { estimated_tokens } => {
                self.status.doing = Doing::ThinkingTokens(estimated_tokens);
            }
            System::ApiRetry {
                attempt,
                max_retries,
                error_status,
                error,
            } => {
                self.status.doing = Doing::Retrying(super::Retry {
                    attempt,
                    most: max_retries,
                    status: error_status,
                    cause: error,
                });
            }
            System::CompactBoundary => self.chat.info("Conversation compacted"),
            System::BackgroundTasksChanged { tasks } => {
                self.redraw.tasks |= self.status.tasks.list(tasks);
            }
            System::TaskStarted {
                task_id,
                task_type,
                subagent_type,
                description,
                is_backgrounded,
                ambient,
            } => {
                let task = wire::BackgroundTask {
                    task_id,
                    task_type: task_type.unwrap_or(wire::TaskKind::Other),
                    subagent_type,
                    description,
                    ambient,
                };
                self.redraw.tasks |= self.status.tasks.start(task, is_backgrounded);
            }
            System::TaskUpdated { task_id, patch } => {
                self.redraw.tasks |= self.status.tasks.update(&task_id, &patch);
            }
            System::TaskNotification { task_id } => {
                self.redraw.tasks |= self.status.tasks.end(&task_id);
            }
            System::TaskProgress {
                task_id,
                usage,
                last_tool_name,
                summary,
            } => {
                let progress = TaskProgress {
                    usage,
                    last_tool: last_tool_name,
                    summary: summary.filter(|line| !line.trim().is_empty()),
                };
                self.redraw.tasks |= self.status.tasks.progress(task_id, progress);
            }
            System::ControlRequestProgress { request_id, status } => {
                if status == Progress::ApiRetry
                    && let Some(Request::Side(index)) = self.requests.get(&request_id)
                {
                    let index = *index;
                    self.side_retrying(index);
                }
            }
            System::PermissionDenied { tool_use_id } => {
                self.chat
                    .set_tool_state(&tool_use_id, crate::chat::ToolState::Denied);
            }
            System::Other => {}
        }
    }

    fn result(&mut self, result: &TurnResult) {
        self.status.turns = self.status.turns.saturating_sub(1);
        self.redraw.tasks |= self.status.tasks.end_turn();
        if let Some(cost) = result.total_cost_usd {
            self.status.cost_usd = cost;
        }
        if let Some(window) = result
            .model_usage
            .values()
            .filter_map(|usage| usage.context_window)
            .max()
        {
            self.status.context_window = window;
        }
        if self.status.turns == 0 {
            self.status.doing = Doing::Nothing;
            self.chat.end_turn();
        }
        // A turn the user stopped ends as failed during execution, and the transcript already
        // says so; one that failed so on its own (an API error after the retries) is told.
        let stop_asked = std::mem::take(&mut self.status.stop_asked);
        let stopped = stop_asked && result.subtype == ResultKind::ErrorDuringExecution;
        // However the turn says it ended, what runs beside it went on.
        if stop_asked {
            self.tasks_after_stop();
        }
        if result.is_error && !stopped {
            let errors: Vec<String> = result
                .errors
                .iter()
                .map(|error| match error.as_str() {
                    Some(text) => text.to_owned(),
                    None => error.to_string(),
                })
                .collect();
            let text = match &result.result {
                Some(text) if !text.is_empty() => text.clone(),
                _ if !errors.is_empty() => errors.join("\n"),
                _ => "The turn failed.".to_owned(),
            };
            self.chat.error(text);
        }
        // A new session gets its file and its title as it goes, and maybe a worktree.
        self.load_sessions();
        self.read_worktree();
        if self.usage.open {
            self.ask_usage();
        }
    }

    /// How much of the plan's usage limits is used, and when each starts over.
    fn take_limits(&mut self, limits: &RateLimits) {
        let used = |window: &Option<wire::LimitWindow>| {
            let window = window.as_ref()?;
            Some(LimitUse {
                used: window.utilization?.clamp(0.0, 1.0),
                resets_at: window.resets_at,
            })
        };
        let windows = &limits.windows;
        if let Some(used) = used(&windows.five_hour) {
            self.status.limits.five_hour = Some(used);
        }
        if let Some(used) = used(&windows.seven_day) {
            self.status.limits.seven_day = Some(used);
        }
    }

    fn control_response(&mut self, response: ControlResponse) {
        let (id, outcome) = match response {
            ControlResponse::Success {
                request_id,
                response,
            } => (request_id, Ok(response)),
            ControlResponse::Error { request_id, error } => (request_id, Err(error)),
        };
        let Some(purpose) = self.requests.remove(&id) else {
            return;
        };
        match (purpose, outcome) {
            (Request::Initialize, Ok(answer)) => match Initialized::deserialize(&answer) {
                Ok(initialized) => self.take_initialized(initialized),
                Err(error) => tracing::warn!(%error, "an initialize answer that doesn't parse"),
            },
            (Request::Model(value), Ok(_)) => self.status.model_choice = Some(value),
            (Request::Mode(mode), Ok(_)) => {
                self.status.mode = Some(Mode::Known(mode));
                self.status.mode_choice = Some(mode);
            }
            // The task's end comes as its notification.
            (Request::Interrupt | Request::StopTask(_), Ok(_)) => {}
            (Request::Side(index), outcome) => self.side_answered(index, outcome),
            (Request::Usage, outcome) => self.usage_answered(outcome),
            (Request::ContextUsage, outcome) => self.context_answered(outcome),
            (Request::Initialize, Err(error)) => {
                tracing::warn!(%error, "claude refused to initialize")
            }
            (Request::Model(value), Err(error)) => {
                self.chat
                    .error(format!("Couldn't switch to {value}: {error}"));
            }
            (Request::Mode(mode), Err(error)) => {
                self.chat
                    .error(format!("Couldn't switch to {}: {error}", mode.label()));
            }
            (Request::Interrupt, Err(error)) => {
                tracing::warn!(%error, "claude refused to interrupt")
            }
            (Request::StopTask(task), Err(error)) => {
                let what = self
                    .status
                    .tasks
                    .running()
                    .find(|running| running.task.task_id == task)
                    .map_or_else(
                        || "the task".to_owned(),
                        |running| format!("\u{201c}{}\u{201d}", running.task.description),
                    );
                self.chat.error(format!("Couldn't stop {what}: {error}"));
            }
        }
        // A refused switch puts the menu back.
        self.redraw.menus = true;
    }

    fn take_initialized(&mut self, initialized: Initialized) {
        self.status.models = initialized.models;
        let mut commands = initialized.commands;
        // The terminal's own; this window answers it in the side panel.
        commands.push(CommandInfo {
            name: "btw".into(),
            description: "Ask a quick side question without interrupting the main conversation"
                .into(),
            argument_hint: "[question]".into(),
        });
        commands.sort_by(|left, right| left.name.cmp(&right.name));
        self.status.commands = commands;
        self.status.account = initialized.account;
        self.status.version = initialized.claude_code_version;
        // The plan's limits at once, not only after the first turn reports them; it asks the
        // plan, not the model.
        if self.status.limits.five_hour.is_none()
            && self.request(&ControlRequest::GetUsage, Request::Usage)
        {
            self.usage.waiting += 1;
        }
    }
}
