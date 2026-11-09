//! The demo's daemon (`edualcd --demo`): Claude Code, played by a script. It takes what a window
//! sends, as the daemon does, and answers as `claude -p` would in stream-json, from a scenario
//! a word in the prompt picks (see `scenarios`), with pauses so the window shows the answer
//! come; what a window reads comes from the demo project (`demo`). Nothing runs, nothing on the
//! disk is read or written.

use std::collections::HashMap;
use std::path::Path;

use std::time::Duration;

use async_channel::{Receiver, Sender};
use serde_json::{Value, json};

use super::scenarios;
use super::world as demo;
use crate::claude::Launch;
use crate::ipc::{
    Act, ActId, Answer, FromDaemon, Generation, PROTOCOL, Place, Read, StartFailure, Stream,
    ToDaemon,
};
use crate::sandbox::Enforcement;
use crate::settings::Settings;

/// What the demo's Claude says it runs as.
const MODEL: &str = "claude-opus-5-5";
/// The pid the demo's sessions run as, in the window's eyes.
const PID: u32 = 4242;

/// Answers a window until it hangs up.
pub async fn run(requests: Receiver<ToDaemon>, outbox: Sender<FromDaemon>) {
    let mut sessions: HashMap<Generation, Session> = HashMap::new();
    while let Ok(request) = requests.recv().await {
        match request {
            ToDaemon::Hello { .. } => {
                greet(&outbox);
            }
            ToDaemon::Start { key, launch } => {
                let session = Session::start(key, outbox.clone(), &launch);
                sessions.insert(key, session);
            }
            ToDaemon::Input { key, line } => {
                if let Some(session) = sessions.get_mut(&key) {
                    session.take(&line);
                }
            }
            ToDaemon::End { key } => {
                if sessions.remove(&key).is_some() {
                    tell(&outbox, FromDaemon::Ended { key, code: Some(0) });
                }
            }
            ToDaemon::Attach { key, .. } => tell(
                &outbox,
                FromDaemon::Failed {
                    key,
                    failure: StartFailure::NotHosted,
                },
            ),
            ToDaemon::Act { request, act } => acted(&outbox, request, act),
            ToDaemon::Read { request, read } => read_for(&outbox, request, read),
            // Nothing is kept, and nothing changes on its own.
            ToDaemon::SaveSettings(_)
            | ToDaemon::Watch { .. }
            | ToDaemon::Unwatch(_)
            | ToDaemon::Detach { .. }
            | ToDaemon::Identify { .. } => {}
        }
    }
}

/// The hello, and the sessions the demo runs (none; four "run in other terminals").
fn greet(outbox: &Sender<FromDaemon>) {
    let settings = Settings {
        trusted_folders: vec![std::path::PathBuf::from(demo::PROJECT)],
        ..Settings::default()
    };
    tell(
        outbox,
        FromDaemon::Hello {
            protocol: PROTOCOL,
            settings: Ok(settings),
            live: demo::running(),
            claude_codes_sandbox: false,
            home: Some(std::path::PathBuf::from(demo::PROJECT)),
        },
    );
    tell(outbox, FromDaemon::Hosted(Vec::new()));
}

/// What a window reads, from the demo project.
fn read_for(outbox: &Sender<FromDaemon>, request: ActId, read: Read) {
    let answer = match read {
        Read::Live => Answer::Live(demo::running()),
        Read::Projects => Answer::Projects(demo::projects()),
        Read::Sessions { cwd, limit, .. } => {
            let mut sessions = demo::saved(&cwd);
            sessions.truncate(limit);
            Answer::Sessions(sessions)
        }
        Read::Transcript { place, from } => {
            let path = match place {
                Place::Path(path) => path,
                Place::Session { cwd, id } => cwd.join(format!("{}.jsonl", id.as_str())),
                // Beside its session's, as Claude Code keeps it: not its session's own.
                Place::Subagent { cwd, id, task, .. } => crate::sessions::transcript_of(
                    &cwd.join(format!("{}.jsonl", id.as_str())),
                    &task,
                ),
            };
            let read = match demo::transcript(&path) {
                Some(text) => {
                    let rest = usize::try_from(from)
                        .ok()
                        .and_then(|at| text.get(at..))
                        .unwrap_or_default();
                    let lines: Vec<String> = rest.lines().map(str::to_owned).collect();
                    if !lines.is_empty() {
                        tell(outbox, FromDaemon::Lines { request, lines });
                    }
                    Ok(u64::try_from(text.len()).unwrap_or(u64::MAX))
                }
                None => Err("not in the demo".to_owned()),
            };
            Answer::Transcript { path, read }
        }
        Read::WorktreeOf { .. } | Read::LinkedWorktree { .. } => Answer::Worktree(None),
        Read::TypedFolder(typed) => Answer::Folder(demo_folder(std::path::Path::new(typed.trim()))),
        Read::Folder(path) => Answer::Folder(demo_folder(&path)),
        Read::Saved(_) => Answer::Saved(crate::sessions::Saved::Nowhere),
    };
    tell(outbox, FromDaemon::Answer { request, answer });
}

