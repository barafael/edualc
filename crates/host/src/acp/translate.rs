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

use std::collections::{BTreeSet, HashMap, VecDeque};
use std::path::PathBuf;

use serde_json::{Map, Value};

use crate::app::Mode;
use crate::external::acp::{
    self, AgentMethod, Change, Chunk, Incoming, Notified, OptionKind, PermissionAsked, Prompted,
    RpcError, StopReason, ToolCall, ToolContent, ToolKind, ToolStatus,
};
use crate::external::agy::{self, QuotaWindow};
use crate::external::tool_input::{
    BashInput, EditInput, GrepInput, KnownCall, ReadInput, WebFetchInput, WriteInput,
};
use crate::external::wire::{
    self, Account, ApiMessage, AssistantError, ContentBlock, ControlResponse, DENIED, Delta,
    IncomingRequest, Initialized, LimitKind, Message, MessageId, MessageOrigin, MessageStart,
    ModeId, ModelInfo, ModelUsage, ProgramMode, ProgramModes, RequestId, ResultKind, SessionId,
    StreamEvent, System, ToolName, ToolUseId, TranscriptLine, TurnResult, Usage, UserContent,
    UserLine, UserMessage,
};
use crate::settings::PeerMessages;

use super::outbox::unwrapped;
use super::{Agent, first_location};

/// JSON-RPC's code for a method the receiver doesn't have.
const METHOD_NOT_FOUND: i64 = -32601;

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
#[derive(Debug)]
pub enum Effect {
    /// A line to the agent.
    ToAgent(acp::ToAgent),
    /// A line to the window, through the daemon.
    ToWindow(Message),
    /// A line for the transcript; the session, folder and time are added as it is written.
    Transcript(TranscriptLine),
    /// A message: carry it out (see [`super::outbox`]) and answer the call.
    Send {
        call: CallId,
        to: String,
        message: String,
    },
    /// The running sessions, for the call.
    List(CallId),
    /// The subagents and swarm agy keeps for conversation `conversation` may have changed: the
    /// window is told their tasks if they did (see [`super::brain`]).
    Subagents { conversation: String },
    /// Ask the agent's plan how much of its quotas is used (see [`Translator::quotas_told`]):
    /// for the window's `get_usage` request, or, with none, for the sidebar's bars.
    Quotas { request: Option<RequestId> },
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
    Model(Option<RequestId>),
    /// One of the agent's modes, for the window's request.
    Mode {
        request: RequestId,
        mode: String,
    },
    Prompt,
}

/// Something to be prompted with once the agent is free.
#[derive(Debug, Clone)]
enum Queued {
    /// The user's prompt, as ACP takes it, and as the transcript keeps it.
    User {
        prompt: Vec<acp::PromptBlock>,
        kept: UserContent,
    },
    /// Another session's message, in its envelope.
    Peer(String),
}

/// A message being streamed: the id it goes under and its blocks so far, the last one open.
#[derive(Debug, Default)]
struct Streaming {
    id: MessageId,
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
    name: ToolName,
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
    held: Vec<wire::Input>,
    /// The window's `initialize`, answered once the session is open.
    initialize: Option<RequestId>,
    /// A prompt is under way.
    busy: bool,
    queue: VecDeque<Queued>,
    streaming: Option<Streaming>,
    /// The turn's text, for its result.
    said: String,
    next_local: u64,
    entries: HashMap<String, Entry>,
    permissions: HashMap<RequestId, Permission>,
    /// The tool call each of edualc's tool's calls is, by the agent's id.
    calls: HashMap<CallId, String>,
    /// How full the model's context is, once the agent said.
    context: Option<ContextUsed>,
    /// Other sessions' messages waiting to come in, by the request asking the window.
    waiting: HashMap<RequestId, String>,
    /// The senders the user let in for the session, by the names they gave.
    senders_let_in: BTreeSet<String>,
}

