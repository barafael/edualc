//! What edualc's messaging tool (see `relay`) reads of the lines a sandboxed session and its
//! window exchange: the CLI's requests to the tool's server (`mcp_message`, with MCP's JSON-RPC
//! inside) and its permission requests, and the window's answers. As Claude Code 2.1.316 and its
//! Agent SDK's types (sdk.d.ts) have them; keys not read are taken, as each release adds some.

use serde::Deserialize;
use serde_json::Value;

use crate::app::Mode;

/// A line either side writes, in the fields the relay reads.
#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Line {
    ControlRequest {
        request_id: String,
        request: Request,
    },
    ControlResponse {
        response: Response,
    },
    /// The session's own reports: at the start of each turn (`init`), and as it changes
    /// (`status`), they say its permission mode.
    System {
        #[serde(default, rename = "permissionMode")]
        permission_mode: Option<Mode>,
    },
    #[serde(other)]
    Other,
}

/// What the CLI asks its host.
#[derive(Debug, Deserialize)]
#[serde(tag = "subtype", rename_all = "snake_case")]
pub enum Request {
    /// A JSON-RPC message for a server the host serves.
    McpMessage {
        server_name: String,
        message: JsonRpc,
    },
    /// Whether a tool call may run.
    CanUseTool {
        tool_name: String,
        #[serde(default)]
        input: Value,
        #[serde(default)]
        tool_use_id: Option<String>,
    },
    #[serde(other)]
    Other,
}

/// An answer to one of the CLI's control requests: for a permission request, `response` is a
/// [`Permission`].
#[derive(Debug, Deserialize)]
pub struct Response {
    #[serde(default)]
    pub request_id: Option<String>,
    #[serde(default)]
    pub response: Option<Value>,
}

/// The answer to a permission request.
#[derive(Debug, Deserialize)]
#[serde(tag = "behavior", rename_all = "snake_case")]
pub enum Permission {
    #[serde(rename_all = "camelCase")]
    Allow {
        #[serde(default)]
        updated_input: Option<Value>,
    },
    Deny {},
    #[serde(other)]
    Other,
}

/// A JSON-RPC 2.0 message: a request (with an id), or a notification (without).
#[derive(Debug, Deserialize)]
pub struct JsonRpc {
    #[serde(default)]
    pub id: Option<Value>,
    #[serde(default)]
    pub method: Option<String>,
    #[serde(default)]
    pub params: Option<Value>,
}

/// The parameters of a `tools/call`.
#[derive(Debug, Deserialize)]
pub struct ToolCall {
    pub name: String,
    #[serde(default)]
    pub arguments: Value,
    #[serde(default, rename = "_meta")]
    pub meta: Option<CallMeta>,
}

/// What the CLI adds to a tool call: the tool use it runs, which its permission request named.
#[derive(Debug, Deserialize)]
pub struct CallMeta {
    #[serde(default, rename = "claudecode/toolUseId")]
    pub tool_use_id: Option<String>,
}

/// What Claude gives `send_message`.
#[derive(Debug, PartialEq, Eq, Deserialize)]
pub struct SendArguments {
    pub to: String,
    pub message: String,
}

/// A line of a session's turn, in the tool calls Claude makes in it.
#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Turn {
    Assistant {
        message: AssistantMessage,
    },
    #[serde(other)]
    Other,
}

#[derive(Debug, Deserialize)]
pub struct AssistantMessage {
    #[serde(default)]
    pub content: Vec<Block>,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Block {
    ToolUse {
        name: String,
        #[serde(default)]
        input: Value,
    },
    #[serde(other)]
    Other,
}
