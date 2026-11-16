//! The Agent Client Protocol (agentclientprotocol.com) as edualc's ACP adapter reads it:
//! JSON-RPC 2.0, one message per line on stdio. Only what the adapter reads and writes: the
//! messages it reads are [`Incoming`], the ones it writes are [`ToAgent`]. Agents add keys of
//! their own (a tool call's `name` and its arguments by the agent's names), read where they say
//! more than the protocol's.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::external::wire::MediaType;

/// A line from the agent: its answer to a request of ours, a request of its own, or news.
#[derive(Debug, Deserialize)]
#[serde(untagged)]
pub enum Incoming {
    Request {
        id: Value,
        method: AgentMethod,
        #[serde(default)]
        params: Value,
    },
    Notification {
        method: AgentMethod,
        #[serde(default)]
        params: Value,
    },
    Response {
        id: Value,
        #[serde(default)]
        result: Option<Value>,
        #[serde(default)]
        error: Option<RpcError>,
    },
}

/// What the agent asks or tells the client.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub enum AgentMethod {
    #[serde(rename = "session/update")]
    SessionUpdate,
    #[serde(rename = "session/request_permission")]
    RequestPermission,
    #[serde(untagged)]
    Other(String),
}

#[derive(Debug, Clone, Deserialize)]
pub struct RpcError {
    #[serde(default)]
    pub code: i64,
    #[serde(default)]
    pub message: String,
}

impl RpcError {
    /// ACP's error for a request the agent can't serve before the client authenticates.
    const AUTH_REQUIRED: i64 = -32000;

    /// Whether the agent refused for want of a valid sign-in, as ACP's code says.
    pub fn sign_in_refused(&self) -> bool {
        self.code == Self::AUTH_REQUIRED
    }
}

/// `session/update`'s parameters.
#[derive(Debug, Deserialize)]
pub struct Notified {
    pub update: Change,
}

/// What changed in a session.
#[derive(Debug, Deserialize)]
#[serde(tag = "sessionUpdate", rename_all = "snake_case")]
pub enum Change {
    AgentMessageChunk {
        content: Chunk,
    },
    AgentThoughtChunk {
        content: Chunk,
    },
    /// The user's prompt, as the agent took it; a session loaded replays them.
    UserMessageChunk,
    ToolCall(ToolCall),
    ToolCallUpdate(ToolCall),
    SessionInfoUpdate {
        #[serde(default)]
        title: Option<String>,
    },
    CurrentModeUpdate {
        #[serde(rename = "currentModeId")]
        current_mode_id: String,
    },
    #[serde(other)]
    Other,
}

/// A piece of a message.
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Chunk {
    Text {
        text: String,
    },
    #[serde(other)]
    Other,
}

