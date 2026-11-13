//! The adapter's core (sans-io): agy's Agent Client Protocol on one side, Claude Code's
//! stream-json on the other, as the window and the daemon read it. Each line read in says what
//! to write where; the adapter's loop (see `agy`) does it.
//!
//! A turn of agy's becomes what a turn of Claude Code's looks like: an `init`, streamed text
//! and thinking (`stream_event`s), an assistant message for each stretch of it with the tool
//! calls that end it, a tool result for each call, and a `result`. Permission requests become
//! `can_use_tool` requests, and the window's answers agy's options. Each line also goes to the
//! session's transcript, in Claude Code's transcript format, for the session to be listed and
//! shown again.

use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;

use serde_json::{Map, Value, json};

use crate::external::acp::{
    self, AgentMethod, Change, Chunk, Incoming, Notified, OptionKind, PermissionAsked, Prompted,
    StopReason, ToolCall, ToolContent, ToolKind, ToolStatus,
};
use crate::external::wire::SessionId;

/// What a denied tool call's result says, as Claude Code words it: the window shows such a
/// call as denied.
const DENIED: &str = "The user doesn't want to proceed with this tool use.";
/// The tool agy sees edualc's messaging tool as, by the MCP server's name and the tool's.
const AGY_SEND: &str = "mcp_edualc_send_message";
/// The same tool by the name Claude Code gives it, which the window knows.
const CLAUDE_SEND: &str = "mcp__edualc__send_message";
/// edualc's tool listing the sessions, as agy and as Claude Code name it.
const AGY_LIST: &str = "mcp_edualc_list_sessions";
const CLAUDE_LIST: &str = "mcp__edualc__list_sessions";
/// The envelope a message from another session comes in (see `outbox`).
const ENVELOPE_OPEN: &str = "<cross-session-message";
const ENVELOPE_CLOSE: &str = "</cross-session-message>";

/// A call of edualc's messaging tool, by the connection it came on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct CallId(pub(crate) u64);

/// How the session starts: afresh, or going on with a conversation of agy's.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Start {
    New,
    /// agy's id for the conversation, as the transcript keeps it.
    Resume(String),
}

/// What agy is given to start edualc's messaging tool: this program, and the socket the tool
/// reaches the adapter at.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ToolServer {
    pub(crate) program: PathBuf,
    pub(crate) socket: PathBuf,
}

/// What is to be done.
#[derive(Debug, PartialEq)]
pub(crate) enum Effect {
    /// A line to agy.
    ToAgy(Value),
    /// A line to the window, through the daemon.
    ToWindow(Value),
    /// A line for the transcript.
    Transcript(Value),
    /// A message: carry it out (see `outbox`) and answer the call.
    Send {
        call: CallId,
        to: String,
        message: String,
    },
    /// The running sessions, for the call.
    List(CallId),
}

/// Where the connection to agy is.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Phase {
    /// `initialize` is asked.
    Connecting,
    /// The session is asked for; a session loaded first replays its history, which the
    /// transcript already holds.
    Opening { loading: bool },
    /// agy's id for the session.
    Ready(String),
    /// It couldn't be opened; nothing more goes to agy.
    Failed,
}

/// What a request to agy was for, by its id.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Asked {
    Initialize,
    Open,
    /// The model; for the window's request, if it asked.
    Model(Option<String>),
    Mode(String),
    Prompt,
}

/// Something to be prompted with once agy is free.
#[derive(Debug, Clone, PartialEq)]
enum Queued {
    /// The user's prompt, as ACP takes it, and as the transcript keeps it.
    User { prompt: Vec<Value>, content: Value },
    /// Another session's message, in its envelope.
    Peer(String),
}

/// A message being streamed: the id it goes under and its blocks so far, the last one open.
#[derive(Debug, Default)]
struct Streaming {
    id: String,
    blocks: Vec<Block>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Block {
    Text(String),
    Thinking(String),
}

/// A tool call as the window was shown it.
#[derive(Debug, Clone, PartialEq)]
struct Shown {
    name: String,
    input: Value,
    done: bool,
    denied: bool,
    /// What edualc's tool answered, for a call of it: agy tells nothing of an MCP tool's
    /// result.
    told: Option<String>,
}

/// A permission request of agy's that the window was asked.
#[derive(Debug, Clone, PartialEq)]
struct Permission {
    id: Value,
    options: Vec<acp::PermissionOption>,
    tool: String,
}

#[derive(Debug)]
pub(crate) struct Translator {
    id: SessionId,
    cwd: PathBuf,
    start: Start,
    tool: ToolServer,
    phase: Phase,
    /// The model asked for, then the one in use.
    model: Option<String>,
    models: Vec<acp::Model>,
    mode: String,
    next_rpc: u64,
    asked: HashMap<u64, Asked>,
    /// What the window said before the session was open.
    held: Vec<String>,
    /// The window's `initialize`, answered once the session is open.
    initialize: Option<String>,
    /// A prompt is under way.
    busy: bool,
    queue: VecDeque<Queued>,
    streaming: Option<Streaming>,
    /// The turn's text, for its result.
    said: String,
    next_local: u64,
    shown: HashMap<String, Shown>,
    permissions: HashMap<String, Permission>,
    /// The tool call each of edualc's tool's calls is, by agy's id.
    calls: HashMap<CallId, String>,
}

impl Translator {
    pub(crate) fn new(
        id: SessionId,
        cwd: PathBuf,
        start: Start,
        model: Option<String>,
        tool: ToolServer,
    ) -> Self {
        Self {
            id,
            cwd,
            start,
            tool,
            phase: Phase::Connecting,
            model,
            models: Vec::new(),
            mode: "default".to_owned(),
            next_rpc: 0,
            asked: HashMap::new(),
            held: Vec::new(),
            initialize: None,
            busy: false,
            queue: VecDeque::new(),
            streaming: None,
            said: String::new(),
            next_local: 0,
            shown: HashMap::new(),
            permissions: HashMap::new(),
            calls: HashMap::new(),
        }
    }

    /// What starts it: agy is asked to initialize.
    pub(crate) fn start(&mut self) -> Vec<Effect> {
        let initialize = self.request(
            "initialize",
            json!({
                "protocolVersion": 1,
                "clientCapabilities": {
                    "fs": {"readTextFile": false, "writeTextFile": false},
                    "terminal": false
                },
                "clientInfo": {"name": "edualc", "version": env!("CARGO_PKG_VERSION")}
            }),
            Asked::Initialize,
        );
        vec![initialize]
    }

