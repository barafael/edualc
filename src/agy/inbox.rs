//! An agy session's inbox, as a Claude Code session's (2.1.322): a socket in the messaging
//! folder, a record in `~/.claude/sessions` naming it, and a key beside the record with the
//! token a peer authenticates with. Other sessions, Claude Code's own and agy's, find the
//! session by its name there and post into it as they post into each other's: an `auth`
//! line, then `user` lines. The adapter serves it, and removes the files as it ends.

use std::os::unix::fs::DirBuilderExt as _;
use std::path::{Path, PathBuf};

use serde_json::Value;
use sha2::Digest as _;
use subtle::ConstantTimeEq as _;
use tokio::io::{AsyncBufReadExt as _, AsyncReadExt as _, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::mpsc;
use zeroize::Zeroizing;

use crate::external::claude_files::{SessionKey, SessionRecord};
use crate::external::wire::SessionId;
use crate::process;
use crate::sessions::MessagingName;

use super::outbox;

/// The most a peer may post on one connection.
const POSTED_BYTES: u64 = 1024 * 1024;
/// Random bytes in a token, as Claude Code makes them (hex-encoded, 32 characters).
const TOKEN_BYTES: usize = 16;

#[derive(Debug, thiserror::Error)]
pub(crate) enum Error {
    #[error("no runtime folder (XDG_RUNTIME_DIR) for the inbox")]
    NoRuntime,
    #[error("couldn't make the inbox: {0}")]
    Socket(#[source] std::io::Error),
    #[error("couldn't publish the session for others to message: {0}")]
    Publish(#[source] std::io::Error),
    #[error("couldn't make a token: {0}")]
    Random(#[source] getrandom::Error),
    #[error("this process has no start time to tell it by")]
    NoStart,
}

/// The inbox, open: its socket listens, and its record and key are published until it is
/// dropped.
#[derive(Debug)]
pub(crate) struct Inbox {
    listener: UnixListener,
    token: Token,
    published: Published,
}

/// The token peers authenticate with. Not `Debug`.
struct Token(Zeroizing<String>);

impl std::fmt::Debug for Token {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("Token(…)")
    }
}

/// The files that make the inbox known: removed as it closes.
#[derive(Debug)]
struct Published {
    socket: PathBuf,
    record: PathBuf,
    key: PathBuf,
}

impl Drop for Published {
    fn drop(&mut self) {
        for path in [&self.record, &self.key, &self.socket] {
            outbox::remove(path);
        }
    }
}

impl Inbox {
    /// Opens the inbox of session `id`, working in `cwd`, named `name` for others to message.
    pub(crate) fn open(id: &SessionId, cwd: &Path, name: &MessagingName) -> Result<Self, Error> {
        let pid = std::process::id();
        let started = process::stat(pid).ok_or(Error::NoStart)?.started;
        let runtime = std::env::var_os("XDG_RUNTIME_DIR")
            .map(PathBuf::from)
            .ok_or(Error::NoRuntime)?;
        let folder = runtime.join("cc-socks");
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&folder)
            .map_err(Error::Socket)?;
        let socket = folder.join(format!("{pid}.sock"));
        // A socket left by an earlier process with this pid.
        outbox::remove(&socket);
        let listener = UnixListener::bind(&socket).map_err(Error::Socket)?;
        let mut random = [0u8; TOKEN_BYTES];
        getrandom::fill(&mut random).map_err(Error::Random)?;
        let token = Token(Zeroizing::new(hex(&random)));
        random.fill(0);

        let sessions = crate::sessions::config_dir().join("sessions");
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&sessions)
            .map_err(Error::Publish)?;
        let domain = pid_domain();
        let key_name = format!(
            "{pid}.{}.key",
            hex(&sha2::Sha256::digest(socket.to_string_lossy().as_bytes()))
        );
        let key = SessionKey {
            peer_token: token.0.to_string(),
            proc_start: started.clone(),
            pid_domain: domain.clone(),
        };
        let key_text = Zeroizing::new(
            serde_json::to_string(&key).map_err(|error| Error::Publish(error.into()))?,
        );
        drop(key);
        let now_ms = now_ms();
        let record = SessionRecord {
            pid,
            session_id: id.clone(),
            cwd: cwd.to_owned(),
            proc_start: started,
            messaging_socket_path: socket.clone(),
            started_at: Some(now_ms),
            version: Some(format!("edualc-agy {}", env!("CARGO_PKG_VERSION"))),
            peer_protocol: Some(1),
            peer_features: Vec::new(),
            kind: Some("interactive".to_owned()),
            entrypoint: Some("sdk-cli".to_owned()),
            pid_domain: domain,
            name: Some(name.to_string()),
            name_source: Some("derived".to_owned()),
            name_since: Some(now_ms),
            status: Some("idle".to_owned()),
            updated_at: Some(now_ms),
            status_updated_at: Some(now_ms),
        };
        let record_text =
            serde_json::to_vec(&record).map_err(|error| Error::Publish(error.into()))?;
        let published = Published {
            socket,
            record: sessions.join(format!("{pid}.json")),
            key: sessions.join(&key_name),
        };
        outbox::write_atomically(&sessions, key_name.as_ref(), key_text.as_bytes())
            .map_err(Error::Publish)?;
        outbox::write_atomically(&sessions, format!("{pid}.json").as_ref(), &record_text)
            .map_err(Error::Publish)?;
        Ok(Self {
            listener,
            token,
            published,
        })
    }

    /// Takes the next peer's connection and reads what it posts, in a task of its own: each
    /// message, once the peer authenticated, goes to `posted`.
    pub(crate) async fn take(&self, posted: &mpsc::Sender<String>) {
        let stream = match self.listener.accept().await {
            Ok((stream, _)) => stream,
            Err(error) => {
                tracing::warn!(%error, "a peer's connection failed");
                return;
            }
        };
        let token = Token(self.token.0.clone());
        tokio::spawn(read_posted(stream, token, posted.clone()));
    }

    pub(crate) fn socket(&self) -> &Path {
        &self.published.socket
    }
}

/// What one peer posts: an `auth` line with the token, then messages.
async fn read_posted(stream: UnixStream, token: Token, posted: mpsc::Sender<String>) {
    let mut lines = BufReader::new(stream.take(POSTED_BYTES)).lines();
    let authenticated = match lines.next_line().await {
        Ok(Some(line)) => serde_json::from_str::<Value>(&line).is_ok_and(|auth| {
            auth.get("type").and_then(Value::as_str) == Some("auth")
                && auth
                    .get("token")
                    .and_then(Value::as_str)
                    .is_some_and(|given| bool::from(given.as_bytes().ct_eq(token.0.as_bytes())))
        }),
        Ok(None) | Err(_) => false,
    };
    if !authenticated {
        tracing::warn!("a peer that didn't authenticate");
        return;
    }
    while let Ok(Some(line)) = lines.next_line().await {
        let Ok(message) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        if message.get("type").and_then(Value::as_str) != Some("user") {
            continue;
        }
        let Some(text) = message.pointer("/message/content").and_then(text_of) else {
            continue;
        };
        if posted.send(text).await.is_err() {
            return;
        }
    }
}

/// A message's content as text: as it is, or its text blocks.
fn text_of(content: &Value) -> Option<String> {
    match content {
        Value::String(text) => Some(text.clone()),
        Value::Array(blocks) => {
            let texts: Vec<&str> = blocks
                .iter()
                .filter_map(|block| block.get("text").and_then(Value::as_str))
                .collect();
            (!texts.is_empty()).then(|| texts.join("\n"))
        }
        Value::Null | Value::Bool(_) | Value::Number(_) | Value::Object(_) => None,
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes
        .iter()
        .flat_map(|byte| [byte >> 4, byte & 0x0f])
        .filter_map(|nibble| char::from_digit(u32::from(nibble), 16))
        .collect()
}

fn now_ms() -> u64 {
    u64::try_from(jiff::Timestamp::now().as_millisecond()).unwrap_or_default()
}

/// Which machine and pid namespace the pid is in, as Claude Code writes it:
/// `linux:<boot id>:pid:[<namespace>]`.
fn pid_domain() -> Option<String> {
    let boot = std::fs::read_to_string("/proc/sys/kernel/random/boot_id").ok()?;
    let namespace = std::fs::read_link("/proc/self/ns/pid").ok()?;
    Some(format!(
        "linux:{}:{}",
        boot.trim().replace('-', ""),
        namespace.to_string_lossy()
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn texts_and_names() {
        assert_eq!(hex(&[0, 15, 255]), "000fff");
        assert_eq!(
            text_of(
                &serde_json::json!([{"type": "text", "text": "a"}, {"type": "text", "text": "b"}])
            ),
            Some("a\nb".to_owned())
        );
        assert_eq!(text_of(&serde_json::json!("hi")), Some("hi".to_owned()));
    }
}
