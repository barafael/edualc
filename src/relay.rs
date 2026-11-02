//! edualc's messaging tool for sandboxed sessions. A sandboxed Claude Code can't reach other
//! sessions' inboxes (see `sandbox`), as a session outside the sandbox could do for it what the
//! sandbox forbids. The daemon serves it a tool of its own instead, over the session's pipe, as a
//! host-served MCP server: `send_message`, which Claude Code asks the window about before every
//! call (`always_ask`), and `list_sessions`. A message the user allowed in the conversation is
//! posted into the recipient's inbox as Claude Code's own sessions post, headed with who sent it
//! and how to answer. Nothing else is delivered, whatever the session's permission mode: under
//! auto or dontAsk the window isn't asked, and the call is refused.
//!
//! The core is sans-io: [`Relay`] reads the lines both ways and says what to answer and what to
//! carry out; [`carry_out`] reads the session records and posts.

use std::collections::HashMap;
use std::ffi::OsStr;
use std::io::Write as _;
use std::os::fd::OwnedFd;
use std::os::unix::fs::{FileTypeExt as _, MetadataExt as _};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::Duration;

use rustix::fs::{Mode, OFlags};
use serde_json::{Value, json};
use strum::IntoEnumIterator as _;

use crate::external::claude_files::{SessionKey, SessionRecord};
use crate::external::mcp::{JsonRpc, Line, Permission, Request, SendArguments, ToolCall};
use crate::spawn::MessagingName;
use crate::{mirror, process, sessions};

/// The server's name, as `initialize` registers it and tools are named after it.
pub(crate) const SERVER: &str = "edualc";
/// Permission requests and allowed calls kept at once; a session asks one at a time.
const PENDING: usize = 64;
/// A message longer than this isn't posted.
const MESSAGE_CHARS: usize = 64 * 1024;
const RECORD_BYTES: u64 = 64 * 1024;
const KEY_BYTES: u64 = 4 * 1024;
/// How long posting into an inbox may take.
const POSTING: Duration = Duration::from_secs(2);

/// The tools the server offers.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, strum::EnumString, strum::IntoStaticStr, strum::EnumIter,
)]
#[strum(serialize_all = "snake_case")]
enum Tool {
    SendMessage,
    ListSessions,
}

impl Tool {
    /// Its name in the CLI's permission requests.
    fn qualified(self) -> String {
        format!("mcp__{SERVER}__{}", <&str>::from(self))
    }

    /// Its listing, as `tools/list` gives it.
    fn listing(self) -> Value {
        match self {
            Self::SendMessage => json!({
                "name": <&str>::from(self),
                "description": "Send a message to another Claude Code session, by the name \
                    list_sessions gives it or the name a message came from. The user is asked \
                    to allow each message. The answer, if any, arrives as a message.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "to": {"type": "string", "description": "The session's name."},
                        "message": {"type": "string"}
                    },
                    "required": ["to", "message"]
                }
            }),
            Self::ListSessions => json!({
                "name": <&str>::from(self),
                "description": "List the other Claude Code sessions running on this computer, \
                    by the names send_message takes.",
                "inputSchema": {"type": "object", "properties": {}}
            }),
        }
    }
}

/// The JSON-RPC methods the server answers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, strum::EnumString)]
enum Method {
    #[strum(serialize = "initialize")]
    Initialize,
    #[strum(serialize = "tools/list")]
    ToolsList,
    #[strum(serialize = "tools/call")]
    ToolsCall,
}