    fn request(&mut self, method: &str, params: Value, asked: Asked) -> Effect {
        self.next_rpc += 1;
        self.asked.insert(self.next_rpc, asked);
        let mut request = Map::new();
        request.insert("jsonrpc".to_owned(), json!("2.0"));
        request.insert("id".to_owned(), json!(self.next_rpc));
        request.insert("method".to_owned(), json!(method));
        request.insert("params".to_owned(), params);
        Effect::ToAgy(Value::Object(request))
    }

    fn local_id(&mut self, kind: &str) -> String {
        self.next_local += 1;
        format!("agy-{kind}-{}", self.next_local)
    }

    fn acp_session(&self) -> Option<&str> {
        match &self.phase {
            Phase::Ready(session) => Some(session),
            Phase::Connecting | Phase::Opening { .. } | Phase::Failed => None,
        }
    }

    /// A line from agy.
    pub(crate) fn agy_said(&mut self, line: &str) -> Vec<Effect> {
        let incoming = match serde_json::from_str::<Incoming>(line) {
            Ok(incoming) => incoming,
            Err(error) => {
                tracing::debug!(%error, "a line from agy that isn't JSON-RPC");
                return Vec::new();
            }
        };
        match incoming {
            Incoming::Response { id, result, error } => {
                let Some(asked) = id.as_u64().and_then(|id| self.asked.remove(&id)) else {
                    return Vec::new();
                };
                match error {
                    Some(error) => self.refused(&asked, &error.message),
                    None => self.answered(asked, result.unwrap_or(Value::Null)),
                }
            }
            Incoming::Notification {
                method: AgentMethod::SessionUpdate,
                params,
            } => match serde_json::from_value::<Notified>(params) {
                Ok(notified) => self.updated(notified.update),
                Err(error) => {
                    tracing::debug!(%error, "an update edualc doesn't read");
                    Vec::new()
                }
            },
            Incoming::Request {
                id,
                method: AgentMethod::RequestPermission,
                params,
            } => match serde_json::from_value::<PermissionAsked>(params) {
                Ok(asked) => self.asked_permission(id, asked),
                Err(error) => {
                    tracing::warn!(%error, "a permission request edualc can't read: denied");
                    vec![Effect::ToAgy(
                        json!({"jsonrpc": "2.0", "id": id, "result": {"outcome": {"outcome": "cancelled"}}}),
                    )]
                }
            },
            Incoming::Request { id, method, .. } => {
                tracing::debug!(?method, "a request of agy's edualc doesn't serve");
                vec![Effect::ToAgy(
                    json!({"jsonrpc": "2.0", "id": id, "error": {"code": -32601, "message": "edualc doesn't serve this"}}),
                )]
            }
            Incoming::Notification { method, .. } => {
                tracing::debug!(?method, "a notification edualc doesn't read");
                Vec::new()
            }
        }
    }

    /// agy's answer to a request.
    fn answered(&mut self, asked: Asked, result: Value) -> Vec<Effect> {
        match asked {
            Asked::Initialize => {
                let servers = json!([{
                    "name": "edualc",
                    "command": self.tool.program,
                    "args": [super::TOOL_FLAG, self.tool.socket],
                    "env": []
                }]);
                let (method, params, loading) = match &self.start {
                    Start::New => (
                        "session/new",
                        json!({"cwd": self.cwd, "mcpServers": servers}),
                        false,
                    ),
                    Start::Resume(session) => (
                        "session/load",
                        json!({"sessionId": session, "cwd": self.cwd, "mcpServers": servers}),
                        true,
                    ),
                };
                self.phase = Phase::Opening { loading };
                vec![self.request(method, params, Asked::Open)]
            }
            Asked::Open => {
                let opened: acp::Opened = serde_json::from_value(result).unwrap_or_default();
                // A load answers without the id: the session is the one asked for.
                let session = match (&self.start, opened.session_id) {
                    (Start::Resume(session), _) => session.clone(),
                    (Start::New, Some(session)) => session,
                    (Start::New, None) => return self.failed("agy opened no session"),
                };
                let mut effects = Vec::new();
                if self.start == Start::New {
                    effects.push(Effect::Transcript(json!({
                        "type": "agy-session",
                        "sessionId": self.id.to_string(),
                        "agySessionId": session,
                        "cwd": self.cwd,
                        "timestamp": now(),
                    })));
                }
                if let Some(models) = opened.models {
                    self.models = models.available_models;
                    if self.model.is_none() {
                        self.model = models.current_model_id;
                    }
                }
                self.phase = Phase::Ready(session.clone());
                if let Some(model) = self.model.clone() {
                    effects.push(self.request(
                        "session/set_model",
                        json!({"sessionId": session, "modelId": model}),
                        Asked::Model(None),
                    ));
                }
                if let Some(request) = self.initialize.take() {
                    effects.push(self.initialized(&request));
                }
                for line in std::mem::take(&mut self.held) {
                    effects.extend(self.window_said(&line));
                }
                effects
            }
            Asked::Model(window) => window
                .map(|request| vec![success(&request, &json!({}))])
                .unwrap_or_default(),
            Asked::Mode(request) => vec![success(&request, &json!({}))],
            Asked::Prompt => {
                let prompted = serde_json::from_value::<Prompted>(result).ok();
                self.turn_ended(prompted.as_ref(), None)
            }
        }
    }

    /// agy refused a request.
    fn refused(&mut self, asked: &Asked, message: &str) -> Vec<Effect> {
        match asked {
            Asked::Initialize | Asked::Open => self.failed(message),
            Asked::Model(Some(request)) | Asked::Mode(request) => vec![failure(request, message)],
            Asked::Model(None) => {
                tracing::warn!(message, "agy didn't take the model");
                Vec::new()
            }
            Asked::Prompt => self.turn_ended(None, Some(message)),
        }
    }

    fn failed(&mut self, message: &str) -> Vec<Effect> {
        self.phase = Phase::Failed;
        let mut effects = vec![Effect::ToWindow(json!({
            "type": "assistant",
            "message": {
                "id": "agy-failed",
                "role": "assistant",
                "content": [{"type": "text", "text": format!("agy couldn't open the session: {message}")}]
            },
            "parent_tool_use_id": null,
            "session_id": self.id.to_string(),
        }))];
        if let Some(request) = self.initialize.take() {
            effects.push(failure(&request, message));
        }
        effects
    }

    /// The answer to the window's `initialize`: agy's models, as the window lists Claude
    /// Code's.
    fn initialized(&self, request: &str) -> Effect {
        let models: Vec<Value> = self
            .models
            .iter()
            .map(|model| json!({"value": model.model_id, "displayName": model.name, "description": ""}))
            .collect();
        success(
            request,
            &json!({
                "models": models,
                "commands": [],
                "account": {"apiProvider": "agy"},
                "claude_code_version": format!("agy, through edualc {}", env!("CARGO_PKG_VERSION")),
            }),
        )
    }

