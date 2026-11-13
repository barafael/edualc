//! The demo's Claude: one session as a script plays it (`Session`), and what it says as stream-json
//! lines, as `claude -p` would, to a [`Sink`] (a window's events in the browser demo, a line's
//! messages for `edualcd --demo`). What it does takes time (pauses, so that the window shows
//! the answer come) and is run by whoever has the executor: `Session::take` hands back what is
//! to run (a [`Task`]), which that one runs, on a thread of its own or the page's.

use std::collections::HashMap;
use std::path::PathBuf;

use async_channel::{Receiver, Sender};
use serde::Serialize;
use serde_json::{Map, Value};
use web_time::Duration;

use super::scenarios;
use crate::app::{Event, Mode, PermissionMode};
use crate::claude::{Launch, Output};
use crate::external::wire::{
    ApiMessage, ContentBlock, ContextReport, ControlRequest, ControlResponse, Delta,
    IncomingRequest, Input, Message, MessageId, MessageStart, ModelUsage, PermissionResult, Reply,
    RequestId, ResultKind, SessionId, SideAnswer, StreamEvent, Suggestion, System, ToolName,
    ToolUseId, TurnResult, Usage, UserContent, UserLine,
};
use crate::ipc::Generation;
#[cfg(not(target_arch = "wasm32"))]
use crate::ipc::{self, FromDaemon, Stream};

/// What the demo's Claude says it runs as.
const MODEL: &str = "claude-opus-5-5";
/// The pid the demo's sessions run as, in the window's eyes.
pub const PID: u32 = 4242;

/// Where what the demo's Claude says goes.
#[derive(Debug, Clone)]
pub enum Sink {
    /// To the window's app as its events: the browser demo, whose daemon is in the page.
    Events(Sender<Event>),
    /// To a line's window as the daemon's messages: edualcd's demo.
    #[cfg(not(target_arch = "wasm32"))]
    Line(tokio::sync::mpsc::Sender<FromDaemon>),
}

impl Sink {
    /// A line the session wrote, as the window takes it.
    fn line(&self, key: Generation, text: String) {
        let told = match self {
            Self::Events(events) => events
                .try_send(Event::Claude {
                    generation: key,
                    output: Output::of_line(text),
                })
                .is_ok(),
            #[cfg(not(target_arch = "wasm32"))]
            Self::Line(outbox) => {
                let mut pieces = ipc::chunks(&text).peekable();
                let mut told = true;
                while let Some(piece) = pieces.next() {
                    let last = pieces.peek().is_none();
                    told &= outbox
                        .try_send(FromDaemon::Output {
                            key,
                            stream: Stream::Out,
                            text: piece.to_owned(),
                            last,
                        })
                        .is_ok();
                }
                told
            }
        };
        if !told {
            tracing::debug!("the window is gone");
        }
    }

    fn started(&self, key: Generation) {
        let told = match self {
            Self::Events(events) => events
                .try_send(Event::Claude {
                    generation: key,
                    output: Output::Started { pid: PID, turns: 0 },
                })
                .is_ok(),
            #[cfg(not(target_arch = "wasm32"))]
            Self::Line(outbox) => outbox
                .try_send(FromDaemon::Started {
                    key,
                    pid: PID,
                    turns: 0,
                })
                .is_ok(),
        };
        if !told {
            tracing::debug!("the window is gone");
        }
    }

    /// The session ended, as the window asked.
    pub fn ended(&self, key: Generation) {
        let told = match self {
            Self::Events(events) => events
                .try_send(Event::Claude {
                    generation: key,
                    output: Output::Ended(Some(0)),
                })
                .is_ok(),
            #[cfg(not(target_arch = "wasm32"))]
            Self::Line(outbox) => outbox
                .try_send(FromDaemon::Ended { key, code: Some(0) })
                .is_ok(),
        };
        if !told {
            tracing::debug!("the window is gone");
        }
    }
}

/// Something a session has started that takes time: to be run, by whoever has the executor.
#[derive(Debug)]
pub enum Task {
    /// A turn, playing the scenario its prompt picks.
    Turn { turn: Turn, prompt: String },
    /// A side question, answered after a moment.
    Side {
        out: Out,
        request_id: RequestId,
        question: String,
    },
}

