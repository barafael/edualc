//! edualc's messaging tool as agy gets it: an MCP server on stdio (`edualc --agy-tool SOCKET`),
//! which agy starts inside its sandbox, with `send_message` and `list_sessions` as edualc's
//! relay serves them to sandboxed Claude sessions (see `relay`). It decides nothing: each call
//! goes to the adapter over the socket, which asks the user in the window before a message is
//! sent, and carries it out.

use std::io::{BufRead as _, BufReader, Write as _};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;

use serde_json::{Value, json};

/// What the tool asks the adapter, a line on the socket.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "call", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum Request {
    Send { to: String, message: String },
    List,
}

/// The adapter's answer, a line on the socket.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Answer {
    pub(crate) text: String,
    pub(crate) error: bool,
}

/// The tools, as `tools/list` lists them.
fn listing() -> Value {
    json!([
        {
            "name": "send_message",
            "description": "Send a message to another agent session on this computer (a Claude \
                Code session, say), by the name list_sessions gives it or the name a message came \
                from. The user is asked to allow each message. The answer, if any, arrives later \
                as a new message in this conversation.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "to": {"type": "string", "description": "The session's name."},
                    "message": {"type": "string"}
                },
                "required": ["to", "message"]
            }
        },
        {
            "name": "list_sessions",
            "description": "List the other agent sessions running on this computer, by the names \
                send_message takes.",
            "inputSchema": {"type": "object", "properties": {}}
        }
    ])
}

/// Runs the tool until agy closes its input.
pub(crate) fn run() -> std::process::ExitCode {
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
        let answer = match message.get("method").and_then(Value::as_str) {
            Some("initialize") => json!({"jsonrpc": "2.0", "id": id, "result": {
                "protocolVersion": message
                    .pointer("/params/protocolVersion")
                    .cloned()
                    .unwrap_or_else(|| json!("2025-06-18")),
                "capabilities": {"tools": {}},
                "serverInfo": {"name": "edualc", "version": env!("CARGO_PKG_VERSION")}
            }}),
            Some("tools/list") => {
                json!({"jsonrpc": "2.0", "id": id, "result": {"tools": listing()}})
            }
            Some("tools/call") => {
                let Answer { text, error } = called(&socket, message.get("params"));
                json!({"jsonrpc": "2.0", "id": id, "result": {
                    "content": [{"type": "text", "text": text}],
                    "isError": error
                }})
            }
            Some(_) | None => json!({"jsonrpc": "2.0", "id": id, "error": {
                "code": -32601, "message": "edualc's tool doesn't serve this"
            }}),
        };
        if writeln!(stdout, "{answer}")
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
                    text: crate::relay::Refusal::Unreadable.to_string(),
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