    /// A line from the window.
    pub(crate) fn window_said(&mut self, line: &str) -> Vec<Effect> {
        let Ok(said) = serde_json::from_str::<Value>(line) else {
            return Vec::new();
        };
        if self.acp_session().is_none() {
            match &self.phase {
                Phase::Failed => {
                    return control_request_id(&said)
                        .map(|request| vec![failure(&request, "agy isn't running")])
                        .unwrap_or_default();
                }
                Phase::Connecting | Phase::Opening { .. } => {
                    if is_initialize(&said) {
                        self.initialize = control_request_id(&said);
                    } else {
                        self.held.push(line.to_owned());
                    }
                    return Vec::new();
                }
                Phase::Ready(_) => {}
            }
        }
        match said.get("type").and_then(Value::as_str) {
            Some("control_request") => self.window_request(&said),
            Some("control_response") => self.window_answered(&said),
            Some("user") => {
                let content = said
                    .pointer("/message/content")
                    .cloned()
                    .unwrap_or(Value::Null);
                let prompt = prompt_blocks(&content);
                if prompt.is_empty() {
                    return Vec::new();
                }
                self.queue.push_back(Queued::User { prompt, content });
                self.next_turn()
            }
            Some("bash_command") => {
                let command = said.get("command").and_then(Value::as_str).unwrap_or("");
                self.shell_refused(command)
            }
            Some(_) | None => Vec::new(),
        }
    }

    /// A `!` command: agy has no shell beside the conversation, so it is told as not run, in
    /// the rows Claude Code replays for one (its echo, then what it printed).
    fn shell_refused(&self, command: &str) -> Vec<Effect> {
        let row = |text: String| {
            Effect::ToWindow(json!({
                "type": "user",
                "isReplay": true,
                "message": {"role": "user", "content": text},
                "parent_tool_use_id": null,
                "session_id": self.id.to_string(),
            }))
        };
        vec![
            row(format!("<bash-input>{}</bash-input>", escaped(command))),
            row(
                "<bash-stdout></bash-stdout><bash-stderr>Not run: agy sessions don't run ! \
                 commands. Ask agy to run it, or run it in a terminal.</bash-stderr>\
                 <bash-exit-code>1</bash-exit-code>"
                    .to_owned(),
            ),
        ]
    }

    fn window_request(&mut self, said: &Value) -> Vec<Effect> {
        let Some(request) = control_request_id(said) else {
            return Vec::new();
        };
        let Some(session) = self.acp_session().map(str::to_owned) else {
            return vec![failure(&request, "agy isn't running")];
        };
        match said.pointer("/request/subtype").and_then(Value::as_str) {
            Some("initialize") => vec![self.initialized(&request)],
            Some("interrupt") => {
                let mut effects = vec![success(&request, &json!({}))];
                if self.busy {
                    effects.push(Effect::ToAgy(json!({
                        "jsonrpc": "2.0", "method": "session/cancel", "params": {"sessionId": session}
                    })));
                }
                effects
            }
            Some("set_model") => {
                let Some(model) = said.pointer("/request/model").and_then(Value::as_str) else {
                    return vec![failure(&request, "no model named")];
                };
                self.model = Some(model.to_owned());
                vec![self.request(
                    "session/set_model",
                    json!({"sessionId": session, "modelId": model}),
                    Asked::Model(Some(request)),
                )]
            }
            Some("set_permission_mode") => {
                let wanted = said.pointer("/request/mode").and_then(Value::as_str);
                let Some(mode) = wanted.and_then(agy_mode) else {
                    return vec![failure(&request, "agy has no such mode")];
                };
                if let Some(wanted) = wanted {
                    wanted.clone_into(&mut self.mode);
                }
                vec![self.request(
                    "session/set_mode",
                    json!({"sessionId": session, "modeId": mode}),
                    Asked::Mode(request),
                )]
            }
            Some(_) | None => vec![failure(&request, "agy doesn't do this")],
        }
    }

    /// The window's answer to a permission request: agy's, or the messaging tool's.
    fn window_answered(&mut self, said: &Value) -> Vec<Effect> {
        let Some(request) = said
            .pointer("/response/request_id")
            .and_then(Value::as_str)
            .map(str::to_owned)
        else {
            return Vec::new();
        };
        let behavior = said
            .pointer("/response/response/behavior")
            .and_then(Value::as_str);
        let allowed = behavior == Some("allow");
        let interrupt = said
            .pointer("/response/response/interrupt")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        if let Some(permission) = self.permissions.remove(&request) {
            let wanted = if allowed {
                OptionKind::AllowOnce
            } else {
                OptionKind::RejectOnce
            };
            if !allowed && let Some(shown) = self.shown.get_mut(&permission.tool) {
                shown.denied = true;
            }
            let outcome = permission
                .options
                .iter()
                .find(|option| option.kind == wanted)
                .map_or_else(
                    || json!({"outcome": "cancelled"}),
                    |option| json!({"outcome": "selected", "optionId": option.option_id}),
                );
            let mut effects = vec![Effect::ToAgy(
                json!({"jsonrpc": "2.0", "id": permission.id, "result": {"outcome": outcome}}),
            )];
            if interrupt && let Some(session) = self.acp_session() {
                effects.push(Effect::ToAgy(json!({
                    "jsonrpc": "2.0", "method": "session/cancel", "params": {"sessionId": session}
                })));
            }
            return effects;
        }
        Vec::new()
    }

    /// agy's messaging tool was called: the message goes without asking, as a Claude Code
    /// session's own do.
    pub(crate) fn tool_called(&mut self, call: CallId, to: String, message: String) -> Vec<Effect> {
        if let Some(id) = self.open_call(CLAUDE_SEND) {
            self.calls.insert(call, id);
        }
        vec![Effect::Send { call, to, message }]
    }

    /// agy's messaging tool asked for the running sessions: no question for that.
    pub(crate) fn listing_asked(&mut self, call: CallId) -> Vec<Effect> {
        if let Some(id) = self.open_call(CLAUDE_LIST) {
            self.calls.insert(call, id);
        }
        vec![Effect::List(call)]
    }

    /// The call of `tool` under way, by agy's id, that no answer was told for yet.
    fn open_call(&self, tool: &str) -> Option<String> {
        let told: Vec<&String> = self.calls.values().collect();
        self.shown
            .iter()
            .find(|(id, shown)| shown.name == tool && !shown.done && !told.contains(id))
            .map(|(id, _)| id.clone())
    }

    /// What edualc's tool answered `call`: the call's result, when agy says it ended.
    pub(crate) fn tool_told(&mut self, call: CallId, answer: &super::tool::Answer) {
        if let Some(shown) = self
            .calls
            .remove(&call)
            .and_then(|id| self.shown.get_mut(&id))
        {
            shown.told = Some(answer.text.clone());
        }
    }

