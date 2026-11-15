//! edualc's messaging tool as the agent gets it: an MCP server on stdio (`edualc --agy-tool
//! SOCKET`), which the agent starts, with `send_message` and `list_sessions`. It decides
//! nothing: each call goes to the adapter over the socket, which carries it out (see
//! [`super::outbox`]).

use std::collections::BTreeMap;
use std::io::{BufRead as _, BufReader, Write as _};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;

use serde::Serialize;
use serde_json::Value;

use crate::external::acp::{JsonRpc, Reply, RpcErrorBody};

/// The protocol version the tool answers with when the agent names none.
const PROTOCOL_VERSION: &str = "2025-06-18";
/// JSON-RPC's code for a method the receiver doesn't have.
const METHOD_NOT_FOUND: i64 = -32601;

/// What the tool asks the adapter, a line on the socket.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "call", rename_all = "snake_case", deny_unknown_fields)]
pub enum Request {
    Send { to: String, message: String },
    List,
}

/// The adapter's answer, a line on the socket.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Answer {
    pub text: String,
    pub error: bool,
}

/// The tools this server offers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
enum ToolName {
    SendMessage,
    ListSessions,
}

/// The arguments the tools take.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
enum Argument {
    To,
    Message,
}

/// The JSON Schema of a tool's arguments, or of one argument.
#[derive(Debug, Serialize)]
#[serde(tag = "type", rename_all = "lowercase")]
enum Schema {
    Object {
        properties: BTreeMap<Argument, Schema>,
        #[serde(skip_serializing_if = "Vec::is_empty")]
        required: Vec<Argument>,
    },
    /// A string; `type: string` in the schema.
    #[serde(rename = "string")]
    Text {
        #[serde(skip_serializing_if = "Option::is_none")]
        description: Option<String>,
    },
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ToolListing {
    name: ToolName,
    description: String,
    input_schema: Schema,
}

/// `tools/list`'s answer.
#[derive(Debug, Serialize)]
struct Listing {
    tools: Vec<ToolListing>,
}

/// `initialize`'s answer.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct Initialized {
    protocol_version: String,
    capabilities: Capabilities,
    server_info: ServerInfo,
}

#[derive(Debug, Serialize)]
struct Capabilities {
    tools: ToolsCapability,
}

/// The server offers tools, and nothing more is set for them.
#[derive(Debug, Serialize)]
struct ToolsCapability {}

#[derive(Debug, Serialize)]
struct ServerInfo {
    name: String,
    version: String,
}

/// `tools/call`'s answer.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct CallResult {
    content: Vec<Content>,
    is_error: bool,
}

#[derive(Debug, Serialize)]
#[serde(tag = "type", rename_all = "lowercase")]
enum Content {
    Text { text: String },
}

/// A line the tool writes back: the answer to a request, under the id it came with.
#[derive(Debug, Serialize)]
struct Response<T> {
    jsonrpc: JsonRpc,
    id: Value,
    #[serde(flatten)]
    reply: Reply<T>,
}

fn response_line<T: Serialize>(id: Value, reply: Reply<T>) -> Result<String, serde_json::Error> {
    serde_json::to_string(&Response {
        jsonrpc: JsonRpc::TwoPointZero,
        id,
        reply,
    })
}

/// The answer to a method the tool doesn't have.
fn unserved(id: Value) -> Result<String, serde_json::Error> {
    response_line::<()>(
        id,
        Reply::Error(RpcErrorBody {
            code: METHOD_NOT_FOUND,
            message: "edualc's tool doesn't serve this".to_owned(),
        }),
    )
}

/// The tools, as `tools/list` lists them.
fn listing() -> Listing {
    Listing {
        tools: vec![
            ToolListing {
                name: ToolName::SendMessage,
                description: "Send a message to another agent session on this computer (a Claude \
                    Code session, say), by the name list_sessions gives it or the name a message came \
                    from. The answer, if any, arrives later as a new message in this conversation."
                    .to_owned(),
                input_schema: Schema::Object {
                    properties: BTreeMap::from([
                        (
                            Argument::To,
                            Schema::Text {
                                description: Some("The session's name.".to_owned()),
                            },
                        ),
                        (Argument::Message, Schema::Text { description: None }),
                    ]),
                    required: vec![Argument::To, Argument::Message],
                },
            },
            ToolListing {
                name: ToolName::ListSessions,
                description: "List the other agent sessions running on this computer, by the names \
                    send_message takes."
                    .to_owned(),
                input_schema: Schema::Object {
                    properties: BTreeMap::new(),
                    required: Vec::new(),
                },
            },
        ],
    }
}