/// One of the demo's folders, or why a path isn't one.
fn demo_folder(path: &std::path::Path) -> Result<std::path::PathBuf, String> {
    demo::projects()
        .into_iter()
        .map(|project| project.path)
        .find(|project| project == path)
        .ok_or_else(|| format!("{} isn't one of the demo's folders", path.display()))
}

fn tell(outbox: &Sender<FromDaemon>, message: FromDaemon) {
    if outbox.try_send(message).is_err() {
        tracing::debug!("the window is gone");
    }
}

/// What the machine is asked, answered as the demo can.
fn acted(outbox: &Sender<FromDaemon>, request: ActId, act: Act) {
    let message = match act {
        Act::CheckTrust { folder } => match demo::untrusted(&folder) {
            Some(runs) => FromDaemon::Trusted {
                request,
                accepted: false,
                runs,
            },
            None => FromDaemon::Trusted {
                request,
                accepted: true,
                runs: Vec::new(),
            },
        },
        Act::StopSession(_) => FromDaemon::Done {
            request,
            result: Ok(()),
        },
    };
    tell(outbox, message);
}

/// Where the demo's Claude writes: the window, as one session's output.
#[derive(Debug, Clone)]
pub(super) struct Out {
    key: Generation,
    outbox: Sender<FromDaemon>,
    session: String,
    pub(super) cwd: String,
}

impl Out {
    /// One stream-json line, as `claude -p` writes it.
    pub(super) fn line(&self, message: &Value) {
        tell(
            &self.outbox,
            FromDaemon::Output {
                key: self.key,
                stream: Stream::Out,
                line: message.to_string(),
            },
        );
    }

    pub(super) fn session(&self) -> &str {
        &self.session
    }

    fn respond(&self, request_id: &Value, response: &Value) {
        self.line(&json!({
            "type": "control_response",
            "response": {"subtype": "success", "request_id": request_id, "response": response},
        }));
    }
}

/// What the window says to a turn under way.
#[derive(Debug)]
pub(super) enum Said {
    /// The answer to the permission request or question it waits on.
    Answer(Value),
    Interrupt,
}

/// One session the demo's Claude runs.
#[derive(Debug)]
struct Session {
    out: Out,
    model: String,
    mode: String,
    turns: u32,
    /// The turn under way, to tell what the window says to it.
    turn: Option<Sender<Said>>,
}

impl Session {
    fn start(key: Generation, outbox: Sender<FromDaemon>, launch: &Launch) -> Self {
        let enforcement = launch.sandbox.as_ref().map(|_| Enforcement::Full);
        tell(
            &outbox,
            FromDaemon::Started {
                key,
                pid: PID,
                enforcement,
                turns: 0,
            },
        );
        Self {
            out: Out {
                key,
                outbox,
                session: launch.conversation.id().as_str().to_owned(),
                cwd: launch.cwd.display().to_string(),
            },
            model: launch.model.clone().unwrap_or_else(|| MODEL.to_owned()),
            mode: "default".to_owned(),
            turns: 0,
            turn: None,
        }
    }

