//! The adapter's core (sans-io): the agent's Agent Client Protocol on one side, Claude Code's
//! stream-json on the other, as the window and the daemon read it. Each line read in says what
//! to write where; the adapter's loop (see [`crate::acp`]) does it.
//!
//! A turn of the agent's becomes what a turn of Claude Code's looks like: an `init`, streamed
//! text and thinking (`stream_event`s), an assistant message for each stretch of it with the
//! tool calls that end it, a tool result for each call, and a `result`. Permission requests
//! become `can_use_tool` requests, and the window's answers the agent's options. Each line also
//! goes to the session's transcript, in Claude Code's transcript format, for the session to be
//! listed and shown again.

use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;

use serde_json::{Map, Value, json};

use crate::external::acp::{
    self, AgentMethod, Change, Chunk, Incoming, Notified, OptionKind, PermissionAsked, Prompted,
    RpcError, StopReason, ToolCall, ToolContent, ToolKind, ToolStatus,
};
use crate::external::tool_input::{BashInput, KnownCall, ReadInput};
use crate::external::wire::{MediaType, SessionId};

use super::{Agent, CLAUDE_LIST, CLAUDE_SEND, first_location};

/// What a denied tool call's result says, as Claude Code words it: the window shows such a
/// call as denied.
const DENIED: &str = "The user doesn't want to proceed with this tool use.";
/// JSON-RPC's code for a method the receiver doesn't have.
const METHOD_NOT_FOUND: i64 = -32601;
/// The envelope a message from another session comes in (see [`super::outbox`]).
const ENVELOPE_OPEN: &str = "<cross-session-message";
const ENVELOPE_CLOSE: &str = "</cross-session-message>";

/// A call of edualc's messaging tool, by the connection it came on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct CallId(pub u64);

/// How the session starts: afresh, or going on with a conversation of the agent's.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Start {
    New,
    /// The agent's id for the conversation, as the transcript keeps it.
    Resume(String),
}

/// What the agent is given to start edualc's messaging tool: this program, and the socket the
/// tool reaches the adapter at.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolServer {
    pub program: PathBuf,
    pub socket: PathBuf,
}

/// What is to be done.
#[derive(Debug, PartialEq, Eq)]
pub enum Effect {
    /// A line to the agent.
    ToAgent(acp::ToAgent),
    /// A line to the window, through the daemon.
    ToWindow(Value),
    /// A line for the transcript.
    Transcript(Value),
    /// A message: carry it out (see [`super::outbox`]) and answer the call.
    Send {
        call: CallId,
        to: String,
        message: String,
    },
    /// The running sessions, for the call.
    List(CallId),
}

/// Where the connection to the agent is.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Phase {
    /// `initialize` is asked.
    Connecting,
    /// The session is asked for; a session loaded first replays its history, which the
    /// transcript already holds.
    Opening { loading: bool },
    /// The agent's id for the session.
    Ready(String),
    /// It couldn't be opened; nothing more goes to the agent.
    Failed,
}

/// Why a prompt's turn failed, as the window is told.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Failure<'a> {
    /// The agent said why: the window shows its words.
    Said(&'a str),
    /// The agent's sign-in was refused: the window offers to sign in and try again.
    SignIn(&'a str),
}

/// What a request to the agent was for, by its id.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Asked {
    Initialize,
    Open,
    /// The model; for the window's request, if it asked.
    Model(Option<String>),
    /// One of the agent's modes, for the window's request.
    Mode {
        request: String,
        mode: String,
    },
    Prompt,
}

/// Something to be prompted with once the agent is free.
#[derive(Debug, Clone, PartialEq)]
enum Queued {
    /// The user's prompt, as ACP takes it, and as the transcript keeps it.
    User {
        prompt: Vec<acp::PromptBlock>,
        content: Value,
    },
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

/// A tool call the agent told: kept until it has its arguments, then shown to the window.
#[derive(Debug, Clone)]
enum Entry {
    /// Not shown yet: the agent hasn't sent the call's arguments.
    Kept(ToolCall),
    /// Shown to the window.
    Shown(Shown),
}

/// A tool call as the window was shown it.
#[derive(Debug, Clone, PartialEq)]
struct Shown {
    name: String,
    input: Value,
    done: bool,
    denied: bool,
    /// What edualc's tool answered, for a call of it: the agent tells nothing of an MCP tool's
    /// result.
    told: Option<String>,
}

/// A permission request of the agent's that the window was asked.
#[derive(Debug, Clone, PartialEq)]
struct Permission {
    id: Value,
    options: Vec<acp::PermissionOption>,
    tool: String,
}

#[derive(Debug)]
pub struct Translator {
    agent: Agent,
    id: SessionId,
    cwd: PathBuf,
    start: Start,
    tool: ToolServer,
    phase: Phase,
    /// The model asked for, then the one in use.
    model: Option<String>,
    models: Vec<acp::Model>,
    /// The agent's modes, as it opened the session with them.
    modes: Vec<acp::SessionMode>,
    /// The mode the agent is in, by its id.
    mode: Option<String>,
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
    entries: HashMap<String, Entry>,
    permissions: HashMap<String, Permission>,
    /// The tool call each of edualc's tool's calls is, by the agent's id.
    calls: HashMap<CallId, String>,
}

impl Translator {
    pub fn new(
        agent: Agent,
        id: SessionId,
        cwd: PathBuf,
        start: Start,
        model: Option<String>,
        tool: ToolServer,
    ) -> Self {
        Self {
            agent,
            id,
            cwd,
            start,
            tool,
            phase: Phase::Connecting,
            model,
            models: Vec::new(),
            modes: Vec::new(),
            mode: None,
            next_rpc: 0,
            asked: HashMap::new(),
            held: Vec::new(),
            initialize: None,
            busy: false,
            queue: VecDeque::new(),
            streaming: None,
            said: String::new(),
            next_local: 0,
            entries: HashMap::new(),
            permissions: HashMap::new(),
            calls: HashMap::new(),
        }
    }

    /// What starts it: the agent is asked to initialize.
    pub fn start(&mut self) -> Vec<Effect> {
        let initialize = self.request(
            acp::ClientCall::Initialize(acp::InitializeParams {
                protocol_version: 1,
                client_capabilities: acp::ClientCapabilities {
                    fs: acp::FileCapabilities {
                        read_text_file: false,
                        write_text_file: false,
                    },
                    terminal: false,
                },
                client_info: acp::ClientInfo {
                    name: "edualc".to_owned(),
                    version: env!("CARGO_PKG_VERSION").to_owned(),
                },
            }),
            Asked::Initialize,
        );
        vec![initialize]
    }

    fn request(&mut self, call: acp::ClientCall, asked: Asked) -> Effect {
        self.next_rpc += 1;
        self.asked.insert(self.next_rpc, asked);
        Effect::ToAgent(acp::ToAgent::Request(acp::Request {
            jsonrpc: acp::JsonRpc::TwoPointZero,
            id: self.next_rpc,
            call,
        }))
    }

