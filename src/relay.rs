//! edualc's messaging tool for sandboxed sessions. A sandboxed Claude Code can't reach other
//! sessions' inboxes (see `sandbox`), as a session outside the sandbox could do for it what the
//! sandbox forbids. The daemon serves it a tool of its own instead, over the session's pipe, as a
//! host-served MCP server: `send_message`, which Claude Code asks the window about before every
//! call (`always_ask`), and `list_sessions`. A message the user allowed in the conversation is
//! posted into the recipient's inbox as Claude Code's own sessions post one: in its envelope,
//! which names the session and the inbox to answer at. Nothing else is delivered, whatever the
//! session's permission mode: under auto or dontAsk the window isn't asked, and the call is
//! refused.
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
use std::time::{Duration, Instant};

use rustix::fs::{Mode, OFlags};
use serde_json::{Value, json};
use strum::IntoEnumIterator as _;

use crate::external::claude_files::{SessionKey, SessionRecord};
use crate::external::mcp::{
    Block, JsonRpc, Line, Permission, Request, SendArguments, ToolCall, Turn,
};
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
    consent: Consent,
    /// By permission request id: the tool use it is for.
    asked: HashMap<String, String>,
    /// By tool use id: what the user allowed it to send.
    allowed: HashMap<String, SendArguments>,
}

/// Whether the user is asked about each message a sandboxed session sends.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Consent {
    EachMessage,
    /// The user's setting: its messages go without asking, the window shown only the call.
    NotAsked,
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
    /// A message the user wasn't asked about yet: whether it answers a session that wrote to
    /// this one (see [`Replies`]) decides, through [`Relay::consulted`].
    Consult(Consult),
}

/// A message to decide on.
#[derive(Debug, PartialEq)]
pub(crate) enum Consult {
    /// The CLI asks whether it may go: the window is asked unless it is a reply.
    Permission {
        request_id: String,
        tool_use: String,
        arguments: SendArguments,
    },
    /// The call itself, nobody asked (auto mode): it goes only as a reply.
    Call {
        reply: Reply,
        arguments: SendArguments,
    },
}

impl Consult {
    /// The session it is for.
    pub(crate) fn to(&self) -> &str {
        match self {
            Self::Permission { arguments, .. } | Self::Call { arguments, .. } => &arguments.to,
        }
    }
}

/// How long a session outside the sandbox that wrote to a sandboxed one may be answered by it
/// without the user being asked.
const REPLY_WINDOW: Duration = Duration::from_secs(10 * 60);
/// Messages kept for answering at once.
const WROTE: usize = 256;

/// The messages sessions outside the sandbox sent, as the daemon hosting them saw them call
/// SendMessage: a sandboxed session they wrote to may answer each once, within minutes, without
/// the user being asked (sans-io; one task of the daemon's holds it). Only a session the daemon
/// hosts counts: what reaches a sandboxed session from elsewhere goes past edualc, and its own
/// record of it is the sandbox's, which could claim anything.
#[derive(Debug, Default)]
pub(crate) struct Replies {
    wrote: Vec<Wrote>,
}

#[derive(Debug)]
struct Wrote {
    from: MessagingName,
    to: String,
    at: Instant,
}

impl Replies {
    /// Session `from`, outside the sandbox, sent a message to `to`.
    pub(crate) fn wrote(&mut self, from: MessagingName, to: &str, at: Instant) {
        self.wrote
            .retain(|wrote| at.saturating_duration_since(wrote.at) < REPLY_WINDOW);
        if self.wrote.len() < WROTE {
            self.wrote.push(Wrote {
                from,
                to: bare(to).to_owned(),
                at,
            });
        }
    }