/// What `initialize` gains for a sandboxed session: the server, with `send_message` asked
/// about before each call.
pub(crate) fn registration() -> [(&'static str, Value); 2] {
    [
        ("sdkMcpServers", json!([SERVER])),
        (
            "sdkMcpServerConfigs",
            json!({ SERVER: { "tools": [{
                "name": <&str>::from(Tool::SendMessage),
                "permission_policy": "always_ask"
            }]}}),
        ),
    ]
}

/// What may be relayed for one sandboxed session (sans-io): the permission requests for sending
/// its window was asked, and the calls the user allowed.
#[derive(Debug)]
pub(crate) struct Relay {
    sender: MessagingName,
    /// By permission request id: the tool use it is for.
    asked: HashMap<String, String>,
    /// By tool use id: what the user allowed it to send.
    allowed: HashMap<String, SendArguments>,
}

/// What becomes of a line the process wrote.
#[derive(Debug, PartialEq)]
pub(crate) enum FromProcess {
    /// Not the relay's: on to the window, as it is.
    PassOn,
    /// Answered here: this line goes back to the process, and the window isn't shown it.
    Answer(String),
    /// To carry out (see [`carry_out`]), then answer with the outcome.
    Work(Work),
}

/// A tool call to carry out, and where its answer goes.
#[derive(Debug, PartialEq)]
pub(crate) struct Work {
    pub(crate) job: Job,
    pub(crate) reply: Reply,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Job {
    Send(SendArguments),
    List,
}

/// The control request and JSON-RPC call a result answers.
#[derive(Debug, PartialEq)]
pub(crate) struct Reply {
    request_id: String,
    call: Value,
}

impl Reply {
    /// The line that answers the call with `outcome`.
    pub(crate) fn answer(&self, outcome: &Outcome) -> String {
        let (text, error) = match outcome {
            Outcome::Sent(to) => (
                format!(
                    "Delivered to {to}. An answer, if it gives one, arrives as a message to this \
                     session."
                ),
                false,
            ),
            Outcome::Listed(names) if names.is_empty() => {
                ("No other session is running.".to_owned(), false)
            }
            Outcome::Listed(names) => (format!("Running sessions:\n{}", names.join("\n")), false),
            Outcome::Refused(refusal) => (refusal.to_string(), true),
        };
        tool_result(&self.request_id, &self.call, &text, error)
    }
}

/// What carrying out a call came to.
#[derive(Debug)]
pub(crate) enum Outcome {
    /// Posted into the inbox of the session named.
    Sent(String),
    /// The other running sessions, a line each.
    Listed(Vec<String>),
    Refused(Refusal),
}

/// Why a message isn't delivered: told to Claude as the call's error.
#[derive(Debug, thiserror::Error)]
pub(crate) enum Refusal {
    #[error(
        "edualc delivers a message only once the user has allowed it, asked in this \
         conversation; this one wasn't allowed, or nobody was asked (in auto or dontAsk mode \
         edualc isn't). Tell the user what you would send instead."
    )]
    NotAllowed,
    #[error("send_message takes `to` and `message`, both text.")]
    Unreadable,
    #[error("no running session is named {0:?}; list_sessions names those there are.")]
    Unknown(String),
    #[error("more than one running session answers to {0:?}.")]
    Ambiguous(String),
    #[error("{0:?} is this session itself.")]
    Itself(String),
    #[error("the message is longer than {MESSAGE_CHARS} characters.")]
    TooLong,
    #[error("{0:?}'s inbox isn't where and what a Claude Code session's is, so nothing is posted.")]
    Inbox(String),
    #[error("couldn't post into {name:?}'s inbox: {source}")]
    Post {
        name: String,
        #[source]
        source: std::io::Error,
    },
    #[error("edualc failed while carrying the message.")]
    Failed,
}

impl Relay {
    pub(crate) fn new(sender: MessagingName) -> Self {
        Self {
            sender,
            asked: HashMap::new(),
            allowed: HashMap::new(),
        }
    }

    pub(crate) fn sender(&self) -> &MessagingName {
        &self.sender
    }

