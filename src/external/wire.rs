//! What `claude -p --output-format stream-json --verbose --include-partial-messages
//! --permission-prompt-tool stdio` says, and what its transcripts hold, as types: parsed where
//! it enters, so the rest matches on variants instead of looking into JSON. Fields the window
//! doesn't use are left out; variants it doesn't know become `Other`. What the window writes to
//! its input (`--input-format stream-json`) is typed the same way, at the end.

use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::app::{Mode, PermissionMode};

/// A session, as Claude Code names its transcript (`<id>.jsonl`) and takes it in `--resume`: a
/// UUID. Ids come from file names and records other processes write, and end up on `claude`'s
/// command line and in paths, so anything else (a file named `--settings=….jsonl`, a `../`) is
/// refused where it enters.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Deserialize, Serialize, derive_more::Display)]
#[serde(try_from = "String")]
pub(crate) struct SessionId(String);

#[derive(Debug, thiserror::Error)]
#[error("{0:?} isn't a session id")]
pub(crate) struct NotASessionId(String);

impl TryFrom<String> for SessionId {
    type Error = NotASessionId;

    fn try_from(id: String) -> Result<Self, Self::Error> {
        let dashes_at = [8, 13, 18, 23];
        let uuid = id.len() == 36
            && id.char_indices().all(|(at, character)| {
                if dashes_at.contains(&at) {
                    character == '-'
                } else {
                    character.is_ascii_hexdigit()
                }
            });
        if uuid {
            Ok(Self(id))
        } else {
            Err(NotASessionId(id))
        }
    }
}

impl SessionId {
    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }

    /// A fresh id, for a session started (or forked) here: Claude Code takes it with
    /// `--session-id`, so it is known before the session's first prompt.
    pub(crate) fn new_random() -> Self {
        Self(uuid::Uuid::new_v4().to_string())
    }
}

/// A tool call, which permission requests and results refer back to.
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash, Deserialize)]
#[serde(transparent)]
pub(crate) struct ToolUseId(String);

/// A task Claude Code runs (a subagent, a shell command, a monitor), the same on every event
/// about it and in its background list.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Deserialize)]
#[serde(transparent)]
pub(crate) struct TaskId(String);

/// An API message, whose stream events and complete copy arrive separately.
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash, Deserialize)]
#[serde(transparent)]
pub(crate) struct MessageId(String);

/// A control request, either way: its answer names it.
#[derive(
    Debug,
    Clone,
    PartialEq,
    Eq,
    Hash,
    Deserialize,
    Serialize,
    derive_more::From,
    derive_more::Display,
)]
#[serde(transparent)]
pub(crate) struct RequestId(String);

/// A tool Claude calls, as the CLI names it; an MCP tool (`mcp__<server>__<tool>`) or one this
/// window doesn't know keeps its name.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, strum::EnumString, strum::Display)]
#[serde(from = "String")]
pub(crate) enum ToolName {
    Bash,
    BashOutput,
    KillShell,
    Monitor,
    Read,
    NotebookRead,
    Write,
    Edit,
    MultiEdit,
    NotebookEdit,
    Glob,
    Grep,
    #[strum(serialize = "LS")]
    Ls,
    WebFetch,
    WebSearch,
    Task,
    Agent,
    Skill,
    TodoWrite,
    AskUserQuestion,
    ExitPlanMode,
    #[strum(default)]
    Other(String),
}

impl From<String> for ToolName {
    fn from(name: String) -> Self {
        name.parse().unwrap_or(Self::Other(name))
    }
}

/// What a call does, as a run of calls sums it up and a permission prompt asks about it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ToolKind {
    Read,
    Edit,
    Write,
    Command,
    Search,
    Web,
    Agent,
    Other,
}

impl ToolName {
    pub(crate) fn kind(&self) -> ToolKind {
        match self {
            Self::Read | Self::NotebookRead => ToolKind::Read,
            Self::Edit | Self::MultiEdit | Self::NotebookEdit => ToolKind::Edit,
            Self::Write => ToolKind::Write,
            Self::Bash | Self::BashOutput | Self::KillShell | Self::Monitor => ToolKind::Command,
            Self::Glob | Self::Grep | Self::Ls => ToolKind::Search,
            Self::WebFetch | Self::WebSearch => ToolKind::Web,
            Self::Task | Self::Agent => ToolKind::Agent,
            Self::Skill
            | Self::TodoWrite
            | Self::AskUserQuestion
            | Self::ExitPlanMode
            | Self::Other(_) => ToolKind::Other,
        }
    }