impl Task {
    pub async fn run(self) {
        match self {
            Self::Turn { turn, prompt } => scenarios::play(turn, prompt).await,
            Self::Side {
                out,
                request_id,
                question,
            } => {
                pause(Duration::from_millis(900)).await;
                let answer = SideAnswer {
                    response: scenarios::side_answer(&question).to_owned(),
                    synthetic: false,
                };
                out.respond(request_id, &answer);
            }
        }
    }
}

/// Where the demo's Claude writes: the window, as one session's output.
#[derive(Debug, Clone)]
pub struct Out {
    key: Generation,
    sink: Sink,
    session: SessionId,
    cwd: PathBuf,
}

impl Out {
    /// One stream-json line, as `claude -p` writes it.
    pub fn line(&self, message: &Message) {
        match serde_json::to_string(message) {
            Ok(line) => self.sink.line(self.key, line),
            Err(error) => tracing::warn!(%error, "a line the demo can't write"),
        }
    }

    /// Answers request `request_id` with `response`, or with why it can't be written.
    fn respond(&self, request_id: RequestId, response: &impl Serialize) {
        let response = match serde_json::to_value(response) {
            Ok(response) => ControlResponse::Success {
                request_id,
                response,
            },
            Err(error) => ControlResponse::Error {
                request_id,
                error: error.to_string(),
            },
        };
        self.line(&Message::ControlResponse { response });
    }
}

/// What the window says to a turn under way.
#[derive(Debug)]
pub enum Said {
    /// The answer to the permission request or question it waits on.
    Answer(PermissionResult),
    Interrupt,
}

/// One session the demo's Claude runs.
#[derive(Debug)]
pub struct Session {
    out: Out,
    model: String,
    mode: PermissionMode,
    turns: u32,
    /// The turn under way, to tell what the window says to it.
    turn: Option<Sender<Said>>,
    /// A line coming in pieces.
    pieces: String,
}

impl Session {
    #[must_use]
    pub fn start(key: Generation, sink: Sink, launch: &Launch) -> Self {
        sink.started(key);
        Self {
            out: Out {
                key,
                sink,
                session: launch.conversation.id().clone(),
                cwd: launch.cwd.clone(),
            },
            model: launch.model.clone().unwrap_or_else(|| MODEL.to_owned()),
            mode: PermissionMode::Default,
            turns: 0,
            turn: None,
            pieces: String::new(),
        }
    }

    /// A piece of a line the window wrote to the process; the last piece ends the line, which
    /// may start something to run (see [`Task`]).
    pub fn take(&mut self, text: &str, last: bool) -> Option<Task> {
        self.pieces.push_str(text);
        if !last {
            return None;
        }
        let line = std::mem::take(&mut self.pieces);
        self.line(&line)
    }

    fn line(&mut self, line: &str) -> Option<Task> {
        let input = match serde_json::from_str::<Input>(line) {
            Ok(input) => input,
            Err(error) => {
                tracing::warn!(%error, "a line from the window the demo can't read");
                return None;
            }
        };
        match input {
            Input::ControlRequest {
                request_id,
                request,
            } => self.control(request_id, request),
            Input::ControlResponse {
                response: Reply::Success { response, .. },
            } => {
                self.tell_turn(Said::Answer(response));
                None
            }
            Input::ControlResponse {
                response: Reply::Error { error, .. },
            } => {
                tracing::warn!(%error, "the window turned a request down");
                None
            }
            Input::User { message, .. } => Some(self.prompt(&message.content)),
            Input::BashCommand { command, .. } => {
                scenarios::shell(&self.out, &command);
                None
            }
        }
    }

    fn tell_turn(&self, said: Said) {
        if let Some(turn) = &self.turn
            && turn.try_send(said).is_err()
        {
            tracing::debug!("no turn is under way");
        }
    }

