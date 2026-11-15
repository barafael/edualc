//! What an agent says to edualc's messaging tool over the Model Context Protocol (JSON-RPC on
//! the tool's stdio), in the fields the tool reads.

use serde::Deserialize;
use serde_json::Value;

/// A line from the agent: a request has an id, a notification none.
#[derive(Debug, Deserialize)]
pub struct Incoming {
    #[serde(default)]
    pub id: Option<Value>,
    pub method: Method,
    /// Read by `method`: [`InitializeParams`] or [`CallParams`].
    #[serde(default)]
    pub params: Value,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
pub enum Method {
    #[serde(rename = "initialize")]
    Initialize,
    #[serde(rename = "tools/list")]
    ToolsList,
    #[serde(rename = "tools/call")]
    ToolsCall,
    #[serde(other)]
    Other,
}

/// `initialize`'s parameters.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InitializeParams {
    pub protocol_version: Option<String>,
}

/// `tools/call`'s parameters: the tool, and its arguments (see
/// [`crate::external::tool_input::SendMessageInput`]).
#[derive(Debug, Deserialize)]
pub struct CallParams {
    pub name: CalledTool,
    #[serde(default)]
    pub arguments: Value,
}

/// edualc's tools, by the names the tool lists them with.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CalledTool {
    SendMessage,
    ListSessions,
    #[serde(other)]
    Other,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A call, and a notification, which has no id.
    #[test]
    fn calls_read() {
        let call: Incoming = serde_json::from_str(
            r#"{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"send_message","arguments":{"to":"calm-heron","message":"hi"}}}"#,
        )
        .expect("a call");
        assert_eq!(call.method, Method::ToolsCall);
        assert_eq!(call.id, Some(Value::from(3)));
        let params: CallParams = serde_json::from_value(call.params).expect("its parameters");
        assert_eq!(params.name, CalledTool::SendMessage);
        let notice: Incoming =
            serde_json::from_str(r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#)
                .expect("a notification");
        assert_eq!((notice.id, notice.method), (None, Method::Other));
    }
}