    /// Whether `sender` may answer `to` without the user being asked: `to` wrote to it in the
    /// last minutes, and wasn't answered so since. Spends it.
    pub(crate) fn may_answer(&mut self, sender: &MessagingName, to: &str, at: Instant) -> bool {
        let to = bare(to);
        let found = self.wrote.iter().position(|wrote| {
            wrote.from.as_str() == to
                && wrote.to == sender.as_str()
                && at.saturating_duration_since(wrote.at) < REPLY_WINDOW
        });
        found.map(|found| self.wrote.remove(found)).is_some()
    }
}

/// A session's name as ListAgents lists it, without the `[ref]` after it.
fn bare(name: &str) -> &str {
    name.split_once(" [").map_or(name, |(bare, _)| bare).trim()
}

/// The name of the running session `to` addresses, so that who wrote to whom is kept by name
/// whichever way they addressed each other: a name as it is, an inbox's address (as an
/// envelope's `from` gives it, see [`envelope`]) as the name in the record of the session
/// whose inbox it is, or as it is where no running session's is.
pub(crate) fn named(places: &Places, to: &str) -> String {
    let to = bare(to);
    let Some(socket) = addressed(to) else {
        return to.to_owned();
    };
    running(places)
        .into_iter()
        .find(|record| record.messaging_socket_path == socket && inbox_ok(places, record))
        .and_then(|record| record.name)
        .unwrap_or_else(|| to.to_owned())
}

/// The inbox `to` addresses, if it is an address, as Claude Code takes one: `uds:` and the
/// socket's path, percent-encoded, or the path itself.
fn addressed(to: &str) -> Option<PathBuf> {
    match to.strip_prefix("uds:") {
        Some(encoded) => Some(PathBuf::from(percent_decoded(encoded))),
        None => to.starts_with('/').then(|| PathBuf::from(to)),
    }
}

/// `text` with its `%XX` escapes decoded, or as it is where they don't decode to UTF-8 text
/// (as JavaScript's `decodeURIComponent` falls back in Claude Code).
fn percent_decoded(text: &str) -> String {
    let mut bytes = Vec::with_capacity(text.len());
    let mut rest = text.as_bytes();
    while let Some((&byte, after)) = rest.split_first() {
        if byte == b'%' {
            let Some((decoded, after)) = after.split_at_checked(2).and_then(|(hex, after)| {
                let hex = std::str::from_utf8(hex).ok()?;
                Some((u8::from_str_radix(hex, 16).ok()?, after))
            }) else {
                return text.to_owned();
            };
            bytes.push(decoded);
            rest = after;
        } else {
            bytes.push(byte);
            rest = after;
        }
    }
    String::from_utf8(bytes).unwrap_or_else(|_| text.to_owned())
}

/// The address of the inbox at `socket`, as Claude Code writes one (2.1.316): `uds:` and the
/// path, any character but letters, digits and `:_/.\-` percent-encoded. None for a path that
/// isn't text, or too long for Claude Code to take as an address.
fn address(socket: &Path) -> Option<String> {
    let encoded: String = socket
        .to_str()?
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || ":_/.\\-".contains(character) {
                character.to_string()
            } else {
                character
                    .to_string()
                    .bytes()
                    .map(|byte| format!("%{byte:02X}"))
                    .collect()
            }
        })
        .collect();
    (encoded.len() <= ADDRESS_CHARS).then(|| format!("uds:{encoded}"))
}

/// The longest path, encoded, that Claude Code takes as an address.
const ADDRESS_CHARS: usize = 200;
/// The longest name Claude Code takes in an envelope as it is; it shortens a longer one.
const NAME_CHARS: usize = 64;
/// The tag of Claude Code's envelope for a message from another session.
const ENVELOPE: &str = "cross-session-message";