    /// The MCP servers the agent starts for the session: edualc's messaging tool.
    fn mcp_servers(&self) -> Vec<acp::McpServer> {
        vec![acp::McpServer {
            name: "edualc".to_owned(),
            command: self.tool.program.clone(),
            args: vec![PathBuf::from(super::TOOL_FLAG), self.tool.socket.clone()],
            env: Vec::new(),
        }]
    }

    /// An answer to a request of the agent's, under the id it came with.
    fn answer(id: Value, reply: acp::Reply<acp::PermissionAnswer>) -> Effect {
        Effect::ToAgent(acp::ToAgent::Answer(acp::Answer {
            jsonrpc: acp::JsonRpc::TwoPointZero,
            id,
            reply,
        }))
    }

    /// Tells the agent to stop the turn under way.
    fn cancel(session: String) -> Effect {
        Effect::ToAgent(acp::ToAgent::Notice(acp::Notice {
            jsonrpc: acp::JsonRpc::TwoPointZero,
            notice: acp::ClientNotice::Cancel(acp::CancelParams {
                session_id: session,
            }),
        }))
    }

    fn local_id(&mut self, kind: &str) -> String {
        self.next_local += 1;
        format!("{}-{kind}-{}", self.agent, self.next_local)
    }

    fn acp_session(&self) -> Option<&str> {
        match &self.phase {
            Phase::Ready(session) => Some(session),
            Phase::Connecting | Phase::Opening { .. } | Phase::Failed => None,
        }
    }

    /// A line from the agent.
    pub fn agent_said(&mut self, line: &str) -> Vec<Effect> {
        let incoming = match serde_json::from_str::<Incoming>(line) {
            Ok(incoming) => incoming,
            Err(error) => {
                tracing::debug!(%error, "a line from the agent that isn't JSON-RPC");
                return Vec::new();
            }
        };
        match incoming {
            Incoming::Response { id, result, error } => {
                let Some(asked) = id.as_u64().and_then(|id| self.asked.remove(&id)) else {
                    return Vec::new();
                };
                match error {
                    Some(error) => self.refused(&asked, &error),
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
                    vec![Self::answer(
                        id,
                        acp::Reply::Result(acp::PermissionAnswer {
                            outcome: acp::Outcome::Cancelled,
                        }),
                    )]
                }
            },
            Incoming::Request { id, method, .. } => {
                tracing::debug!(?method, "a request of the agent's edualc doesn't serve");
                vec![Self::answer(
                    id,
                    acp::Reply::Error(acp::RpcErrorBody {
                        code: METHOD_NOT_FOUND,
                        message: "edualc doesn't serve this".to_owned(),
                    }),
                )]
            }
            Incoming::Notification { method, .. } => {
                tracing::debug!(?method, "a notification edualc doesn't read");
                Vec::new()
            }
        }
    }

