//! What an agy session sends to other sessions with edualc's messaging tool (see `tool`): a
//! message posted into the recipient's inbox as Claude Code's own sessions post one, in its
//! envelope, which names the session and the inbox to answer at; and the list of the sessions
//! running. Read off the records Claude Code keeps in `~/.claude/sessions` (see `inbox`).

use std::ffi::OsStr;
use std::io::{Read as _, Write as _};
use std::os::fd::OwnedFd;
use std::os::unix::fs::{FileTypeExt as _, MetadataExt as _, OpenOptionsExt as _};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::Duration;

use rustix::fs::{FileType, Mode, OFlags};
use serde_json::json;

use crate::external::claude_files::{SessionKey, SessionRecord};
use crate::sessions::MessagingName;
use crate::{process, sessions};

/// A message longer than this isn't posted.
const MESSAGE_CHARS: usize = 64 * 1024;
const RECORD_BYTES: u64 = 64 * 1024;
const KEY_BYTES: u64 = 4 * 1024;
/// How long posting into an inbox may take.
const POSTING: Duration = Duration::from_secs(2);

/// What agy gives `send_message`.
#[derive(Debug, PartialEq, Eq)]
pub(super) struct SendArguments {
    pub(super) to: String,
    pub(super) message: String,
}

#[derive(Debug, PartialEq, Eq)]
pub(super) enum Job {
    Send(SendArguments),
    List,
}

/// What carrying out a call came to.
#[derive(Debug)]
pub(super) enum Outcome {
    /// Posted into the inbox of the session named.
    Sent(String),
    /// The other running sessions, a line each.
    Listed(Vec<String>),
    Refused(Refusal),
}

impl Outcome {
    /// What the calling session is told, and whether as an error.
    pub(super) fn told(&self) -> (String, bool) {
        match self {
            Self::Sent(to) => (
                format!(
                    "Delivered to {to}. An answer, if it gives one, arrives as a message to this \
                     session."
                ),
                false,
            ),
            Self::Listed(names) if names.is_empty() => {
                ("No other session is running.".to_owned(), false)
            }
            Self::Listed(names) => (format!("Running sessions:\n{}", names.join("\n")), false),
            Self::Refused(refusal) => (refusal.to_string(), true),
        }
    }
}