/// The message as Claude Code's own sessions post one (2.1.316), for the recipient to show who
/// sent it and answer it: the envelope, with the sender's inbox (`from`) and name, then the
/// text. Claude Code reads an envelope back only in exactly the form it writes one, attributes
/// in this order and the text escaped as [`escaped`] does; anything else reaches the session as
/// plain text. It says nothing of the user's approval, which a message can't carry.
fn envelope(from: Option<&str>, name: &MessagingName, text: &str) -> String {
    let from = from.map(|from| format!(" from=\"{from}\""));
    let name =
        (name.as_str().chars().count() <= NAME_CHARS).then(|| format!(" from-name=\"{name}\""));
    format!(
        "<{ENVELOPE}{}{}>\n{}\n</{ENVELOPE}>",
        from.unwrap_or_default(),
        name.unwrap_or_default(),
        escaped(text)
    )
}

/// The text as Claude Code puts it in an envelope: with a `\` after the `<` of anything that
/// would close the envelope, so that it ends only where it ends. Claude Code also takes closing
/// tags spelled with look-alike letters as such; a text with one of those doesn't read back as
/// an envelope, and reaches the session as plain text.
fn escaped(text: &str) -> String {
    let mut escaped = String::with_capacity(text.len());
    for (index, character) in text.char_indices() {
        escaped.push(character);
        if character == '<' && text.get(index + 1..).is_some_and(closes_envelope) {
            escaped.push('\\');
        }
    }
    escaped
}

/// Whether what follows a `<` closes an envelope, as Claude Code tells: no `\` right after it;
/// then anything but word characters, brackets and slashes; a slash; anything but word
/// characters and brackets; the tag, in any case; and no word character after it.
fn closes_envelope(after: &str) -> bool {
    let word = |character: char| character.is_ascii_alphanumeric() || "_-".contains(character);
    let bracket = |character: char| "<>".contains(character);
    let slash = |character: char| "/\u{ff0f}\u{2215}\u{2044}".contains(character);
    if after.starts_with('\\') {
        return false;
    }
    let after = after.trim_start_matches(|character| {
        !(word(character) || bracket(character) || slash(character))
    });
    let Some(after) = after.strip_prefix(slash) else {
        return false;
    };
    let mut after = after
        .trim_start_matches(|character| !(word(character) || bracket(character)))
        .chars();
    let spelled = ENVELOPE.chars().all(|expected| {
        after.next().is_some_and(|character| match expected {
            '-' => "-_".contains(character),
            letter => character.eq_ignore_ascii_case(&letter),
        })
    });
    spelled && !after.next().is_some_and(word)
}