    /// Another session's message came into the inbox: shown as such, and given to agy as a
    /// prompt once it is free.
    pub(crate) fn peer(&mut self, content: String) -> Vec<Effect> {
        let (name, body) = unwrapped(&content);
        let origin = json!({"kind": "peer", "name": name, "body": body});
        let mut effects = vec![
            Effect::ToWindow(json!({
                "type": "user",
                "isReplay": true,
                "origin": origin,
                "message": {"role": "user", "content": content},
                "parent_tool_use_id": null,
                "session_id": self.id.to_string(),
            })),
            self.transcript(json!({
                "type": "user",
                "isMeta": true,
                "origin": origin,
                "message": {"role": "user", "content": content},
            })),
        ];
        self.queue.push_back(Queued::Peer(content));
        effects.extend(self.next_turn());
        effects
    }

    /// Prompts agy with what waits, if it is free.
    fn next_turn(&mut self) -> Vec<Effect> {
        if self.busy {
            return Vec::new();
        }
        let Some(session) = self.acp_session().map(str::to_owned) else {
            return Vec::new();
        };
        let Some(queued) = self.queue.pop_front() else {
            return Vec::new();
        };
        let mut effects = vec![Effect::ToWindow(json!({
            "type": "system",
            "subtype": "init",
            "session_id": self.id.to_string(),
            "model": self.model.clone().unwrap_or_default(),
            "permissionMode": self.mode,
            "cwd": self.cwd,
            "tools": [],
        }))];
        let prompt = match queued {
            Queued::User { prompt, content } => {
                effects.push(self.transcript(json!({
                    "type": "user",
                    "message": {"role": "user", "content": content},
                })));
                prompt
            }
            Queued::Peer(text) => vec![json!({"type": "text", "text": text})],
        };
        self.busy = true;
        self.said.clear();
        effects.push(self.request(
            "session/prompt",
            json!({"sessionId": session, "prompt": prompt}),
            Asked::Prompt,
        ));
        effects
    }

    fn transcript(&self, mut line: Value) -> Effect {
        if let Some(fields) = line.as_object_mut() {
            fields.insert("sessionId".to_owned(), json!(self.id.to_string()));
            fields.insert("cwd".to_owned(), json!(self.cwd));
            fields.insert("timestamp".to_owned(), json!(now()));
        }
        Effect::Transcript(line)
    }

    fn stream_event(&self, event: &Value) -> Effect {
        Effect::ToWindow(json!({
            "type": "stream_event",
            "event": event,
            "parent_tool_use_id": null,
            "session_id": self.id.to_string(),
        }))
    }

    /// What changed in the session.
    fn updated(&mut self, update: Change) -> Vec<Effect> {
        if matches!(self.phase, Phase::Opening { loading: true }) {
            // A loaded session's history: the transcript has it.
            return Vec::new();
        }
        match update {
            Change::AgentMessageChunk {
                content: Chunk::Text { text },
            } => {
                self.said.push_str(&text);
                self.streamed(&Block::Text(text))
            }
            Change::AgentThoughtChunk {
                content: Chunk::Text { text },
            } => self.streamed(&Block::Thinking(text)),
            Change::ToolCall(call) => self.tool_call(&call),
            Change::ToolCallUpdate(call) => self.tool_call_update(&call),
            Change::SessionInfoUpdate { title: Some(title) } => {
                vec![self.transcript(json!({"type": "ai-title", "aiTitle": title}))]
            }
            Change::AgentMessageChunk { .. }
            | Change::AgentThoughtChunk { .. }
            | Change::UserMessageChunk
            | Change::SessionInfoUpdate { title: None }
            | Change::Other => Vec::new(),
        }
    }

    /// A piece of text or thinking, streamed into the message under way.
    fn streamed(&mut self, piece: &Block) -> Vec<Effect> {
        let mut effects = Vec::new();
        if self.streaming.is_none() {
            let id = self.local_id("message");
            effects
                .push(self.stream_event(&json!({"type": "message_start", "message": {"id": id}})));
            self.streaming = Some(Streaming {
                id,
                blocks: Vec::new(),
            });
        }
        let Some(streaming) = self.streaming.as_mut() else {
            return effects;
        };
        let same_kind = matches!(
            (streaming.blocks.last(), piece),
            (Some(Block::Text(_)), Block::Text(_)) | (Some(Block::Thinking(_)), Block::Thinking(_))
        );
        let mut events = Vec::new();
        if !same_kind {
            if let Some(open) = streaming.blocks.len().checked_sub(1) {
                events.push(json!({"type": "content_block_stop", "index": open}));
            }
            let start = match piece {
                Block::Text(_) => json!({"type": "text", "text": ""}),
                Block::Thinking(_) => json!({"type": "thinking", "thinking": ""}),
            };
            events.push(json!({"type": "content_block_start", "index": streaming.blocks.len(), "content_block": start}));
            streaming.blocks.push(match piece {
                Block::Text(_) => Block::Text(String::new()),
                Block::Thinking(_) => Block::Thinking(String::new()),
            });
        }
        let index = streaming.blocks.len().saturating_sub(1);
        let delta = match (piece, streaming.blocks.last_mut()) {
            (Block::Text(text), Some(Block::Text(grown))) => {
                grown.push_str(text);
                json!({"type": "text_delta", "text": text})
            }
            (Block::Thinking(text), Some(Block::Thinking(grown))) => {
                grown.push_str(text);
                json!({"type": "thinking_delta", "thinking": text})
            }
            (Block::Text(_) | Block::Thinking(_), _) => return effects,
        };
        events.push(json!({"type": "content_block_delta", "index": index, "delta": delta}));
        effects.extend(events.iter().map(|event| self.stream_event(event)));
        effects
    }

    /// Ends the message being streamed: its blocks, then `tool_use` if a tool call ends it, as
    /// one assistant message.
    fn message_done(&mut self, tool_use: Option<Value>) -> Vec<Effect> {
        let streaming = self.streaming.take();
        let mut effects = Vec::new();
        let (id, mut content) = match streaming {
            Some(Streaming { id, blocks }) => {
                if let Some(open) = blocks.len().checked_sub(1) {
                    effects.push(
                        self.stream_event(&json!({"type": "content_block_stop", "index": open})),
                    );
                }
                let content: Vec<Value> = blocks
                    .into_iter()
                    .map(|block| match block {
                        Block::Text(text) => json!({"type": "text", "text": text}),
                        Block::Thinking(thinking) => {
                            json!({"type": "thinking", "thinking": thinking})
                        }
                    })
                    .collect();
                (id, content)
            }
            None if tool_use.is_some() => (self.local_id("message"), Vec::new()),
            None => return effects,
        };
        content.extend(tool_use);
        let message = json!({"id": id, "role": "assistant", "content": content});
        effects.push(Effect::ToWindow(json!({
            "type": "assistant",
            "message": message,
            "parent_tool_use_id": null,
            "session_id": self.id.to_string(),
        })));
        effects.push(self.transcript(json!({"type": "assistant", "message": message})));
        effects
    }