    fn control(&mut self, request_id: RequestId, request: ControlRequest) -> Option<Task> {
        let nothing = Map::new();
        match request {
            ControlRequest::Initialize { .. } => {
                self.out.respond(request_id, &scenarios::initialized());
            }
            ControlRequest::Interrupt => {
                self.tell_turn(Said::Interrupt);
                self.out.respond(request_id, &nothing);
            }
            ControlRequest::SetModel { model } => {
                self.model = Some(model)
                    .filter(|model| !model.is_empty())
                    .unwrap_or_else(|| MODEL.to_owned());
                self.out.respond(request_id, &nothing);
            }
            ControlRequest::SetPermissionMode { mode } => {
                self.mode = mode;
                self.out.respond(request_id, &nothing);
            }
            ControlRequest::SideQuestion { question, .. } => {
                // Answered after a moment, as a model would; the window waits for it.
                return Some(Task::Side {
                    out: self.out.clone(),
                    request_id,
                    question,
                });
            }
            ControlRequest::GetContextUsage => {
                let report = ContextReport {
                    categories: Vec::new(),
                    total_tokens: 38_200,
                    max_tokens: 200_000,
                };
                self.out.respond(request_id, &report);
            }
            ControlRequest::StopTask { .. }
            | ControlRequest::GetTaskOutput { .. }
            | ControlRequest::SetProgramMode { .. }
            | ControlRequest::GetUsage => self.out.respond(request_id, &nothing),
        }
        None
    }

    /// A prompt: a turn plays the scenario it picks.
    fn prompt(&mut self, content: &crate::external::wire::Content) -> Task {
        use crate::external::wire::{Block, Content};
        let prompt = match content {
            Content::Text(text) => text.clone(),
            Content::Blocks(blocks) => blocks
                .iter()
                .filter_map(|block| match block {
                    Block::Text { text } => Some(text.as_str()),
                    Block::Image(_) => None,
                })
                .collect::<Vec<_>>()
                .join(" "),
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
        self.out.line(&Message::System(System::Init {
            session_id: self.out.session.clone(),
            model: self.model.clone(),
            permission_mode: Mode::Known(self.mode),
            cwd: self.out.cwd.clone(),
        }));
        Task::Turn { turn, prompt }
    }
}

/// A tool's input, from its type in [`crate::external::tool_input`] or the wire's. Fails only
/// for a path that isn't UTF-8, which the demo's aren't: logged, the call shows no input.
pub fn input(typed: &impl Serialize) -> Value {
    serde_json::to_value(typed).unwrap_or_else(|error| {
        tracing::warn!(%error, "a tool input the demo can't write");
        Value::Null
    })
}

/// The turn under way: where it writes, and what the window says to it.
#[derive(Debug)]
pub struct Turn {
    pub out: Out,
    inbox: Receiver<Said>,
    model: String,
    number: u32,
}

/// The window interrupted the turn.
#[derive(Debug)]
pub struct Interrupted;

impl Turn {
    /// Stops at an interrupt the window sent meanwhile.
    fn heard(&self) -> Result<(), Interrupted> {
        match self.inbox.try_recv() {
            Ok(Said::Interrupt) => Err(Interrupted),
            Ok(Said::Answer(_)) | Err(_) => Ok(()),
        }
    }

    pub async fn wait(&self, milliseconds: u64) -> Result<(), Interrupted> {
        pause(Duration::from_millis(milliseconds)).await;
        self.heard()
    }

    fn assistant(&self, content: Vec<ContentBlock>, id: MessageId) {
        self.out.line(&Message::Assistant {
            message: ApiMessage {
                id,
                content,
                usage: Usage {
                    input_tokens: 2400,
                    cache_creation_input_tokens: 0,
                    cache_read_input_tokens: 18_000,
                    output_tokens: 350,
                },
            },
            parent_tool_use_id: None,
            error: None,
        });
    }