    /// A line the process wrote: its requests to the server are answered here, and its
    /// permission requests for sending noted on their way to the window.
    pub(crate) fn process_said(&mut self, line: &str) -> FromProcess {
        // Most lines are Claude's text, read no further.
        if !line.contains("\"control_request\"") {
            return FromProcess::PassOn;
        }
        let Ok(Line::ControlRequest {
            request_id,
            request,
        }) = serde_json::from_str(line)
        else {
            return FromProcess::PassOn;
        };
        match request {
            Request::CanUseTool {
                tool_name,
                input,
                tool_use_id,
            } => {
                if tool_name == Tool::ListSessions.qualified() {
                    // Reading the names of running sessions changes nothing.
                    let allow = json!({"behavior": "allow", "updatedInput": input});
                    FromProcess::Answer(answer(&request_id, &allow))
                } else {
                    if tool_name == Tool::SendMessage.qualified()
                        && let Some(tool_use) = tool_use_id
                        && self.asked.len() < PENDING
                    {
                        self.asked.insert(request_id, tool_use);
                    }
                    FromProcess::PassOn
                }
            }
            Request::McpMessage {
                server_name,
                message,
            } if server_name == SERVER => self.served(&request_id, message),
            Request::McpMessage { .. } | Request::Other => FromProcess::PassOn,
        }
    }

    /// A line the window sends the process: its answer to a permission request for sending is
    /// noted, an allowance with what it allows.
    pub(crate) fn window_said(&mut self, line: &str) {
        if !line.contains("\"control_response\"") {
            return;
        }
        let Ok(Line::ControlResponse { response }) = serde_json::from_str(line) else {
            return;
        };
        let Some(tool_use) = response
            .request_id
            .and_then(|request| self.asked.remove(&request))
        else {
            return;
        };
        let permission = response
            .response
            .and_then(|answer| serde_json::from_value::<Permission>(answer).ok());
        if let Some(Permission::Allow {
            updated_input: Some(input),
        }) = permission
            && let Ok(arguments) = serde_json::from_value::<SendArguments>(input)
            && self.allowed.len() < PENDING
        {
            self.allowed.insert(tool_use, arguments);
        }
    }

    /// A JSON-RPC message for the server.
    fn served(&mut self, request_id: &str, message: JsonRpc) -> FromProcess {
        let Some(call) = message.id else {
            // A notification: acknowledged, and answered nothing in JSON-RPC.
            let acknowledged = json!({"jsonrpc": "2.0", "result": {}, "id": 0});
            return FromProcess::Answer(mcp_answer(request_id, &acknowledged));
        };
        let result = match message.method.as_deref().map(str::parse::<Method>) {
            Some(Ok(Method::Initialize)) => json!({
                "protocolVersion": message
                    .params
                    .as_ref()
                    .and_then(|params| params.get("protocolVersion"))
                    .cloned()
                    .unwrap_or(Value::Null),
                "capabilities": {"tools": {}},
                "serverInfo": {"name": SERVER, "version": env!("CARGO_PKG_VERSION")}
            }),
            Some(Ok(Method::ToolsList)) => {
                json!({"tools": Tool::iter().map(Tool::listing).collect::<Vec<_>>()})
            }
            Some(Ok(Method::ToolsCall)) => return self.called(request_id, call, message.params),
            Some(Err(_)) | None => {
                let unknown = json!({
                    "jsonrpc": "2.0", "id": call,
                    "error": {"code": -32601, "message": "edualc's server has no such method"}
                });
                return FromProcess::Answer(mcp_answer(request_id, &unknown));
            }
        };
        let answered = json!({"jsonrpc": "2.0", "id": call, "result": result});
        FromProcess::Answer(mcp_answer(request_id, &answered))
    }