    /// A line the window wrote to the process.
    fn take(&mut self, line: &str) {
        let Ok(message) = serde_json::from_str::<Value>(line) else {
            return;
        };
        match message.get("type").and_then(Value::as_str) {
            Some("control_request") => self.control(&message),
            Some("control_response") => {
                let answer = message
                    .pointer("/response/response")
                    .cloned()
                    .unwrap_or(Value::Null);
                self.tell_turn(Said::Answer(answer));
            }
            Some("user") => self.prompt(&message),
            Some("bash_command") => {
                let command = message.get("command").and_then(Value::as_str);
                scenarios::shell(&self.out, command.unwrap_or_default());
            }
            Some(_) | None => {}
        }
    }

    fn tell_turn(&self, said: Said) {
        if let Some(turn) = &self.turn
            && turn.try_send(said).is_err()
        {
            tracing::debug!("no turn is under way");
        }
    }

    fn control(&mut self, message: &Value) {
        let request_id = message.get("request_id").cloned().unwrap_or(Value::Null);
        let request = message.get("request").cloned().unwrap_or(Value::Null);
        let field = |name: &str| {
            request
                .get(name)
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned()
        };
        let response = match field("subtype").as_str() {
            "initialize" => scenarios::initialize(),
            "interrupt" => {
                self.tell_turn(Said::Interrupt);
                json!({})
            }
            "set_model" => {
                self.model = Some(field("model"))
                    .filter(|model| !model.is_empty())
                    .unwrap_or_else(|| MODEL.to_owned());
                json!({})
            }
            "set_permission_mode" => {
                self.mode = field("mode");
                json!({})
            }
            "side_question" => {
                // Answered after a moment, as a model would; the window waits for it.
                let out = self.out.clone();
                let question = field("question");
                tokio::spawn(async move {
                    pause(Duration::from_millis(900)).await;
                    out.respond(
                        &request_id,
                        &json!({"response": scenarios::side_answer(&question), "synthetic": false}),
                    );
                });
                return;
            }
            "get_context_usage" => {
                json!({"totalTokens": 38_200, "maxTokens": 200_000, "percentage": 19})
            }
            _ => json!({}),
        };
        self.out.respond(&request_id, &response);
    }

    /// A prompt: a turn plays the scenario it picks.
    fn prompt(&mut self, message: &Value) {
        let content = message.pointer("/message/content");
        let prompt = if let Some(text) = content.and_then(Value::as_str) {
            text.to_owned()
        } else {
            content
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(|block| block.get("text").and_then(Value::as_str))
                .collect::<Vec<_>>()
                .join(" ")
        };
        self.turns += 1;
        let (said, inbox) = async_channel::unbounded();
        self.turn = Some(said);
        let turn = Turn {
            out: self.out.clone(),
            inbox,
            model: self.model.clone(),
            number: self.turns,
        };
        self.out.line(&json!({
            "type": "system", "subtype": "init", "session_id": self.out.session(),
            "model": self.model, "permissionMode": self.mode, "cwd": self.out.cwd,
            "tools": ["Bash", "Read", "Edit", "Write", "Grep", "Glob"], "mcp_servers": [],
        }));
        tokio::spawn(scenarios::play(turn, prompt));
    }
}

/// The turn under way: where it writes, and what the window says to it.
#[derive(Debug)]
pub(super) struct Turn {
    pub(super) out: Out,
    inbox: Receiver<Said>,
    model: String,
    number: u32,
}

/// The window interrupted the turn.
#[derive(Debug)]
pub(super) struct Interrupted;

impl Turn {
    /// Stops at an interrupt the window sent meanwhile.
    fn heard(&self) -> Result<(), Interrupted> {
        match self.inbox.try_recv() {
            Ok(Said::Interrupt) => Err(Interrupted),
            Ok(Said::Answer(_)) | Err(_) => Ok(()),
        }
    }

    pub(super) async fn wait(&self, milliseconds: u64) -> Result<(), Interrupted> {
        pause(Duration::from_millis(milliseconds)).await;
        self.heard()
    }

    fn assistant(&self, content: &Value, id: &str) {
        self.out.line(&json!({
            "type": "assistant", "session_id": self.out.session(), "parent_tool_use_id": null,
            "message": {"id": id, "role": "assistant", "model": self.model, "content": content,
                        "usage": {"input_tokens": 2400, "cache_read_input_tokens": 18_000,
                                  "output_tokens": 350}},
        }));
    }

