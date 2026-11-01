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
}

/// A tool call, which permission requests and results refer back to.
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash, Deserialize)]
#[serde(transparent)]
pub(crate) struct ToolUseId(String);

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
    Image,
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
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum PeekedKind {
    System,
    ControlRequest,
    ControlCancelRequest,
    ControlResponse,
    #[serde(other)]
    Other,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum PeekedSubtype {
    Init,
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
}

impl<'a> Input<'a> {
    pub(crate) fn prompt(text: &'a str) -> Self {
        Self::User {
            message: Prompt {
                role: Role::User,
                content: text,
            },
            parent_tool_use_id: None,
            session_id: "default",
        }
    }
}

#[derive(Debug, Serialize)]
pub(crate) struct Prompt<'a> {
    role: Role,
    content: &'a str,
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
        let line = json!({ "type": "keep_alive" });
        assert!(matches!(serde_json::from_value(line), Ok(Message::Other)));
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
            encoded(&Input::prompt("Hi")),
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
