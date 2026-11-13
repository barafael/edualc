//! The demo's Claude: one session as a script plays it (`Session`), and what it says as stream-json
//! lines, as `claude -p` would, to a [`Sink`] (a window's events in the browser demo, a line's
//! messages for `edualcd --demo`). What it does takes time (pauses, so that the window shows
//! the answer come) and is run by whoever has the executor: `Session::take` hands back what is
//! to run (a [`Task`]), which that one runs, on a thread of its own or the page's.

use std::path::Path;

use async_channel::{Receiver, Sender};
use serde_json::{Value, json};
use web_time::Duration;

use super::scenarios;
use crate::app::Event;
use crate::claude::{Launch, Output};
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
        request_id: Value,
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
                out.respond(
                    &request_id,
                    &json!({"response": scenarios::side_answer(&question), "synthetic": false}),
                );
            }
        }
    }
}

/// Where the demo's Claude writes: the window, as one session's output.
#[derive(Debug, Clone)]
pub struct Out {
    key: Generation,
    sink: Sink,
    session: String,
    pub cwd: String,
}

impl Out {
    /// One stream-json line, as `claude -p` writes it.
    pub fn line(&self, message: &Value) {
        self.sink.line(self.key, message.to_string());
    }

    pub fn session(&self) -> &str {
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
pub enum Said {
    /// The answer to the permission request or question it waits on.
    Answer(Value),
    Interrupt,
}

/// One session the demo's Claude runs.
#[derive(Debug)]
pub struct Session {
    out: Out,
    model: String,
    mode: String,
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
                session: launch.conversation.id().as_str().to_owned(),
                cwd: launch.cwd.display().to_string(),
            },
            model: launch.model.clone().unwrap_or_else(|| MODEL.to_owned()),
            mode: "default".to_owned(),
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
        let Ok(message) = serde_json::from_str::<Value>(line) else {
            return None;
        };
        match message.get("type").and_then(Value::as_str) {
            Some("control_request") => self.control(&message),
            Some("control_response") => {
                let answer = message
                    .pointer("/response/response")
                    .cloned()
                    .unwrap_or(Value::Null);
                self.tell_turn(Said::Answer(answer));
                None
            }
            Some("user") => Some(self.prompt(&message)),
            Some("bash_command") => {
                let command = message.get("command").and_then(Value::as_str);
                scenarios::shell(&self.out, command.unwrap_or_default());
                None
            }
            Some(_) | None => None,
        }
    }

    fn tell_turn(&self, said: Said) {
        if let Some(turn) = &self.turn
            && turn.try_send(said).is_err()
        {
            tracing::debug!("no turn is under way");
        }
    }

    fn control(&mut self, message: &Value) -> Option<Task> {
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
                return Some(Task::Side {
                    out: self.out.clone(),
                    request_id,
                    question: field("question"),
                });
            }
            "get_context_usage" => {
                json!({"totalTokens": 38_200, "maxTokens": 200_000, "percentage": 19})
            }
            _ => json!({}),
        };
        self.out.respond(&request_id, &response);
        None
    }

    /// A prompt: a turn plays the scenario it picks.
    fn prompt(&mut self, message: &Value) -> Task {
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
        Task::Turn { turn, prompt }
    }
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

    fn assistant(&self, content: &Value, id: &str) {
        self.out.line(&json!({
            "type": "assistant", "session_id": self.out.session(), "parent_tool_use_id": null,
            "message": {"id": id, "role": "assistant", "model": self.model, "content": content,
                        "usage": {"input_tokens": 2400, "cache_read_input_tokens": 18_000,
                                  "output_tokens": 350}},
        }));
    }

    /// Text, streamed as it is written, then whole.
    pub async fn say(&self, text: &str) -> Result<(), Interrupted> {
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
    pub fn call(&self, name: &str, input: &Value) -> String {
        let id = format!("toolu_{}", uuid::Uuid::new_v4().simple());
        self.assistant(
            &json!([{"type": "tool_use", "id": id, "name": name, "input": input}]),
            &format!("msg_{}", uuid::Uuid::new_v4().simple()),
        );
        id
    }

    pub fn result(&self, id: &str, content: &Value, error: bool) {
        self.out.line(&json!({
            "type": "user", "session_id": self.out.session(), "parent_tool_use_id": null,
            "message": {"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": id, "content": content, "is_error": error}]},
        }));
    }

    /// A tool call that needs no asking, its result after `milliseconds`.
    pub async fn run(
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
    pub async fn ask(
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
    pub fn finish(&self, text: &str) {
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
    pub fn interrupted(&self) {
        self.out.line(&json!({
            "type": "user", "parent_tool_use_id": null,
            "message": {"role": "user", "content": [
                {"type": "text", "text": "[Request interrupted by user]"}]},
        }));
        self.finish("interrupted");
    }

    /// The folder the turn works in, for the paths its tools name.
    pub fn path(&self, relative: &str) -> String {
        Path::new(&self.out.cwd)
            .join(relative)
            .display()
            .to_string()
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