/// Runs the tool until the agent closes its input.
pub fn run() -> std::process::ExitCode {
    let Some(socket) = std::env::args_os().nth(2).map(PathBuf::from) else {
        eprintln!("edualc: the messaging tool needs the adapter's socket");
        return std::process::ExitCode::from(2);
    };
    let mut stdout = std::io::stdout().lock();
    for line in std::io::stdin().lock().lines() {
        let Ok(line) = line else {
            break;
        };
        let Ok(message) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        // Notifications want no answer.
        let Some(id) = message.get("id").cloned() else {
            continue;
        };
        let line = match message.get("method").and_then(Value::as_str) {
            Some("initialize") => response_line(
                id,
                Reply::Result(Initialized {
                    protocol_version: message
                        .pointer("/params/protocolVersion")
                        .and_then(Value::as_str)
                        .unwrap_or(PROTOCOL_VERSION)
                        .to_owned(),
                    capabilities: Capabilities {
                        tools: ToolsCapability {},
                    },
                    server_info: ServerInfo {
                        name: "edualc".to_owned(),
                        version: env!("CARGO_PKG_VERSION").to_owned(),
                    },
                }),
            ),
            Some("tools/list") => response_line(id, Reply::Result(listing())),
            Some("tools/call") => {
                let Answer { text, error } = called(&socket, message.get("params"));
                response_line(
                    id,
                    Reply::Result(CallResult {
                        content: vec![Content::Text { text }],
                        is_error: error,
                    }),
                )
            }
            Some(_) | None => unserved(id),
        };
        let line = match line {
            Ok(line) => line,
            Err(error) => {
                tracing::warn!(%error, "an answer the tool can't write");
                continue;
            }
        };
        if writeln!(stdout, "{line}")
            .and_then(|()| stdout.flush())
            .is_err()
        {
            break;
        }
    }
    std::process::ExitCode::SUCCESS
}

/// A call, as the adapter answers it.
fn called(socket: &std::path::Path, params: Option<&Value>) -> Answer {
    let name = params
        .and_then(|params| params.get("name"))
        .and_then(Value::as_str);
    let arguments = params.and_then(|params| params.get("arguments"));
    let text = |field: &str| {
        arguments
            .and_then(|arguments| arguments.get(field))
            .and_then(Value::as_str)
            .map(str::to_owned)
    };
    let request = match name {
        Some("send_message") => match (text("to"), text("message")) {
            (Some(to), Some(message)) => Request::Send { to, message },
            (None | Some(_), None | Some(_)) => {
                return Answer {
                    text: super::outbox::Refusal::Unreadable.to_string(),
                    error: true,
                };
            }
        },
        Some("list_sessions") => Request::List,
        Some(_) | None => {
            return Answer {
                text: "edualc has no such tool.".to_owned(),
                error: true,
            };
        }
    };
    asked(socket, &request).unwrap_or_else(|error| Answer {
        text: format!("edualc couldn't be reached: {error}"),
        error: true,
    })
}

fn asked(socket: &std::path::Path, request: &Request) -> std::io::Result<Answer> {
    let mut stream = UnixStream::connect(socket)?;
    let line = serde_json::to_string(request)?;
    writeln!(stream, "{line}")?;
    let mut answer = String::new();
    BufReader::new(stream).read_line(&mut answer)?;
    Ok(serde_json::from_str(&answer)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requests_and_answers_read_back() {
        let request = Request::Send {
            to: "calm-heron".to_owned(),
            message: "hi".to_owned(),
        };
        let line = serde_json::to_string(&request).expect("a line");
        assert_eq!(line, r#"{"call":"send","to":"calm-heron","message":"hi"}"#);
        assert_eq!(
            serde_json::from_str::<Request>(r#"{"call":"list"}"#).expect("a list"),
            Request::List
        );
        assert!(
            serde_json::from_str::<Request>(r#"{"call":"send","to":"a","message":"b","from":"c"}"#)
                .is_err()
        );
    }
}