/// Why a message isn't delivered: told to agy as the call's error.
#[derive(Debug, thiserror::Error)]
pub(super) enum Refusal {
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
/// sent it and answer it: the envelope, with the sender's inbox (`from`), name and permission
/// class (`from-mode`), then the text. Claude Code tells sessions apart by that class when one
/// messages another: a receiver holds a message from another class, and, running with
/// permissions bypassed, one that names none. An agy session asks before its commands, so its
/// class is `prompting`. Claude Code reads an envelope back only in exactly the form it writes
/// one, attributes in this order and the text escaped as [`escaped`] does; anything else
/// reaches the session as plain text.
fn envelope(from: Option<&str>, name: &MessagingName, text: &str) -> String {
    let from = from.map(|from| format!(" from=\"{from}\""));
    let name =
        (name.as_str().chars().count() <= NAME_CHARS).then(|| format!(" from-name=\"{name}\""));
    format!(
        "<{ENVELOPE}{}{} from-mode=\"prompting\">\n{}\n</{ENVELOPE}>",
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

/// Where Claude Code keeps the records of running sessions, and the folders their inboxes
/// may be in.
#[derive(Debug, Clone)]
pub(super) struct Places {
    pub(super) sessions: PathBuf,
    pub(super) inboxes: Vec<PathBuf>,
}

impl Places {
    /// The user's: `~/.claude/sessions`, and `$XDG_RUNTIME_DIR/cc-socks` or the fallback in
    /// /tmp that Claude Code takes when that path is too long for a socket.
    pub(super) fn of_user() -> Self {
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

/// Carries out a call for the session `sender`: lists the running sessions, or posts a
/// message into one's inbox.
pub(super) fn carry_out(places: &Places, sender: &MessagingName, job: &Job) -> Outcome {
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

/// The inbox of session `sender`, as its record among the running ones gives it (see
/// `inbox`), for the sessions it writes to to answer at; none where no single such record says.
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
        .filter_map(|name| read_regular(&folder, &name, RECORD_BYTES))
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
        .filter_map(|name| read_regular(&folder, &name, KEY_BYTES))
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

/// The text of `name` in the opened `folder`, if it is a regular file (no link) of at most
/// `limit` bytes.
fn read_regular(folder: &OwnedFd, name: &OsStr, limit: u64) -> Option<String> {
    let file = rustix::fs::openat(
        folder,
        name,
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .ok()?;
    let stat = rustix::fs::fstat(&file).ok()?;
    let regular = FileType::from_raw_mode(stat.st_mode) == FileType::RegularFile;
    if !regular || u64::try_from(stat.st_size).ok()? > limit {
        return None;
    }
    let mut text = String::new();
    std::fs::File::from(file)
        .take(limit)
        .read_to_string(&mut text)
        .ok()?;
    Some(text)
}

/// Writes `name` in `folder` whole or not at all, readable by this user alone, as Claude Code
/// writes its own.
pub(super) fn write_atomically(folder: &Path, name: &OsStr, bytes: &[u8]) -> std::io::Result<()> {
    let staged = folder.join(format!(".edualc-{}.tmp", uuid::Uuid::new_v4()));
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&staged)?;
    let written = file
        .write_all(bytes)
        .and_then(|()| std::fs::rename(&staged, folder.join(name)));
    if written.is_err() {
        remove(&staged);
    }
    written
}

pub(super) fn remove(path: &Path) {
    if let Err(error) = std::fs::remove_file(path)
        && error.kind() != std::io::ErrorKind::NotFound
    {
        tracing::debug!(%error, path = %path.display(), "a session's file stays");
    }
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
    use serde_json::Value;
    use std::io::{BufRead as _, BufReader};
    use std::os::unix::net::UnixListener;

    const SENDER_ID: &str = "11111111-2222-4333-8444-555555555555";
    const PEER_ID: &str = "99999999-2222-4333-8444-555555555555";

    fn name(id: &str) -> MessagingName {
        let id = crate::external::wire::SessionId::try_from(id.to_owned()).expect("an id");
        MessagingName::of(&id, Path::new("/work/project"))
    }

    /// A running session's record, key and inbox, in a scratch `~/.claude/sessions` and
    /// messaging folder: this test's process stands in for it.
    struct Peer {
        scratch: PathBuf,
        places: Places,
        inbox: UnixListener,
        token: String,
    }

    impl Peer {
        fn new(test: &str, peer: &MessagingName) -> Self {
            let scratch =
                std::env::temp_dir().join(format!("edualc-outbox-{test}-{}", std::process::id()));
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

    /// A message reaches the inbox of the session named, after its token, in Claude Code's
    /// envelope: the sender's inbox to answer at, its name, its class, the text. The list names
    /// the other sessions running.
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
                    "<cross-session-message from=\"{own_address}\" from-name=\"{sender}\" \
                     from-mode=\"prompting\">\nhello\n</cross-session-message>"
                )
                .as_str()
            )
        );
        let listed = carry_out(&peer.places, &sender, &Job::List);
        assert!(
            matches!(&listed, Outcome::Listed(names) if *names == [format!("{peer_name} (idle)")]),
            "{listed:?}"
        );
        peer.done();
    }

    /// The envelope is Claude Code's to the byte, as a session reads one back: the inbox to
    /// answer at, the name, the class, then the text on lines of its own.
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
                 from-name=\"{sender}\" from-mode=\"prompting\">\nhi\nthere\n\
                 </cross-session-message>"
            )
        );
        assert_eq!(
            envelope(None, &sender, ""),
            format!(
                "<cross-session-message from-name=\"{sender}\" from-mode=\"prompting\">\n\n\
                 </cross-session-message>"
            )
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