/// A tool call, as first told or as it changed: an update carries only what changed.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolCall {
    pub tool_call_id: String,
    /// agy's own name for the tool (`run_command`, `edit_file`).
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub kind: Option<ToolKind>,
    #[serde(default)]
    pub status: Option<ToolStatus>,
    #[serde(default)]
    pub raw_input: Option<Value>,
    #[serde(default)]
    pub raw_output: Option<Value>,
    #[serde(default)]
    pub content: Option<Vec<ToolContent>>,
    #[serde(default)]
    pub locations: Option<Vec<Location>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolKind {
    Read,
    Edit,
    Delete,
    Move,
    Search,
    Execute,
    Think,
    Fetch,
    #[serde(other)]
    Other,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolStatus {
    Pending,
    InProgress,
    Completed,
    Failed,
}

/// What a tool call produced.
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ToolContent {
    Content {
        content: Chunk,
    },
    /// A file changed: a new file has no old text.
    #[serde(rename_all = "camelCase")]
    Diff {
        path: PathBuf,
        #[serde(default)]
        old_text: Option<String>,
        new_text: String,
    },
    #[serde(other)]
    Other,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Location {
    pub path: PathBuf,
}

/// A tool call's `rawOutput` when it is what a command printed, as agy gives it; other tools'
/// outputs have shapes of their own.
#[derive(Debug, Deserialize)]
pub struct PrintedOutput {
    pub output: String,
}

/// A tool call's arguments (`rawInput`) as agy 1.5.6 names them, each as far as agy has given
/// it.
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct AgyArguments {
    pub path: Option<PathBuf>,
    pub command_line: Option<String>,
    #[serde(rename = "toolSummary")]
    pub tool_summary: Option<String>,
    pub content: Option<String>,
    pub append: Option<bool>,
    pub target: Option<String>,
    pub replacement: Option<String>,
    /// edualc's messaging tool's, by its own names.
    #[serde(rename = "to")]
    pub to: Option<String>,
    #[serde(rename = "message")]
    pub message: Option<String>,
}

/// A tool call's arguments (`rawInput`) as opencode names them, each as far as opencode has
/// given it.
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OpencodeArguments {
    pub path: Option<PathBuf>,
    pub command: Option<String>,
    pub content: Option<String>,
    pub old_string: Option<String>,
    pub new_string: Option<String>,
}

/// `session/request_permission`'s parameters.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PermissionAsked {
    pub tool_call: ToolCall,
    pub options: Vec<PermissionOption>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PermissionOption {
    pub option_id: String,
    pub kind: OptionKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OptionKind {
    AllowOnce,
    AllowAlways,
    RejectOnce,
    RejectAlways,
}

/// `session/new`'s and `session/load`'s answer, as far as it is read.
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Opened {
    #[serde(default)]
    pub session_id: Option<String>,
    #[serde(default)]
    pub models: Option<Models>,
    #[serde(default)]
    pub modes: Option<SessionModes>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Models {
    #[serde(default)]
    pub available_models: Vec<Model>,
    #[serde(default)]
    pub current_model_id: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Model {
    pub model_id: String,
    #[serde(default)]
    pub name: String,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionModes {
    #[serde(default)]
    pub available_modes: Vec<SessionMode>,
    #[serde(default)]
    pub current_mode_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionMode {
    pub id: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
}

/// `session/prompt`'s answer: why the turn ended, and what it used.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Prompted {
    pub stop_reason: StopReason,
    #[serde(default)]
    pub usage: Option<PromptUsage>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StopReason {
    EndTurn,
    MaxTokens,
    MaxTurnRequests,
    Refusal,
    Cancelled,
    #[serde(other)]
    Other,
}

#[derive(Debug, Clone, Copy, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PromptUsage {
    #[serde(default)]
    pub input_tokens: u64,
    #[serde(default)]
    pub output_tokens: u64,
    #[serde(default)]
    pub cached_read_tokens: u64,
}

/// The `jsonrpc` field of every message.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub enum JsonRpc {
    #[serde(rename = "2.0")]
    TwoPointZero,
}

/// A request edualc makes of the agent, by its method and parameters.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "method", content = "params")]
pub enum ClientCall {
    #[serde(rename = "initialize")]
    Initialize(InitializeParams),
    #[serde(rename = "session/new")]
    NewSession(NewSessionParams),
    #[serde(rename = "session/load")]
    LoadSession(LoadSessionParams),
    #[serde(rename = "session/set_model")]
    SetModel(SetModelParams),
    #[serde(rename = "session/set_mode")]
    SetMode(SetModeParams),
    #[serde(rename = "session/prompt")]
    Prompt(PromptParams),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct InitializeParams {
    pub protocol_version: u32,
    pub client_capabilities: ClientCapabilities,
    pub client_info: ClientInfo,
}

/// What edualc lets the agent ask of it: none of the file and terminal requests.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ClientCapabilities {
    pub fs: FileCapabilities,
    pub terminal: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FileCapabilities {
    pub read_text_file: bool,
    pub write_text_file: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ClientInfo {
    pub name: String,
    pub version: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct NewSessionParams {
    pub cwd: PathBuf,
    pub mcp_servers: Vec<McpServer>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LoadSessionParams {
    pub session_id: String,
    pub cwd: PathBuf,
    pub mcp_servers: Vec<McpServer>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SetModelParams {
    pub session_id: String,
    pub model_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SetModeParams {
    pub session_id: String,
    pub mode_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PromptParams {
    pub session_id: String,
    pub prompt: Vec<PromptBlock>,
}

/// An MCP server the agent starts for the session: a program with its arguments.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct McpServer {
    pub name: String,
    pub command: PathBuf,
    pub args: Vec<PathBuf>,
    pub env: Vec<EnvVariable>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct EnvVariable {
    pub name: String,
    pub value: String,
}

/// A part of a prompt the agent is given.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(
    tag = "type",
    rename_all = "snake_case",
    rename_all_fields = "camelCase"
)]
pub enum PromptBlock {
    Text { text: String },
    Image { mime_type: MediaType, data: String },
}

/// A message to the agent that asks for no answer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "method", content = "params")]
pub enum ClientNotice {
    #[serde(rename = "session/cancel")]
    Cancel(CancelParams),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CancelParams {
    pub session_id: String,
}

/// The answer to a permission request: the option the user chose, or none.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(
    tag = "outcome",
    rename_all = "snake_case",
    rename_all_fields = "camelCase"
)]
pub enum Outcome {
    Selected { option_id: String },
    Cancelled,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PermissionAnswer {
    pub outcome: Outcome,
}

/// The `result` or the `error` of an answer to a request of the agent's.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Reply<T> {
    Result(T),
    Error(RpcErrorBody),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RpcErrorBody {
    pub code: i64,
    pub message: String,
}

/// A request edualc makes; the agent answers with the same `id`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Request {
    pub jsonrpc: JsonRpc,
    pub id: u64,
    #[serde(flatten)]
    pub call: ClientCall,
}

/// A message edualc sends that asks for no answer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Notice {
    pub jsonrpc: JsonRpc,
    #[serde(flatten)]
    pub notice: ClientNotice,
}

/// An answer to a request of the agent's, under the `id` it came with.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Answer {
    pub jsonrpc: JsonRpc,
    pub id: Value,
    #[serde(flatten)]
    pub reply: Reply<PermissionAnswer>,
}

/// What edualc writes to the agent, one line each.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToAgent {
    Request(Request),
    Notice(Notice),
    Answer(Answer),
}

impl ToAgent {
    /// The message as one line of JSON, without its line break. Fails only when a path in it
    /// isn't UTF-8.
    pub fn to_line(&self) -> Result<String, serde_json::Error> {
        match self {
            Self::Request(request) => serde_json::to_string(request),
            Self::Notice(notice) => serde_json::to_string(notice),
            Self::Answer(answer) => serde_json::to_string(answer),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Lines as agy 1.5.6 wrote them (paths shortened), read as what they are.
    #[test]
    fn agys_lines_read() {
        let update = r#"{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"s","update":{"sessionUpdate":"tool_call_update","toolCallId":"call_1","name":"edit_file","args":{},"status":"completed","kind":"edit","title":"File notes.txt","rawInput":{"Path":"/w/notes.txt","Replacement":"b","Target":"a"},"rawOutput":{},"content":[{"type":"diff","path":"/w/notes.txt","newText":"b\n","oldText":"a\n"}],"locations":[{"path":"/w/notes.txt"}]}}}"#;
        let Ok(Incoming::Notification {
            method: AgentMethod::SessionUpdate,
            params,
        }) = serde_json::from_str(update)
        else {
            panic!("a session update");
        };
        let notified: Notified = serde_json::from_value(params).expect("an update");
        let Change::ToolCallUpdate(call) = notified.update else {
            panic!("a tool call's update");
        };
        assert_eq!(call.name.as_deref(), Some("edit_file"));
        assert_eq!(call.status, Some(ToolStatus::Completed));
        assert!(matches!(
            call.content.as_deref(),
            Some([ToolContent::Diff { old_text, new_text, .. }])
                if old_text.as_deref() == Some("a\n") && new_text == "b\n"
        ));

        let asked = r#"{"jsonrpc":"2.0","method":"session/request_permission","params":{"sessionId":"s","toolCall":{"sessionUpdate":"tool_call","toolCallId":"call_2","name":"run_command","status":"pending","kind":"execute","title":"wc -l notes.txt","rawInput":{"CommandLine":"wc -l notes.txt"}},"options":[{"optionId":"allow","name":"Allow","kind":"allow_once"},{"optionId":"deny","name":"Deny","kind":"reject_once"}]},"id":"1"}"#;
        let Ok(Incoming::Request {
            method: AgentMethod::RequestPermission,
            params,
            ..
        }) = serde_json::from_str(asked)
        else {
            panic!("a permission request");
        };
        let asked: PermissionAsked = serde_json::from_value(params).expect("its parameters");
        assert_eq!(asked.options.len(), 2);

        let answered = r#"{"jsonrpc":"2.0","result":{"stopReason":"end_turn","usage":{"inputTokens":18742,"outputTokens":885,"cachedReadTokens":0}},"id":5}"#;
        let Ok(Incoming::Response {
            result: Some(result),
            ..
        }) = serde_json::from_str(answered)
        else {
            panic!("an answer");
        };
        let prompted: Prompted = serde_json::from_value(result).expect("a prompt's answer");
        assert_eq!(prompted.stop_reason, StopReason::EndTurn);

        let opened_json = r#"{"sessionId":"s","modes":{"availableModes":[{"id":"default","name":"Default","description":"Ask"},{"id":"accept-edits","name":"Accept edits"}],"currentModeId":"default"}}"#;
        let opened: Opened = serde_json::from_str(opened_json).expect("opened session with modes");
        let modes = opened.modes.expect("modes present");
        assert_eq!(modes.available_modes.len(), 2);
        assert_eq!(
            modes.available_modes.get(1).map(|mode| mode.id.as_str()),
            Some("accept-edits")
        );
        assert_eq!(modes.current_mode_id.as_deref(), Some("default"));

        let mode_update = r#"{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"s","update":{"sessionUpdate":"current_mode_update","currentModeId":"accept-edits"}}}"#;
        let Ok(Incoming::Notification {
            method: AgentMethod::SessionUpdate,
            params,
        }) = serde_json::from_str(mode_update)
        else {
            panic!("a session update");
        };
        let notified: Notified = serde_json::from_value(params).expect("an update");
        let Change::CurrentModeUpdate { current_mode_id } = notified.update else {
            panic!("a current mode update");
        };
        assert_eq!(current_mode_id, "accept-edits");
    }
}