/// How much of the model's context window the session uses, and the window's size, in tokens.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ContextUsed {
    used: u64,
    size: u64,
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
            context: None,
            waiting: HashMap::new(),
            senders_let_in: BTreeSet::new(),
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

    /// For an agent that keeps records of the subagents it starts, a look at them: they change
    /// as its tool calls end, and as its turns do.
    fn subagents(&self) -> Option<Effect> {
        self.agent
            .keeps_subagents()
            .then(|| self.acp_session())
            .flatten()
            .map(|session| Effect::Subagents {
                conversation: session.to_owned(),
            })
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
                let opened: acp::Opened = match serde_json::from_value(result) {
                    Ok(opened) => opened,
                    Err(error) => match &self.start {
                        Start::New => {
                            return self.failed(&format!(
                                "its answer to opening the session can't be read: {error}"
                            ));
                        }
                        // The session asked for is open whatever else the answer says.
                        Start::Resume(_) => {
                            tracing::warn!(%error, agent = %self.agent, "the answer to loading the session can't be read: its models and modes are left out");
                            acp::Opened::default()
                        }
                    },
                };
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
                    effects.push(Effect::Transcript(TranscriptLine::AgySession {
                        agent_session_id: session.clone(),
                    }));
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
                    effects.push(self.initialized(request));
                }
                for input in std::mem::take(&mut self.held) {
                    effects.extend(self.window_input(input));
                }
                effects.extend(self.subagents());
                effects
            }
            Asked::Model(window) => window
                .map(|request| vec![success(request, Value::Object(Map::new()))])
                .unwrap_or_default(),
            Asked::Mode { request, mode } => {
                self.mode = Some(mode);
                vec![success(request, Value::Object(Map::new()))]
            }
            Asked::Prompt => match serde_json::from_value::<Prompted>(result) {
                Ok(prompted) => self.turn_ended(Some(&prompted), None),
                Err(error) => {
                    let said = format!("its answer to the prompt can't be read: {error}");
                    self.turn_ended(None, Some(Failure::Said(&said)))
                }
            },
        }
    }

    /// The agent refused a request.
    fn refused(&mut self, asked: &Asked, error: &RpcError) -> Vec<Effect> {
        let message = error.message.as_str();
        match asked {
            Asked::Initialize | Asked::Open => self.failed(message),
            Asked::Model(Some(request)) | Asked::Mode { request, .. } => {
                vec![failure(request.clone(), message)]
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
        let mut effects = vec![Effect::ToWindow(Message::Assistant {
            message: ApiMessage {
                id: MessageId::from(format!("{agent}-failed")),
                content: vec![ContentBlock::Text {
                    text: format!("{agent} couldn't open the session: {message}"),
                }],
                usage: Usage::default(),
            },
            parent_tool_use_id: None,
            error: None,
        })];
        if let Some(request) = self.initialize.take() {
            effects.push(failure(request, message));
        }
        effects
    }

    /// The answer to the window's `initialize`: the agent's models, and its own modes with the
    /// one it is in.
    fn initialized(&self, request: RequestId) -> Effect {
        let agent = self.agent;
        let initialized = Initialized {
            models: self
                .models
                .iter()
                .map(|model| ModelInfo {
                    value: model.model_id.clone(),
                    display_name: model.name.clone(),
                    resolved_model: String::new(),
                    description: String::new(),
                })
                .collect(),
            commands: Vec::new(),
            account: Account {
                api_provider: agent.to_string(),
                ..Account::default()
            },
            claude_code_version: format!("{agent}, through edualc {}", env!("CARGO_PKG_VERSION")),
            program_modes: Some(ProgramModes {
                offered: self
                    .modes
                    .iter()
                    .map(|mode| ProgramMode {
                        id: ModeId::from(mode.id.clone()),
                        name: mode.name.clone(),
                        description: mode.description.clone().unwrap_or_default(),
                    })
                    .collect(),
                current: self.mode.clone().map(ModeId::from),
            }),
        };
        match serde_json::to_value(initialized) {
            Ok(response) => success(request, response),
            Err(error) => failure(request, &error.to_string()),
        }
    }

    /// A line from the window.
    pub fn window_said(&mut self, line: &str) -> Vec<Effect> {
        let input = match serde_json::from_str::<wire::Input>(line) {
            Ok(input) => input,
            Err(error) => return unreadable_from_window(line, &error),
        };
        match &self.phase {
            Phase::Ready(_) => self.window_input(input),
            Phase::Failed => match input {
                wire::Input::ControlRequest { request_id, .. } => vec![failure(
                    request_id,
                    &format!("{} isn't running", self.agent),
                )],
                wire::Input::User { .. }
                | wire::Input::ControlResponse { .. }
                | wire::Input::BashCommand { .. } => Vec::new(),
            },
            Phase::Connecting | Phase::Opening { .. } => {
                match input {
                    wire::Input::ControlRequest {
                        request_id,
                        request: wire::ControlRequest::Initialize { .. },
                    } => self.initialize = Some(request_id),
                    held @ (wire::Input::ControlRequest { .. }
                    | wire::Input::User { .. }
                    | wire::Input::ControlResponse { .. }
                    | wire::Input::BashCommand { .. }) => self.held.push(held),
                }
                Vec::new()
            }
        }
    }

    /// A line the window has been read as: a prompt, a `!` command, a request or an answer.
    fn window_input(&mut self, input: wire::Input) -> Vec<Effect> {
        match input {
            wire::Input::ControlRequest {
                request_id,
                request,
            } => self.window_request(request_id, request),
            wire::Input::ControlResponse { response } => self.window_answered(response),
            wire::Input::User { message, .. } => {
                let prompt = prompt_blocks(&message.content);
                if prompt.is_empty() {
                    return Vec::new();
                }
                self.queue.push_back(Queued::User {
                    prompt,
                    kept: UserContent::from(message.content),
                });
                self.next_turn()
            }
            wire::Input::BashCommand { command, .. } => self.shell_refused(&command),
        }
    }

    /// A `!` command: the agent has no shell beside the conversation, so it is told as not run,
    /// in the rows Claude Code replays for one (its echo, then what it printed).
    fn shell_refused(&self, command: &str) -> Vec<Effect> {
        let agent = self.agent;
        let row = |text: String| {
            Effect::ToWindow(Message::User(UserLine::replayed(UserContent::Text(text))))
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

    fn window_request(
        &mut self,
        request_id: RequestId,
        request: wire::ControlRequest,
    ) -> Vec<Effect> {
        let Some(session) = self.acp_session().map(str::to_owned) else {
            return vec![failure(
                request_id,
                &format!("{} isn't running", self.agent),
            )];
        };
        match request {
            wire::ControlRequest::Initialize { .. } => vec![self.initialized(request_id)],
            wire::ControlRequest::Interrupt => {
                let mut effects = vec![success(request_id, Value::Object(Map::new()))];
                if self.busy {
                    effects.push(Self::cancel(session));
                }
                effects
            }
            wire::ControlRequest::SetModel { model } => {
                self.model = Some(model.clone());
                vec![self.request(
                    acp::ClientCall::SetModel(acp::SetModelParams {
                        session_id: session,
                        model_id: model,
                    }),
                    Asked::Model(Some(request_id)),
                )]
            }
            wire::ControlRequest::SetProgramMode { mode } => {
                let mode = mode.to_string();
                vec![self.request(
                    acp::ClientCall::SetMode(acp::SetModeParams {
                        session_id: session,
                        mode_id: mode.clone(),
                    }),
                    Asked::Mode {
                        request: request_id,
                        mode,
                    },
                )]
            }
            wire::ControlRequest::SetPermissionMode { .. } => {
                vec![failure(
                    request_id,
                    &format!("{} has modes of its own, not Claude Code's", self.agent),
                )]
            }
            wire::ControlRequest::GetUsage if self.agent.reports_quotas() => {
                vec![Effect::Quotas {
                    request: Some(request_id),
                }]
            }
            wire::ControlRequest::GetContextUsage => match self.context {
                Some(context) => vec![success_as(request_id, &context_report(context))],
                None => vec![failure(
                    request_id,
                    &format!("{} hasn't said how full the context is yet", self.agent),
                )],
            },
            wire::ControlRequest::StopTask { .. }
            | wire::ControlRequest::GetTaskOutput { .. }
            | wire::ControlRequest::SideQuestion { .. }
            | wire::ControlRequest::GetUsage => {
                vec![failure(
                    request_id,
                    &format!("{} doesn't do this", self.agent),
                )]
            }
        }
    }

    /// The window's answer to a permission request: the agent's, or the messaging tool's.
    fn window_answered(&mut self, reply: wire::Reply) -> Vec<Effect> {
        let request = match &reply {
            wire::Reply::Success { request_id, .. } | wire::Reply::Error { request_id, .. } => {
                request_id
            }
        };
        if let Some(content) = self.waiting.remove(request) {
            return match reply {
                wire::Reply::Success {
                    response:
                        wire::PermissionResult::Allow {
                            updated_permissions,
                            ..
                        },
                    ..
                } => self.let_in(content, &updated_permissions),
                // Turned down: the agent never sees it, and its turn goes on.
                wire::Reply::Success {
                    response: wire::PermissionResult::Deny { .. },
                    ..
                } => Vec::new(),
                wire::Reply::Error { error, .. } => {
                    tracing::warn!(%error, "the window couldn't ask about a message: turned down");
                    Vec::new()
                }
            };
        }
        let (request_id, verdict) = match reply {
            wire::Reply::Success {
                request_id,
                response: wire::PermissionResult::Allow { .. },
            } => (request_id, Verdict::Allow),
            wire::Reply::Success {
                request_id,
                response: wire::PermissionResult::Deny { interrupt, .. },
            } => (request_id, Verdict::Deny { interrupt }),
            wire::Reply::Error { request_id, error } => {
                tracing::warn!(%error, "the window couldn't answer a permission request: cancelled");
                (request_id, Verdict::Unanswered)
            }
        };
        let Some(permission) = self.permissions.remove(&request_id) else {
            return Vec::new();
        };
        let outcome = match verdict {
            Verdict::Allow => outcome_of(&permission.options, OptionKind::AllowOnce),
            Verdict::Deny { .. } => outcome_of(&permission.options, OptionKind::RejectOnce),
            Verdict::Unanswered => acp::Outcome::Cancelled,
        };
        if let Verdict::Deny { .. } = verdict
            && let Some(Entry::Shown(shown)) = self.entries.get_mut(&permission.tool)
        {
            shown.denied = true;
        }
        let mut effects = vec![Self::answer(
            permission.id,
            acp::Reply::Result(acp::PermissionAnswer { outcome }),
        )];
        if let Verdict::Deny { interrupt: true } = verdict
            && let Some(session) = self.acp_session()
        {
            effects.push(Self::cancel(session.to_owned()));
        }
        effects
    }

    /// The agent's messaging tool was called: the message goes without asking, as a Claude Code
    /// session's own do.
    pub fn tool_called(&mut self, call: CallId, to: String, message: String) -> Vec<Effect> {
        if let Some(id) = self.open_call(&ToolName::EdualcSendMessage) {
            self.calls.insert(call, id);
        }
        vec![Effect::Send { call, to, message }]
    }

    /// The agent's messaging tool asked for the running sessions: no question for that.
    pub fn listing_asked(&mut self, call: CallId) -> Vec<Effect> {
        if let Some(id) = self.open_call(&ToolName::EdualcListSessions) {
            self.calls.insert(call, id);
        }
        vec![Effect::List(call)]
    }

    /// The call of `tool` under way, by the agent's id, that no answer was told for yet.
    fn open_call(&self, tool: &ToolName) -> Option<String> {
        let told: Vec<&String> = self.calls.values().collect();
        self.entries.iter().find_map(|(id, entry)| match entry {
            Entry::Shown(shown) if shown.name == *tool && !shown.done && !told.contains(&id) => {
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

    /// Another session's message came into the inbox. One from a sender the user let in, or
    /// any with `peer_messages` letting all in, comes in; another waits while the window asks.
    pub fn peer(&mut self, content: String, peer_messages: PeerMessages) -> Vec<Effect> {
        let (name, body) = unwrapped(&content);
        let let_in = match peer_messages {
            PeerMessages::LetAllIn => true,
            PeerMessages::AskEachSender => name
                .as_ref()
                .is_some_and(|name| self.senders_let_in.contains(name)),
        };
        if let_in {
            return self.came_in(content);
        }
        // A sender without a name can't be let in for later: each of its messages is asked.
        let request = match IncomingRequest::incoming_message(name.as_deref(), body) {
            Ok(request) => request,
            Err(error) => {
                tracing::warn!(%error, "a message waiting to come in can't be asked about: turned down");
                return Vec::new();
            }
        };
        let request_id = RequestId::from(self.local_id("message"));
        self.waiting.insert(request_id.clone(), content);
        vec![Effect::ToWindow(Message::ControlRequest {
            request_id,
            request,
        })]
    }

    /// A message the window was asked about: the user let it in, and maybe its sender.
    fn let_in(&mut self, content: String, offered: &[wire::PermissionUpdate]) -> Vec<Effect> {
        if let Some(sender) = wire::sender_let_in(offered) {
            self.senders_let_in.insert(sender);
        }
        self.came_in(content)
    }

    /// Another session's message, let in: shown as such, and given to the agent as a prompt
    /// once it is free.
    fn came_in(&mut self, content: String) -> Vec<Effect> {
        let (name, body) = unwrapped(&content);
        let mut effects = vec![
            Effect::ToWindow(Message::User(UserLine::from_peer(
                content.clone(),
                name.clone(),
                Some(body.clone()),
            ))),
            Effect::Transcript(TranscriptLine::User {
                message: UserMessage {
                    content: UserContent::Text(content.clone()),
                },
                is_meta: true,
                is_sidechain: false,
                origin: Some(MessageOrigin::Peer {
                    name,
                    body: Some(body),
                }),
            }),
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
        let mut effects = vec![Effect::ToWindow(Message::System(System::Init {
            session_id: self.id.clone(),
            model: self.model.clone().unwrap_or_default(),
            permission_mode: Mode::Unknown(self.mode.clone().unwrap_or_default()),
            cwd: self.cwd.clone(),
        }))];
        let prompt = match queued {
            Queued::User { prompt, kept } => {
                effects.push(Effect::Transcript(TranscriptLine::User {
                    message: UserMessage { content: kept },
                    is_meta: false,
                    is_sidechain: false,
                    origin: None,
                }));
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

    fn stream_event(event: StreamEvent) -> Effect {
        Effect::ToWindow(Message::StreamEvent {
            event,
            parent_tool_use_id: None,
        })
    }

    /// What changed in the session.
    fn updated(&mut self, update: Change) -> Vec<Effect> {
        // How full the context is holds for a loaded session as for a new one.
        if let Change::UsageUpdate { used, size } = update {
            self.context = Some(ContextUsed { used, size });
            return vec![Effect::ToWindow(Message::System(System::ContextUsed {
                used,
                size,
            }))];
        }
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
                vec![Effect::Transcript(TranscriptLine::AiTitle {
                    ai_title: title,
                })]
            }
            Change::CurrentModeUpdate { current_mode_id } => {
                let effect = Effect::ToWindow(Message::System(System::ProgramMode {
                    mode: ModeId::from(current_mode_id.clone()),
                }));
                self.mode = Some(current_mode_id);
                vec![effect]
            }
            Change::AgentMessageChunk { .. }
            | Change::AgentThoughtChunk { .. }
            | Change::UserMessageChunk
            | Change::SessionInfoUpdate { title: None }
            | Change::UsageUpdate { .. }
            | Change::Other => Vec::new(),
        }
    }

    /// A piece of text or thinking, streamed into the message under way.
    fn streamed(&mut self, piece: &Block) -> Vec<Effect> {
        let mut effects = Vec::new();
        if self.streaming.is_none() {
            let id = MessageId::from(self.local_id("message"));
            effects.push(Self::stream_event(StreamEvent::MessageStart {
                message: MessageStart { id: id.clone() },
            }));
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
            if let Some(open) = open_index(&streaming.blocks) {
                events.push(StreamEvent::ContentBlockStop { index: open });
            }
            let (block, content_block) = match piece {
                Block::Text(_) => (
                    Block::Text(String::new()),
                    ContentBlock::Text {
                        text: String::new(),
                    },
                ),
                Block::Thinking(_) => (
                    Block::Thinking(String::new()),
                    ContentBlock::Thinking {
                        thinking: String::new(),
                    },
                ),
            };
            streaming.blocks.push(block);
            if let Some(index) = open_index(&streaming.blocks) {
                events.push(StreamEvent::ContentBlockStart {
                    index,
                    content_block,
                });
            }
        }
        let Some(index) = open_index(&streaming.blocks) else {
            return effects;
        };
        let delta = match (piece, streaming.blocks.last_mut()) {
            (Block::Text(text), Some(Block::Text(grown))) => {
                grown.push_str(text);
                Delta::Text { text: text.clone() }
            }
            (Block::Thinking(text), Some(Block::Thinking(grown))) => {
                grown.push_str(text);
                Delta::Thinking {
                    thinking: text.clone(),
                }
            }
            (Block::Text(_) | Block::Thinking(_), _) => return effects,
        };
        events.push(StreamEvent::ContentBlockDelta { index, delta });
        effects.extend(events.into_iter().map(Self::stream_event));
        effects
    }

    /// Ends the message being streamed: its blocks, then `tool_use` if a tool call ends it, as
    /// one assistant message.
    fn message_done(&mut self, tool_use: Option<ContentBlock>) -> Vec<Effect> {
        let streaming = self.streaming.take();
        let mut effects = Vec::new();
        let (id, mut content) = match streaming {
            Some(Streaming { id, blocks }) => {
                if let Some(open) = open_index(&blocks) {
                    effects.push(Self::stream_event(StreamEvent::ContentBlockStop {
                        index: open,
                    }));
                }
                let content: Vec<ContentBlock> = blocks
                    .into_iter()
                    .map(|block| match block {
                        Block::Text(text) => ContentBlock::Text { text },
                        Block::Thinking(thinking) => ContentBlock::Thinking { thinking },
                    })
                    .collect();
                (id, content)
            }
            None if tool_use.is_some() => (MessageId::from(self.local_id("message")), Vec::new()),
            None => return effects,
        };
        content.extend(tool_use);
        let message = ApiMessage {
            id,
            content,
            usage: Usage::default(),
        };
        effects.push(Effect::ToWindow(Message::Assistant {
            message: message.clone(),
            parent_tool_use_id: None,
            error: None,
        }));
        effects.push(Effect::Transcript(TranscriptLine::Assistant {
            message,
            is_sidechain: false,
        }));
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
        let tool_use = ContentBlock::ToolUse {
            id: ToolUseId::from(call.tool_call_id.clone()),
            name,
            input,
        };
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
        let content = UserContent::Blocks(vec![ContentBlock::ToolResult {
            tool_use_id: ToolUseId::from(call.tool_call_id.clone()),
            content: Some(UserContent::Text(text)),
            is_error: failed || shown.denied,
        }]);
        let mut effects = vec![
            Effect::ToWindow(Message::User(UserLine::of_session(content.clone()))),
            Effect::Transcript(TranscriptLine::User {
                message: UserMessage { content },
                is_meta: false,
                is_sidechain: false,
                origin: None,
            }),
        ];
        effects.extend(self.subagents());
        effects
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
        if matches!(
            name,
            ToolName::EdualcSendMessage | ToolName::EdualcListSessions
        ) {
            let outcome = outcome_of(&asked.options, OptionKind::AllowOnce);
            effects.push(Self::answer(
                id,
                acp::Reply::Result(acp::PermissionAnswer { outcome }),
            ));
            return effects;
        }
        let request = RequestId::from(self.local_id("permission"));
        self.permissions.insert(
            request.clone(),
            Permission {
                id,
                options: asked.options,
                tool: tool.clone(),
            },
        );
        effects.push(Effect::ToWindow(Message::ControlRequest {
            request_id: request,
            request: IncomingRequest::CanUseTool {
                tool_name: name,
                input,
                permission_suggestions: Vec::new(),
                description: asked.tool_call.title.unwrap_or_default(),
                tool_use_id: Some(ToolUseId::from(tool)),
            },
        }));
        effects
    }

    /// The prompt's answer: the turn ends, and what waits goes next. A refused sign-in is told
    /// as an HTTP 401 in the turn's result, which is what it was, and the window offers to sign
    /// in and try again instead of showing the agent's words.
    fn turn_ended(&mut self, prompted: Option<&Prompted>, failure: Option<Failure>) -> Vec<Effect> {
        let mut effects = self.message_done(None);
        // A turn the agent cut short ends as failed, saying why, so it doesn't read as done.
        let failure = failure.or_else(|| {
            prompted
                .and_then(|prompted| cut_short(prompted.stop_reason))
                .map(Failure::Said)
        });
        let error = failure.map(|failure| match failure {
            Failure::Said(message) | Failure::SignIn(message) => message,
        });
        let agent = self.agent;
        if let Some(Failure::Said(error)) = failure {
            let id = MessageId::from(self.local_id("message"));
            effects.push(Effect::ToWindow(Message::Assistant {
                message: ApiMessage {
                    id,
                    content: vec![ContentBlock::Text {
                        text: format!("{agent}: {error}"),
                    }],
                    usage: Usage::default(),
                },
                parent_tool_use_id: None,
                error: Some(AssistantError::Other),
            }));
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
        let model = self.model.clone().unwrap_or_else(|| agent.to_string());
        effects.push(Effect::ToWindow(Message::Result(TurnResult {
            subtype: if failed {
                ResultKind::ErrorDuringExecution
            } else {
                ResultKind::Success
            },
            is_error: failed,
            result: Some(std::mem::take(&mut self.said)),
            errors: error
                .map(|error| Value::String(error.to_owned()))
                .into_iter()
                .collect(),
            total_cost_usd: None,
            model_usage: HashMap::from([(
                model,
                ModelUsage {
                    context_window: self.context.map(|context| context.size),
                },
            )]),
            api_error_status: matches!(failure, Some(Failure::SignIn(_))).then_some(401),
        })));
        self.busy = false;
        effects.extend(self.subagents());
        if self.agent.reports_quotas() {
            effects.push(Effect::Quotas { request: None });
        }
        effects.extend(self.next_turn());
        effects
    }
}

impl Translator {
    /// What the agent's plan said of its quotas: the answer to the window's `get_usage`, or the
    /// sidebar's bars from the quotas the session's model draws on.
    pub fn quotas_told(
        &self,
        request: Option<RequestId>,
        told: Result<agy::Quotas, super::QuotaError>,
    ) -> Vec<Effect> {
        let quotas = match told {
            Ok(quotas) => quotas,
            Err(error) => {
                tracing::warn!(%error, agent = %self.agent, "the plan's quotas couldn't be read");
                return request
                    .map(|request| {
                        failure(
                            request,
                            &format!("{} couldn't say its plan's quotas: {error}", self.agent),
                        )
                    })
                    .into_iter()
                    .collect();
            }
        };
        let model = self.model.as_deref().and_then(|id| {
            self.models
                .iter()
                .find(|model| model.model_id == id)
                .map(|model| model.name.as_str())
        });
        let limits = plan_limits(&quotas, model);
        match request {
            Some(request) => vec![success_as(
                request,
                &wire::UsageReport {
                    rate_limits: wire::RateLimitReport { limits },
                    ..wire::UsageReport::default()
                },
            )],
            None => vec![Effect::ToWindow(Message::RateLimitEvent {
                rate_limit_info: wire::RateLimits {
                    windows: limit_windows(&limits),
                },
            })],
        }
    }
}

/// Why the agent ended a turn before it was done, in words; none for a turn it finished or the
/// user interrupted.
fn cut_short(reason: StopReason) -> Option<&'static str> {
    match reason {
        StopReason::EndTurn | StopReason::Cancelled => None,
        StopReason::MaxTokens => Some("stopped: the answer reached the most it may write at once"),
        StopReason::MaxTurnRequests => {
            Some("stopped: the turn reached the most requests it may make")
        }
        StopReason::Refusal => Some("refused to go on"),
        StopReason::Other => Some("stopped, for a reason edualc doesn't know"),
    }
}

/// Text as Claude Code puts it in its `<bash-…>` rows: `&`, `<` and `>` as entities.
fn escaped(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// The index of the last of `blocks`, the open one, as the stream counts them.
fn open_index(blocks: &[Block]) -> Option<u64> {
    (0_u64..).zip(blocks).last().map(|(index, _)| index)
}

/// A line from the window that isn't one of its lines as edualc reads them. A control request
/// is answered with the reason, so the window doesn't wait on it; anything else is dropped.
fn unreadable_from_window(line: &str, error: &serde_json::Error) -> Vec<Effect> {
    match serde_json::from_str::<wire::ControlPeek>(line) {
        Ok(wire::ControlPeek {
            kind: wire::PeekedKind::ControlRequest,
            request_id: Some(request_id),
            ..
        }) => vec![failure(
            RequestId::from(request_id),
            &format!("edualc can't read this request: {error}"),
        )],
        Ok(wire::ControlPeek {
            kind: wire::PeekedKind::User,
            ..
        }) => {
            tracing::warn!(%error, "a prompt from the window that can't be read: dropped");
            Vec::new()
        }
        Ok(wire::ControlPeek { .. }) | Err(_) => {
            tracing::warn!(%error, "a line from the window that can't be read: dropped");
            Vec::new()
        }
    }
}

/// What the window answered a permission request with, as edualc acts on it.
enum Verdict {
    Allow,
    Deny {
        interrupt: bool,
    },
    /// The window couldn't answer: the request is cancelled.
    Unanswered,
}

/// The context's use as `get_context_usage` answers it: the conversation, and what is free.
fn context_report(context: ContextUsed) -> wire::ContextReport {
    let free = context.size.saturating_sub(context.used);
    wire::ContextReport {
        categories: [
            ("Conversation", context.used, wire::ContextKind::Used),
            ("Free space", free, wire::ContextKind::Free),
        ]
        .into_iter()
        .filter(|(_, tokens, _)| *tokens > 0)
        .map(|(name, tokens, kind)| wire::ContextCategory {
            name: name.to_owned(),
            tokens,
            kind,
        })
        .collect(),
        total_tokens: context.used,
        max_tokens: context.size,
    }
}

/// How much of a quota is used, from 0 to 1.
fn quota_used(quota: &agy::Quota) -> f32 {
    (1.0 - quota.remaining_fraction).clamp(0.0, 1.0)
}

/// agy's quotas as the plan's limits, each named by its group: those of the group that model
/// `model` draws on as the session's five hours and week, which the sidebar shows.
fn plan_limits(quotas: &agy::Quotas, model: Option<&str>) -> Vec<wire::PlanLimit> {
    quotas
        .groups
        .iter()
        .flat_map(|group| {
            let drawn_on = model.is_some_and(|model| group.covers(model));
            group.buckets.iter().map(move |quota| {
                let (kind, span) = match (quota.window, drawn_on) {
                    (QuotaWindow::FiveHours, true) => (LimitKind::Session, "5 hours"),
                    (QuotaWindow::Week, true) => (LimitKind::WeeklyAll, "this week"),
                    (QuotaWindow::FiveHours, false) => (LimitKind::Other, "5 hours"),
                    (QuotaWindow::Week, false) => (LimitKind::Other, "this week"),
                    (QuotaWindow::Other, _) => (LimitKind::Other, "another span"),
                };
                wire::PlanLimit {
                    kind,
                    percent: quota_used(quota) * 100.0,
                    resets_at: Some(quota.reset_time),
                    scope: None,
                    label: Some(format!("{}, {span}", group.name)),
                }
            })
        })
        .collect()
}

/// The sidebar's bars from the plan's limits: the session's five hours and its week.
fn limit_windows(limits: &[wire::PlanLimit]) -> wire::LimitWindows {
    let window = |kind| {
        limits
            .iter()
            .find(|limit| limit.kind == kind)
            .map(|limit| wire::LimitWindow {
                utilization: Some(limit.percent / 100.0),
                resets_at: limit.resets_at,
            })
    };
    wire::LimitWindows {
        five_hour: window(LimitKind::Session),
        seven_day: window(LimitKind::WeeklyAll),
    }
}

/// A success answer carrying `response`; a failure if it can't be written.
fn success_as(request: RequestId, response: &impl serde::Serialize) -> Effect {
    match serde_json::to_value(response) {
        Ok(response) => success(request, response),
        Err(error) => failure(request, &format!("the answer can't be written: {error}")),
    }
}

fn success(request: RequestId, response: Value) -> Effect {
    Effect::ToWindow(Message::ControlResponse {
        response: ControlResponse::Success {
            request_id: request,
            response,
        },
    })
}

fn failure(request: RequestId, error: &str) -> Effect {
    Effect::ToWindow(Message::ControlResponse {
        response: ControlResponse::Error {
            request_id: request,
            error: error.to_owned(),
        },
    })
}

/// A prompt as the window sent it (text, or blocks of text and images), as ACP takes it.
fn prompt_blocks(content: &wire::Content) -> Vec<acp::PromptBlock> {
    match content {
        wire::Content::Text(text) => vec![acp::PromptBlock::Text { text: text.clone() }],
        wire::Content::Blocks(blocks) => blocks.iter().map(prompt_block).collect(),
    }
}

/// One block of the window's prompt, as ACP takes it.
fn prompt_block(block: &wire::Block) -> acp::PromptBlock {
    match block {
        wire::Block::Text { text } => acp::PromptBlock::Text { text: text.clone() },
        wire::Block::Image(image) => acp::PromptBlock::Image {
            mime_type: image.media_type(),
            data: image.data().to_owned(),
        },
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
/// (see [`Agent::tool`]), else by the file change it shows, else by ACP's kind, with its title
/// as what it runs, searches for or fetches; a tool none of these tells about keeps the agent's
/// name.
pub(super) fn as_claude_tool(agent: Agent, call: &ToolCall) -> (ToolName, Value) {
    let input = call.raw_input.clone().unwrap_or(Value::Null);
    if let Some(known) = call.name.as_deref().and_then(|name| agent.tool(name, call)) {
        return as_known(&known, input);
    }
    if let Some(known) = file_change(call) {
        return as_known(&known, input);
    }
    let title = || call.title.clone().unwrap_or_default();
    let known = match call.kind {
        Some(ToolKind::Execute) => KnownCall::Bash(BashInput::command(title(), None)),
        Some(ToolKind::Read) => KnownCall::Read(ReadInput::whole(
            first_location(call).unwrap_or_else(|| PathBuf::from(title())),
        )),
        Some(ToolKind::Search) => KnownCall::Grep(GrepInput {
            pattern: title(),
            path: first_location(call),
            glob: None,
        }),
        Some(ToolKind::Fetch) => KnownCall::WebFetch(WebFetchInput {
            url: title(),
            prompt: None,
        }),
        Some(
            ToolKind::Edit | ToolKind::Delete | ToolKind::Move | ToolKind::Think | ToolKind::Other,
        )
        | None => {
            let name = call
                .name
                .clone()
                .or_else(|| call.title.clone())
                .unwrap_or_else(|| "tool".to_owned());
            return (ToolName::from(name), input);
        }
    };
    as_known(&known, input)
}

/// The first file change a call shows, as an `Edit`, or a `Write` of a new file.
fn file_change(call: &ToolCall) -> Option<KnownCall> {
    call.content.iter().flatten().find_map(|part| match part {
        ToolContent::Diff {
            path,
            old_text: Some(old_text),
            new_text,
        } => Some(KnownCall::Edit(EditInput::once(
            path.clone(),
            old_text.clone(),
            new_text.clone(),
        ))),
        ToolContent::Diff {
            path,
            old_text: None,
            new_text,
        } => Some(KnownCall::Write(WriteInput {
            file_path: path.clone(),
            content: new_text.clone(),
        })),
        ToolContent::Content { .. } | ToolContent::Other => None,
    })
}

/// A known call by Claude Code's name and input; one whose input can't be written (a path
/// that isn't UTF-8) shows the agent's own input instead.
fn as_known(known: &KnownCall, raw: Value) -> (ToolName, Value) {
    let input = known.input().unwrap_or_else(|error| {
        tracing::warn!(%error, tool = %known.name(), "a tool's input that can't be written");
        raw
    });
    (known.name(), input)
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
        && let Some(Ok(printed)) = call
            .raw_output
            .as_ref()
            .map(<acp::PrintedOutput as serde::Deserialize>::deserialize)
    {
        parts.push(printed.output);
    }
    if parts.is_empty() {
        return if failed { "Failed." } else { "Done." }.to_owned();
    }
    parts.join("\n")
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::app::PermissionMode;
    use crate::external::acp::Location;
    use crate::external::wire::{
        ControlRequest, Input, MediaType, ModeId, PermissionResult, Reply, ToolInput,
    };

    const ID: &str = "11111111-2222-4333-8444-555555555555";

    /// A line the window writes, as it writes it.
    fn said(input: &Input) -> String {
        serde_json::to_string(input).expect("a line the window writes")
    }

    /// A prompt of the window's, of text alone.
    fn prompt(text: &str) -> String {
        said(&Input::prompt_with(&[], Vec::new(), text))
    }

    /// A control request of the window's.
    fn asks(request_id: &str, request: ControlRequest) -> String {
        said(&Input::ControlRequest {
            request_id: RequestId::from(request_id.to_owned()),
            request,
        })
    }

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

    /// The lines the window is sent, as the JSON it reads.
    fn window_lines(effects: &[Effect]) -> Vec<Value> {
        effects
            .iter()
            .filter_map(|effect| match effect {
                Effect::ToWindow(line) => {
                    Some(serde_json::to_value(line).expect("a line for the window"))
                }
                Effect::ToAgent(_)
                | Effect::Transcript(_)
                | Effect::Send { .. }
                | Effect::List(_)
                | Effect::Subagents { .. }
                | Effect::Quotas { .. } => None,
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
                | Effect::List(_)
                | Effect::Subagents { .. }
                | Effect::Quotas { .. } => None,
            })
            .map(|line| serde_json::from_str(&line).expect("a line of JSON"))
            .collect()
    }

    fn kinds(lines: &[Value]) -> Vec<String> {
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
        effects.extend(translator.window_said(&asks(
            "w1",
            ControlRequest::Initialize {
                per_task_stop_affordance: true,
            },
        )));
        effects.extend(
            translator.agent_said(r#"{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":1}}"#),
        );
        effects.extend(translator.agent_said(
            r#"{"jsonrpc":"2.0","id":2,"result":{"sessionId":"s","models":{"availableModels":[{"modelId":"m1","name":"Model one"}],"currentModelId":"m1"},"modes":{"availableModes":[{"id":"fast","name":"Fast"}],"currentModeId":"fast"}}}"#,
        ));
        effects.extend(translator.window_said(&prompt("go")));
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
            PeerMessages::LetAllIn,
        ));
        effects.extend(translator.window_said(&said(&Input::bash_command("ls"))));
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
                    let line = serde_json::to_value(line).expect("a line for the window");
                    read_by_the_window(&line);
                    window.extend(kinds(&[line]));
                }
                Effect::Transcript(line) => {
                    let line = serde_json::to_value(line).expect("a line for the transcript");
                    read_from_the_transcript(&line);
                    transcript.extend(kinds(&[line]));
                }
                Effect::ToAgent(_)
                | Effect::Send { .. }
                | Effect::List(_)
                | Effect::Subagents { .. }
                | Effect::Quotas { .. } => {}
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
        let early = translator.window_said(&asks(
            "r1",
            ControlRequest::Initialize {
                per_task_stop_affordance: true,
            },
        ));
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
                Effect::Transcript(line) => {
                    Some(serde_json::to_value(line).expect("a line for the transcript"))
                }
                Effect::ToAgent(_)
                | Effect::ToWindow(_)
                | Effect::Send { .. }
                | Effect::List(_)
                | Effect::Subagents { .. }
                | Effect::Quotas { .. } => None,
            })
            .expect("a transcript line");
        assert_eq!(first.get("agySessionId"), Some(&json!("s")));
    }

    /// A turn reads as Claude Code's: init, streamed text, the assistant message ending in the
    /// tool call, its result, then the result of the turn.
    #[test]
    fn a_turn_reads_as_claude_codes() {
        let mut translator = opened();
        let prompted = translator.window_said(&prompt("Count the lines"));
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
        assert_eq!(name, ToolName::Edit);
        assert_eq!(input.get("file_path"), Some(&json!("/w/notes.txt")));
        assert_eq!(input.get("old_string"), Some(&json!("two")));
        assert_eq!(input.get("new_string"), Some(&json!("TWO")));
    }

    /// agy asks before edualc's own messaging tools too: the adapter allows them, and the
    /// window isn't asked.
    #[test]
    fn edualcs_own_tools_go_without_asking() {
        let mut translator = opened();
        translator.window_said(&prompt("go"));
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
        translator.window_said(&prompt("go"));
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
        let denied = translator.window_said(&said(&Input::ControlResponse {
            response: Reply::Success {
                request_id: RequestId::from(request_id.to_owned()),
                response: PermissionResult::Deny {
                    message: "no".to_owned(),
                    interrupt: false,
                },
            },
        }));
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
        let image = wire::ImageBlock::base64(MediaType::Png, "QUJD".to_owned());
        let sent = translator.window_said(&said(&Input::prompt_with(&[], vec![image], "look")));
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
                        {"type": "image", "mimeType": "image/png", "data": "QUJD"},
                        {"type": "text", "text": "look"}
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
        translator.window_said(&prompt("go"));
        let asked = translator.agent_said(
            r#"{"jsonrpc":"2.0","method":"session/request_permission","params":{"sessionId":"s","toolCall":{"toolCallId":"c2","name":"run_command","kind":"execute","status":"pending","title":"touch /tmp/x","rawInput":{"CommandLine":"touch /tmp/x"}},"options":[{"optionId":"allow","name":"Allow","kind":"allow_once"},{"optionId":"deny","name":"Deny","kind":"reject_once"}]},"id":"7"}"#,
        );
        let request_id = window_lines(&asked)
            .into_iter()
            .find(|line| line.get("type") == Some(&json!("control_request")))
            .and_then(|line| {
                line.get("request_id")
                    .and_then(Value::as_str)
                    .map(str::to_owned)
            })
            .expect("the window asked");
        let answered = translator.window_said(&said(&Input::ControlResponse {
            response: Reply::Success {
                request_id: RequestId::from(request_id),
                response: PermissionResult::Allow {
                    updated_input: ToolInput::from(json!({"command": "touch /tmp/x"})),
                    updated_permissions: Vec::new(),
                },
            },
        }));
        assert_eq!(
            agent_lines(&answered),
            vec![json!({
                "jsonrpc": "2.0",
                "id": "7",
                "result": {"outcome": {"outcome": "selected", "optionId": "allow"}}
            })]
        );
    }

    /// A turn the agent ends by refusing, or that an answer edualc can't read ends, ends as
    /// failed with the reason in the conversation; one it ends as done ends as a success.
    #[test]
    fn a_turn_cut_short_ends_as_failed_saying_why() {
        let ended_with = |answer: &str| {
            let mut translator = opened();
            let prompted = translator.window_said(&prompt("go"));
            let rpc = prompt_id(&prompted);
            let ended = translator.agent_said(&format!(
                r#"{{"jsonrpc":"2.0","id":{rpc},"result":{answer}}}"#
            ));
            let lines = window_lines(&ended);
            let failed = lines
                .iter()
                .find(|line| line.get("type") == Some(&json!("result")))
                .and_then(|line| line.get("is_error"))
                .and_then(Value::as_bool)
                .expect("a result");
            let said = lines
                .iter()
                .filter_map(|line| line.pointer("/message/content/0/text"))
                .filter_map(Value::as_str)
                .collect::<Vec<_>>()
                .join(" ");
            (failed, said)
        };
        let (failed, said) = ended_with(r#"{"stopReason":"refusal"}"#);
        assert!(failed);
        assert!(said.contains("refused to go on"), "{said}");
        let (failed, said) = ended_with(r#"{"stopReason":"max_tokens"}"#);
        assert!(failed);
        assert!(said.contains("the most it may write"), "{said}");
        let (failed, said) = ended_with(r#"{"no":"stop reason"}"#);
        assert!(failed);
        assert!(said.contains("can't be read"), "{said}");
        let (failed, _) = ended_with(r#"{"stopReason":"end_turn"}"#);
        assert!(!failed);
    }

    /// A turn that fails because Google's API refused agy's sign-in ends with a 401 in its
    /// result, for the window to offer signing in and trying again, and agy's words aren't shown
    /// as a message. Any other failure is shown in agy's words.
    #[test]
    fn a_refused_sign_in_ends_the_turn_as_a_401() {
        let failed_with = |error: &str| {
            let mut translator = opened();
            let prompted = translator.window_said(&prompt("go"));
            let prompt_id = agent_lines(&prompted)
                .into_iter()
                .find(|line| line.get("method") == Some(&json!("session/prompt")))
                .and_then(|line| line.get("id").cloned())
                .expect("prompted");
            let ended = translator.agent_said(&format!(
                r#"{{"jsonrpc":"2.0","id":{prompt_id},"error":{error}}}"#
            ));
            window_lines(&ended)
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
        let first = translator.window_said(&prompt("hello"));
        let prompt_id = agent_lines(&first)
            .into_iter()
            .find(|line| line.get("method") == Some(&json!("session/prompt")))
            .and_then(|line| line.get("id").cloned())
            .expect("prompted");
        let envelope = "<cross-session-message from-name=\"tidy-otter-work-12\" from-mode=\"prompting\">\nHi there\n</cross-session-message>";
        let peer = translator.peer(envelope.to_owned(), PeerMessages::LetAllIn);
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

    /// A message from a sender the user hasn't let in waits while the window asks, offering
    /// to let the sender in. Turned down, the agent never sees it; let in with its sender, it
    /// comes in, and so does the sender's next message, unasked. Another sender is asked about.
    #[test]
    fn a_new_senders_message_waits_to_be_let_in() {
        let mut translator = opened();
        let from = |name: &str, text: &str| {
            format!(
                "<cross-session-message from-name=\"{name}\" from-mode=\"prompting\">\n{text}\n</cross-session-message>"
            )
        };
        let asked = window_lines(
            &translator.peer(from("tidy-otter", "First"), PeerMessages::AskEachSender),
        );
        let [card] = asked.as_slice() else {
            panic!("one question: {asked:?}");
        };
        assert_eq!(
            card.pointer("/request/tool_name"),
            Some(&json!("edualc_incoming_message"))
        );
        assert_eq!(
            card.pointer("/request/input"),
            Some(&json!({"from": "tidy-otter", "message": "First"}))
        );
        let offered = card
            .pointer("/request/permission_suggestions/0")
            .cloned()
            .expect("letting the sender in, offered");
        let answer = |request: &Value, response: Value| {
            serde_json::to_string(&json!({
                "type": "control_response",
                "response": {"subtype": "success", "request_id": request, "response": response},
            }))
            .expect("a line")
        };
        let request = card.get("request_id").cloned().expect("an id");
        let turned_down = translator.window_said(&answer(
            &request,
            json!({"behavior": "deny", "message": "No."}),
        ));
        assert!(turned_down.is_empty(), "{turned_down:?}");

        let asked = window_lines(
            &translator.peer(from("tidy-otter", "Second"), PeerMessages::AskEachSender),
        );
        let request = asked
            .first()
            .and_then(|card| card.get("request_id"))
            .cloned()
            .expect("asked again");
        let let_in = translator.window_said(&answer(
            &request,
            json!({"behavior": "allow", "updatedInput": {}, "updatedPermissions": [offered]}),
        ));
        assert_eq!(
            agent_lines(&let_in)
                .iter()
                .find_map(|line| line.pointer("/params/prompt/0/text"))
                .cloned(),
            Some(json!(from("tidy-otter", "Second")))
        );

        let unasked = translator.peer(from("tidy-otter", "Third"), PeerMessages::AskEachSender);
        assert!(
            window_lines(&unasked)
                .iter()
                .all(|line| line.get("type") != Some(&json!("control_request"))),
            "the sender was let in"
        );
        let other =
            window_lines(&translator.peer(from("calm-heron", "Hi"), PeerMessages::AskEachSender));
        assert_eq!(
            other.first().and_then(|line| line.get("type")),
            Some(&json!("control_request"))
        );
    }

    /// agy's messaging tool sends at once: nothing is asked in the window.
    #[test]
    fn messages_go_without_asking() {
        let mut translator = opened();
        let sent = translator.tool_called(CallId(4), "calm-heron".to_owned(), "hi".to_owned());
        assert!(
            matches!(
                sent.as_slice(),
                [Effect::Send { call: CallId(4), to, message }] if to == "calm-heron" && message == "hi"
            ),
            "one message sent, and nothing else: {sent:?}"
        );
    }

    /// agy says nothing of what an MCP tool gave: the call's result is what edualc's tool
    /// answered it.
    #[test]
    fn the_tools_answer_is_the_calls_result() {
        let mut translator = opened();
        translator.window_said(&prompt("go"));
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
        assert_eq!(name, ToolName::Edit);
        assert_eq!(input.get("old_string"), Some(&json!("")));
        assert_eq!(input.get("new_string"), Some(&json!("three\n")));
        let (name, input) = as_claude_tool(
            Agent::Agy,
            &call(
                "write_file",
                json!({"Content": "hi\n", "Overwrite": true, "Path": "/w/hello.txt"}),
            ),
        );
        assert_eq!(name, ToolName::Write);
        assert_eq!(input.get("content"), Some(&json!("hi\n")));
        let (name, input) = as_claude_tool(
            Agent::Agy,
            &call(
                "edit_file",
                json!({"Path": "/w/f.txt", "Replacement": "two\nthree", "Target": "two"}),
            ),
        );
        assert_eq!(name, ToolName::Edit);
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
        assert_eq!(name, ToolName::Bash);
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
        assert_eq!(name, ToolName::Read);
        assert_eq!(input.get("file_path"), Some(&json!("/w/f.txt")));
    }

    /// agy's swarm tools read as Claude Code's, with arguments as agy 1.5.9 gives them: a post
    /// to the board as a message, a member joining as an agent with its brief, its questions
    /// as AskUserQuestion's; what else `swarm` does keeps agy's name.
    #[test]
    fn agys_swarm_tools_read_as_claude_codes() {
        let shown = |name: &str, args: Value| {
            as_claude_tool(
                Agent::Agy,
                &ToolCall {
                    tool_call_id: "c".to_owned(),
                    name: Some(name.to_owned()),
                    raw_input: Some(args),
                    ..ToolCall::default()
                },
            )
        };
        assert_eq!(
            shown("post_message", json!({ "text": "@verifier Ready." })),
            (
                ToolName::EdualcSendMessage,
                json!({ "to": "the board", "message": "@verifier Ready." })
            )
        );
        let (name, input) = shown(
            "swarm",
            json!({ "action": "add", "name": "monitor", "brief": "Watch for stalls." }),
        );
        assert_eq!(name, ToolName::Agent);
        assert_eq!(input.get("subagent_type"), Some(&json!("monitor")));
        assert_eq!(input.get("prompt"), Some(&json!("Watch for stalls.")));
        assert_eq!(
            shown("swarm", json!({ "action": "status" })).0,
            ToolName::from("swarm".to_owned())
        );
        let (name, input) = shown(
            "ask_question",
            json!({ "questions": [{ "question": "Which?", "options": ["A", "B"], "is_multi_select": true }] }),
        );
        assert_eq!(name, ToolName::AskUserQuestion);
        assert_eq!(
            input,
            json!({ "questions": [{
                "question": "Which?",
                "options": [{ "label": "A" }, { "label": "B" }],
                "multiSelect": true,
            }] })
        );
    }

    /// agy's subagents are looked at as a turn ends, under agy's id for the conversation;
    /// opencode keeps no records of them, and its are never looked for.
    #[test]
    fn subagents_are_looked_at_as_a_turn_ends() {
        let looked = |agent| {
            let mut translator = opened_with(agent);
            let prompted = translator.window_said(&prompt("go"));
            let rpc = prompt_id(&prompted);
            translator
                .agent_said(&format!(
                    r#"{{"jsonrpc":"2.0","id":{rpc},"result":{{"stopReason":"end_turn"}}}}"#
                ))
                .into_iter()
                .filter_map(|effect| match effect {
                    Effect::Subagents { conversation } => Some(conversation),
                    Effect::ToAgent(_)
                    | Effect::ToWindow(_)
                    | Effect::Transcript(_)
                    | Effect::Send { .. }
                    | Effect::List(_)
                    | Effect::Quotas { .. } => None,
                })
                .collect::<Vec<_>>()
        };
        assert_eq!(looked(Agent::Agy), ["s"]);
        assert!(looked(Agent::Opencode).is_empty());
    }

    /// What a command printed, as agy gives it in the call's raw output, is the call's result;
    /// a raw output of another shape isn't.
    #[test]
    fn a_commands_printed_output_is_its_result() {
        let ended = |raw_output| {
            result_text(
                &ToolCall {
                    tool_call_id: "c".to_owned(),
                    raw_output: Some(raw_output),
                    ..ToolCall::default()
                },
                false,
            )
        };
        assert_eq!(ended(json!({ "output": "3 notes.txt\n" })), "3 notes.txt\n");
        assert_eq!(ended(json!({ "files": ["a"] })), "Done.");
    }

    /// A tool whose name edualc doesn't know reads by the file change it shows, else by its
    /// kind; one neither tells about keeps the agent's name.
    #[test]
    fn unknown_tools_read_by_their_change_or_kind() {
        let shown = |name: &str, kind, rest: ToolCall| {
            as_claude_tool(
                Agent::Agy,
                &ToolCall {
                    tool_call_id: "c".to_owned(),
                    name: Some(name.to_owned()),
                    kind: Some(kind),
                    ..rest
                },
            )
        };
        let changed = |old_text: Option<&str>| ToolCall {
            content: Some(vec![ToolContent::Diff {
                path: PathBuf::from("/w/a.rs"),
                old_text: old_text.map(str::to_owned),
                new_text: "new".to_owned(),
            }]),
            ..ToolCall::default()
        };
        assert_eq!(
            shown("custom_edit", ToolKind::Edit, changed(Some("old"))),
            (
                ToolName::Edit,
                json!({ "file_path": "/w/a.rs", "old_string": "old", "new_string": "new" })
            )
        );
        assert_eq!(
            shown("custom_create", ToolKind::Edit, changed(None)),
            (
                ToolName::Write,
                json!({ "file_path": "/w/a.rs", "content": "new" })
            )
        );
        assert_eq!(
            shown(
                "custom_search",
                ToolKind::Search,
                ToolCall {
                    title: Some("needle".to_owned()),
                    locations: Some(vec![Location {
                        path: PathBuf::from("/w/src"),
                    }]),
                    ..ToolCall::default()
                }
            ),
            (
                ToolName::Grep,
                json!({ "pattern": "needle", "path": "/w/src" })
            )
        );
        assert_eq!(
            shown(
                "custom_fetch",
                ToolKind::Fetch,
                ToolCall {
                    title: Some("https://example.com".to_owned()),
                    ..ToolCall::default()
                }
            ),
            (ToolName::WebFetch, json!({ "url": "https://example.com" }))
        );
        assert_eq!(
            shown(
                "custom_tool",
                ToolKind::Other,
                ToolCall {
                    raw_input: Some(json!({ "foo": "bar" })),
                    ..ToolCall::default()
                }
            ),
            (
                ToolName::from("custom_tool".to_owned()),
                json!({ "foo": "bar" })
            )
        );
    }

    /// A `!` command is told as not run, in the rows the window reads a command's run from,
    /// and nothing reaches agy.
    #[test]
    fn shell_commands_are_refused() {
        let mut translator = opened();
        let refused = translator.window_said(&said(&Input::bash_command("ls <dir>")));
        assert!(agent_lines(&refused).is_empty());
        let window = window_lines(&refused);
        let rows: Vec<&str> = window
            .iter()
            .filter_map(|line| line.pointer("/message/content").and_then(Value::as_str))
            .collect();
        assert_eq!(
            rows.first(),
            Some(&"<bash-input>ls &lt;dir&gt;</bash-input>")
        );
        assert!(rows.get(1).is_some_and(|row| row.contains("Not run")));
    }

    /// A request the window writes that the adapter doesn't do is answered with a failure, so
    /// the window isn't left waiting on it.
    #[test]
    fn a_request_the_adapter_doesnt_do_is_failed() {
        let mut translator = opened_with(Agent::Opencode);
        let answered = translator.window_said(&asks("u1", ControlRequest::GetUsage));
        assert!(agent_lines(&answered).is_empty());
        let answers = window_lines(&answered);
        assert_eq!(answers.len(), 1, "one answer: {answers:?}");
        let answer = answers.first().expect("the answer");
        assert_eq!(answer.pointer("/response/subtype"), Some(&json!("error")));
        assert_eq!(answer.pointer("/response/request_id"), Some(&json!("u1")));
        assert_eq!(
            answer.pointer("/response/error"),
            Some(&json!("opencode doesn't do this"))
        );
    }

    /// How full the context is, as the agent tells it, reaches the window at once, answers
    /// `get_context_usage`, and gives the turn's result the window's size.
    #[test]
    fn the_contexts_use_reaches_the_window() {
        let mut translator = opened_with(Agent::Opencode);
        let unknown =
            window_lines(&translator.window_said(&asks("c0", ControlRequest::GetContextUsage)));
        assert_eq!(
            unknown
                .first()
                .and_then(|line| line.pointer("/response/subtype")),
            Some(&json!("error"))
        );
        let told = window_lines(&translator.agent_said(
            r#"{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"s","update":{"sessionUpdate":"usage_update","used":42000,"size":1048576,"cost":{"amount":0,"currency":"USD"}}}}"#,
        ));
        assert_eq!(
            told.first()
                .map(|line| (line.get("subtype"), line.get("used"), line.get("size"))),
            Some((
                Some(&json!("context_used")),
                Some(&json!(42_000)),
                Some(&json!(1_048_576))
            ))
        );
        let answered =
            window_lines(&translator.window_said(&asks("c1", ControlRequest::GetContextUsage)));
        let report = answered
            .first()
            .and_then(|line| line.pointer("/response/response"))
            .expect("a report");
        assert_eq!(report.get("totalTokens"), Some(&json!(42_000)));
        assert_eq!(report.get("maxTokens"), Some(&json!(1_048_576)));
        assert_eq!(
            report.pointer("/categories/1/tokens"),
            Some(&json!(1_048_576 - 42_000))
        );
        let prompted = translator.window_said(&prompt("Hi"));
        let rpc = agent_lines(&prompted)
            .iter()
            .find(|line| line.get("method") == Some(&json!("session/prompt")))
            .and_then(|line| line.get("id").cloned())
            .expect("prompted");
        let ended = window_lines(&translator.agent_said(&format!(
            r#"{{"jsonrpc":"2.0","id":{rpc},"result":{{"stopReason":"end_turn"}}}}"#
        )));
        let result = ended
            .iter()
            .find(|line| line.get("type") == Some(&json!("result")))
            .expect("a result");
        assert_eq!(
            result.pointer("/modelUsage/gemini-3.8-flash-high/contextWindow"),
            Some(&json!(1_048_576))
        );
    }

    /// agy's quotas as `agy -p /usage` says them (shortened).
    fn quotas() -> agy::Quotas {
        serde_json::from_str::<agy::UsageAnswer>(
            r#"{"command":{"data":{"groups":[{"name":"Gemini Models","description":"Models within this group: Gemini 4 Hydrogen, Gemini 3.8 Flash","buckets":[{"window":"weekly","remaining_fraction":0.75,"reset_time":"2026-11-22T20:18:23Z"},{"window":"5h","remaining_fraction":0.5,"reset_time":"2026-11-16T13:26:14Z"}]},{"name":"Gemini 4 Argon","description":"Argon quota is separate.","buckets":[{"window":"weekly","remaining_fraction":0,"reset_time":"2026-11-22T20:14:06Z"}]}]}}}"#,
        )
        .expect("quotas")
        .command
        .data
    }

    /// agy is asked for its quotas for the window's `get_usage` and as each turn ends; the
    /// sidebar's bars are the quotas of the group the session's model draws on, and the Usage
    /// panel lists every group's, each named.
    #[test]
    fn agys_quotas_are_the_plans_limits() {
        let mut translator = opened();
        let asked = translator.window_said(&asks("u1", ControlRequest::GetUsage));
        assert!(
            matches!(asked.as_slice(), [Effect::Quotas { request: Some(request) }] if request.to_string() == "u1"),
            "{asked:?}"
        );
        let prompted = translator.window_said(&prompt("Hi"));
        let rpc = agent_lines(&prompted)
            .iter()
            .find(|line| line.get("method") == Some(&json!("session/prompt")))
            .and_then(|line| line.get("id").cloned())
            .expect("prompted");
        let ended = translator.agent_said(&format!(
            r#"{{"jsonrpc":"2.0","id":{rpc},"result":{{"stopReason":"end_turn"}}}}"#
        ));
        assert!(
            ended
                .iter()
                .any(|effect| matches!(effect, Effect::Quotas { request: None }))
        );

        let bars = window_lines(&translator.quotas_told(None, Ok(quotas())));
        let windows = bars
            .first()
            .and_then(|line| line.pointer("/rate_limit_info/unifiedWindows"))
            .expect("the bars");
        assert_eq!(windows.pointer("/five_hour/utilization"), Some(&json!(0.5)));
        assert_eq!(
            windows.pointer("/seven_day/utilization"),
            Some(&json!(0.25))
        );

        let report = window_lines(
            &translator.quotas_told(Some(RequestId::from("u1".to_owned())), Ok(quotas())),
        );
        let limits = report
            .first()
            .and_then(|line| line.pointer("/response/response/rate_limits/limits"))
            .and_then(Value::as_array)
            .expect("the limits");
        let named: Vec<(Option<&Value>, Option<&Value>)> = limits
            .iter()
            .map(|limit| (limit.get("kind"), limit.get("label")))
            .collect();
        assert_eq!(
            named,
            [
                (
                    Some(&json!("weekly_all")),
                    Some(&json!("Gemini Models, this week"))
                ),
                (
                    Some(&json!("session")),
                    Some(&json!("Gemini Models, 5 hours"))
                ),
                (
                    Some(&json!("other")),
                    Some(&json!("Gemini 4 Argon, this week"))
                ),
            ]
        );
    }

    /// Quotas agy couldn't say fail the window's `get_usage` with why, and leave the bars.
    #[test]
    fn quotas_that_cant_be_read_fail_the_request() {
        let translator = opened();
        assert!(
            translator
                .quotas_told(None, Err(super::super::QuotaError::TimedOut))
                .is_empty()
        );
        let failed = window_lines(&translator.quotas_told(
            Some(RequestId::from("u1".to_owned())),
            Err(super::super::QuotaError::TimedOut),
        ));
        assert_eq!(
            failed
                .first()
                .and_then(|line| line.pointer("/response/error")),
            Some(&json!(
                "agy couldn't say its plan's quotas: it didn't answer in time"
            ))
        );
    }

    /// A line the window wrote that edualc can't read, but that is a control request, is
    /// answered with a failure under its request id, saying edualc can't read it.
    #[test]
    fn a_control_request_edualc_cant_read_is_failed_under_its_id() {
        let mut translator = opened();
        let unreadable = translator.window_said(
            r#"{"type":"control_request","request_id":"x1","request":{"subtype":"from_a_newer_edualc"}}"#,
        );
        assert!(agent_lines(&unreadable).is_empty());
        let answers = window_lines(&unreadable);
        assert_eq!(answers.len(), 1, "one answer: {answers:?}");
        let answer = answers.first().expect("the answer");
        assert_eq!(answer.pointer("/response/subtype"), Some(&json!("error")));
        assert_eq!(answer.pointer("/response/request_id"), Some(&json!("x1")));
        assert!(
            answer
                .pointer("/response/error")
                .and_then(Value::as_str)
                .is_some_and(|error| error.starts_with("edualc can't read this request:")),
            "the reason is said: {answer}"
        );
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
        translator.window_said(&asks(
            "r1",
            ControlRequest::Initialize {
                per_task_stop_affordance: true,
            },
        ));
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

        let asked = translator.window_said(&asks(
            "r2",
            ControlRequest::SetProgramMode {
                mode: ModeId::from("accept-edits".to_owned()),
            },
        ));
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
        let again = translator.window_said(&asks(
            "r3",
            ControlRequest::Initialize {
                per_task_stop_affordance: true,
            },
        ));
        assert_eq!(
            window_lines(&again)
                .first()
                .and_then(|line| line.pointer("/response/response/programModes/current")),
            Some(&json!("accept-edits"))
        );

        let claudes = translator.window_said(&asks(
            "r4",
            ControlRequest::SetPermissionMode {
                mode: PermissionMode::Plan,
            },
        ));
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