    fn tool_call(&mut self, call: &ToolCall) -> Vec<Effect> {
        if self.shown.contains_key(&call.tool_call_id) {
            return self.tool_call_update(call);
        }
        let (name, input) = as_claude_tool(call);
        self.shown.insert(
            call.tool_call_id.clone(),
            Shown {
                name: name.clone(),
                input: input.clone(),
                done: false,
                denied: false,
                told: None,
            },
        );
        let tool_use =
            json!({"type": "tool_use", "id": call.tool_call_id, "name": name, "input": input});
        let mut effects = self.message_done(Some(tool_use));
        if matches!(
            call.status,
            Some(ToolStatus::Completed | ToolStatus::Failed)
        ) {
            effects.extend(self.tool_call_update(call));
        }
        effects
    }

    fn tool_call_update(&mut self, call: &ToolCall) -> Vec<Effect> {
        let failed = match call.status {
            Some(ToolStatus::Completed) => false,
            Some(ToolStatus::Failed) => true,
            Some(ToolStatus::Pending | ToolStatus::InProgress) | None => return Vec::new(),
        };
        let Some(shown) = self.shown.get_mut(&call.tool_call_id) else {
            return Vec::new();
        };
        if shown.done {
            return Vec::new();
        }
        shown.done = true;
        let text = match (&shown.told, shown.denied) {
            (_, true) => DENIED.to_owned(),
            (Some(told), false) => told.clone(),
            (None, false) => result_text(call, failed),
        };
        let message = json!({
            "role": "user",
            "content": [{"type": "tool_result", "tool_use_id": call.tool_call_id, "content": text, "is_error": failed || shown.denied}]
        });
        vec![
            Effect::ToWindow(json!({
                "type": "user",
                "message": message,
                "parent_tool_use_id": null,
                "session_id": self.id.to_string(),
            })),
            self.transcript(json!({"type": "user", "message": message})),
        ]
    }

    /// agy asks whether a tool call may go ahead: the window asks the user.
    fn asked_permission(&mut self, id: Value, asked: PermissionAsked) -> Vec<Effect> {
        let tool = asked.tool_call.tool_call_id.clone();
        let mut effects = Vec::new();
        if !self.shown.contains_key(&tool) {
            effects.extend(self.tool_call(&asked.tool_call));
        }
        let (name, input) = self.shown.get(&tool).map_or_else(
            || as_claude_tool(&asked.tool_call),
            |shown| (shown.name.clone(), shown.input.clone()),
        );
        let request = self.local_id("permission");
        self.permissions.insert(
            request.clone(),
            Permission {
                id,
                options: asked.options,
                tool: tool.clone(),
            },
        );
        effects.push(Effect::ToWindow(json!({
            "type": "control_request",
            "request_id": request,
            "request": {
                "subtype": "can_use_tool",
                "tool_name": name,
                "input": input,
                "tool_use_id": tool,
                "description": asked.tool_call.title.unwrap_or_default(),
                "permission_suggestions": [],
            },
        })));
        effects
    }