    /// The agent's answer to a request.
    fn answered(&mut self, asked: Asked, result: Value) -> Vec<Effect> {
        match asked {
            Asked::Initialize => {
                let (call, loading) = match &self.start {
                    Start::New => (
                        acp::ClientCall::NewSession(acp::NewSessionParams {
                            cwd: self.cwd.clone(),
                            mcp_servers: self.mcp_servers(),
                        }),
                        false,
                    ),
                    Start::Resume(session) => (
                        acp::ClientCall::LoadSession(acp::LoadSessionParams {
                            session_id: session.clone(),
                            cwd: self.cwd.clone(),
                            mcp_servers: self.mcp_servers(),
                        }),
                        true,
                    ),
                };
                self.phase = Phase::Opening { loading };
                vec![self.request(call, Asked::Open)]
            }
            Asked::Open => {
                let opened: acp::Opened = serde_json::from_value(result).unwrap_or_default();
                // A load answers without the id: the session is the one asked for.
                let session = match (&self.start, opened.session_id) {
                    (Start::Resume(session), _) => session.clone(),
                    (Start::New, Some(session)) => session,
                    (Start::New, None) => {
                        return self.failed(&format!("{} opened no session", self.agent));
                    }
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
                if let Some(modes) = opened.modes {
                    self.modes = modes.available_modes;
                    self.mode = modes.current_mode_id;
                }
                self.phase = Phase::Ready(session.clone());
                if let Some(model) = self.model.clone() {
                    effects.push(self.request(
                        acp::ClientCall::SetModel(acp::SetModelParams {
                            session_id: session,
                            model_id: model,
                        }),
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
            Asked::Mode { request, mode } => {
                self.mode = Some(mode);
                vec![success(&request, &json!({}))]
            }
            Asked::Prompt => {
                let prompted = serde_json::from_value::<Prompted>(result).ok();
                self.turn_ended(prompted.as_ref(), None)
            }
        }
    }

    /// The agent refused a request.
    fn refused(&mut self, asked: &Asked, error: &RpcError) -> Vec<Effect> {
        let message = error.message.as_str();
        match asked {
            Asked::Initialize | Asked::Open => self.failed(message),
            Asked::Model(Some(request)) | Asked::Mode { request, .. } => {
                vec![failure(request, message)]
            }
            Asked::Model(None) => {
                tracing::warn!(message, agent = %self.agent, "the agent didn't take the model");
                Vec::new()
            }
            Asked::Prompt if self.agent.sign_in_refused(error) => {
                self.turn_ended(None, Some(Failure::SignIn(message)))
            }
            Asked::Prompt => self.turn_ended(None, Some(Failure::Said(message))),
        }
    }

    fn failed(&mut self, message: &str) -> Vec<Effect> {
        self.phase = Phase::Failed;
        let agent = self.agent;
        let mut effects = vec![Effect::ToWindow(json!({
            "type": "assistant",
            "message": {
                "id": format!("{agent}-failed"),
                "role": "assistant",
                "content": [{"type": "text", "text": format!("{agent} couldn't open the session: {message}")}]
            },
            "parent_tool_use_id": null,
            "session_id": self.id.to_string(),
        }))];
        if let Some(request) = self.initialize.take() {
            effects.push(failure(&request, message));
        }
        effects
    }

    /// The answer to the window's `initialize`: the agent's models, and its own modes with the
    /// one it is in.
    fn initialized(&self, request: &str) -> Effect {
        let models: Vec<Value> = self
            .models
            .iter()
            .map(|model| json!({"value": model.model_id, "displayName": model.name, "description": ""}))
            .collect();
        let modes: Vec<Value> = self
            .modes
            .iter()
            .map(|mode| json!({"id": mode.id, "name": mode.name, "description": mode.description.clone().unwrap_or_default()}))
            .collect();
        let agent = self.agent;
        success(
            request,
            &json!({
                "models": models,
                "programModes": {"offered": modes, "current": self.mode},
                "commands": [],
                "account": {"apiProvider": agent.to_string()},
                "claude_code_version": format!("{agent}, through edualc {}", env!("CARGO_PKG_VERSION")),
            }),
        )
    }

    /// A line from the window.
    pub fn window_said(&mut self, line: &str) -> Vec<Effect> {
        let Ok(said) = serde_json::from_str::<Value>(line) else {
            return Vec::new();
        };
        if self.acp_session().is_none() {
            match &self.phase {
                Phase::Failed => {
                    return control_request_id(&said)
                        .map(|request| {
                            vec![failure(&request, &format!("{} isn't running", self.agent))]
                        })
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

    /// A `!` command: the agent has no shell beside the conversation, so it is told as not run,
    /// in the rows Claude Code replays for one (its echo, then what it printed).
    fn shell_refused(&self, command: &str) -> Vec<Effect> {
        let agent = self.agent;
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
            row(format!(
                "<bash-stdout></bash-stdout><bash-stderr>Not run: {agent} sessions don't run ! \
                 commands. Ask {agent} to run it, or run it in a terminal.</bash-stderr>\
                 <bash-exit-code>1</bash-exit-code>"
            )),
        ]
    }

    fn window_request(&mut self, said: &Value) -> Vec<Effect> {
        let Some(request) = control_request_id(said) else {
            return Vec::new();
        };
        let Some(session) = self.acp_session().map(str::to_owned) else {
            return vec![failure(&request, &format!("{} isn't running", self.agent))];
        };
        match said.pointer("/request/subtype").and_then(Value::as_str) {
            Some("initialize") => vec![self.initialized(&request)],
            Some("interrupt") => {
                let mut effects = vec![success(&request, &json!({}))];
                if self.busy {
                    effects.push(Self::cancel(session));
                }
                effects
            }
            Some("set_model") => {
                let Some(model) = said.pointer("/request/model").and_then(Value::as_str) else {
                    return vec![failure(&request, "no model named")];
                };
                self.model = Some(model.to_owned());
                vec![self.request(
                    acp::ClientCall::SetModel(acp::SetModelParams {
                        session_id: session,
                        model_id: model.to_owned(),
                    }),
                    Asked::Model(Some(request)),
                )]
            }
            Some("set_program_mode") => {
                let Some(mode) = said.pointer("/request/mode").and_then(Value::as_str) else {
                    return vec![failure(&request, "no mode named")];
                };
                vec![self.request(
                    acp::ClientCall::SetMode(acp::SetModeParams {
                        session_id: session,
                        mode_id: mode.to_owned(),
                    }),
                    Asked::Mode {
                        request,
                        mode: mode.to_owned(),
                    },
                )]
            }
            Some("set_permission_mode") => {
                vec![failure(
                    &request,
                    &format!("{} has modes of its own, not Claude Code's", self.agent),
                )]
            }
            Some(_) | None => vec![failure(
                &request,
                &format!("{} doesn't do this", self.agent),
            )],
        }
    }

    /// The window's answer to a permission request: the agent's, or the messaging tool's.
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
            if !allowed && let Some(Entry::Shown(shown)) = self.entries.get_mut(&permission.tool) {
                shown.denied = true;
            }
            let outcome = outcome_of(&permission.options, wanted);
            let mut effects = vec![Self::answer(
                permission.id,
                acp::Reply::Result(acp::PermissionAnswer { outcome }),
            )];
            if interrupt && let Some(session) = self.acp_session() {
                effects.push(Self::cancel(session.to_owned()));
            }
            return effects;
        }
        Vec::new()
    }

    /// The agent's messaging tool was called: the message goes without asking, as a Claude Code
    /// session's own do.
    pub fn tool_called(&mut self, call: CallId, to: String, message: String) -> Vec<Effect> {
        if let Some(id) = self.open_call(CLAUDE_SEND) {
            self.calls.insert(call, id);
        }
        vec![Effect::Send { call, to, message }]
    }

    /// The agent's messaging tool asked for the running sessions: no question for that.
    pub fn listing_asked(&mut self, call: CallId) -> Vec<Effect> {
        if let Some(id) = self.open_call(CLAUDE_LIST) {
            self.calls.insert(call, id);
        }
        vec![Effect::List(call)]
    }

    /// The call of `tool` under way, by the agent's id, that no answer was told for yet.
    fn open_call(&self, tool: &str) -> Option<String> {
        let told: Vec<&String> = self.calls.values().collect();
        self.entries.iter().find_map(|(id, entry)| match entry {
            Entry::Shown(shown) if shown.name == tool && !shown.done && !told.contains(&id) => {
                Some(id.clone())
            }
            Entry::Kept(_) | Entry::Shown(_) => None,
        })
    }

    /// What edualc's tool answered `call`: the call's result, when the agent says it ended.
    pub fn tool_told(&mut self, call: CallId, answer: &super::tool::Answer) {
        if let Some(Entry::Shown(shown)) = self
            .calls
            .remove(&call)
            .and_then(|id| self.entries.get_mut(&id))
        {
            shown.told = Some(answer.text.clone());
        }
    }

    /// Another session's message came into the inbox: shown as such, and given to the agent as
    /// a prompt once it is free.
    pub fn peer(&mut self, content: String) -> Vec<Effect> {
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

    /// Prompts the agent with what waits, if it is free.
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
            "permissionMode": self.mode.clone().unwrap_or_default(),
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
            Queued::Peer(text) => vec![acp::PromptBlock::Text { text }],
        };
        self.busy = true;
        self.said.clear();
        effects.push(self.request(
            acp::ClientCall::Prompt(acp::PromptParams {
                session_id: session,
                prompt,
            }),
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
            Change::CurrentModeUpdate { current_mode_id } => {
                let effect = Effect::ToWindow(json!({
                    "type": "system",
                    "subtype": "program_mode",
                    "mode": current_mode_id,
                    "session_id": self.id.to_string(),
                }));
                self.mode = Some(current_mode_id);
                vec![effect]
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
        match self.entries.get(&call.tool_call_id) {
            Some(Entry::Kept(kept)) => {
                let merged = merged(kept, call);
                return self.call_updated(merged);
            }
            Some(Entry::Shown(_)) => return self.tool_call_update(call),
            None => {}
        }
        self.call_updated(call.clone())
    }

    /// A tool call's fields have come: kept until it has its arguments or is done, then shown.
    fn call_updated(&mut self, call: ToolCall) -> Vec<Effect> {
        if has_arguments(self.agent, &call) || is_done(&call) {
            self.show_call(&call)
        } else {
            self.entries
                .insert(call.tool_call_id.clone(), Entry::Kept(call));
            Vec::new()
        }
    }

    fn show_call(&mut self, call: &ToolCall) -> Vec<Effect> {
        let (name, input) = as_claude_tool(self.agent, call);
        self.entries.insert(
            call.tool_call_id.clone(),
            Entry::Shown(Shown {
                name: name.clone(),
                input: input.clone(),
                done: false,
                denied: false,
                told: None,
            }),
        );
        let tool_use =
            json!({"type": "tool_use", "id": call.tool_call_id, "name": name, "input": input});
        let mut effects = self.message_done(Some(tool_use));
        if is_done(call) {
            effects.extend(self.tool_call_update(call));
        }
        effects
    }

    fn tool_call_update(&mut self, call: &ToolCall) -> Vec<Effect> {
        if let Some(Entry::Kept(kept)) = self.entries.get(&call.tool_call_id) {
            let merged = merged(kept, call);
            return self.call_updated(merged);
        }
        let failed = match call.status {
            Some(ToolStatus::Completed) => false,
            Some(ToolStatus::Failed) => true,
            Some(ToolStatus::Pending | ToolStatus::InProgress) | None => return Vec::new(),
        };
        let Some(Entry::Shown(shown)) = self.entries.get_mut(&call.tool_call_id) else {
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

    /// The agent asks whether a tool call may go ahead: the window asks the user.
    fn asked_permission(&mut self, id: Value, asked: PermissionAsked) -> Vec<Effect> {
        let tool = asked.tool_call.tool_call_id.clone();
        let mut effects = Vec::new();
        match self.entries.get(&tool) {
            Some(Entry::Kept(kept)) => {
                let call = merged(kept, &asked.tool_call);
                effects.extend(self.show_call(&call));
            }
            Some(Entry::Shown(_)) => {}
            None => effects.extend(self.show_call(&asked.tool_call)),
        }
        let (name, input) = match self.entries.get(&tool) {
            Some(Entry::Shown(shown)) => (shown.name.clone(), shown.input.clone()),
            Some(Entry::Kept(_)) | None => as_claude_tool(self.agent, &asked.tool_call),
        };
        // The agent asks before every MCP tool, in every mode. edualc's own go without asking,
        // as a Claude Code session's messages do.
        if name == CLAUDE_SEND || name == CLAUDE_LIST {
            let outcome = outcome_of(&asked.options, OptionKind::AllowOnce);
            effects.push(Self::answer(
                id,
                acp::Reply::Result(acp::PermissionAnswer { outcome }),
            ));
            return effects;
        }
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

    /// The prompt's answer: the turn ends, and what waits goes next. A refused sign-in is told
    /// as an HTTP 401 in the turn's result, which is what it was, and the window offers to sign
    /// in and try again instead of showing the agent's words.
    fn turn_ended(&mut self, prompted: Option<&Prompted>, failure: Option<Failure>) -> Vec<Effect> {
        let mut effects = self.message_done(None);
        let error = failure.map(|failure| match failure {
            Failure::Said(message) | Failure::SignIn(message) => message,
        });
        let agent = self.agent;
        if let Some(Failure::Said(error)) = failure {
            let id = self.local_id("message");
            let message = json!({"id": id, "role": "assistant", "content": [{"type": "text", "text": format!("{agent}: {error}")}]});
            effects.push(Effect::ToWindow(json!({
                "type": "assistant", "message": message, "parent_tool_use_id": null,
                "session_id": self.id.to_string(), "error": "unknown",
            })));
        }
        // Calls the agent never answered for, or never gave arguments for, are done now.
        let cancelled =
            prompted.is_some_and(|prompted| prompted.stop_reason == StopReason::Cancelled);
        let open: Vec<String> = self
            .entries
            .iter()
            .filter_map(|(id, entry)| match entry {
                Entry::Shown(shown) if !shown.done => Some(id.clone()),
                Entry::Kept(_) => Some(id.clone()),
                Entry::Shown(_) => None,
            })
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
        let model = self.model.clone().unwrap_or_else(|| agent.to_string());
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
            "api_error_status": matches!(failure, Some(Failure::SignIn(_))).then_some(401),
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

/// A prompt as the window sent it (text, or blocks of text and images), as ACP takes it.
fn prompt_blocks(content: &Value) -> Vec<acp::PromptBlock> {
    match content {
        Value::String(text) => vec![acp::PromptBlock::Text { text: text.clone() }],
        Value::Array(blocks) => blocks.iter().filter_map(prompt_block).collect(),
        Value::Null | Value::Bool(_) | Value::Number(_) | Value::Object(_) => Vec::new(),
    }
}

/// One block of the window's prompt, if it is text or an image edualc can send.
fn prompt_block(block: &Value) -> Option<acp::PromptBlock> {
    match block.get("type").and_then(Value::as_str) {
        Some("text") => Some(acp::PromptBlock::Text {
            text: block.get("text")?.as_str()?.to_owned(),
        }),
        Some("image") => {
            let source = block.get("source")?;
            let media_type = source.get("media_type")?.as_str()?;
            let Ok(mime_type) = media_type.parse::<MediaType>() else {
                tracing::debug!(
                    media_type,
                    "an image of a type edualc doesn't send: left out"
                );
                return None;
            };
            Some(acp::PromptBlock::Image {
                mime_type,
                data: source.get("data")?.as_str()?.to_owned(),
            })
        }
        Some(_) | None => None,
    }
}

/// The outcome of a permission request: the first option of `wanted`'s kind, or none.
fn outcome_of(options: &[acp::PermissionOption], wanted: OptionKind) -> acp::Outcome {
    options
        .iter()
        .find(|option| option.kind == wanted)
        .map_or(acp::Outcome::Cancelled, |option| acp::Outcome::Selected {
            option_id: option.option_id.clone(),
        })
}

/// The tool call `kept`, with each field `update` has replacing it.
fn merged(kept: &ToolCall, update: &ToolCall) -> ToolCall {
    let mut call = kept.clone();
    let update = update.clone();
    if update.name.is_some() {
        call.name = update.name;
    }
    if update.title.is_some() {
        call.title = update.title;
    }
    if update.kind.is_some() {
        call.kind = update.kind;
    }
    if update.status.is_some() {
        call.status = update.status;
    }
    if update.raw_input.is_some() {
        call.raw_input = update.raw_input;
    }
    if update.raw_output.is_some() {
        call.raw_output = update.raw_output;
    }
    if update.content.is_some() {
        call.content = update.content;
    }
    if update.locations.is_some() {
        call.locations = update.locations;
    }
    call
}

/// Whether a tool call's arguments have come: an argument the agent sends before them (see
/// [`Agent::early_arguments`]) doesn't count.
fn has_arguments(agent: Agent, call: &ToolCall) -> bool {
    match &call.raw_input {
        None => false,
        Some(Value::Object(fields)) => fields
            .keys()
            .any(|field| !agent.early_arguments().contains(&field.as_str())),
        Some(_) => true,
    }
}

fn is_done(call: &ToolCall) -> bool {
    matches!(
        call.status,
        Some(ToolStatus::Completed | ToolStatus::Failed)
    )
}

/// A tool call as the window knows Claude Code's: by the agent's own names where it gives one
/// (see [`Agent::tool`]), else by ACP's kind; a tool neither tells about keeps the agent's name.
fn as_claude_tool(agent: Agent, call: &ToolCall) -> (String, Value) {
    let input = call.raw_input.clone().unwrap_or(Value::Null);
    if let Some(known) = call.name.as_deref().and_then(|name| agent.tool(name, call)) {
        return as_known(&known, input);
    }
    let title = || call.title.clone().unwrap_or_default();
    let known = match (call.name.as_deref(), call.kind) {
        (Some(name), _) => return (name.to_owned(), input),
        (None, Some(ToolKind::Execute)) => KnownCall::Bash(BashInput {
            command: title(),
            description: None,
        }),
        (None, Some(ToolKind::Read)) => KnownCall::Read(ReadInput {
            file_path: first_location(call).unwrap_or_else(|| PathBuf::from(title())),
        }),
        (None, _) => {
            return (
                call.title.clone().unwrap_or_else(|| "tool".to_owned()),
                input,
            );
        }
    };
    as_known(&known, input)
}

/// A known call by Claude Code's name and input; one whose input can't be written (a path
/// that isn't UTF-8) shows the agent's own input instead.
fn as_known(known: &KnownCall, raw: Value) -> (String, Value) {
    let input = known.input().unwrap_or_else(|error| {
        tracing::warn!(%error, tool = %known.name(), "a tool's input that can't be written");
        raw
    });
    (known.name().to_string(), input)
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

/// The sender's name and the text of a message in another session's envelope (see
/// [`super::outbox`]);
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
    use crate::external::acp::Location;

    const ID: &str = "11111111-2222-4333-8444-555555555555";

    fn translator(start: Start) -> Translator {
        translator_for(Agent::Agy, start)
    }

    fn translator_for(agent: Agent, start: Start) -> Translator {
        Translator::new(
            agent,
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
                Effect::ToAgent(_)
                | Effect::Transcript(_)
                | Effect::Send { .. }
                | Effect::List(_) => None,
            })
            .collect()
    }

    /// The lines the agent is sent, as the JSON it reads.
    fn agent_lines(effects: &[Effect]) -> Vec<Value> {
        effects
            .iter()
            .filter_map(|effect| match effect {
                Effect::ToAgent(line) => Some(line.to_line().expect("a line for the agent")),
                Effect::ToWindow(_)
                | Effect::Transcript(_)
                | Effect::Send { .. }
                | Effect::List(_) => None,
            })
            .map(|line| serde_json::from_str(&line).expect("a line of JSON"))
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

    /// The id of the last `session/prompt` among `effects`.
    fn prompt_id(effects: &[Effect]) -> u64 {
        agent_lines(effects)
            .into_iter()
            .rev()
            .find(|line| line.get("method") == Some(&json!("session/prompt")))
            .and_then(|line| line.get("id").and_then(Value::as_u64))
            .expect("a prompt")
    }

    /// Panics unless the window reads `line` as a line it knows, down to its blocks and events:
    /// a line it doesn't know would parse as an `Other` and be dropped without a word.
    fn read_by_the_window(line: &Value) {
        use crate::external::wire::{
            ContentBlock, Delta, IncomingRequest, Message, StreamEvent, System, UserContent,
        };
        let message: Message = serde_json::from_value(line.clone())
            .unwrap_or_else(|error| panic!("the window can't read {line}: {error}"));
        let blocks_known = |blocks: &[ContentBlock]| {
            assert!(
                !blocks
                    .iter()
                    .any(|block| matches!(block, ContentBlock::Other)),
                "a block the window doesn't know in {line}"
            );
        };
        match &message {
            Message::Assistant { message, .. } => blocks_known(&message.content),
            Message::User(user) => {
                if let UserContent::Blocks(blocks) = &user.message.content {
                    blocks_known(blocks);
                }
            }
            Message::StreamEvent { event, .. } => {
                assert!(
                    !matches!(
                        event,
                        StreamEvent::Other
                            | StreamEvent::ContentBlockDelta {
                                delta: Delta::Other,
                                ..
                            }
                    ),
                    "an event the window doesn't know: {line}"
                );
                if let StreamEvent::ContentBlockStart { content_block, .. } = event {
                    blocks_known(std::slice::from_ref(content_block));
                }
            }
            Message::System(system) => {
                assert!(
                    !matches!(system, System::Other),
                    "a system line the window doesn't know: {line}"
                );
            }
            Message::ControlRequest { request, .. } => {
                assert!(
                    !matches!(request, IncomingRequest::Other),
                    "a request the window doesn't know: {line}"
                );
            }
            Message::Result(_)
            | Message::ControlResponse { .. }
            | Message::RateLimitEvent { .. }
            | Message::ControlCancelRequest { .. } => {}
            Message::Other => panic!("a line the window doesn't know: {line}"),
        }
    }

    /// Panics unless the transcript's reader knows `line`. The session, folder and time a line
    /// is kept with are added as it is written (`wire::TranscriptEntry`, tested there).
    fn read_from_the_transcript(line: &Value) {
        use crate::external::wire::TranscriptLine;
        let read: TranscriptLine = serde_json::from_value(line.clone())
            .unwrap_or_else(|error| panic!("the transcript can't be read: {line}: {error}"));
        assert!(
            !matches!(read, TranscriptLine::Other),
            "a transcript line its reader doesn't know: {line}"
        );
    }

    /// Every line the adapter writes for the window reads as a line of Claude Code's, and every
    /// line it keeps as a transcript line: through a turn with streamed thinking and text, a
    /// tool call asked about and answered, a mode and a title, another session's message, a `!`
    /// command, a failed turn and one that went well.
    #[test]
    fn the_window_and_the_transcript_read_every_line() {
        let mut translator = translator_for(Agent::Agy, Start::New);
        let mut effects = translator.start();
        effects.extend(translator.window_said(
            r#"{"type":"control_request","request_id":"w1","request":{"subtype":"initialize"}}"#,
        ));
        effects.extend(
            translator.agent_said(r#"{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":1}}"#),
        );
        effects.extend(translator.agent_said(
            r#"{"jsonrpc":"2.0","id":2,"result":{"sessionId":"s","models":{"availableModels":[{"modelId":"m1","name":"Model one"}],"currentModelId":"m1"},"modes":{"availableModes":[{"id":"fast","name":"Fast"}],"currentModeId":"fast"}}}"#,
        ));
        effects.extend(
            translator.window_said(r#"{"type":"user","message":{"role":"user","content":"go"}}"#),
        );
        let first = prompt_id(&effects);
        for update in [
            r#"{"sessionUpdate":"agent_thought_chunk","content":{"type":"text","text":"Hmm"}}"#,
            r#"{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"Listing."}}"#,
            r#"{"sessionUpdate":"tool_call","toolCallId":"c1","name":"run_command","kind":"execute","status":"pending","title":"ls","rawInput":{"CommandLine":"ls"}}"#,
            r#"{"sessionUpdate":"current_mode_update","currentModeId":"fast"}"#,
            r#"{"sessionUpdate":"session_info_update","title":"Listing files"}"#,
        ] {
            effects.extend(translator.agent_said(&format!(
                r#"{{"jsonrpc":"2.0","method":"session/update","params":{{"sessionId":"s","update":{update}}}}}"#
            )));
        }
        effects.extend(translator.agent_said(
            r#"{"jsonrpc":"2.0","id":"p1","method":"session/request_permission","params":{"sessionId":"s","toolCall":{"toolCallId":"c1","title":"ls"},"options":[{"optionId":"a","name":"Allow","kind":"allow_once"},{"optionId":"r","name":"Reject","kind":"reject_once"}]}}"#,
        ));
        effects.extend(translator.agent_said(
            r#"{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"s","update":{"sessionUpdate":"tool_call_update","toolCallId":"c1","status":"completed","content":[{"type":"content","content":{"type":"text","text":"a.txt"}}]}}}"#,
        ));
        effects.extend(translator.peer(
            "<cross-session-message from=\"calm-heron\">hello</cross-session-message>".to_owned(),
        ));
        effects.extend(translator.window_said(
            r#"{"type":"bash_command","command":"ls","uuid":"00000000-0000-4000-8000-000000000000"}"#,
        ));
        effects.extend(translator.agent_said(&format!(
            r#"{{"jsonrpc":"2.0","id":{first},"error":{{"code":-32603,"message":"overloaded"}}}}"#
        )));
        let second = prompt_id(&effects);
        effects.extend(translator.agent_said(&format!(
            r#"{{"jsonrpc":"2.0","id":{second},"result":{{"stopReason":"end_turn"}}}}"#
        )));
        let mut window = std::collections::BTreeSet::new();
        let mut transcript = std::collections::BTreeSet::new();
        for effect in &effects {
            match effect {
                Effect::ToWindow(line) => {
                    read_by_the_window(line);
                    window.extend(kinds(&[line]));
                }
                Effect::Transcript(line) => {
                    read_from_the_transcript(line);
                    transcript.extend(kinds(&[line]));
                }
                Effect::ToAgent(_) | Effect::Send { .. } | Effect::List(_) => {}
            }
        }
        let expected = |names: &[&str]| -> std::collections::BTreeSet<String> {
            names.iter().map(|name| (*name).to_owned()).collect()
        };
        assert_eq!(
            window,
            expected(&[
                "assistant",
                "control_request",
                "control_response",
                "result",
                "stream_event:content_block_delta",
                "stream_event:content_block_start",
                "stream_event:content_block_stop",
                "stream_event:message_start",
                "system",
                "user",
            ])
        );
        assert_eq!(
            transcript,
            expected(&["agy-session", "ai-title", "assistant", "user"])
        );
    }

    /// A translator past `initialize` and `session/new`, its session `s`.
    fn opened() -> Translator {
        opened_with(Agent::Agy)
    }

    fn opened_with(agent: Agent) -> Translator {
        let mut translator = translator_for(agent, Start::New);
        translator.start();
        translator.agent_said(r#"{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":1}}"#);
        translator.agent_said(
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
            translator.agent_said(r#"{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":1}}"#);
        let asked = agent_lines(&initialized);
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
        let opened = translator.agent_said(
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
                Effect::ToAgent(_)
                | Effect::ToWindow(_)
                | Effect::Send { .. }
                | Effect::List(_) => None,
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
        let prompt = agent_lines(&prompted)
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
            lines.extend(translator.agent_said(&line));
        }
        lines.extend(translator.agent_said(&format!(
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

    /// agy sends a call with its arguments: it is shown at once, before it completes.
    #[test]
    fn agys_call_shows_at_once() {
        let mut translator = opened();
        let announced = translator.agent_said(
            r#"{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"s","update":{"sessionUpdate":"tool_call","toolCallId":"c1","name":"run_command","kind":"execute","status":"pending","title":"ls","rawInput":{"CommandLine":"ls"}}}}"#,
        );
        let window = window_lines(&announced);
        assert_eq!(kinds(&window), ["assistant"]);
        let tool_use = window
            .first()
            .and_then(|line| line.pointer("/message/content/0"))
            .expect("the tool call");
        assert_eq!(tool_use.get("name"), Some(&json!("Bash")));
        assert_eq!(tool_use.pointer("/input/command"), Some(&json!("ls")));
    }

    /// opencode names a tool before it sends its arguments: the call isn't shown until they
    /// come, and then it is one Bash command.
    #[test]
    fn opencodes_call_shows_once_its_arguments_come() {
        let mut translator = opened_with(Agent::Opencode);
        let announced = translator.agent_said(
            r#"{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"s","update":{"sessionUpdate":"tool_call","toolCallId":"c1","name":"shell","title":"shell","kind":"execute","status":"pending","locations":[{"path":"/w"}],"rawInput":{"cwd":"/w"}}}}"#,
        );
        assert!(
            window_lines(&announced).is_empty(),
            "nothing before the arguments"
        );
        let updated = translator.agent_said(
            r#"{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"s","update":{"sessionUpdate":"tool_call_update","toolCallId":"c1","status":"in_progress","kind":"execute","title":"ls","locations":[{"path":"/w"}],"rawInput":{"command":"ls","cwd":"/w"}}}}"#,
        );
        let window = window_lines(&updated);
        assert_eq!(kinds(&window), ["assistant"]);
        let tool_use = window
            .first()
            .and_then(|line| line.pointer("/message/content/0"))
            .expect("the tool call");
        assert_eq!(tool_use.get("name"), Some(&json!("Bash")));
        assert_eq!(tool_use.pointer("/input/command"), Some(&json!("ls")));
    }

    /// A permission request for a call kept without its arguments shows the call, with the
    /// arguments the request has.
    #[test]
    fn a_permission_request_shows_a_kept_call() {
        let mut translator = opened_with(Agent::Opencode);
        let kept = translator.agent_said(
            r#"{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"s","update":{"sessionUpdate":"tool_call","toolCallId":"c1","name":"shell","title":"shell","kind":"execute","status":"pending","locations":[{"path":"/w"}],"rawInput":{"cwd":"/w"}}}}"#,
        );
        assert!(window_lines(&kept).is_empty());
        let asked = translator.agent_said(
            r#"{"jsonrpc":"2.0","method":"session/request_permission","params":{"sessionId":"s","toolCall":{"toolCallId":"c1","name":"shell","title":"ls","kind":"execute","status":"pending","rawInput":{"command":"ls","cwd":"/w"}},"options":[{"optionId":"allow","name":"Allow","kind":"allow_once"},{"optionId":"deny","name":"Deny","kind":"reject_once"}]},"id":"7"}"#,
        );
        let window = window_lines(&asked);
        let tool_use = window
            .iter()
            .find_map(|line| line.pointer("/message/content/0"))
            .expect("the call shown");
        assert_eq!(tool_use.get("name"), Some(&json!("Bash")));
        assert_eq!(tool_use.pointer("/input/command"), Some(&json!("ls")));
        let request = window
            .iter()
            .find(|line| line.get("type") == Some(&json!("control_request")))
            .expect("the window asked");
        assert_eq!(
            request.pointer("/request/input/command"),
            Some(&json!("ls"))
        );
        assert_eq!(request.pointer("/request/tool_use_id"), Some(&json!("c1")));
    }

    /// opencode's file paths are relative in its arguments and absolute in its locations: the
    /// edit reads as Claude Code's, with the absolute path.
    #[test]
    fn opencodes_edit_reads_as_claude_codes() {
        let call = ToolCall {
            tool_call_id: "c4".to_owned(),
            name: Some("edit".to_owned()),
            kind: Some(ToolKind::Edit),
            locations: Some(vec![Location {
                path: PathBuf::from("/w/notes.txt"),
            }]),
            raw_input: Some(json!({
                "path": "notes.txt",
                "oldString": "two",
                "newString": "TWO",
            })),
            ..ToolCall::default()
        };
        let (name, input) = as_claude_tool(Agent::Opencode, &call);
        assert_eq!(name, "Edit");
        assert_eq!(input.get("file_path"), Some(&json!("/w/notes.txt")));
        assert_eq!(input.get("old_string"), Some(&json!("two")));
        assert_eq!(input.get("new_string"), Some(&json!("TWO")));
    }

    /// agy asks before edualc's own messaging tools too: the adapter allows them, and the
    /// window isn't asked.
    #[test]
    fn edualcs_own_tools_go_without_asking() {
        let mut translator = opened();
        translator.window_said(r#"{"type":"user","message":{"role":"user","content":"go"}}"#);
        let asked = translator.agent_said(
            r#"{"jsonrpc":"2.0","method":"session/request_permission","params":{"sessionId":"s","toolCall":{"toolCallId":"c3","name":"mcp_edualc_list_sessions","kind":"other","status":"pending","title":"list_sessions","rawInput":{}},"options":[{"optionId":"allow","name":"Allow","kind":"allow_once"},{"optionId":"deny","name":"Deny","kind":"reject_once"}]},"id":"8"}"#,
        );
        assert!(
            window_lines(&asked)
                .iter()
                .all(|line| line.get("type") != Some(&json!("control_request"))),
            "the window wasn't asked"
        );
        assert_eq!(
            agent_lines(&asked)
                .first()
                .and_then(|line| line.pointer("/result/outcome/optionId")),
            Some(&json!("allow"))
        );
    }

    /// agy's permission request is asked in the window; the answer picks agy's option, and a
    /// denied call's result says it was denied, as Claude Code's does.
    #[test]
    fn permissions_go_through_the_window() {
        let mut translator = opened();
        translator.window_said(r#"{"type":"user","message":{"role":"user","content":"go"}}"#);
        let asked = translator.agent_said(
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
            agent_lines(&denied)
                .first()
                .and_then(|line| line.pointer("/result/outcome/optionId")),
            Some(&json!("deny"))
        );
        let failed = translator.agent_said(
            r#"{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"s","update":{"sessionUpdate":"tool_call_update","toolCallId":"c2","status":"failed","content":[]}}}"#,
        );
        assert_eq!(
            window_lines(&failed)
                .first()
                .and_then(|line| line.pointer("/message/content/0/content")),
            Some(&json!(DENIED))
        );
    }

    /// The prompt goes to the agent as ACP names its parts: text as `text`, an image with its
    /// `mimeType` and its base64 `data`, under the request's `method` and `params`.
    #[test]
    fn a_prompt_is_sent_as_acp_names_it() {
        let mut translator = opened();
        let sent = translator.window_said(
            r#"{"type":"user","message":{"role":"user","content":[{"type":"text","text":"look"},{"type":"image","source":{"type":"base64","media_type":"image/png","data":"QUJD"}}]}}"#,
        );
        let id = prompt_id(&sent);
        assert_eq!(
            agent_lines(&sent),
            vec![json!({
                "jsonrpc": "2.0",
                "id": id,
                "method": "session/prompt",
                "params": {
                    "sessionId": "s",
                    "prompt": [
                        {"type": "text", "text": "look"},
                        {"type": "image", "mimeType": "image/png", "data": "QUJD"}
                    ]
                }
            })]
        );
    }

    /// A permission answer goes back under the id the request came with, its outcome the
    /// option the user chose, as `selected` with that option's `optionId`.
    #[test]
    fn a_permission_answer_is_acps() {
        let mut translator = opened();
        translator.window_said(r#"{"type":"user","message":{"role":"user","content":"go"}}"#);
        let asked = translator.agent_said(
            r#"{"jsonrpc":"2.0","method":"session/request_permission","params":{"sessionId":"s","toolCall":{"toolCallId":"c2","name":"run_command","kind":"execute","status":"pending","title":"touch /tmp/x","rawInput":{"CommandLine":"touch /tmp/x"}},"options":[{"optionId":"allow","name":"Allow","kind":"allow_once"},{"optionId":"deny","name":"Deny","kind":"reject_once"}]},"id":"7"}"#,
        );
        let request_id = window_lines(&asked)
            .into_iter()
            .find(|line| line.get("type") == Some(&json!("control_request")))
            .and_then(|line| line.get("request_id").and_then(Value::as_str))
            .expect("the window asked")
            .to_owned();
        let answered = translator.window_said(&format!(
            r#"{{"type":"control_response","response":{{"subtype":"success","request_id":"{request_id}","response":{{"behavior":"allow"}}}}}}"#
        ));
        assert_eq!(
            agent_lines(&answered),
            vec![json!({
                "jsonrpc": "2.0",
                "id": "7",
                "result": {"outcome": {"outcome": "selected", "optionId": "allow"}}
            })]
        );
    }

    /// A turn that fails because Google's API refused agy's sign-in ends with a 401 in its
    /// result, for the window to offer signing in and trying again, and agy's words aren't shown
    /// as a message. Any other failure is shown in agy's words.
    #[test]
    fn a_refused_sign_in_ends_the_turn_as_a_401() {
        let failed_with = |error: &str| {
            let mut translator = opened();
            let prompted = translator
                .window_said(r#"{"type":"user","message":{"role":"user","content":"go"}}"#);
            let prompt_id = agent_lines(&prompted)
                .into_iter()
                .find(|line| line.get("method") == Some(&json!("session/prompt")))
                .and_then(|line| line.get("id").cloned())
                .expect("prompted");
            let ended = translator.agent_said(&format!(
                r#"{{"jsonrpc":"2.0","id":{prompt_id},"error":{error}}}"#
            ));
            let lines: Vec<Value> = window_lines(&ended).into_iter().cloned().collect();
            lines
        };
        let result = |lines: &[Value]| {
            lines
                .iter()
                .find(|line| line.get("type") == Some(&json!("result")))
                .cloned()
                .expect("a result")
        };
        let said = |lines: &[Value]| {
            lines
                .iter()
                .any(|line| line.get("type") == Some(&json!("assistant")))
        };

        let unauthenticated = failed_with(
            r#"{"code":-32603,"message":"UNAUTHENTICATED (code 401): Request had invalid authentication credentials."}"#,
        );
        assert_eq!(
            result(&unauthenticated).get("api_error_status"),
            Some(&json!(401))
        );
        assert!(!said(&unauthenticated), "agy's words aren't shown");

        let auth_required = failed_with(r#"{"code":-32000,"message":"Authentication required"}"#);
        assert_eq!(
            result(&auth_required).get("api_error_status"),
            Some(&json!(401))
        );

        let other = failed_with(r#"{"code":-32603,"message":"quota exhausted"}"#);
        assert_eq!(result(&other).get("api_error_status"), Some(&Value::Null));
        assert!(said(&other), "agy's words are shown");
    }

    /// A message from another session is shown as one, and prompts agy once its turn ends.
    #[test]
    fn a_peer_message_waits_for_its_turn() {
        let mut translator = opened();
        let first = translator
            .window_said(r#"{"type":"user","message":{"role":"user","content":"hello"}}"#);
        let prompt_id = agent_lines(&first)
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
        assert!(agent_lines(&peer).is_empty(), "agy is busy");
        let ended = translator.agent_said(&format!(
            r#"{{"jsonrpc":"2.0","id":{prompt_id},"result":{{"stopReason":"end_turn"}}}}"#
        ));
        let next = agent_lines(&ended)
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
        translator.agent_said(
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
        let ended = translator.agent_said(
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
        let (name, input) = as_claude_tool(
            Agent::Agy,
            &call(
                "write_file",
                json!({"Append": true, "Content": "three\n", "Path": "/w/f.txt"}),
            ),
        );
        assert_eq!(name, "Edit");
        assert_eq!(input.get("old_string"), Some(&json!("")));
        assert_eq!(input.get("new_string"), Some(&json!("three\n")));
        let (name, input) = as_claude_tool(
            Agent::Agy,
            &call(
                "write_file",
                json!({"Content": "hi\n", "Overwrite": true, "Path": "/w/hello.txt"}),
            ),
        );
        assert_eq!(name, "Write");
        assert_eq!(input.get("content"), Some(&json!("hi\n")));
        let (name, input) = as_claude_tool(
            Agent::Agy,
            &call(
                "edit_file",
                json!({"Path": "/w/f.txt", "Replacement": "two\nthree", "Target": "two"}),
            ),
        );
        assert_eq!(name, "Edit");
        assert_eq!(input.get("old_string"), Some(&json!("two")));
    }

    /// A tool call without the agent's own name reads by ACP's kind: a command as Bash with its
    /// title, a read as Read of its first location.
    #[test]
    fn unnamed_tools_read_by_their_kind() {
        let (name, input) = as_claude_tool(
            Agent::Agy,
            &ToolCall {
                tool_call_id: "c".to_owned(),
                title: Some("ls -la".to_owned()),
                kind: Some(ToolKind::Execute),
                ..ToolCall::default()
            },
        );
        assert_eq!(name, "Bash");
        assert_eq!(input.get("command"), Some(&json!("ls -la")));
        let (name, input) = as_claude_tool(
            Agent::Agy,
            &ToolCall {
                tool_call_id: "c".to_owned(),
                title: Some("Read f.txt".to_owned()),
                kind: Some(ToolKind::Read),
                locations: Some(vec![Location {
                    path: PathBuf::from("/w/f.txt"),
                }]),
                ..ToolCall::default()
            },
        );
        assert_eq!(name, "Read");
        assert_eq!(input.get("file_path"), Some(&json!("/w/f.txt")));
    }

    /// A `!` command is told as not run, in the rows the window reads a command's run from,
    /// and nothing reaches agy.
    #[test]
    fn shell_commands_are_refused() {
        let mut translator = opened();
        let refused = translator.window_said(
            r#"{"type":"bash_command","command":"ls <dir>","uuid":"00000000-0000-4000-8000-000000000000"}"#,
        );
        assert!(agent_lines(&refused).is_empty());
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
            translator.agent_said(r#"{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":1}}"#);
        assert_eq!(
            agent_lines(&loading)
                .first()
                .and_then(|line| line.get("method")),
            Some(&json!("session/load"))
        );
        let replayed = translator.agent_said(
            r#"{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"s","update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"old"}}}}"#,
        );
        assert!(replayed.is_empty());
    }

    /// The window is told agy's own modes and the one agy is in, picks one by agy's id, and
    /// hears when agy changes mode; Claude Code's modes are refused.
    #[test]
    fn agys_modes_reach_the_window() {
        let mut translator = translator(Start::New);
        translator.start();
        translator.window_said(
            r#"{"type":"control_request","request_id":"r1","request":{"subtype":"initialize"}}"#,
        );
        translator.agent_said(r#"{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":1}}"#);
        let opened = translator.agent_said(
            r#"{"jsonrpc":"2.0","id":2,"result":{"sessionId":"s","modes":{"availableModes":[{"id":"default","name":"Default","description":"Ask before acting"},{"id":"accept-edits","name":"Accept edits"}],"currentModeId":"default"}}}"#,
        );
        let answer = window_lines(&opened)
            .into_iter()
            .find(|line| line.get("type") == Some(&json!("control_response")))
            .expect("initialize answered");
        assert_eq!(
            answer.pointer("/response/response/programModes"),
            Some(&json!({
                "offered": [
                    {"id": "default", "name": "Default", "description": "Ask before acting"},
                    {"id": "accept-edits", "name": "Accept edits", "description": ""}
                ],
                "current": "default"
            }))
        );

        let asked = translator.window_said(
            r#"{"type":"control_request","request_id":"r2","request":{"subtype":"set_program_mode","mode":"accept-edits"}}"#,
        );
        let set_mode = agent_lines(&asked)
            .into_iter()
            .find(|line| line.get("method") == Some(&json!("session/set_mode")))
            .expect("agy asked to switch");
        assert_eq!(
            set_mode.get("params"),
            Some(&json!({"sessionId": "s", "modeId": "accept-edits"}))
        );
        let rpc = set_mode.get("id").and_then(Value::as_u64).expect("an id");
        let switched =
            translator.agent_said(&format!(r#"{{"jsonrpc":"2.0","id":{rpc},"result":{{}}}}"#));
        assert_eq!(
            window_lines(&switched)
                .first()
                .and_then(|line| line.pointer("/response/subtype")),
            Some(&json!("success"))
        );
        let again = translator.window_said(
            r#"{"type":"control_request","request_id":"r3","request":{"subtype":"initialize"}}"#,
        );
        assert_eq!(
            window_lines(&again)
                .first()
                .and_then(|line| line.pointer("/response/response/programModes/current")),
            Some(&json!("accept-edits"))
        );

        let claudes = translator.window_said(
            r#"{"type":"control_request","request_id":"r4","request":{"subtype":"set_permission_mode","mode":"plan"}}"#,
        );
        assert!(agent_lines(&claudes).is_empty());
        assert_eq!(
            window_lines(&claudes)
                .first()
                .and_then(|line| line.pointer("/response/subtype")),
            Some(&json!("error"))
        );

        let changed = translator.agent_said(
            r#"{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"s","update":{"sessionUpdate":"current_mode_update","currentModeId":"plan"}}}"#,
        );
        let told = window_lines(&changed);
        assert_eq!(
            told.first()
                .map(|line| (line.get("subtype"), line.get("mode"))),
            Some((Some(&json!("program_mode")), Some(&json!("plan"))))
        );
    }
}