    /// A tool call: a message goes out only as the user allowed it, for this very call.
    fn called(&mut self, request_id: &str, call: Value, params: Option<Value>) -> FromProcess {
        let reply = Reply {
            request_id: request_id.to_owned(),
            call,
        };
        let refused = |reply: Reply, refusal: Refusal| {
            FromProcess::Answer(reply.answer(&Outcome::Refused(refusal)))
        };
        let Some(called) =
            params.and_then(|params| serde_json::from_value::<ToolCall>(params).ok())
        else {
            return refused(reply, Refusal::Unreadable);
        };
        match called.name.parse::<Tool>() {
            Ok(Tool::ListSessions) => FromProcess::Work(Work {
                job: Job::List,
                reply,
            }),
            Ok(Tool::SendMessage) => {
                let Ok(asked) = serde_json::from_value::<SendArguments>(called.arguments) else {
                    return refused(reply, Refusal::Unreadable);
                };
                let allowed = called
                    .meta
                    .and_then(|meta| meta.tool_use_id)
                    .and_then(|tool_use| self.allowed.remove(&tool_use));
                if allowed.as_ref() == Some(&asked) {
                    FromProcess::Work(Work {
                        job: Job::Send(asked),
                        reply,
                    })
                } else {
                    refused(reply, Refusal::NotAllowed)
                }
            }
            Err(_) => refused(reply, Refusal::Unreadable),
        }
    }
}

/// A control response carrying `response`.
fn answer(request_id: &str, response: &Value) -> String {
    json!({
        "type": "control_response",
        "response": {"subtype": "success", "request_id": request_id, "response": response}
    })
    .to_string()
}

/// A control response carrying the server's JSON-RPC message.
fn mcp_answer(request_id: &str, message: &Value) -> String {
    answer(request_id, &json!({ "mcp_response": message }))
}

/// A tool call's result, as text.
fn tool_result(request_id: &str, call: &Value, text: &str, error: bool) -> String {
    let result = json!({"content": [{"type": "text", "text": text}], "isError": error});
    mcp_answer(
        request_id,
        &json!({"jsonrpc": "2.0", "id": call, "result": result}),
    )
}

/// Where Claude Code keeps the records of running sessions, and the folders their inboxes
/// may be in.
#[derive(Debug, Clone)]
pub(crate) struct Places {
    pub(crate) sessions: PathBuf,
    pub(crate) inboxes: Vec<PathBuf>,
}

impl Places {
    /// The user's: `~/.claude/sessions`, and `$XDG_RUNTIME_DIR/cc-socks` or the fallback in
    /// /tmp that Claude Code takes when that path is too long for a socket.
    pub(crate) fn of_user() -> Self {
        let uid = rustix::process::getuid().as_raw();
        let mut inboxes = vec![PathBuf::from(format!("/tmp/cc-socks-{uid}"))];
        if let Some(runtime) = std::env::var_os("XDG_RUNTIME_DIR") {
            inboxes.push(PathBuf::from(runtime).join("cc-socks"));
        }
        Self {
            sessions: sessions::config_dir().join("sessions"),
            inboxes,
        }
    }
}

/// Carries out a call for the session `sender`: lists the running sessions, or posts an allowed
/// message into one's inbox.
pub(crate) fn carry_out(places: &Places, sender: &MessagingName, job: &Job) -> Outcome {
    match job {
        Job::List => Outcome::Listed(
            running(places)
                .into_iter()
                .filter(|record| record.name.as_deref() != Some(sender.as_str()))
                .filter_map(|record| {
                    let name = record.name?;
                    Some(match record.status {
                        Some(status) => format!("{name} ({status})"),
                        None => name,
                    })
                })
                .collect(),
        ),
        Job::Send(arguments) => match send(places, sender, arguments) {
            Ok(to) => Outcome::Sent(to),
            Err(refusal) => Outcome::Refused(refusal),
        },
    }
}