    /// The prompt's answer: the turn ends, as Claude Code ends one, and what waits goes next.
    fn turn_ended(&mut self, prompted: Option<&Prompted>, error: Option<&str>) -> Vec<Effect> {
        let mut effects = self.message_done(None);
        if let Some(error) = error {
            let id = self.local_id("message");
            let message = json!({"id": id, "role": "assistant", "content": [{"type": "text", "text": format!("agy: {error}")}]});
            effects.push(Effect::ToWindow(json!({
                "type": "assistant", "message": message, "parent_tool_use_id": null,
                "session_id": self.id.to_string(), "error": "unknown",
            })));
        }
        // Denied calls agy never answered for are done now.
        let cancelled =
            prompted.is_some_and(|prompted| prompted.stop_reason == StopReason::Cancelled);
        let open: Vec<String> = self
            .shown
            .iter()
            .filter(|(_, shown)| !shown.done)
            .map(|(id, _)| id.clone())
            .collect();
        for id in open {
            effects.extend(self.tool_call_update(&ToolCall {
                tool_call_id: id,
                status: Some(ToolStatus::Failed),
                ..ToolCall::default()
            }));
        }
        let failed = error.is_some() || cancelled;
        let usage = prompted
            .and_then(|prompted| prompted.usage)
            .unwrap_or_default();
        let model = self.model.clone().unwrap_or_else(|| "agy".to_owned());
        let mut model_usage = Map::new();
        model_usage.insert(
            model,
            json!({"inputTokens": usage.input_tokens, "outputTokens": usage.output_tokens, "contextWindow": null}),
        );
        effects.push(Effect::ToWindow(json!({
            "type": "result",
            "subtype": if failed { "error_during_execution" } else { "success" },
            "is_error": failed,
            "result": self.said,
            "errors": error.map(|error| vec![error.to_owned()]).unwrap_or_default(),
            "total_cost_usd": null,
            "modelUsage": model_usage,
            "usage": {
                "input_tokens": usage.input_tokens,
                "cache_read_input_tokens": usage.cached_read_tokens,
                "output_tokens": usage.output_tokens,
            },
            "session_id": self.id.to_string(),
        })));
        self.busy = false;
        self.said.clear();
        effects.extend(self.next_turn());
        effects
    }
}

/// Text as Claude Code puts it in its `<bash-…>` rows: `&`, `<` and `>` as entities.
fn escaped(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

fn now() -> String {
    jiff::Timestamp::now().to_string()
}

fn is_initialize(said: &Value) -> bool {
    said.get("type").and_then(Value::as_str) == Some("control_request")
        && said.pointer("/request/subtype").and_then(Value::as_str) == Some("initialize")
}

fn control_request_id(said: &Value) -> Option<String> {
    (said.get("type").and_then(Value::as_str) == Some("control_request"))
        .then(|| {
            said.get("request_id")
                .and_then(Value::as_str)
                .map(str::to_owned)
        })
        .flatten()
}

fn success(request: &str, response: &Value) -> Effect {
    Effect::ToWindow(json!({
        "type": "control_response",
        "response": {"subtype": "success", "request_id": request, "response": response}
    }))
}

fn failure(request: &str, error: &str) -> Effect {
    Effect::ToWindow(json!({
        "type": "control_response",
        "response": {"subtype": "error", "request_id": request, "error": error}
    }))
}

/// Claude Code's permission mode as agy names it; none for one agy hasn't.
fn agy_mode(mode: &str) -> Option<&'static str> {
    match mode {
        "default" => Some("default"),
        "acceptEdits" => Some("accept-edits"),
        "plan" => Some("plan"),
        "auto" => Some("auto"),
        _ => None,
    }
}

/// A prompt as the window sent it (text, or blocks of text and images), as ACP takes it.
fn prompt_blocks(content: &Value) -> Vec<Value> {
    match content {
        Value::String(text) => vec![json!({"type": "text", "text": text})],
        Value::Array(blocks) => blocks
            .iter()
            .filter_map(|block| match block.get("type").and_then(Value::as_str) {
                Some("text") => block
                    .get("text")
                    .map(|text| json!({"type": "text", "text": text})),
                Some("image") => {
                    let source = block.get("source")?;
                    Some(json!({
                        "type": "image",
                        "mimeType": source.get("media_type")?,
                        "data": source.get("data")?,
                    }))
                }
                Some(_) | None => None,
            })
            .collect(),
        Value::Null | Value::Bool(_) | Value::Number(_) | Value::Object(_) => Vec::new(),
    }
}

/// A tool call as the window knows Claude Code's: agy's file and command tools as Read, Write,
/// Edit and Bash, with their arguments by Claude Code's names; others by agy's name.
fn as_claude_tool(call: &ToolCall) -> (String, Value) {
    let input = call.raw_input.clone().unwrap_or(Value::Null);
    let field = |name: &str| input.get(name).cloned().unwrap_or(Value::Null);
    let path = || {
        input.get("Path").cloned().or_else(|| {
            call.locations
                .as_ref()
                .and_then(|locations| locations.first())
                .map(|location| json!(location.path))
        })
    };
    let title = || json!(call.title.clone().unwrap_or_default());
    match (call.name.as_deref(), call.kind) {
        (Some(AGY_SEND), _) => (
            CLAUDE_SEND.to_owned(),
            json!({"to": field("to"), "message": field("message")}),
        ),
        (Some(AGY_LIST), _) => (CLAUDE_LIST.to_owned(), json!({})),
        (Some("run_command"), _) | (None, Some(ToolKind::Execute)) => (
            "Bash".to_owned(),
            json!({
                "command": input.get("CommandLine").cloned().unwrap_or_else(title),
                "description": field("toolSummary"),
            }),
        ),
        (Some("read_file" | "view_file"), _) | (None, Some(ToolKind::Read)) => (
            "Read".to_owned(),
            json!({"file_path": path().unwrap_or_else(title)}),
        ),
        // Appending (agy's `Append`) adds to the file: as an edit that adds the lines, not a
        // Write, which would read as the whole file.
        (Some("write_file"), _) if input.get("Append") == Some(&Value::Bool(true)) => (
            "Edit".to_owned(),
            json!({
                "file_path": path().unwrap_or_else(title),
                "old_string": "",
                "new_string": field("Content"),
            }),
        ),
        (Some("write_file"), _) => (
            "Write".to_owned(),
            json!({"file_path": path().unwrap_or_else(title), "content": field("Content")}),
        ),
        (Some("edit_file"), _) => (
            "Edit".to_owned(),
            json!({
                "file_path": path().unwrap_or_else(title),
                "old_string": field("Target"),
                "new_string": field("Replacement"),
            }),
        ),
        (Some(name), _) => (name.to_owned(), input),
        (None, _) => (
            call.title.clone().unwrap_or_else(|| "tool".to_owned()),
            input,
        ),
    }
}

/// What a finished tool call gave, as text.
fn result_text(call: &ToolCall, failed: bool) -> String {
    let mut parts: Vec<String> = Vec::new();
    for content in call.content.iter().flatten() {
        match content {
            ToolContent::Content {
                content: Chunk::Text { text },
            } => parts.push(text.clone()),
            ToolContent::Diff { path, .. } => parts.push(format!("Changed {}", path.display())),
            ToolContent::Content {
                content: Chunk::Other,
            }
            | ToolContent::Other => {}
        }
    }
    if parts.is_empty()
        && let Some(output) = call
            .raw_output
            .as_ref()
            .and_then(|output| output.get("output"))
            .and_then(Value::as_str)
    {
        parts.push(output.to_owned());
    }
    if parts.is_empty() {
        return if failed { "Failed." } else { "Done." }.to_owned();
    }
    parts.join("\n")
}

/// The sender's name and the text of a message in another session's envelope (see `outbox`);
/// no name, and all of it, for one without.
fn unwrapped(content: &str) -> (Option<String>, String) {
    let Some(rest) = content.strip_prefix(ENVELOPE_OPEN) else {
        return (None, content.to_owned());
    };
    let Some((attributes, body)) = rest.split_once(">\n") else {
        return (None, content.to_owned());
    };
    let name = attributes
        .split_once("from-name=\"")
        .and_then(|(_, after)| after.split_once('"'))
        .map(|(name, _)| name.to_owned());
    let body = body
        .strip_suffix(ENVELOPE_CLOSE)
        .map_or(body, |body| body.strip_suffix('\n').unwrap_or(body))
        .replace("<\\/cross-session-message", "</cross-session-message");
    (name, body)
}

#[cfg(test)]
mod tests {
    use super::*;

    const ID: &str = "11111111-2222-4333-8444-555555555555";

    fn translator(start: Start) -> Translator {
        Translator::new(
            SessionId::try_from(ID.to_owned()).expect("an id"),
            PathBuf::from("/work"),
            start,
            Some("gemini-3.8-flash-high".to_owned()),
            ToolServer {
                program: PathBuf::from("/usr/bin/edualc"),
                socket: PathBuf::from("/run/user/1000/edualc-agy/tool.sock"),
            },
        )
    }

    fn window_lines(effects: &[Effect]) -> Vec<&Value> {
        effects
            .iter()
            .filter_map(|effect| match effect {
                Effect::ToWindow(line) => Some(line),
                Effect::ToAgy(_)
                | Effect::Transcript(_)
                | Effect::Send { .. }
                | Effect::List(_) => None,
            })
            .collect()
    }

    fn agy_lines(effects: &[Effect]) -> Vec<&Value> {
        effects
            .iter()
            .filter_map(|effect| match effect {
                Effect::ToAgy(line) => Some(line),
                Effect::ToWindow(_)
                | Effect::Transcript(_)
                | Effect::Send { .. }
                | Effect::List(_) => None,
            })
            .collect()
    }