    /// Calls whose card opens without a click: what changes, and plans to approve.
    pub(crate) fn opens_expanded(&self) -> bool {
        match self {
            Self::Write
            | Self::Edit
            | Self::MultiEdit
            | Self::NotebookEdit
            | Self::ExitPlanMode => true,
            Self::Bash
            | Self::BashOutput
            | Self::KillShell
            | Self::Monitor
            | Self::Read
            | Self::NotebookRead
            | Self::Glob
            | Self::Grep
            | Self::Ls
            | Self::WebFetch
            | Self::WebSearch
            | Self::Task
            | Self::Agent
            | Self::Skill
            | Self::TodoWrite
            | Self::AskUserQuestion
            | Self::Other(_) => false,
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(crate) enum Message {
    StreamEvent {
        event: StreamEvent,
        parent_tool_use_id: Option<ToolUseId>,
    },
    Assistant {
        message: ApiMessage,
        parent_tool_use_id: Option<ToolUseId>,
    },
    User {
        message: UserMessage,
    },
    System(System),
    Result(TurnResult),
    RateLimitEvent {
        rate_limit_info: RateLimits,
    },
    ControlRequest {
        request_id: RequestId,
        request: IncomingRequest,
    },
    ControlResponse {
        response: ControlResponse,
    },
    ControlCancelRequest {
        request_id: RequestId,
    },
    #[serde(other)]
    Other,
}

/// A raw API stream event.
#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(crate) enum StreamEvent {
    MessageStart {
        message: MessageStart,
    },
    ContentBlockStart {
        index: u64,
        content_block: ContentBlock,
    },
    ContentBlockDelta {
        index: u64,
        delta: Delta,
    },
    ContentBlockStop {
        index: u64,
    },
    #[serde(other)]
    Other,
}

#[derive(Debug, Deserialize)]
pub(crate) struct MessageStart {
    pub(crate) id: MessageId,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(crate) enum Delta {
    #[serde(rename = "text_delta")]
    Text { text: String },
    #[serde(rename = "thinking_delta")]
    Thinking { thinking: String },
    #[serde(rename = "input_json_delta")]
    InputJson { partial_json: String },
    #[serde(other)]
    Other,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(crate) enum ContentBlock {
    Text {
        text: String,
    },
    Thinking {
        #[serde(default)]
        thinking: String,
    },
    ToolUse {
        id: ToolUseId,
        name: ToolName,
        #[serde(default)]
        input: Value,
    },
    ServerToolUse {
        id: ToolUseId,
        name: ToolName,
        #[serde(default)]
        input: Value,
    },
    ToolResult {
        tool_use_id: ToolUseId,
        content: Option<UserContent>,
        #[serde(default)]
        is_error: bool,
    },
    /// An image the user sent, or a tool returned (a file read, a screenshot).
    Image {
        #[serde(default)]
        source: Option<ReceivedImage>,
    },
    #[serde(other)]
    Other,
}

/// Where an image in a message has its bytes. Its media type isn't taken: the decoder tells
/// from the bytes.
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(crate) enum ReceivedImage {
    Base64 {
        data: String,
    },
    #[serde(other)]
    Other,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct ApiMessage {
    #[serde(default)]
    pub(crate) id: MessageId,
    #[serde(default)]
    pub(crate) content: Vec<ContentBlock>,
    #[serde(default)]
    pub(crate) usage: Usage,
}

#[derive(Debug, Clone, Copy, Default, Deserialize)]
pub(crate) struct Usage {
    #[serde(default)]
    pub(crate) input_tokens: u64,
    #[serde(default)]
    pub(crate) cache_creation_input_tokens: u64,
    #[serde(default)]
    pub(crate) cache_read_input_tokens: u64,
    #[serde(default)]
    pub(crate) output_tokens: u64,
}

impl Usage {
    /// The context the request took: everything sent, and what came back.
    pub(crate) fn context(self) -> u64 {
        self.input_tokens
            + self.cache_creation_input_tokens
            + self.cache_read_input_tokens
            + self.output_tokens
    }
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct UserMessage {
    pub(crate) content: UserContent,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub(crate) enum UserContent {
    Text(String),
    Blocks(Vec<ContentBlock>),
}

#[derive(Debug, Deserialize)]
#[serde(tag = "subtype", rename_all = "snake_case")]
pub(crate) enum System {
    /// Sent at the start of every turn.
    Init {
        session_id: SessionId,
        model: String,
        #[serde(rename = "permissionMode")]
        permission_mode: Mode,
        /// Where the session works now: a worktree, once it entered one.
        cwd: PathBuf,
    },
    /// A long control request of ours (a side question) is under way.
    ControlRequestProgress {
        request_id: RequestId,
        status: Progress,
    },
    Status {
        status: Option<Activity>,
    },
    ThinkingTokens {
        estimated_tokens: u64,
    },
    CompactBoundary,
    /// Denied by a rule or the mode, without asking.
    PermissionDenied {
        tool_use_id: ToolUseId,
    },
    /// What runs in the background now (subagents, shell commands, monitors), all of it,
    /// whenever that changes. Their tool calls are done by then, so nothing else shows them.
    BackgroundTasksChanged {
        tasks: Vec<BackgroundTask>,
    },
    /// A task began: in the background, or in the foreground with its tool call waiting for it.
    /// Foreground tasks are in no background list until they are moved there.
    TaskStarted {
        task_id: TaskId,
        #[serde(default)]
        task_type: Option<TaskKind>,
        #[serde(default)]
        subagent_type: Option<String>,
        #[serde(default)]
        description: String,
        /// Told for agents and shell commands only.
        #[serde(default)]
        is_backgrounded: Option<bool>,
        #[serde(default)]
        ambient: bool,
    },
    TaskUpdated {
        task_id: TaskId,
        patch: TaskPatch,
    },
    /// A task ended, however it did.
    TaskNotification {
        task_id: TaskId,
    },
    /// A request to the API failed; Claude Code tries it again after a while.
    ApiRetry {
        attempt: u32,
        max_retries: u32,
        #[serde(default)]
        error_status: Option<u16>,
        #[serde(default)]
        error: RetryCause,
    },
    #[serde(other)]
    Other,
}

/// Why Claude Code tries a request to the API again, as it sorts the failure.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum RetryCause {
    Overloaded,
    RateLimit,
    AuthenticationFailed,
    ServerError,
    CloudCredentialError,
    /// No HTTP status: the connection failed, or something Claude Code doesn't sort.
    #[default]
    #[serde(other)]
    Unknown,
}

/// What changed about a task.
#[derive(Debug, Deserialize)]
pub(crate) struct TaskPatch {
    #[serde(default)]
    pub(crate) status: Option<TaskStatus>,
    /// Moved to the background (true).
    #[serde(default)]
    pub(crate) is_backgrounded: Option<bool>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum TaskStatus {
    Pending,
    Running,
    Paused,
    Completed,
    Failed,
    Killed,
    #[serde(other)]
    Other,
}

impl TaskStatus {
    /// Whether the task has ended.
    pub(crate) fn ended(self) -> bool {
        matches!(self, Self::Completed | Self::Failed | Self::Killed)
    }
}

/// A task Claude Code runs in the background, as its list names it.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub(crate) struct BackgroundTask {
    pub(crate) task_id: TaskId,
    pub(crate) task_type: TaskKind,
    /// An agent's kind ("general-purpose", "Explore").
    #[serde(default)]
    pub(crate) subagent_type: Option<String>,
    #[serde(default)]
    pub(crate) description: String,
    /// Run for Claude Code itself, not for the conversation; its own list leaves these out.
    #[serde(default)]
    pub(crate) ambient: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum TaskKind {
    LocalAgent,
    RemoteAgent,
    /// An agent team's member.
    InProcessTeammate,
    LocalBash,
    LocalWorkflow,
    MonitorMcp,
    MonitorWs,
    #[serde(other)]
    Other,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Progress {
    Started,
    ApiRetry,
    #[serde(other)]
    Other,
}

/// The answer to a `side_question` (the CLI's `/btw`).
#[derive(Debug, Deserialize)]
pub(crate) struct SideAnswer {
    pub(crate) response: String,
    /// Made up by the CLI rather than the model, e.g. "No answer available".
    #[serde(default)]
    pub(crate) synthetic: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Activity {
    Requesting,
    Compacting,
    #[serde(other)]
    Other,
}

#[derive(Debug, Deserialize)]
pub(crate) struct TurnResult {
    pub(crate) subtype: ResultKind,
    #[serde(default)]
    pub(crate) is_error: bool,
    pub(crate) result: Option<String>,
    #[serde(default)]
    pub(crate) errors: Vec<Value>,
    pub(crate) total_cost_usd: Option<f64>,
    #[serde(default, rename = "modelUsage")]
    pub(crate) model_usage: HashMap<String, ModelUsage>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ResultKind {
    Success,
    /// Interrupted, among others.
    ErrorDuringExecution,
    #[serde(other)]
    Other,
}

#[derive(Debug, Deserialize)]
pub(crate) struct ModelUsage {
    #[serde(rename = "contextWindow")]
    pub(crate) context_window: Option<u64>,
}

#[derive(Debug, Default, Deserialize)]
pub(crate) struct RateLimits {
    #[serde(default, rename = "unifiedWindows")]
    pub(crate) windows: LimitWindows,
}

#[derive(Debug, Default, Deserialize)]
pub(crate) struct LimitWindows {
    pub(crate) five_hour: Option<LimitWindow>,
    pub(crate) seven_day: Option<LimitWindow>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct LimitWindow {
    /// Used, from 0 to 1 (as Slint takes it, for a bar).
    pub(crate) utilization: Option<f32>,
    /// When the window starts over (sent in seconds since the epoch).
    #[serde(
        default,
        rename = "resetsAt",
        with = "jiff::fmt::serde::timestamp::second::optional"
    )]
    pub(crate) resets_at: Option<jiff::Timestamp>,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "subtype", rename_all = "snake_case")]
pub(crate) enum IncomingRequest {
    CanUseTool {
        tool_name: ToolName,
        #[serde(default)]
        input: Value,
        /// Rules the CLI offers to remember, which it wants back as they are.
        #[serde(default)]
        permission_suggestions: Vec<Value>,
        #[serde(default)]
        description: String,
        tool_use_id: Option<ToolUseId>,
    },
    #[serde(other)]
    Other,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "subtype", rename_all = "snake_case")]
pub(crate) enum ControlResponse {
    Success {
        request_id: RequestId,
        #[serde(default)]
        response: Value,
    },
    Error {
        request_id: RequestId,
        #[serde(default)]
        error: String,
    },
}

/// The answer to `initialize`: what the CLI offers.
#[derive(Debug, Default, Deserialize)]
pub(crate) struct Initialized {
    #[serde(default)]
    pub(crate) models: Vec<ModelInfo>,
    #[serde(default)]
    pub(crate) commands: Vec<CommandInfo>,
    #[serde(default)]
    pub(crate) account: Account,
    #[serde(default)]
    pub(crate) claude_code_version: String,
}

#[derive(Debug, Deserialize)]
pub(crate) struct ModelInfo {
    pub(crate) value: String,
    #[serde(rename = "displayName")]
    pub(crate) display_name: String,
    #[serde(default, rename = "resolvedModel")]
    pub(crate) resolved_model: String,
    #[serde(default)]
    pub(crate) description: String,
}

#[derive(Debug, Deserialize)]
pub(crate) struct CommandInfo {
    pub(crate) name: String,
    #[serde(default)]
    pub(crate) description: String,
    #[serde(default, rename = "argumentHint")]
    pub(crate) argument_hint: String,
}

#[derive(Debug, Default, Deserialize)]
pub(crate) struct Account {
    #[serde(default)]
    pub(crate) email: String,
    #[serde(default, rename = "subscriptionType")]
    pub(crate) subscription_type: String,
    #[serde(default)]
    pub(crate) organization: String,
    #[serde(default, rename = "apiProvider")]
    pub(crate) api_provider: String,
}

/// The answer to `get_usage` (the CLI's `/usage`), as far as the window shows it.
#[derive(Debug, Default, Deserialize)]
pub(crate) struct UsageReport {
    #[serde(default)]
    pub(crate) session: SessionUsage,
    #[serde(default)]
    pub(crate) rate_limits: RateLimitReport,
    #[serde(default)]
    pub(crate) behaviors: Behaviors,
}

#[derive(Debug, Default, Deserialize)]
pub(crate) struct RateLimitReport {
    #[serde(default)]
    pub(crate) limits: Vec<PlanLimit>,
}

#[derive(Debug, Default, Deserialize)]
pub(crate) struct SessionUsage {
    #[serde(default)]
    pub(crate) total_cost_usd: f64,
    #[serde(default)]
    pub(crate) total_api_duration_ms: u64,
    #[serde(default)]
    pub(crate) total_duration_ms: u64,
    #[serde(default)]
    pub(crate) total_lines_added: u64,
    #[serde(default)]
    pub(crate) total_lines_removed: u64,
    #[serde(default)]
    pub(crate) model_usage: HashMap<String, ModelTokens>,
}

/// A model's share of the session, as `modelUsage` names it (camelCase) or snake_case.
#[derive(Debug, Default, Deserialize)]
pub(crate) struct ModelTokens {
    #[serde(default, alias = "inputTokens")]
    pub(crate) input_tokens: u64,
    #[serde(default, alias = "outputTokens")]
    pub(crate) output_tokens: u64,
    #[serde(default, alias = "cacheReadInputTokens")]
    pub(crate) cache_read_input_tokens: u64,
    #[serde(default, alias = "cacheCreationInputTokens")]
    pub(crate) cache_creation_input_tokens: u64,
    #[serde(default, alias = "costUSD")]
    pub(crate) cost_usd: f64,
}

/// A limit of the plan: the session's five hours, the week's, the week's for one model.
#[derive(Debug, Deserialize)]
pub(crate) struct PlanLimit {
    pub(crate) kind: LimitKind,
    #[serde(default)]
    pub(crate) percent: f32,
    pub(crate) resets_at: Option<jiff::Timestamp>,
    pub(crate) scope: Option<LimitScope>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum LimitKind {
    /// The five hours a session window lasts.
    Session,
    WeeklyAll,
    /// The week's, for one model.
    WeeklyScoped,
    #[serde(other)]
    Other,
}

#[derive(Debug, Deserialize)]
pub(crate) struct LimitScope {
    pub(crate) model: Option<ScopeModel>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct ScopeModel {
    pub(crate) display_name: Option<String>,
}

/// How the account was used over the last day and week, with what the CLI recommends.
#[derive(Debug, Default, Deserialize)]
pub(crate) struct Behaviors {
    pub(crate) day: Option<Period>,
    pub(crate) week: Option<Period>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct Period {
    #[serde(default)]
    pub(crate) request_count: u64,
    #[serde(default)]
    pub(crate) session_count: u64,
    #[serde(default)]
    pub(crate) recommendations: Vec<Recommendation>,
    #[serde(default)]
    pub(crate) agents: Vec<Share>,
    #[serde(default)]
    pub(crate) skills: Vec<Share>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct Recommendation {
    pub(crate) title: String,
    #[serde(default)]
    pub(crate) detail: String,
    /// Of the period's requests, in percent, that it is about.
    #[serde(default)]
    pub(crate) pct: f32,
}

#[derive(Debug, Deserialize)]
pub(crate) struct Share {
    pub(crate) name: String,
    #[serde(default)]
    pub(crate) pct: f32,
}

/// The answer to `get_context_usage` (the CLI's `/context`).
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ContextReport {
    #[serde(default)]
    pub(crate) categories: Vec<ContextCategory>,
    #[serde(default)]
    pub(crate) total_tokens: u64,
    #[serde(default)]
    pub(crate) max_tokens: u64,
}

#[derive(Debug, Deserialize)]
pub(crate) struct ContextCategory {
    pub(crate) name: String,
    #[serde(default)]
    pub(crate) tokens: u64,
    #[serde(default)]
    pub(crate) kind: ContextKind,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ContextKind {
    #[default]
    Used,
    /// Loaded only when needed (tools and skills held back).
    Deferred,
    /// Kept free for compacting.
    Buffer,
    Free,
    #[serde(other)]
    Other,
}

/// A rule the CLI suggests remembering, as far as the window names it.
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub(crate) enum Suggestion {
    AddRules {
        rules: Vec<Rule>,
        #[serde(default)]
        destination: RuleDestination,
    },
    SetMode {
        mode: Mode,
    },
    AddDirectories {
        directories: Vec<String>,
        #[serde(default)]
        destination: RuleDestination,
    },
    #[serde(other)]
    Other,
}

/// Where a remembered rule is kept, and so for how long it holds.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) enum RuleDestination {
    #[default]
    Session,
    /// `.claude/settings.local.json` in the project.
    LocalSettings,
    /// `.claude/settings.json` in the project, shared with everyone who checks it out.
    ProjectSettings,
    /// `~/.claude/settings.json`: every project.
    UserSettings,
    #[serde(other)]
    Other,
}

/// A rule the CLI offers to remember: read once, and given back as it came.
#[derive(Debug, Clone)]
pub(crate) struct Offered {
    /// What the window understood of it; `None` when it didn't, and offers it not.
    pub(crate) rule: Option<Suggestion>,
    pub(crate) raw: Value,
}

impl Offered {
    /// Kept in the user's own settings, for every project.
    pub(crate) fn for_every_project(&self) -> bool {
        matches!(
            self.rule,
            Some(
                Suggestion::AddRules {
                    destination: RuleDestination::UserSettings,
                    ..
                } | Suggestion::AddDirectories {
                    destination: RuleDestination::UserSettings,
                    ..
                }
            )
        )
    }

    pub(crate) fn read(raw: Value) -> Self {
        let rule = match Suggestion::deserialize(&raw) {
            Ok(Suggestion::Other) => None,
            Ok(rule) => Some(rule),
            Err(error) => {
                tracing::debug!(%error, "a suggested rule that doesn't parse");
                None
            }
        };
        Self { rule, raw }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct Rule {
    #[serde(rename = "toolName")]
    pub(crate) tool_name: String,
    #[serde(rename = "ruleContent")]
    pub(crate) rule_content: Option<String>,
}

/// AskUserQuestion's input, given back with the user's answers (by question). What else Claude
/// sent goes back as it came.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub(crate) struct Questions {
    pub(crate) questions: Vec<Question>,
    /// The user's alone: answers Claude puts in its input are dropped, not taken as theirs.
    #[serde(
        default,
        deserialize_with = "dropped",
        skip_serializing_if = "BTreeMap::is_empty"
    )]
    pub(crate) answers: BTreeMap<String, String>,
    /// The user's notes, and the preview of the option they picked, by question: theirs alone,
    /// as the answers are.
    #[serde(
        default,
        deserialize_with = "dropped",
        skip_serializing_if = "BTreeMap::is_empty"
    )]
    pub(crate) annotations: BTreeMap<String, Annotation>,
    #[serde(flatten)]
    pub(crate) rest: Map<String, Value>,
}

/// What the user adds to an answer, as AskUserQuestion takes it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub(crate) struct Annotation {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) notes: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) preview: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Question {
    pub(crate) question: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub(crate) header: String,
    #[serde(default)]
    pub(crate) options: Vec<QuestionOption>,
    #[serde(default)]
    pub(crate) multi_select: bool,
    #[serde(flatten)]
    pub(crate) rest: Map<String, Value>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub(crate) struct QuestionOption {
    pub(crate) label: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub(crate) description: String,
    /// What the option would look like (code, a mockup), shown beside the options in a
    /// monospace box.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub(crate) preview: String,
    #[serde(flatten)]
    pub(crate) rest: Map<String, Value>,
}

/// Reads a field's value and drops it, so that it isn't kept as an unknown field either.
fn dropped<'de, D: serde::Deserializer<'de>, T: Default>(deserializer: D) -> Result<T, D::Error> {
    serde::de::IgnoredAny::deserialize(deserializer).map(|_| T::default())
}

/// ExitPlanMode's input, given back with the plan as the user annotated it.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub(crate) struct PlanInput {
    pub(crate) plan: String,
    #[serde(flatten)]
    pub(crate) rest: Map<String, Value>,
}

/// A line of a saved transcript (`~/.claude/projects/<project>/<id>.jsonl`).
#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
pub(crate) enum TranscriptLine {
    User {
        message: UserMessage,
        #[serde(default, rename = "isMeta")]
        is_meta: bool,
        #[serde(default, rename = "isSidechain")]
        is_sidechain: bool,
    },
    Assistant {
        message: ApiMessage,
        #[serde(default, rename = "isSidechain")]
        is_sidechain: bool,
    },
    System(System),
    CustomTitle {
        #[serde(rename = "customTitle")]
        custom_title: String,
    },
    AiTitle {
        #[serde(rename = "aiTitle")]
        ai_title: String,
    },
    Summary {
        summary: String,
    },
    /// The worktree the session works in, written as it goes; `None` once it left it.
    WorktreeState {
        #[serde(rename = "worktreeSession")]
        worktree: Option<WorktreeSession>,
    },
    #[serde(other)]
    Other,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct WorktreeSession {
    #[serde(rename = "worktreePath")]
    pub(crate) path: PathBuf,
    #[serde(rename = "worktreeName", default)]
    pub(crate) name: String,
    #[serde(rename = "worktreeBranch", default)]
    pub(crate) branch: String,
}

/// The folder a transcript line says the session worked in, read from its first lines.
#[derive(Debug, Deserialize)]
pub(crate) struct TranscriptCwd {
    pub(crate) cwd: PathBuf,
}

/// Just enough of a line the process writes, or is sent, to tell its `init` and its permission
/// requests and their answers apart; the daemon passes the line itself on as it is.
#[derive(Debug, Deserialize)]
pub(crate) struct ControlPeek {
    #[serde(rename = "type")]
    pub(crate) kind: PeekedKind,
    #[serde(default)]
    pub(crate) subtype: Option<PeekedSubtype>,
    #[serde(default)]
    pub(crate) request_id: Option<String>,
    #[serde(default)]
    pub(crate) response: Option<PeekedResponse>,
    #[serde(default)]
    pub(crate) task_id: Option<TaskId>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum PeekedKind {
    System,
    ControlRequest,
    ControlCancelRequest,
    ControlResponse,
    /// From the window, a prompt: a turn starts.
    User,
    /// From the process, a turn's end.
    Result,
    #[serde(other)]
    Other,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum PeekedSubtype {
    Init,
    BackgroundTasksChanged,
    TaskStarted,
    TaskUpdated,
    TaskNotification,
    #[serde(other)]
    Other,
}

#[derive(Debug, Deserialize)]
pub(crate) struct PeekedResponse {
    #[serde(default)]
    pub(crate) request_id: Option<String>,
}

/// A line to the CLI's input.
#[derive(Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(crate) enum Input<'a> {
    /// A prompt, as the user wrote it.
    User {
        message: Prompt<'a>,
        /// Only a subagent's prompts have one.
        parent_tool_use_id: Option<&'a str>,
        session_id: &'a str,
    },
    ControlRequest {
        request_id: &'a RequestId,
        request: &'a ControlRequest,
    },
    /// The answer to one of the CLI's control requests.
    ControlResponse { response: Reply<'a> },
    /// A `!` command, which the CLI runs in a shell beside the conversation without the model
    /// seeing it, answering with replayed user rows: its echo (`<bash-input>`), then its output.
    /// One runs at a time; an interrupt stops it. Undocumented: the SDK's schema marks it
    /// internal (Claude Code 2.1.316).
    BashCommand {
        command: &'a str,
        /// A repeat of an id is skipped.
        uuid: uuid::Uuid,
    },
}

impl<'a> Input<'a> {
    /// A prompt as the user sent it. First what they ran with `!` since the last: those rows,
    /// as the terminal gives them to the model, ahead of the prompt. Then the images, as the
    /// Messages API takes them, then the text. A prompt of text alone stays a plain string.
    pub(crate) fn prompt_with(shell: &'a [String], images: Vec<ImageBlock>, text: &'a str) -> Self {
        if shell.is_empty() && images.is_empty() {
            return Self::user(Content::Text(text));
        }
        let mut blocks: Vec<Block<'a>> = shell
            .iter()
            .map(|row| Block::Text { text: row.as_str() })
            .collect();
        blocks.extend(images.into_iter().map(Block::Image));
        if !text.is_empty() {
            blocks.push(Block::Text { text });
        }
        Self::user(Content::Blocks(blocks))
    }

    pub(crate) fn bash_command(command: &'a str) -> Self {
        Self::BashCommand {
            command,
            uuid: uuid::Uuid::new_v4(),
        }
    }

    fn user(content: Content<'a>) -> Self {
        Self::User {
            message: Prompt {
                role: Role::User,
                content,
            },
            parent_tool_use_id: None,
            session_id: "default",
        }
    }
}

#[derive(Debug, Serialize)]
pub(crate) struct Prompt<'a> {
    role: Role,
    content: Content<'a>,
}

/// A prompt's content: its text alone, or blocks of images and text.
#[derive(Debug, Serialize)]
#[serde(untagged)]
enum Content<'a> {
    Text(&'a str),
    Blocks(Vec<Block<'a>>),
}

#[derive(Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum Block<'a> {
    Image(ImageBlock),
    Text { text: &'a str },
}

/// An image in a prompt, its bytes base64-encoded.
#[derive(Debug, Serialize)]
pub(crate) struct ImageBlock {
    source: ImageSource,
}

impl ImageBlock {
    pub(crate) fn base64(media_type: MediaType, data: String) -> Self {
        Self {
            source: ImageSource {
                kind: SourceKind::Base64,
                media_type,
                data,
            },
        }
    }
}

#[derive(Debug, Serialize)]
struct ImageSource {
    #[serde(rename = "type")]
    kind: SourceKind,
    media_type: MediaType,
    data: String,
}

#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "lowercase")]
enum SourceKind {
    Base64,
}

/// The image formats edualc sends: Anthropic's vision docs, "Claude supports JPEG, PNG, GIF,
/// and WebP images (`image/jpeg`, `image/png`, `image/gif`, `image/webp`)". Named so in JSON;
/// postcard, between edualc's processes, writes only the variant.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    Serialize,
    Deserialize,
    strum::AsRefStr,
    strum::Display,
    strum::EnumString,
)]
pub(crate) enum MediaType {
    #[serde(rename = "image/png")]
    #[strum(serialize = "image/png")]
    Png,
    #[serde(rename = "image/jpeg")]
    #[strum(serialize = "image/jpeg")]
    Jpeg,
}

#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "lowercase")]
enum Role {
    User,
}

/// What the window asks of the CLI.
#[derive(Debug, Serialize)]
#[serde(tag = "subtype", rename_all = "snake_case")]
pub(crate) enum ControlRequest {
    Initialize,
    Interrupt,
    SetModel {
        model: String,
    },
    SetPermissionMode {
        mode: PermissionMode,
    },
    /// `/btw`: a question on the side, with the side questions answered so far.
    SideQuestion {
        question: String,
        history: Vec<SideTurn>,
    },
    /// `/usage`.
    GetUsage,
    /// `/context`.
    GetContextUsage,
}

#[derive(Debug, PartialEq, Eq, Serialize)]
pub(crate) struct SideTurn {
    pub(crate) question: String,
    pub(crate) response: String,
}

#[derive(Debug, Serialize)]
#[serde(tag = "subtype", rename_all = "snake_case")]
pub(crate) enum Reply<'a> {
    Success {
        request_id: &'a RequestId,
        response: &'a PermissionResult,
    },
    /// Turns down a request the window doesn't handle.
    Error {
        request_id: &'a RequestId,
        error: &'a str,
    },
}

/// The answer to a permission request (`can_use_tool`).
#[derive(Debug, Serialize)]
#[serde(tag = "behavior", rename_all = "snake_case")]
pub(crate) enum PermissionResult {
    #[serde(rename_all = "camelCase")]
    Allow {
        updated_input: ToolInput,
        #[serde(skip_serializing_if = "Vec::is_empty")]
        updated_permissions: Vec<PermissionUpdate>,
    },
    Deny {
        message: String,
        /// Ends the turn, as denying with nothing said does in the terminal.
        #[serde(skip_serializing_if = "std::ops::Not::not")]
        interrupt: bool,
    },
}

/// What an allowed tool runs with: as Claude asked, or with the user's part in it.
#[derive(Debug, Serialize)]
#[serde(untagged)]
pub(crate) enum ToolInput {
    AsAsked(Value),
    Plan(PlanInput),
    Answered(Questions),
}

/// A rule to remember along with an answer.
#[derive(Debug, Serialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub(crate) enum PermissionUpdate {
    SetMode {
        mode: PermissionMode,
        destination: Destination,
    },
    /// One the CLI offered, as it wants it back.
    #[serde(untagged)]
    Offered(Value),
}

/// For how long a remembered rule holds.
#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) enum Destination {
    Session,
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn messages_parse_as_the_cli_sends_them() {
        let line = json!({
            "type": "control_request",
            "request_id": "r1",
            "request": {
                "subtype": "can_use_tool",
                "tool_name": "Write",
                "input": { "file_path": "/a" },
                "permission_suggestions": [
                    { "type": "setMode", "mode": "acceptEdits", "destination": "session" },
                ],
                "tool_use_id": "t1",
            },
        });
        let Ok(Message::ControlRequest {
            request:
                IncomingRequest::CanUseTool {
                    tool_name,
                    permission_suggestions,
                    ..
                },
            ..
        }) = serde_json::from_value(line)
        else {
            panic!("a permission request");
        };
        assert_eq!(tool_name, ToolName::Write);
        assert_eq!(permission_suggestions.len(), 1);

        let line = json!({ "type": "system", "subtype": "hook_started", "session_id": "s" });
        assert!(matches!(
            serde_json::from_value(line),
            Ok(Message::System(System::Other))
        ));

        // As Claude Code 2.1.316's code writes it: a failed connection has no status, and a
        // cause it may name later is taken as unknown.
        let retry = |error_status: Value, error: &str| {
            let line = json!({
                "type": "system",
                "subtype": "api_retry",
                "attempt": 2,
                "max_retries": 10,
                "retry_delay_ms": 1200,
                "error_status": error_status,
                "error": error,
                "no_response": { "waited_ms": 30000, "retry_wait_ms": 1200 },
                "uuid": "u",
                "session_id": "s",
            });
            match serde_json::from_value(line) {
                Ok(Message::System(System::ApiRetry {
                    attempt: 2,
                    max_retries: 10,
                    error_status,
                    error,
                })) => (error_status, error),
                other => panic!("a retry, not {other:?}"),
            }
        };
        assert_eq!(retry(Value::Null, "unknown"), (None, RetryCause::Unknown));
        assert_eq!(
            retry(json!(529), "overloaded"),
            (Some(529), RetryCause::Overloaded)
        );
        assert_eq!(
            retry(json!(500), "a_cause_to_come"),
            (Some(500), RetryCause::Unknown)
        );

        // As Claude Code 2.1.316 wrote it, with a background agent and a command running.
        let line = json!({
            "type": "system",
            "subtype": "background_tasks_changed",
            "tasks": [
                { "task_id": "abd53f17ea727bd1f", "run_id": "0mvy7o1km-7d6f4be0", "task_type": "local_agent", "subagent_type": "general-purpose", "description": "Count to three" },
                { "task_id": "bd2gnyclv", "run_id": "0mvy7o1vc-5d0c7ae5", "task_type": "local_bash", "description": "Sleep three seconds" },
                { "task_id": "x", "task_type": "a_kind_to_come", "description": "Something new", "ambient": true },
            ],
            "uuid": "u",
            "session_id": "s",
        });
        let Ok(Message::System(System::BackgroundTasksChanged { tasks })) =
            serde_json::from_value(line)
        else {
            panic!("the background tasks");
        };
        assert_eq!(
            tasks,
            [
                BackgroundTask {
                    task_id: TaskId("abd53f17ea727bd1f".to_owned()),
                    task_type: TaskKind::LocalAgent,
                    subagent_type: Some("general-purpose".to_owned()),
                    description: "Count to three".to_owned(),
                    ambient: false,
                },
                BackgroundTask {
                    task_id: TaskId("bd2gnyclv".to_owned()),
                    task_type: TaskKind::LocalBash,
                    subagent_type: None,
                    description: "Sleep three seconds".to_owned(),
                    ambient: false,
                },
                BackgroundTask {
                    task_id: TaskId("x".to_owned()),
                    task_type: TaskKind::Other,
                    subagent_type: None,
                    description: "Something new".to_owned(),
                    ambient: true,
                },
            ]
        );
        let line = json!({ "type": "keep_alive" });
        assert!(matches!(serde_json::from_value(line), Ok(Message::Other)));
    }

    /// A task's start, change and end, in the shapes of Claude Code 2.1.316's SDK types; a
    /// foreground agent tells it isn't backgrounded.
    #[test]
    fn task_events_name_their_task() {
        let parse = |line: Value| serde_json::from_value::<Message>(line).ok();
        let started = parse(json!({
            "type": "system",
            "subtype": "task_started",
            "task_id": "a1",
            "tool_use_id": "toolu_1",
            "description": "Find the parser",
            "subagent_type": "Explore",
            "is_backgrounded": false,
            "task_type": "local_agent",
            "uuid": "u",
            "session_id": "s",
        }));
        let Some(Message::System(System::TaskStarted {
            task_id,
            task_type,
            subagent_type,
            is_backgrounded,
            ambient,
            ..
        })) = started
        else {
            panic!("a task's start");
        };
        assert_eq!(task_id, TaskId("a1".to_owned()));
        assert_eq!(task_type, Some(TaskKind::LocalAgent));
        assert_eq!(subagent_type.as_deref(), Some("Explore"));
        assert_eq!((is_backgrounded, ambient), (Some(false), false));

        let updated = parse(json!({
            "type": "system",
            "subtype": "task_updated",
            "task_id": "a1",
            "patch": { "is_backgrounded": true, "status": "running" },
            "uuid": "u",
            "session_id": "s",
        }));
        let Some(Message::System(System::TaskUpdated { patch, .. })) = updated else {
            panic!("a task's change");
        };
        assert_eq!(patch.is_backgrounded, Some(true));
        assert!(patch.status.is_some_and(|status| !status.ended()));

        let ended = parse(json!({
            "type": "system",
            "subtype": "task_notification",
            "task_id": "a1",
            "status": "completed",
            "output_file": "/tmp/x",
            "summary": "Found it",
            "uuid": "u",
            "session_id": "s",
        }));
        assert!(matches!(
            ended,
            Some(Message::System(System::TaskNotification { task_id })) if task_id == TaskId("a1".to_owned())
        ));
    }

    #[test]
    fn transcripts_say_where_the_worktree_is() {
        let line = json!({
            "type": "worktree-state",
            "worktreeSession": {
                "originalCwd": "/r",
                "worktreePath": "/r/.claude/worktrees/w",
                "worktreeName": "w",
                "worktreeBranch": "w",
            },
            "sessionId": "s",
        });
        let Ok(TranscriptLine::WorktreeState {
            worktree: Some(worktree),
        }) = serde_json::from_value(line)
        else {
            panic!("a worktree");
        };
        assert_eq!(worktree.name, "w");
        let line = json!({ "type": "worktree-state", "worktreeSession": null });
        assert!(matches!(
            serde_json::from_value(line),
            Ok(TranscriptLine::WorktreeState { worktree: None })
        ));
    }

    #[test]
    fn usage_reports_parse() {
        let answer = json!({
            "session": { "total_cost_usd": 0.5, "model_usage": {} },
            "rate_limits": {
                "five_hour": { "utilization": 2 },
                "limits": [{
                    "kind": "session",
                    "percent": 2,
                    "severity": "normal",
                    "resets_at": "2026-11-01T07:00:00.913778+00:00",
                    "scope": null,
                    "is_active": false,
                }],
            },
            "behaviors": {
                "day": {
                    "request_count": 3,
                    "session_count": 1,
                    "recommendations": [{ "key": "k", "pct": 83, "title": "Start fresh" }],
                    "agents": [],
                    "skills": [],
                },
            },
        });
        let usage: UsageReport = serde_json::from_value(answer).expect("a usage report");
        assert_eq!(usage.rate_limits.limits.len(), 1);
        assert!(
            usage
                .rate_limits
                .limits
                .first()
                .is_some_and(|limit| limit.resets_at.is_some())
        );
    }

    /// What the window writes, against the shape the CLI reads.
    #[test]
    fn input_is_written_as_the_cli_reads_it() {
        let encoded = |input: &Input<'_>| serde_json::to_value(input).expect("encodable");
        assert_eq!(
            encoded(&Input::prompt_with(&[], Vec::new(), "Hi")),
            json!({
                "type": "user",
                "message": { "role": "user", "content": "Hi" },
                "parent_tool_use_id": null,
                "session_id": "default",
            })
        );
        let id = RequestId::from("gui_1".to_owned());
        let request = ControlRequest::SetPermissionMode {
            mode: PermissionMode::AcceptEdits,
        };
        assert_eq!(
            encoded(&Input::ControlRequest {
                request_id: &id,
                request: &request,
            }),
            json!({
                "type": "control_request",
                "request_id": "gui_1",
                "request": { "subtype": "set_permission_mode", "mode": "acceptEdits" },
            })
        );
        let request = ControlRequest::GetContextUsage;
        assert_eq!(
            encoded(&Input::ControlRequest {
                request_id: &id,
                request: &request,
            }),
            json!({
                "type": "control_request",
                "request_id": "gui_1",
                "request": { "subtype": "get_context_usage" },
            })
        );
        let offered = json!({ "type": "addRules", "rules": [], "behavior": "allow" });
        let allow = PermissionResult::Allow {
            updated_input: ToolInput::AsAsked(json!({ "command": "ls" })),
            updated_permissions: vec![
                PermissionUpdate::SetMode {
                    mode: PermissionMode::Default,
                    destination: Destination::Session,
                },
                PermissionUpdate::Offered(offered.clone()),
            ],
        };
        assert_eq!(
            encoded(&Input::ControlResponse {
                response: Reply::Success {
                    request_id: &id,
                    response: &allow,
                },
            }),
            json!({
                "type": "control_response",
                "response": {
                    "subtype": "success",
                    "request_id": "gui_1",
                    "response": {
                        "behavior": "allow",
                        "updatedInput": { "command": "ls" },
                        "updatedPermissions": [
                            { "type": "setMode", "mode": "default", "destination": "session" },
                            offered,
                        ],
                    },
                },
            })
        );
        let deny = PermissionResult::Deny {
            message: "No".into(),
            interrupt: false,
        };
        assert_eq!(
            serde_json::to_value(&deny).expect("encodable"),
            json!({ "behavior": "deny", "message": "No" })
        );
    }

    #[test]
    fn only_uuids_are_session_ids() {
        let id = |text: &str| SessionId::try_from(text.to_owned()).is_ok();
        assert!(id("ee79c043-bfa7-44c0-82b6-509839a05c22"));
        assert!(!id("--dangerously-skip-permissions"));
        assert!(!id("../../../etc/passwd"));
        assert!(!id("ee79c043-bfa7-44c0-82b6-509839a05c2"));
        let record = json!({ "sessionId": "--settings={}" });
        assert!(serde_json::from_value::<HashMap<String, SessionId>>(record).is_err());
    }

    #[test]
    fn unknown_tools_keep_their_names() {
        assert_eq!(ToolName::from("LS".to_owned()), ToolName::Ls);
        let mcp = ToolName::from("mcp__github__create_issue".to_owned());
        assert_eq!(mcp.to_string(), "mcp__github__create_issue");
        assert_eq!(ToolName::Edit.to_string(), "Edit");
    }

    /// Images in a prompt as Anthropic's vision docs send them: `"type": "image"`, its
    /// `"source"` of `"type": "base64"`, `"media_type"` and `"data"`, then the text. A prompt
    /// without images stays a plain string.
    #[test]
    fn a_prompt_with_images_is_written_as_the_vision_docs_show() {
        let image = ImageBlock::base64(MediaType::Jpeg, "QUJD".into());
        assert_eq!(
            serde_json::to_value(Input::prompt_with(&[], vec![image], "Describe this image."))
                .expect("encodable"),
            json!({
                "type": "user",
                "message": { "role": "user", "content": [
                    { "type": "image", "source": {
                        "type": "base64", "media_type": "image/jpeg", "data": "QUJD" } },
                    { "type": "text", "text": "Describe this image." },
                ] },
                "parent_tool_use_id": null,
                "session_id": "default",
            })
        );
        let plain =
            serde_json::to_value(Input::prompt_with(&[], Vec::new(), "Hi")).expect("encodable");
        assert_eq!(plain.pointer("/message/content"), Some(&json!("Hi")));
    }

    /// A `!` command goes as the CLI reads one, each with an id of its own; what it printed
    /// goes ahead of the next prompt, as the rows the CLI replayed, then images, then the text.
    #[test]
    fn a_shell_command_and_its_rows_are_written_as_the_cli_reads_them() {
        let command = serde_json::to_value(Input::bash_command("ls -a")).expect("encodable");
        assert_eq!(command.pointer("/type"), Some(&json!("bash_command")));
        assert_eq!(command.pointer("/command"), Some(&json!("ls -a")));
        let uuid = |input: &Value| {
            input
                .pointer("/uuid")
                .and_then(Value::as_str)
                .and_then(|text| uuid::Uuid::parse_str(text).ok())
        };
        let again = serde_json::to_value(Input::bash_command("ls -a")).expect("encodable");
        assert!(uuid(&command).is_some());
        assert_ne!(uuid(&command), uuid(&again), "a repeat is run again");

        let shell = [
            "<bash-input>ls -a</bash-input>".to_owned(),
            "<bash-stdout>.\n..</bash-stdout><bash-stderr></bash-stderr>".to_owned(),
        ];
        let image = ImageBlock::base64(MediaType::Png, "QUJD".into());
        assert_eq!(
            serde_json::to_value(Input::prompt_with(&shell, vec![image], "Why two?"))
                .expect("encodable")
                .pointer("/message/content"),
            Some(&json!([
                { "type": "text", "text": "<bash-input>ls -a</bash-input>" },
                { "type": "text",
                  "text": "<bash-stdout>.\n..</bash-stdout><bash-stderr></bash-stderr>" },
                { "type": "image", "source": {
                    "type": "base64", "media_type": "image/png", "data": "QUJD" } },
                { "type": "text", "text": "Why two?" },
            ]))
        );
    }

    /// A plan and questions go back as Claude sent them, with the user's part in them.
    #[test]
    fn tool_input_goes_back_whole() {
        let asked = json!({
            "questions": [{
                "question": "Which?",
                "header": "Pick",
                "options": [{ "label": "A", "description": "First", "preview": "a" }],
                "multiSelect": false,
            }],
            "metadata": { "source": "x" },
            "answers": { "Which?": "Claude's own pick" },
            "annotations": { "Which?": { "notes": "Claude's own note" } },
        });
        let mut questions: Questions = serde_json::from_value(asked.clone()).expect("questions");
        questions.answers.insert("Which?".into(), "A".into());
        questions.annotations.insert(
            "Which?".into(),
            Annotation {
                notes: Some("but faster".into()),
                preview: Some("a".into()),
            },
        );
        // The answers and notes are the user's: what Claude filled in is replaced.
        let mut expected = asked;
        if let Some(fields) = expected.as_object_mut() {
            fields.insert("answers".into(), json!({ "Which?": "A" }));
            fields.insert(
                "annotations".into(),
                json!({ "Which?": { "notes": "but faster", "preview": "a" } }),
            );
        }
        assert_eq!(
            serde_json::to_value(ToolInput::Answered(questions)).expect("encodable"),
            expected
        );

        let mut plan: PlanInput =
            serde_json::from_value(json!({ "plan": "Do it", "allowedPrompts": [] }))
                .expect("a plan");
        plan.plan = "Do it carefully".into();
        assert_eq!(
            serde_json::to_value(ToolInput::Plan(plan)).expect("encodable"),
            json!({ "plan": "Do it carefully", "allowedPrompts": [] })
        );
    }
}