/// Posts `arguments.message`, headed with who sent it, into the inbox of the session it names.
fn send(
    places: &Places,
    sender: &MessagingName,
    arguments: &SendArguments,
) -> Result<String, Refusal> {
    if arguments.message.chars().count() > MESSAGE_CHARS {
        return Err(Refusal::TooLong);
    }
    let to = arguments.to.trim();
    let address = to.strip_prefix("uds:").map(Path::new);
    let matching: Vec<SessionRecord> = running(places)
        .into_iter()
        .filter(|record| match address {
            Some(socket) => record.messaging_socket_path == socket,
            None => record.name.as_deref() == Some(to),
        })
        .collect();
    let record = match <[SessionRecord; 1]>::try_from(matching) {
        Ok([record]) => record,
        Err(matching) if matching.is_empty() => return Err(Refusal::Unknown(to.to_owned())),
        Err(_) => return Err(Refusal::Ambiguous(to.to_owned())),
    };
    let name = record.name.clone().unwrap_or_else(|| to.to_owned());
    if name == sender.as_str() {
        return Err(Refusal::Itself(name));
    }
    let Some(token) = inbox_ok(places, &record)
        .then(|| peer_token(places, &record))
        .flatten()
    else {
        return Err(Refusal::Inbox(name));
    };
    post(
        &record.messaging_socket_path,
        &token,
        &headed(sender, &arguments.message),
    )
    .map_err(|source| Refusal::Post {
        name: name.clone(),
        source,
    })?;
    Ok(name)
}

/// The message as the recipient reads it: who sent it and how to answer, never that the user
/// approved it, which a message from another session can't carry.
fn headed(sender: &MessagingName, message: &str) -> String {
    format!(
        "[A message from {sender}, a Claude Code session that edualc keeps in a sandbox. To \
         answer, use SendMessage with to: \"{sender}\".]\n\n{message}"
    )
}

/// The records of sessions whose process runs, as the record says it started.
fn running(places: &Places) -> Vec<SessionRecord> {
    let Some(folder) = open_folder(&places.sessions) else {
        return Vec::new();
    };
    let Ok(entries) = std::fs::read_dir(&places.sessions) else {
        return Vec::new();
    };
    entries
        .filter_map(Result::ok)
        .map(|entry| entry.file_name())
        .filter(|name| {
            name.to_str()
                .and_then(|name| name.strip_suffix(".json"))
                .is_some_and(|pid| pid.parse::<u32>().is_ok())
        })
        .filter_map(|name| mirror::read_regular(&folder, &name, RECORD_BYTES))
        .filter_map(|text| serde_json::from_str::<SessionRecord>(&text).ok())
        .filter(|record| {
            process::stat(record.pid).is_some_and(|stat| stat.started == record.proc_start)
        })
        .collect()
}

/// Whether the record's inbox is a socket of this user's, named for its process, in a folder
/// Claude Code keeps inboxes in.
fn inbox_ok(places: &Places, record: &SessionRecord) -> bool {
    let socket = &record.messaging_socket_path;
    let named = socket.file_name() == Some(OsStr::new(&format!("{}.sock", record.pid)));
    let placed = socket
        .parent()
        .is_some_and(|folder| places.inboxes.iter().any(|inbox| inbox == folder));
    let ours = std::fs::symlink_metadata(socket).is_ok_and(|metadata| {
        metadata.file_type().is_socket() && metadata.uid() == rustix::process::getuid().as_raw()
    });
    named && placed && ours
}

/// The token the record's session takes in its inbox, from its key file
/// (`<pid>.<64 hex>.key`), whose start matches the record's.
fn peer_token(places: &Places, record: &SessionRecord) -> Option<String> {
    let folder = open_folder(&places.sessions)?;
    let prefix = format!("{}.", record.pid);
    std::fs::read_dir(&places.sessions)
        .ok()?
        .filter_map(Result::ok)
        .map(|entry| entry.file_name())
        .filter(|name| {
            name.to_str().is_some_and(|name| {
                name.strip_prefix(&prefix)
                    .and_then(|rest| rest.strip_suffix(".key"))
                    .is_some_and(|hash| {
                        hash.len() == 64 && hash.bytes().all(|byte| byte.is_ascii_hexdigit())
                    })
            })
        })
        .filter_map(|name| mirror::read_regular(&folder, &name, KEY_BYTES))
        .filter_map(|text| serde_json::from_str::<SessionKey>(&text).ok())
        .find(|key| {
            key.proc_start == record.proc_start
                && !key.peer_token.is_empty()
                && key.peer_token.len() <= 256
                && key
                    .peer_token
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
        })
        .map(|key| key.peer_token)
}