    /// Text, streamed as it is written, then whole.
    pub(super) async fn say(&self, text: &str) -> Result<(), Interrupted> {
        let id = format!("msg_{}", uuid::Uuid::new_v4().simple());
        let event = |event: Value| {
            self.out.line(&json!({
                "type": "stream_event", "event": event, "parent_tool_use_id": null,
            }));
        };
        event(json!({"type": "message_start", "message": {"id": id}}));
        event(json!({"type": "content_block_start", "index": 0,
                     "content_block": {"type": "text", "text": ""}}));
        let characters: Vec<char> = text.chars().collect();
        for piece in characters.chunks(24) {
            let piece: String = piece.iter().collect();
            event(json!({"type": "content_block_delta", "index": 0,
                         "delta": {"type": "text_delta", "text": piece}}));
            self.wait(20).await?;
        }
        event(json!({"type": "content_block_stop", "index": 0}));
        self.assistant(&json!([{"type": "text", "text": text}]), &id);
        Ok(())
    }

    /// A tool call; its id, for its result.
    pub(super) fn call(&self, name: &str, input: &Value) -> String {
        let id = format!("toolu_{}", uuid::Uuid::new_v4().simple());
        self.assistant(
            &json!([{"type": "tool_use", "id": id, "name": name, "input": input}]),
            &format!("msg_{}", uuid::Uuid::new_v4().simple()),
        );
        id
    }

    pub(super) fn result(&self, id: &str, content: &Value, error: bool) {
        self.out.line(&json!({
            "type": "user", "session_id": self.out.session(), "parent_tool_use_id": null,
            "message": {"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": id, "content": content, "is_error": error}]},
        }));
    }

    /// A tool call that needs no asking, its result after `milliseconds`.
    pub(super) async fn run(
        &self,
        name: &str,
        input: &Value,
        output: &str,
        milliseconds: u64,
    ) -> Result<(), Interrupted> {
        let id = self.call(name, input);
        self.wait(milliseconds).await?;
        self.result(&id, &json!(output), false);
        Ok(())
    }

    /// Asks the window to allow tool call `id`; its answer, as the window gave it.
    pub(super) async fn ask(
        &self,
        name: &str,
        input: &Value,
        id: &str,
        suggestions: &Value,
    ) -> Result<Value, Interrupted> {
        self.out.line(&json!({
            "type": "control_request", "request_id": format!("perm_{id}"),
            "request": {"subtype": "can_use_tool", "tool_name": name, "input": input,
                        "permission_suggestions": suggestions, "tool_use_id": id},
        }));
        match self.inbox.recv().await {
            Ok(Said::Answer(answer)) => Ok(answer),
            Ok(Said::Interrupt) | Err(_) => Err(Interrupted),
        }
    }

    /// The turn's end.
    pub(super) fn finish(&self, text: &str) {
        self.out.line(&json!({
            "type": "result", "subtype": "success", "is_error": false, "duration_ms": 4200,
            "duration_api_ms": 3900, "num_turns": self.number, "result": text,
            "session_id": self.out.session(), "total_cost_usd": 0.0123 * f64::from(self.number),
            "usage": {"input_tokens": 2400, "output_tokens": 350},
            "modelUsage": {self.model.as_str(): {
                "inputTokens": 2400, "outputTokens": 350, "cacheReadInputTokens": 18_000,
                "cacheCreationInputTokens": 0, "costUSD": 0.0123, "contextWindow": 200_000}},
        }));
    }

    /// The turn's end, as an interrupt makes it.
    pub(super) fn interrupted(&self) {
        self.out.line(&json!({
            "type": "user", "parent_tool_use_id": null,
            "message": {"role": "user", "content": [
                {"type": "text", "text": "[Request interrupted by user]"}]},
        }));
        self.finish("interrupted");
    }

    /// The folder the turn works in, for the paths its tools name.
    pub(super) fn path(&self, relative: &str) -> String {
        Path::new(&self.out.cwd)
            .join(relative)
            .display()
            .to_string()
    }
}

/// Waits `duration`.
pub(super) async fn pause(duration: Duration) {
    tokio::time::sleep(duration).await;
}
