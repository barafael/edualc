//! The Agent Client Protocol (agentclientprotocol.com) as `agy acp` speaks it (agy 1.5.6):
//! JSON-RPC 2.0, one message per line on stdio. Only what edualc's adapter (see `agy`) reads;
//! what it sends it writes itself. agy adds keys of its own (a tool call's `name` and its
//! arguments by agy's names), read where they say more than the protocol's.

use std::path::PathBuf;

use serde::Deserialize;
use serde_json::Value;

/// A line from the agent: its answer to a request of ours, a request of its own, or news.
#[derive(Debug, Deserialize)]
#[serde(untagged)]
pub(crate) enum Incoming {
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
pub(crate) enum AgentMethod {
    #[serde(rename = "session/update")]
    SessionUpdate,
    #[serde(rename = "session/request_permission")]
    RequestPermission,
    #[serde(untagged)]
    Other(String),
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct RpcError {
    #[serde(default)]
    pub(crate) message: String,
}

/// `session/update`'s parameters.
#[derive(Debug, Deserialize)]
pub(crate) struct Notified {
    pub(crate) update: Change,
}

/// What changed in a session.
#[derive(Debug, Deserialize)]
#[serde(tag = "sessionUpdate", rename_all = "snake_case")]
pub(crate) enum Change {
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
    #[serde(other)]
    Other,
}

/// A piece of a message.
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(crate) enum Chunk {
    Text {
        text: String,
    },
    #[serde(other)]
    Other,
}

/// A tool call, as first told or as it changed: an update carries only what changed.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ToolCall {
    pub(crate) tool_call_id: String,
    /// agy's own name for the tool (`run_command`, `edit_file`).
    #[serde(default)]
    pub(crate) name: Option<String>,
    #[serde(default)]
    pub(crate) title: Option<String>,
    #[serde(default)]
    pub(crate) kind: Option<ToolKind>,
    #[serde(default)]
    pub(crate) status: Option<ToolStatus>,
    #[serde(default)]
    pub(crate) raw_input: Option<Value>,
    #[serde(default)]
    pub(crate) raw_output: Option<Value>,
    #[serde(default)]
    pub(crate) content: Option<Vec<ToolContent>>,
    #[serde(default)]
    pub(crate) locations: Option<Vec<Location>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ToolKind {
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
pub(crate) enum ToolStatus {
    Pending,
    InProgress,
    Completed,
    Failed,
}

/// What a tool call produced.
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(crate) enum ToolContent {
    Content {
        content: Chunk,
    },
    /// A file changed; the window shows the change from the call's input.
    Diff {
        path: PathBuf,
    },
    #[serde(other)]
    Other,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct Location {
    pub(crate) path: PathBuf,
}

/// `session/request_permission`'s parameters.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct PermissionAsked {
    pub(crate) tool_call: ToolCall,
    pub(crate) options: Vec<PermissionOption>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct PermissionOption {
    pub(crate) option_id: String,
    pub(crate) kind: OptionKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum OptionKind {
    AllowOnce,
    AllowAlways,
    RejectOnce,
    RejectAlways,
}

/// `session/new`'s and `session/load`'s answer, as far as it is read.
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Opened {
    #[serde(default)]
    pub(crate) session_id: Option<String>,
    #[serde(default)]
    pub(crate) models: Option<Models>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Models {
    #[serde(default)]
    pub(crate) available_models: Vec<Model>,
    #[serde(default)]
    pub(crate) current_model_id: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Model {
    pub(crate) model_id: String,
    #[serde(default)]
    pub(crate) name: String,
}

/// `session/prompt`'s answer: why the turn ended, and what it used.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Prompted {
    pub(crate) stop_reason: StopReason,
    #[serde(default)]
    pub(crate) usage: Option<PromptUsage>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum StopReason {
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
pub(crate) struct PromptUsage {
    #[serde(default)]
    pub(crate) input_tokens: u64,
    #[serde(default)]
    pub(crate) output_tokens: u64,
    #[serde(default)]
    pub(crate) cached_read_tokens: u64,
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
            Some([ToolContent::Diff { .. }])
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
    }
}