    fn kinds(lines: &[&Value]) -> Vec<String> {
        lines
            .iter()
            .map(|line| {
                let kind = line.get("type").and_then(Value::as_str).unwrap_or_default();
                match line.pointer("/event/type").and_then(Value::as_str) {
                    Some(event) => format!("{kind}:{event}"),
                    None => kind.to_owned(),
                }
            })
            .collect()
    }

    /// A translator past `initialize` and `session/new`, its session `s`.
    fn opened() -> Translator {
        let mut translator = translator(Start::New);
        translator.start();
        translator.agy_said(r#"{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":1}}"#);
        translator.agy_said(
            r#"{"jsonrpc":"2.0","id":2,"result":{"sessionId":"s","models":{"availableModels":[{"modelId":"gemini-3.8-flash-high","name":"Gemini 3.8 Flash (High)"}],"currentModelId":"gemini-3.8-flash-high"}}}"#,
        );
        translator
    }

    /// The window's `initialize`, sent before the session is open, is answered once it is,
    /// with agy's models; a new session's transcript starts with agy's id for it.
    #[test]
    fn initialize_waits_for_the_session() {
        let mut translator = translator(Start::New);
        translator.start();
        let early = translator.window_said(
            r#"{"type":"control_request","request_id":"r1","request":{"subtype":"initialize"}}"#,
        );
        assert!(early.is_empty());
        let initialized =
            translator.agy_said(r#"{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":1}}"#);
        let asked = agy_lines(&initialized);
        assert_eq!(
            asked.first().and_then(|line| line.get("method")),
            Some(&json!("session/new"))
        );
        assert_eq!(
            asked
                .first()
                .and_then(|line| line.pointer("/params/mcpServers/0/args/0")),
            Some(&json!(super::super::TOOL_FLAG))
        );
        let opened = translator.agy_said(
            r#"{"jsonrpc":"2.0","id":2,"result":{"sessionId":"s","models":{"availableModels":[{"modelId":"m1","name":"Model one"}],"currentModelId":"m1"}}}"#,
        );
        let answer = window_lines(&opened)
            .into_iter()
            .find(|line| line.get("type") == Some(&json!("control_response")))
            .expect("initialize answered");
        assert_eq!(
            answer.pointer("/response/response/models/0/value"),
            Some(&json!("m1"))
        );
        let first = opened
            .iter()
            .find_map(|effect| match effect {
                Effect::Transcript(line) => Some(line),
                Effect::ToAgy(_) | Effect::ToWindow(_) | Effect::Send { .. } | Effect::List(_) => {
                    None
                }
            })
            .expect("a transcript line");
        assert_eq!(first.get("agySessionId"), Some(&json!("s")));
        assert_eq!(first.get("cwd"), Some(&json!("/work")));
    }

    /// A turn reads as Claude Code's: init, streamed text, the assistant message ending in the
    /// tool call, its result, then the result of the turn.
    #[test]
    fn a_turn_reads_as_claude_codes() {
        let mut translator = opened();
        let prompted = translator.window_said(
            r#"{"type":"user","message":{"role":"user","content":"Count the lines"},"parent_tool_use_id":null,"session_id":"x"}"#,
        );
        assert_eq!(kinds(&window_lines(&prompted)), ["system"]);
        let prompt = agy_lines(&prompted)
            .into_iter()
            .find(|line| line.get("method") == Some(&json!("session/prompt")))
            .expect("agy prompted");
        let prompt_id = prompt.get("id").cloned().expect("an id");
        let mut lines = Vec::new();
        for update in [
            r#"{"sessionUpdate":"agent_thought_chunk","content":{"type":"text","text":"Reading."}}"#,
            r#"{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"Let me count."}}"#,
            r#"{"sessionUpdate":"tool_call","toolCallId":"c1","name":"run_command","kind":"execute","status":"pending","title":"wc -l notes.txt","rawInput":{"CommandLine":"wc -l notes.txt","toolSummary":"Count"}}"#,
            r#"{"sessionUpdate":"tool_call_update","toolCallId":"c1","status":"completed","content":[{"type":"content","content":{"type":"text","text":"3 notes.txt"}}]}"#,
            r#"{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"Three."}}"#,
        ] {
            let line = format!(
                r#"{{"jsonrpc":"2.0","method":"session/update","params":{{"sessionId":"s","update":{update}}}}}"#
            );
            lines.extend(translator.agy_said(&line));
        }
        lines.extend(translator.agy_said(&format!(
            r#"{{"jsonrpc":"2.0","id":{prompt_id},"result":{{"stopReason":"end_turn","usage":{{"inputTokens":10,"outputTokens":2}}}}}}"#
        )));
        let window = window_lines(&lines);
        assert_eq!(
            kinds(&window),
            [
                "stream_event:message_start",
                "stream_event:content_block_start",
                "stream_event:content_block_delta",
                "stream_event:content_block_stop",
                "stream_event:content_block_start",
                "stream_event:content_block_delta",
                "stream_event:content_block_stop",
                "assistant",
                "user",
                "stream_event:message_start",
                "stream_event:content_block_start",
                "stream_event:content_block_delta",
                "stream_event:content_block_stop",
                "assistant",
                "result",
            ]
        );
        let tool_use = window
            .iter()
            .find_map(|line| line.pointer("/message/content/2"))
            .expect("the tool call");
        assert_eq!(tool_use.get("name"), Some(&json!("Bash")));
        assert_eq!(
            tool_use.pointer("/input/command"),
            Some(&json!("wc -l notes.txt"))
        );
        let result = window.last().expect("the result");
        assert_eq!(result.get("result"), Some(&json!("Let me count.Three.")));
        assert_eq!(result.get("is_error"), Some(&json!(false)));
    }

    /// agy's permission request is asked in the window; the answer picks agy's option, and a
    /// denied call's result says it was denied, as Claude Code's does.
    #[test]
    fn permissions_go_through_the_window() {
        let mut translator = opened();
        translator.window_said(r#"{"type":"user","message":{"role":"user","content":"go"}}"#);
        let asked = translator.agy_said(
            r#"{"jsonrpc":"2.0","method":"session/request_permission","params":{"sessionId":"s","toolCall":{"toolCallId":"c2","name":"run_command","kind":"execute","status":"pending","title":"touch /tmp/x","rawInput":{"CommandLine":"touch /tmp/x"}},"options":[{"optionId":"allow","name":"Allow","kind":"allow_once"},{"optionId":"deny","name":"Deny","kind":"reject_once"}]},"id":"7"}"#,
        );
        let request = window_lines(&asked)
            .into_iter()
            .find(|line| line.get("type") == Some(&json!("control_request")))
            .expect("the window asked");
        assert_eq!(request.pointer("/request/tool_name"), Some(&json!("Bash")));
        let request_id = request
            .get("request_id")
            .and_then(Value::as_str)
            .expect("an id");
        let denied = translator.window_said(&format!(
            r#"{{"type":"control_response","response":{{"subtype":"success","request_id":"{request_id}","response":{{"behavior":"deny","message":"no"}}}}}}"#
        ));
        assert_eq!(
            agy_lines(&denied)
                .first()
                .and_then(|line| line.pointer("/result/outcome/optionId")),
            Some(&json!("deny"))
        );
        let failed = translator.agy_said(
            r#"{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"s","update":{"sessionUpdate":"tool_call_update","toolCallId":"c2","status":"failed","content":[]}}}"#,
        );
        assert_eq!(
            window_lines(&failed)
                .first()
                .and_then(|line| line.pointer("/message/content/0/content")),
            Some(&json!(DENIED))
        );
    }

    /// A message from another session is shown as one, and prompts agy once its turn ends.
    #[test]
    fn a_peer_message_waits_for_its_turn() {
        let mut translator = opened();
        let first = translator
            .window_said(r#"{"type":"user","message":{"role":"user","content":"hello"}}"#);
        let prompt_id = agy_lines(&first)
            .into_iter()
            .find(|line| line.get("method") == Some(&json!("session/prompt")))
            .and_then(|line| line.get("id").cloned())
            .expect("prompted");
        let envelope = "<cross-session-message from-name=\"tidy-otter-work-12\" from-mode=\"prompting\">\nHi there\n</cross-session-message>";
        let peer = translator.peer(envelope.to_owned());
        let shown = window_lines(&peer);
        assert_eq!(
            shown.first().and_then(|line| line.pointer("/origin/name")),
            Some(&json!("tidy-otter-work-12"))
        );
        assert_eq!(
            shown.first().and_then(|line| line.pointer("/origin/body")),
            Some(&json!("Hi there"))
        );
        assert!(agy_lines(&peer).is_empty(), "agy is busy");
        let ended = translator.agy_said(&format!(
            r#"{{"jsonrpc":"2.0","id":{prompt_id},"result":{{"stopReason":"end_turn"}}}}"#
        ));
        let next = agy_lines(&ended)
            .into_iter()
            .find(|line| line.get("method") == Some(&json!("session/prompt")))
            .expect("the peer's message prompted");
        assert_eq!(
            next.pointer("/params/prompt/0/text"),
            Some(&json!(envelope))
        );
    }

    /// agy's messaging tool sends at once: nothing is asked in the window.
    #[test]
    fn messages_go_without_asking() {
        let mut translator = opened();
        let sent = translator.tool_called(CallId(4), "calm-heron".to_owned(), "hi".to_owned());
        assert_eq!(
            sent,
            [Effect::Send {
                call: CallId(4),
                to: "calm-heron".to_owned(),
                message: "hi".to_owned()
            }]
        );
    }

    /// agy says nothing of what an MCP tool gave: the call's result is what edualc's tool
    /// answered it.
    #[test]
    fn the_tools_answer_is_the_calls_result() {
        let mut translator = opened();
        translator.window_said(r#"{"type":"user","message":{"role":"user","content":"go"}}"#);
        translator.agy_said(
            r#"{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"s","update":{"sessionUpdate":"tool_call","toolCallId":"c3","name":"mcp_edualc_send_message","kind":"other","status":"in_progress","rawInput":{"to":"calm-heron","message":"hi"}}}}"#,
        );
        translator.tool_called(CallId(9), "calm-heron".to_owned(), "hi".to_owned());
        translator.tool_told(
            CallId(9),
            &super::super::tool::Answer {
                text: "Delivered to calm-heron.".to_owned(),
                error: false,
            },
        );
        let ended = translator.agy_said(
            r#"{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"s","update":{"sessionUpdate":"tool_call_update","toolCallId":"c3","status":"completed"}}}"#,
        );
        assert_eq!(
            window_lines(&ended)
                .first()
                .and_then(|line| line.pointer("/message/content/0/content")),
            Some(&json!("Delivered to calm-heron."))
        );
    }

    /// agy's file tools read as Claude Code's, an append as the lines it adds (agy 1.5.6's
    /// arguments, as logged).
    #[test]
    fn file_tools_read_as_claude_codes() {
        let call = |name: &str, input: Value| ToolCall {
            tool_call_id: "c".to_owned(),
            name: Some(name.to_owned()),
            raw_input: Some(input),
            ..ToolCall::default()
        };
        let (name, input) = as_claude_tool(&call(
            "write_file",
            json!({"Append": true, "Content": "three\n", "Path": "/w/f.txt"}),
        ));
        assert_eq!(name, "Edit");
        assert_eq!(input.get("old_string"), Some(&json!("")));
        assert_eq!(input.get("new_string"), Some(&json!("three\n")));
        let (name, input) = as_claude_tool(&call(
            "write_file",
            json!({"Content": "hi\n", "Overwrite": true, "Path": "/w/hello.txt"}),
        ));
        assert_eq!(name, "Write");
        assert_eq!(input.get("content"), Some(&json!("hi\n")));
        let (name, input) = as_claude_tool(&call(
            "edit_file",
            json!({"Path": "/w/f.txt", "Replacement": "two\nthree", "Target": "two"}),
        ));
        assert_eq!(name, "Edit");
        assert_eq!(input.get("old_string"), Some(&json!("two")));
    }

    /// A `!` command is told as not run, in the rows the window reads a command's run from,
    /// and nothing reaches agy.
    #[test]
    fn shell_commands_are_refused() {
        let mut translator = opened();
        let refused = translator.window_said(
            r#"{"type":"bash_command","command":"ls <dir>","uuid":"00000000-0000-4000-8000-000000000000"}"#,
        );
        assert!(agy_lines(&refused).is_empty());
        let rows: Vec<&str> = window_lines(&refused)
            .iter()
            .filter_map(|line| line.pointer("/message/content").and_then(Value::as_str))
            .collect();
        assert_eq!(
            rows.first(),
            Some(&"<bash-input>ls &lt;dir&gt;</bash-input>")
        );
        assert!(rows.get(1).is_some_and(|row| row.contains("Not run")));
    }

    /// A loaded session's replayed history isn't shown again: the transcript has it.
    #[test]
    fn a_loaded_sessions_history_isnt_repeated() {
        let mut translator = translator(Start::Resume("s".to_owned()));
        translator.start();
        let loading =
            translator.agy_said(r#"{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":1}}"#);
        assert_eq!(
            agy_lines(&loading)
                .first()
                .and_then(|line| line.get("method")),
            Some(&json!("session/load"))
        );
        let replayed = translator.agy_said(
            r#"{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"s","update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"old"}}}}"#,
        );
        assert!(replayed.is_empty());
    }
}