/// The sessions a line from a session outside the sandbox sends messages to: its SendMessage
/// calls, as Claude makes them.
pub(crate) fn sent_to(line: &str) -> Vec<String> {
    if !line.contains("\"SendMessage\"") {
        return Vec::new();
    }
    let Ok(Turn::Assistant { message }) = serde_json::from_str(line) else {
        return Vec::new();
    };
    message
        .content
        .into_iter()
        .filter_map(|block| match block {
            Block::ToolUse { name, input } if name == "SendMessage" => {
                input.get("to").and_then(Value::as_str).map(str::to_owned)
            }
            Block::ToolUse { .. } | Block::Other => None,
        })
        .collect()
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
    pub(crate) fn new(sender: MessagingName, consent: Consent) -> Self {
        Self {
            sender,
            consent,
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
                // Reading the names of running sessions changes nothing; a message goes unasked
                // where the user set it so.
                let unasked = tool_name == Tool::ListSessions.qualified()
                    || (tool_name == Tool::SendMessage.qualified()
                        && self.consent == Consent::NotAsked);
                if unasked {
                    let allow = json!({"behavior": "allow", "updatedInput": input});
                    FromProcess::Answer(answer(&request_id, &allow))
                } else if tool_name == Tool::SendMessage.qualified()
                    && let Some(tool_use) = tool_use_id
                    && let Ok(arguments) = serde_json::from_value::<SendArguments>(input)
                {
                    FromProcess::Consult(Consult::Permission {
                        request_id,
                        tool_use,
                        arguments,
                    })
                } else {
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

    /// A message decided on: one that answers a session that wrote to this one goes without the
    /// user being asked (`may_answer`, see [`Replies`]); any other is asked about in the window,
    /// or refused where nobody may be asked.
    pub(crate) fn consulted(&mut self, consult: Consult, may_answer: bool) -> FromProcess {
        match (consult, may_answer) {
            (
                Consult::Permission {
                    tool_use,
                    arguments,
                    request_id,
                },
                true,
            ) => {
                let allow = json!({"behavior": "allow", "updatedInput": {
                    "to": arguments.to, "message": arguments.message
                }});
                if self.allowed.len() < PENDING {
                    self.allowed.insert(tool_use, arguments);
                }
                FromProcess::Answer(answer(&request_id, &allow))
            }
            (
                Consult::Permission {
                    request_id,
                    tool_use,
                    ..
                },
                false,
            ) => {
                if self.asked.len() < PENDING {
                    self.asked.insert(request_id, tool_use);
                }
                FromProcess::PassOn
            }
            (Consult::Call { reply, arguments }, true) => FromProcess::Work(Work {
                job: Job::Send(arguments),
                reply,
            }),
            (Consult::Call { reply, .. }, false) => {
                FromProcess::Answer(reply.answer(&Outcome::Refused(Refusal::NotAllowed)))
            }
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
                if allowed.as_ref() == Some(&asked) || self.consent == Consent::NotAsked {
                    FromProcess::Work(Work {
                        job: Job::Send(asked),
                        reply,
                    })
                } else {
                    FromProcess::Consult(Consult::Call {
                        reply,
                        arguments: asked,
                    })
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

/// Posts `arguments.message`, in the envelope that names its sender (see [`envelope`]), into
/// the inbox of the session it names.
fn send(
    places: &Places,
    sender: &MessagingName,
    arguments: &SendArguments,
) -> Result<String, Refusal> {
    if arguments.message.chars().count() > MESSAGE_CHARS {
        return Err(Refusal::TooLong);
    }
    let to = arguments.to.trim();
    let socket = addressed(to);
    let records = running(places);
    let from = own_inbox(places, &records, sender).and_then(|socket| address(&socket));
    let matching: Vec<SessionRecord> = records
        .into_iter()
        .filter(|record| match &socket {
            Some(socket) => record.messaging_socket_path == *socket,
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
        &envelope(from.as_deref(), sender, &arguments.message),
    )
    .map_err(|source| Refusal::Post {
        name: name.clone(),
        source,
    })?;
    Ok(name)
}

/// The inbox of the sandboxed session `sender`, as its record mirrored among the running ones
/// gives it (see `mirror`), for the sessions it writes to to answer at; none where no single
/// such record says.
fn own_inbox(
    places: &Places,
    records: &[SessionRecord],
    sender: &MessagingName,
) -> Option<PathBuf> {
    let mut own = records
        .iter()
        .filter(|record| record.name.as_deref() == Some(sender.as_str()));
    let (Some(record), None) = (own.next(), own.next()) else {
        return None;
    };
    inbox_ok(places, record).then(|| record.messaging_socket_path.clone())
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

    /// The window knows the permission requests for send_message by the name the CLI gives
    /// them, and asks about each message so.
    #[test]
    fn the_window_knows_send_message_by_its_name() {
        use crate::external::wire::ToolName;
        assert_eq!(
            ToolName::from(Tool::SendMessage.qualified()),
            ToolName::RelaySendMessage
        );
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

    /// A line the process wrote, decided on as the daemon does, `may_answer` what the keeper of
    /// who wrote to whom says.
    fn decided(relay: &mut Relay, line: &str, may_answer: bool) -> FromProcess {
        match relay.process_said(line) {
            FromProcess::Consult(consult) => relay.consulted(consult, may_answer),
            other @ (FromProcess::PassOn | FromProcess::Answer(_) | FromProcess::Work(_)) => other,
        }
    }

    /// A message goes out only for the very call the user allowed, as they allowed it, once;
    /// one denied, never asked, changed since, or asked again is refused.
    #[test]
    fn a_message_goes_only_as_the_user_allowed_it() {
        let mut relay = Relay::new(name(SENDER_ID), Consent::EachMessage);
        let input = json!({"to": "peer-07", "message": "hello"});
        let allow = json!({"behavior": "allow", "updatedInput": input});
        let deny = json!({"behavior": "deny", "message": "no"});
        let mut said = |line: &str| decided(&mut relay, line, false);
        assert_eq!(
            said(&asked("r1", Tool::SendMessage, "t1", &input)),
            FromProcess::PassOn,
            "the window is asked"
        );
        relay.window_said(&answered("r1", &allow));
        let sent = SendArguments {
            to: "peer-07".to_owned(),
            message: "hello".to_owned(),
        };
        match decided(&mut relay, &call("m1", "t1", &input), false) {
            FromProcess::Work(work) => assert_eq!(work.job, Job::Send(sent)),
            other @ (FromProcess::PassOn | FromProcess::Answer(_) | FromProcess::Consult(_)) => {
                panic!("not carried out: {other:?}");
            }
        }
        let mut said = |line: &str| decided(&mut relay, line, false);
        assert!(refused(&said(&call("m2", "t1", &input))), "once");
        assert!(refused(&said(&call("m3", "t9", &input))), "never asked");
        said(&asked("r2", Tool::SendMessage, "t2", &input));
        relay.window_said(&answered("r2", &deny));
        assert!(
            refused(&decided(&mut relay, &call("m4", "t2", &input), false)),
            "denied"
        );
        decided(
            &mut relay,
            &asked("r3", Tool::SendMessage, "t3", &input),
            false,
        );
        relay.window_said(&answered("r3", &allow));
        let changed = json!({"to": "peer-07", "message": "something else"});
        assert!(
            refused(&decided(&mut relay, &call("m5", "t3", &changed), false)),
            "changed"
        );
    }

    /// A session that wrote to a sandboxed one may be answered by it once, within minutes, and
    /// only by the session it wrote to.
    #[test]
    fn a_session_that_wrote_may_be_answered_once_within_minutes() {
        let (sender, peer, other) = (
            name(SENDER_ID),
            name(PEER_ID),
            name("33333333-2222-4333-8444-555555555555"),
        );
        let now = Instant::now();
        let mut replies = Replies::default();
        replies.wrote(peer.clone(), &format!("{sender} [3ea497]"), now);
        let later = now + Duration::from_secs(60);
        assert!(
            !replies.may_answer(&other, peer.as_str(), later),
            "not to it"
        );
        assert!(replies.may_answer(&sender, &format!("{peer} [f639dc]"), later));
        assert!(!replies.may_answer(&sender, peer.as_str(), later), "once");
        replies.wrote(peer.clone(), sender.as_str(), now);
        let too_late = now + REPLY_WINDOW;
        assert!(
            !replies.may_answer(&sender, peer.as_str(), too_late),
            "minutes"
        );
    }

    /// A reply needs no question in the window, nor anyone asked at all (auto mode); a message
    /// that answers nobody is asked about, or refused where nobody is asked.
    #[test]
    fn a_reply_goes_without_a_question() {
        let mut relay = Relay::new(name(SENDER_ID), Consent::EachMessage);
        let input = json!({"to": "peer-07", "message": "thanks"});
        let allowed = decided(
            &mut relay,
            &asked("r1", Tool::SendMessage, "t1", &input),
            true,
        );
        assert!(matches!(&allowed, FromProcess::Answer(line) if line.contains("\"allow\"")));
        assert!(matches!(
            decided(&mut relay, &call("m1", "t1", &input), false),
            FromProcess::Work(_)
        ));
        assert!(matches!(
            decided(&mut relay, &call("m2", "t2", &input), true),
            FromProcess::Work(_)
        ));
        assert!(refused(&decided(
            &mut relay,
            &call("m3", "t3", &input),
            false
        )));
    }

    /// What a session outside the sandbox sends: its SendMessage calls' recipients, nothing of
    /// its other tools or lines.
    #[test]
    fn the_messages_a_session_sends_are_read_from_its_calls() {
        let turn = json!({"type": "assistant", "message": {"content": [
            {"type": "text", "text": "I'll ask."},
            {"type": "tool_use", "name": "SendMessage", "input": {"to": "peer-07", "message": "hi"}},
            {"type": "tool_use", "name": "Bash", "input": {"command": "echo SendMessage"}}
        ]}});
        assert_eq!(sent_to(&turn.to_string()), ["peer-07"]);
        let result = json!({"type": "result", "result": "SendMessage"});
        assert!(sent_to(&result.to_string()).is_empty());
    }

    /// Where the user set messages to go unasked, the window isn't asked, and a call goes even
    /// where nobody was (auto mode).
    #[test]
    fn unasked_messages_go_without_the_window() {
        let mut relay = Relay::new(name(SENDER_ID), Consent::NotAsked);
        let input = json!({"to": "peer-07", "message": "hello"});
        let allowed = relay.process_said(&asked("r1", Tool::SendMessage, "t1", &input));
        let FromProcess::Answer(line) = allowed else {
            panic!("the window was asked: {allowed:?}");
        };
        assert!(line.contains("\"behavior\":\"allow\""));
        for (request, tool_use) in [("m1", "t1"), ("m2", "never-asked")] {
            assert!(
                matches!(
                    relay.process_said(&call(request, tool_use, &input)),
                    FromProcess::Work(_)
                ),
                "{tool_use}"
            );
        }
    }

    /// The server answers MCP's handshake and listing, and lists its two tools; listing sessions
    /// isn't asked about; everything else goes on to the window.
    #[test]
    fn the_server_answers_for_itself_and_passes_the_rest_on() {
        let mut relay = Relay::new(name(SENDER_ID), Consent::EachMessage);
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

        /// The record and inbox of a second running session, `sender`, beside the first: the
        /// test's parent process stands in for it. Its inbox, kept open while it is needed.
        fn beside(&self, sender: &MessagingName) -> (PathBuf, UnixListener) {
            let pid = std::os::unix::process::parent_id();
            let started = process::stat(pid).expect("the parent process").started;
            let socket = self.places.inboxes.first().expect("an inbox folder");
            let socket = socket.join(format!("{pid}.sock"));
            let inbox = UnixListener::bind(&socket).expect("an inbox");
            let record = json!({"pid": pid, "sessionId": SENDER_ID, "cwd": "/work/project",
                "procStart": started, "messagingSocketPath": socket, "name": sender.as_str()});
            std::fs::write(
                self.places.sessions.join(format!("{pid}.json")),
                record.to_string(),
            )
            .expect("a record");
            (socket, inbox)
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

    /// An allowed message reaches the inbox of the session named, after its token, in Claude
    /// Code's envelope: the sender's inbox to answer at, its name, the text.
    #[test]
    fn a_message_reaches_the_inbox_of_the_session_named() {
        let (sender, peer_name) = (name(SENDER_ID), name(PEER_ID));
        let peer = Peer::new("send", &peer_name);
        let (own_socket, _own_inbox) = peer.beside(&sender);
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
        let own_address = address(&own_socket).expect("an address");
        assert_eq!(
            message.pointer("/message/content").and_then(Value::as_str),
            Some(
                format!(
                    "<cross-session-message from=\"{own_address}\" from-name=\"{sender}\">\
                     \nhello\n</cross-session-message>"
                )
                .as_str()
            )
        );
        let listed = carry_out(&peer.places, &sender, &Job::List);
        assert!(
            matches!(&listed, Outcome::Listed(names) if *names == [format!("{peer_name} (idle)")]),
            "{listed:?}"
        );
        assert_eq!(named(&peer.places, &own_address), sender.as_str());
        peer.done();
    }

    /// An inbox's address, as an envelope gives it or as its path, is read as the name of the
    /// session whose inbox it is; a name is taken as it is, and an address no running session's
    /// stays as it is.
    #[test]
    fn an_address_is_read_as_its_sessions_name() {
        let peer_name = name(PEER_ID);
        let peer = Peer::new("named", &peer_name);
        let socket = peer.places.inboxes.first().expect("an inbox folder");
        let socket = socket.join(format!("{}.sock", std::process::id()));
        let path = socket.to_str().expect("a text path");
        for to in [address(&socket).expect("an address"), path.to_owned()] {
            assert_eq!(named(&peer.places, &to), peer_name.as_str(), "{to}");
        }
        assert_eq!(named(&peer.places, "peer-07 [3ea497]"), "peer-07");
        assert_eq!(
            named(&peer.places, "uds:/elsewhere/1.sock"),
            "uds:/elsewhere/1.sock"
        );
        peer.done();
    }

    /// The envelope is Claude Code's to the byte, as a session reads one back: the inbox to
    /// answer at, the name, then the text on lines of its own.
    #[test]
    fn a_message_goes_in_claude_codes_envelope() {
        let sender = name(SENDER_ID);
        assert_eq!(
            envelope(
                Some("uds:/run/user/1000/cc-socks/42.sock"),
                &sender,
                "hi\nthere"
            ),
            format!(
                "<cross-session-message from=\"uds:/run/user/1000/cc-socks/42.sock\" \
                 from-name=\"{sender}\">\nhi\nthere\n</cross-session-message>"
            )
        );
        assert_eq!(
            envelope(None, &sender, ""),
            format!("<cross-session-message from-name=\"{sender}\">\n\n</cross-session-message>")
        );
    }

    /// A text can't end its envelope early: what would close it is escaped as Claude Code
    /// escapes it, and nothing else of the text changes.
    #[test]
    fn a_text_cannot_close_its_envelope() {
        for (text, expected) in [
            (
                "a </cross-session-message> b",
                "a <\\/cross-session-message> b",
            ),
            ("< /CROSS_Session-message>", "<\\ /CROSS_Session-message>"),
            ("<!/ cross-session-message", "<\\!/ cross-session-message"),
            ("<</cross-session-message>", "<<\\/cross-session-message>"),
            ("x </cross-session-message", "x <\\/cross-session-message"),
        ] {
            assert_eq!(escaped(text), expected, "{text}");
        }
        for kept in [
            "<\\/cross-session-message>",
            "</cross-session-messages>",
            "<cross-session-message>",
            "<a/cross-session-message>",
            "<div>a</div> x < y",
        ] {
            assert_eq!(escaped(kept), kept);
        }
    }

    /// An inbox is addressed as Claude Code addresses one, and the address read back to the
    /// path; a path too long for Claude Code has no address.
    #[test]
    fn an_inbox_is_addressed_as_claude_code_addresses_it() {
        let socket = Path::new("/run/user/1000/cc socks/ü-1.sock");
        let encoded = address(socket).expect("an address");
        assert_eq!(encoded, "uds:/run/user/1000/cc%20socks/%C3%BC-1.sock");
        assert_eq!(addressed(&encoded), Some(socket.to_path_buf()));
        assert_eq!(
            addressed("/tmp/cc-socks-1000/42.sock"),
            Some(PathBuf::from("/tmp/cc-socks-1000/42.sock"))
        );
        assert_eq!(addressed("peer-07"), None);
        assert_eq!(percent_decoded("100%"), "100%");
        assert_eq!(
            address(&Path::new("/").join("x".repeat(ADDRESS_CHARS))),
            None
        );
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