fn open_folder(path: &Path) -> Option<OwnedFd> {
    rustix::fs::open(
        path,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .ok()
}

/// Posts one message into an inbox as Claude Code's own sessions do: the token, then the
/// message as a user line.
fn post(socket: &Path, token: &str, content: &str) -> std::io::Result<()> {
    let mut stream = UnixStream::connect(socket)?;
    stream.set_write_timeout(Some(POSTING))?;
    let auth = json!({"type": "auth", "token": token}).to_string();
    let line = json!({"type": "user", "message": {"role": "user", "content": content}}).to_string();
    stream.write_all(format!("{auth}\n{line}\n").as_bytes())?;
    stream.shutdown(std::net::Shutdown::Write)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead as _, BufReader};
    use std::os::unix::net::UnixListener;

    const SENDER_ID: &str = "11111111-2222-4333-8444-555555555555";
    const PEER_ID: &str = "99999999-2222-4333-8444-555555555555";

    fn name(id: &str) -> MessagingName {
        let id = crate::external::wire::SessionId::try_from(id.to_owned()).expect("an id");
        MessagingName::of(&id, Path::new("/work/project"))
    }

    fn asked(request_id: &str, tool: Tool, tool_use: &str, input: &Value) -> String {
        json!({"type": "control_request", "request_id": request_id, "request": {
            "subtype": "can_use_tool", "tool_name": tool.qualified(), "input": input,
            "tool_use_id": tool_use
        }})
        .to_string()
    }

    fn answered(request_id: &str, permission: &Value) -> String {
        json!({"type": "control_response", "response": {
            "subtype": "success", "request_id": request_id, "response": permission
        }})
        .to_string()
    }

    fn rpc(request_id: &str, message: &Value) -> String {
        json!({"type": "control_request", "request_id": request_id, "request": {
            "subtype": "mcp_message", "server_name": SERVER, "message": message
        }})
        .to_string()
    }

    fn call(request_id: &str, tool_use: &str, arguments: &Value) -> String {
        rpc(
            request_id,
            &json!({"jsonrpc": "2.0", "id": 7, "method": "tools/call", "params": {
                "name": "send_message", "arguments": arguments,
                "_meta": {"claudecode/toolUseId": tool_use}
            }}),
        )
    }

    /// The JSON-RPC message in an answer the relay gave.
    fn mcp_response(said: &FromProcess) -> Value {
        let FromProcess::Answer(line) = said else {
            panic!("not answered here: {said:?}");
        };
        let line: Value = serde_json::from_str(line).expect("JSON");
        line.pointer("/response/response/mcp_response")
            .cloned()
            .expect("an MCP response")
    }

    fn refused(said: &FromProcess) -> bool {
        mcp_response(said).pointer("/result/isError") == Some(&Value::Bool(true))
    }

    /// A message goes out only for the very call the user allowed, as they allowed it, once;
    /// one denied, never asked, changed since, or asked again is refused.
    #[test]
    fn a_message_goes_only_as_the_user_allowed_it() {
        let mut relay = Relay::new(name(SENDER_ID));
        let input = json!({"to": "peer-07", "message": "hello"});
        let allow = json!({"behavior": "allow", "updatedInput": input});
        let deny = json!({"behavior": "deny", "message": "no"});
        assert_eq!(
            relay.process_said(&asked("r1", Tool::SendMessage, "t1", &input)),
            FromProcess::PassOn,
            "the window is asked"
        );
        relay.window_said(&answered("r1", &allow));
        let sent = SendArguments {
            to: "peer-07".to_owned(),
            message: "hello".to_owned(),
        };
        match relay.process_said(&call("m1", "t1", &input)) {
            FromProcess::Work(work) => assert_eq!(work.job, Job::Send(sent)),
            other @ (FromProcess::PassOn | FromProcess::Answer(_)) => {
                panic!("not carried out: {other:?}");
            }
        }
        assert!(
            refused(&relay.process_said(&call("m2", "t1", &input))),
            "once"
        );
        assert!(
            refused(&relay.process_said(&call("m3", "t9", &input))),
            "never asked"
        );
        relay.process_said(&asked("r2", Tool::SendMessage, "t2", &input));
        relay.window_said(&answered("r2", &deny));
        assert!(
            refused(&relay.process_said(&call("m4", "t2", &input))),
            "denied"
        );
        relay.process_said(&asked("r3", Tool::SendMessage, "t3", &input));
        relay.window_said(&answered("r3", &allow));
        let changed = json!({"to": "peer-07", "message": "something else"});
        assert!(
            refused(&relay.process_said(&call("m5", "t3", &changed))),
            "changed"
        );
    }

    /// The server answers MCP's handshake and listing, and lists its two tools; listing sessions
    /// isn't asked about; everything else goes on to the window.
    #[test]
    fn the_server_answers_for_itself_and_passes_the_rest_on() {
        let mut relay = Relay::new(name(SENDER_ID));
        let initialize = json!({"jsonrpc": "2.0", "id": 1, "method": "initialize",
            "params": {"protocolVersion": "2025-06-18"}});
        let initialized = mcp_response(&relay.process_said(&rpc("a", &initialize)));
        assert_eq!(
            initialized.pointer("/result/protocolVersion"),
            Some(&json!("2025-06-18"))
        );
        let notified = json!({"jsonrpc": "2.0", "method": "notifications/initialized"});
        assert!(matches!(
            relay.process_said(&rpc("b", &notified)),
            FromProcess::Answer(_)
        ));
        let listing = json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list"});
        let tools = mcp_response(&relay.process_said(&rpc("c", &listing)));
        let names: Vec<&str> = tools
            .pointer("/result/tools")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|tool| tool.get("name").and_then(Value::as_str))
            .collect();
        assert_eq!(names, ["send_message", "list_sessions"]);
        let unknown = json!({"jsonrpc": "2.0", "id": 3, "method": "resources/list"});
        let error = mcp_response(&relay.process_said(&rpc("d", &unknown)));
        assert_eq!(error.pointer("/error/code"), Some(&json!(-32601)));
        assert!(matches!(
            relay.process_said(&asked("e", Tool::ListSessions, "t", &json!({}))),
            FromProcess::Answer(_)
        ));
        let bash = json!({"type": "control_request", "request_id": "f", "request": {
            "subtype": "can_use_tool", "tool_name": "Bash", "input": {"command": "ls"}}});
        assert_eq!(relay.process_said(&bash.to_string()), FromProcess::PassOn);
        let other_server = json!({"type": "control_request", "request_id": "g", "request": {
            "subtype": "mcp_message", "server_name": "other", "message": listing}});
        assert_eq!(
            relay.process_said(&other_server.to_string()),
            FromProcess::PassOn
        );
        let text = r#"{"type":"assistant","message":{"content":"control_request"}}"#;
        assert_eq!(relay.process_said(text), FromProcess::PassOn);
    }

    /// A running session's record, key and inbox, as Claude Code leaves them, in a scratch
    /// folder: this test's own process stands in for the session.
    struct Peer {
        scratch: PathBuf,
        places: Places,
        inbox: UnixListener,
        token: String,
    }

    impl Peer {
        fn new(test: &str, peer: &MessagingName) -> Self {
            let scratch =
                std::env::temp_dir().join(format!("edualc-relay-{test}-{}", std::process::id()));
            let sessions = scratch.join("sessions");
            let inboxes = scratch.join("cc-socks");
            std::fs::create_dir_all(&sessions).expect("a sessions folder");
            std::fs::create_dir_all(&inboxes).expect("an inbox folder");
            let pid = std::process::id();
            let started = process::stat(pid).expect("this process").started;
            let socket = inboxes.join(format!("{pid}.sock"));
            let inbox = UnixListener::bind(&socket).expect("an inbox");
            let record = json!({"pid": pid, "sessionId": PEER_ID, "cwd": "/work/project",
                "procStart": started, "messagingSocketPath": socket, "name": peer.as_str(),
                "status": "idle"});
            std::fs::write(sessions.join(format!("{pid}.json")), record.to_string())
                .expect("a record");
            let token = "0123456789abcdef0123456789abcdef".to_owned();
            let key = json!({"peerToken": token, "procStart": started});
            std::fs::write(
                sessions.join(format!("{pid}.{}.key", "a".repeat(64))),
                key.to_string(),
            )
            .expect("a key");
            Self {
                scratch,
                places: Places {
                    sessions,
                    inboxes: vec![inboxes],
                },
                inbox,
                token,
            }
        }

        /// The two lines the inbox was given.
        fn received(&self) -> (Value, Value) {
            let (connection, _) = self.inbox.accept().expect("a connection");
            let mut lines = BufReader::new(connection).lines();
            let mut next = || -> Value {
                serde_json::from_str(&lines.next().expect("a line").expect("read")).expect("JSON")
            };
            (next(), next())
        }

        fn done(self) {
            std::fs::remove_dir_all(&self.scratch).expect("the scratch folder goes");
        }
    }

    /// An allowed message reaches the inbox of the session named, after its token, headed with
    /// who sent it and how to answer.
    #[test]
    fn a_message_reaches_the_inbox_of_the_session_named() {
        let (sender, peer_name) = (name(SENDER_ID), name(PEER_ID));
        let peer = Peer::new("send", &peer_name);
        let job = Job::Send(SendArguments {
            to: peer_name.to_string(),
            message: "hello".to_owned(),
        });
        let outcome = carry_out(&peer.places, &sender, &job);
        assert!(
            matches!(&outcome, Outcome::Sent(to) if to == peer_name.as_str()),
            "{outcome:?}"
        );
        let (auth, message) = peer.received();
        assert_eq!(auth, json!({"type": "auth", "token": peer.token}));
        let content = message
            .pointer("/message/content")
            .and_then(Value::as_str)
            .expect("a text");
        assert!(content.starts_with(&format!("[A message from {sender},")));
        assert!(content.contains(&format!("to: \"{sender}\"")));
        assert!(content.ends_with("\n\nhello"));
        assert!(
            !content.contains("approved"),
            "no claim of the user's approval"
        );
        let listed = carry_out(&peer.places, &sender, &Job::List);
        assert!(
            matches!(&listed, Outcome::Listed(names) if *names == [format!("{peer_name} (idle)")]),
            "{listed:?}"
        );
        peer.done();
    }

    /// Nothing is posted to a session not running under that name, to the sender itself, or into
    /// an inbox that isn't where and what a Claude Code session's is.
    #[test]
    fn nothing_is_posted_where_it_shouldnt_be() {
        let peer_name = name(PEER_ID);
        let peer = Peer::new("refuse", &peer_name);
        let send = |places: &Places, sender: &MessagingName, to: &str| {
            let job = Job::Send(SendArguments {
                to: to.to_owned(),
                message: "hello".to_owned(),
            });
            carry_out(places, sender, &job)
        };
        let sender = name(SENDER_ID);
        assert!(matches!(
            send(&peer.places, &sender, "nobody-01"),
            Outcome::Refused(Refusal::Unknown(_))
        ));
        assert!(matches!(
            send(&peer.places, &peer_name, peer_name.as_str()),
            Outcome::Refused(Refusal::Itself(_))
        ));
        let elsewhere = Places {
            sessions: peer.places.sessions.clone(),
            inboxes: vec![peer.scratch.join("elsewhere")],
        };
        assert!(matches!(
            send(&elsewhere, &sender, peer_name.as_str()),
            Outcome::Refused(Refusal::Inbox(_))
        ));
        let long = Job::Send(SendArguments {
            to: peer_name.to_string(),
            message: "x".repeat(MESSAGE_CHARS + 1),
        });
        assert!(matches!(
            carry_out(&peer.places, &sender, &long),
            Outcome::Refused(Refusal::TooLong)
        ));
        peer.done();
    }
}