    /// Text, streamed as it is written, then whole.
    pub async fn say(&self, text: &str) -> Result<(), Interrupted> {
        let id = MessageId::from(format!("msg_{}", uuid::Uuid::new_v4().simple()));
        let event = |event: StreamEvent| {
            self.out.line(&Message::StreamEvent {
                event,
                parent_tool_use_id: None,
            });
        };
        event(StreamEvent::MessageStart {
            message: MessageStart { id: id.clone() },
        });
        event(StreamEvent::ContentBlockStart {
            index: 0,
            content_block: ContentBlock::Text {
                text: String::new(),
            },
        });
        let characters: Vec<char> = text.chars().collect();
        for piece in characters.chunks(24) {
            event(StreamEvent::ContentBlockDelta {
                index: 0,
                delta: Delta::Text {
                    text: piece.iter().collect(),
                },
            });
            self.wait(20).await?;
        }
        event(StreamEvent::ContentBlockStop { index: 0 });
        self.assistant(
            vec![ContentBlock::Text {
                text: text.to_owned(),
            }],
            id,
        );
        Ok(())
    }

    /// A tool call; its id, for its result.
    pub fn call(&self, name: ToolName, input: Value) -> ToolUseId {
        let id = ToolUseId::from(format!("toolu_{}", uuid::Uuid::new_v4().simple()));
        self.assistant(
            vec![ContentBlock::ToolUse {
                id: id.clone(),
                name,
                input,
            }],
            MessageId::from(format!("msg_{}", uuid::Uuid::new_v4().simple())),
        );
        id
    }

    pub fn result(&self, id: &ToolUseId, content: UserContent, is_error: bool) {
        self.out
            .line(&Message::User(UserLine::of_session(UserContent::Blocks(
                vec![ContentBlock::ToolResult {
                    tool_use_id: id.clone(),
                    content: Some(content),
                    is_error,
                }],
            ))));
    }

    /// A tool call that needs no asking, its result after `milliseconds`.
    pub async fn run(
        &self,
        name: ToolName,
        input: Value,
        output: &str,
        milliseconds: u64,
    ) -> Result<(), Interrupted> {
        let id = self.call(name, input);
        self.wait(milliseconds).await?;
        self.result(&id, UserContent::Text(output.to_owned()), false);
        Ok(())
    }

    /// Asks the window to allow tool call `id`; its answer, as the window gave it.
    pub async fn ask(
        &self,
        name: ToolName,
        input: Value,
        id: &ToolUseId,
        suggestions: &[Suggestion],
    ) -> Result<PermissionResult, Interrupted> {
        self.out.line(&Message::ControlRequest {
            request_id: RequestId::from(format!("perm_{id}")),
            request: IncomingRequest::CanUseTool {
                tool_name: name,
                input,
                permission_suggestions: suggestions.iter().map(self::input).collect(),
                description: String::new(),
                tool_use_id: Some(id.clone()),
            },
        });
        match self.inbox.recv().await {
            Ok(Said::Answer(answer)) => Ok(answer),
            Ok(Said::Interrupt) | Err(_) => Err(Interrupted),
        }
    }

    /// The turn's end.
    pub fn finish(&self, text: &str) {
        self.out.line(&Message::Result(TurnResult {
            subtype: ResultKind::Success,
            is_error: false,
            result: Some(text.to_owned()),
            errors: Vec::new(),
            total_cost_usd: Some(0.0123 * f64::from(self.number)),
            model_usage: HashMap::from([(
                self.model.clone(),
                ModelUsage {
                    context_window: Some(200_000),
                },
            )]),
            api_error_status: None,
            duration_ms: Some(4200),
            usage: Usage {
                input_tokens: 2400,
                cache_creation_input_tokens: 0,
                cache_read_input_tokens: 0,
                output_tokens: 350,
            },
        }));
    }

    /// The turn's end, as an interrupt makes it.
    pub fn interrupted(&self) {
        self.out
            .line(&Message::User(UserLine::of_session(UserContent::Blocks(
                vec![ContentBlock::Text {
                    text: "[Request interrupted by user]".to_owned(),
                }],
            ))));
        self.finish("interrupted");
    }

    /// The folder the turn works in, for the paths its tools name.
    pub fn path(&self, relative: &str) -> PathBuf {
        self.out.cwd.join(relative)
    }
}

/// Waits `duration` on the page's clock.
#[cfg(target_arch = "wasm32")]
pub async fn pause(duration: Duration) {
    gloo_timers::future::sleep(duration).await;
}

/// Waits `duration`.
#[cfg(not(target_arch = "wasm32"))]
pub async fn pause(duration: Duration) {
    tokio::time::sleep(duration).await;
}
