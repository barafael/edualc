//! A sandboxed session's record, mirrored into the user's `~/.claude/sessions`, where other
//! Claude Code sessions look for sessions to message. The session writes its record in its
//! own home (see `home`), which nothing outside the sandbox reads; what crosses is checked
//! here. The record must describe the process edualc started (its pid, its start, its session,
//! its folder) and name that process's own socket in the messaging folder; only the fields
//! other sessions read cross, each of a bounded size, and the key beside it only when it
//! names the same process. So all a sandboxed session can tell the others is how to reach it.
//!
//! The kernel says when the record changes (inotify): a session that waits for messages writes
//! its record at its start, before it says anything on its output.

use std::ffi::{OsStr, OsString};
use std::io::{Read as _, Write as _};
use std::mem::MaybeUninit;
use std::os::fd::OwnedFd;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

use rustix::fs::inotify::{self, CreateFlags, WatchFlags};
use rustix::fs::{FileType, Mode, OFlags};
use rustix::io::Errno;
use tokio::sync::mpsc;

use crate::external::claude_files::{SessionKey, SessionRecord};
use crate::external::wire::SessionId;
use crate::home::Home;
use crate::sessions::MessagingName;
use crate::{process, sessions};

/// A record runs to a kilobyte; anything far larger isn't one.
const RECORD_BYTES: u64 = 64 * 1024;
/// A key runs to a few hundred bytes.
const KEY_BYTES: u64 = 4 * 1024;
/// The longest text a mirrored field may hold.
const FIELD_CHARS: usize = 256;
/// The most features a record may name.
const FEATURES: usize = 32;

#[derive(Debug, thiserror::Error)]
pub(crate) enum Error {
    #[error("couldn't watch {path} for the session's record: {source}")]
    Watch {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("the session's process {0} has no start time to tell it from a later one")]
    NoStart(u32),
}

/// What a sandboxed start leaves for its record to be mirrored by, once its process runs.
#[derive(Debug)]
pub(crate) struct Source {
    pub(crate) home: Home,
    pub(crate) id: SessionId,
    pub(crate) cwd: PathBuf,
    /// The name other sessions message it by, as its record gives it.
    pub(crate) name: MessagingName,
    /// Whether the user is asked about each message it sends (see `relay`).
    pub(crate) consent: crate::relay::Consent,
}

/// Who the mirrored session must be.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Expected {
    pid: u32,
    /// Ticks after boot, as `/proc` gives them (`procStart`).
    started: String,
    id: SessionId,
    cwd: PathBuf,
    socket: PathBuf,
}

impl Expected {
    /// The session `id` in `cwd`, run by process `pid`.
    fn of(pid: u32, id: SessionId, cwd: PathBuf) -> Result<Self, Error> {
        let started = process::stat(pid).ok_or(Error::NoStart(pid))?.started;
        let socket = std::env::var_os("XDG_RUNTIME_DIR")
            .map(PathBuf::from)
            .unwrap_or_default()
            .join("cc-socks")
            .join(format!("{pid}.sock"));
        Ok(Self {
            pid,
            started,
            id,
            cwd,
            socket,
        })
    }
}

/// The record as it may cross: the session's own, bounded; `None` for anything else.
fn checked_record(text: &str, expected: &Expected) -> Option<SessionRecord> {
    let record: SessionRecord = serde_json::from_str(text).ok()?;
    let ours = record.pid == expected.pid
        && record.proc_start == expected.started
        && record.session_id == expected.id
        && record.cwd == expected.cwd
        && record.messaging_socket_path == expected.socket;
    let texts = [
        &record.version,
        &record.kind,
        &record.entrypoint,
        &record.pid_domain,
        &record.name,
        &record.name_source,
        &record.status,
    ];
    let bounded = texts
        .iter()
        .all(|text| text.as_ref().is_none_or(|text| short(text)))
        && record.peer_features.len() <= FEATURES
        && record.peer_features.iter().all(|feature| short(feature));
    (ours && bounded).then_some(record)
}

/// The key as it may cross: named for the session's process, its start the same.
fn checked_key(name: &OsStr, text: &str, expected: &Expected) -> Option<SessionKey> {
    let name = name.to_str()?;
    let hash = name
        .strip_prefix(&format!("{}.", expected.pid))?
        .strip_suffix(".key")?;
    let named = hash.len() == 64 && hash.bytes().all(|byte| byte.is_ascii_hexdigit());
    let key: SessionKey = serde_json::from_str(text).ok()?;
    let token = !key.peer_token.is_empty()
        && key.peer_token.len() <= FIELD_CHARS
        && key
            .peer_token
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_');
    let same = key.proc_start == expected.started
        && key.pid_domain.as_ref().is_none_or(|domain| short(domain));
    (named && token && same).then_some(key)
}

fn short(text: &str) -> bool {
    text.chars().count() <= FIELD_CHARS && !text.chars().any(char::is_control)
}

/// The mirroring for one session: what it reads from the session's home, where it writes in
/// the user's, and what it wrote there, which goes when it is dropped.
#[derive(Debug)]
struct Records {
    /// The session's `sessions` folder in its own home, opened following no link.
    own: OwnedFd,
    users: PathBuf,
    expected: Expected,
    written: Vec<OsString>,
}

impl Records {
    /// Brings the user's copy in line with the session's own: written if it checks out,
    /// removed if it doesn't or is gone.
    fn refresh(&mut self) {
        let record_name = OsString::from(format!("{}.json", self.expected.pid));
        let record = read_regular(&self.own, &record_name, RECORD_BYTES)
            .and_then(|text| checked_record(&text, &self.expected))
            .and_then(|record| serde_json::to_vec(&record).ok());
        let mut wanted: Vec<(OsString, Vec<u8>)> = Vec::new();
        if let Some(record) = record {
            wanted.push((record_name, record));
            for name in self.key_names() {
                let key = read_regular(&self.own, &name, KEY_BYTES)
                    .and_then(|text| checked_key(&name, &text, &self.expected))
                    .and_then(|key| serde_json::to_vec(&key).ok());
                if let Some(key) = key {
                    wanted.push((name, key));
                }
            }
        }
        for name in std::mem::take(&mut self.written) {
            if !wanted.iter().any(|(wanted, _)| *wanted == name) {
                remove(&self.users.join(&name));
            }
        }
        for (name, bytes) in wanted {
            match write_atomically(&self.users, &name, &bytes) {
                Ok(()) => self.written.push(name),
                Err(error) => {
                    tracing::warn!(%error, name = ?name, "couldn't mirror a sandboxed session's record");
                }
            }
        }
    }

    /// The names in the session's folder that could be its keys.
    fn key_names(&self) -> Vec<OsString> {
        let prefix = format!("{}.", self.expected.pid);
        let Ok(dir) = rustix::fs::Dir::read_from(&self.own) else {
            return Vec::new();
        };
        dir.filter_map(Result::ok)
            .map(|entry| OsStr::from_bytes(entry.file_name().to_bytes()).to_owned())
            .filter(|name| {
                name.to_str()
                    .is_some_and(|name| name.starts_with(&prefix) && name.ends_with(".key"))
            })
            .collect()
    }
}

impl Drop for Records {
    fn drop(&mut self) {
        for name in &self.written {
            remove(&self.users.join(name));
        }
    }
}

/// A file in the opened folder, if it is a regular one (no link, no pipe that would hold the
/// reading up) of at most `limit` bytes.
pub(crate) fn read_regular(folder: &OwnedFd, name: &OsStr, limit: u64) -> Option<String> {
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
fn write_atomically(folder: &Path, name: &OsStr, bytes: &[u8]) -> std::io::Result<()> {
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

fn remove(path: &Path) {
    if let Err(error) = std::fs::remove_file(path)
        && error.kind() != std::io::ErrorKind::NotFound
    {
        tracing::debug!(%error, path = %path.display(), "a mirrored record stays");
    }
}

/// A sandboxed session's record, followed as it changes and mirrored into the user's home
/// while the session runs.
#[derive(Debug)]
pub(crate) struct Mirror {
    inotify: OwnedFd,
    watched: i32,
    changes: mpsc::Receiver<()>,
    records: Records,
}

impl Mirror {
    /// Follows the record of the session `source` started as process `pid`; call from the
    /// daemon's runtime.
    pub(crate) fn start(source: &Source, pid: u32) -> Result<Self, Error> {
        let Source { home, id, cwd, .. } = source;
        let expected = Expected::of(pid, id.clone(), cwd.clone())?;
        let own_path = home.path().join("sessions");
        let watch = |source: std::io::Error| Error::Watch {
            path: own_path.clone(),
            source,
        };
        match std::fs::DirBuilder::new().create(&own_path) {
            Err(error) if error.kind() != std::io::ErrorKind::AlreadyExists => {
                return Err(watch(error));
            }
            Ok(()) | Err(_) => {}
        }
        let home_fd = rustix::fs::open(
            home.path(),
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .map_err(|errno| watch(errno.into()))?;
        let own = rustix::fs::openat(
            &home_fd,
            "sessions",
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .map_err(|errno| watch(errno.into()))?;
        let inotify = inotify::init(CreateFlags::CLOEXEC).map_err(|errno| watch(errno.into()))?;
        // The folder opened, not its path again: a link put there since isn't watched.
        let opened = PathBuf::from(format!(
            "/proc/self/fd/{}",
            rustix::fd::AsRawFd::as_raw_fd(&own)
        ));
        let watched = inotify::add_watch(
            &inotify,
            &opened,
            WatchFlags::ONLYDIR
                | WatchFlags::CLOSE_WRITE
                | WatchFlags::MOVED_TO
                | WatchFlags::MOVED_FROM
                | WatchFlags::DELETE,
        )
        .map_err(|errno| watch(errno.into()))?;
        // The reading thread has a descriptor of its own for the watch, closed on exec like the
        // first, so no program the daemon starts holds it.
        let reading =
            rustix::io::fcntl_dupfd_cloexec(&inotify, 0).map_err(|errno| watch(errno.into()))?;
        let (tell, changes) = mpsc::channel(1);
        std::thread::spawn(move || notice(&reading, &tell));
        let mut records = Records {
            own,
            users: sessions::config_dir().join("sessions"),
            expected,
            written: Vec::new(),
        };
        records.refresh();
        Ok(Self {
            inotify,
            watched,
            changes,
            records,
        })
    }

    /// Waits for the session's folder to change, then mirrors what it holds.
    pub(crate) async fn follow(&mut self) {
        if self.changes.recv().await.is_none() {
            // The watch stopped: nothing is noticed any more.
            std::future::pending::<()>().await;
        }
        self.records.refresh();
    }
}

impl Drop for Mirror {
    /// The session has ended: its socket goes too, which it couldn't remove itself (see
    /// `sandbox`), if what is there is still a socket.
    fn drop(&mut self) {
        // Ends the reading thread: the kernel tells it the watch is gone.
        if let Err(error) = inotify::remove_watch(&self.inotify, self.watched) {
            tracing::debug!(%error, "a record's watch that was gone already");
        }
        let socket = &self.records.expected.socket;
        let still_a_socket = rustix::fs::lstat(socket)
            .is_ok_and(|stat| FileType::from_raw_mode(stat.st_mode) == FileType::Socket);
        if still_a_socket {
            remove(socket);
        }
    }
}

/// Tells of changes in the watched folder until the watch is removed. Changes that come while
/// one is still untold are told as that one.
fn notice(inotify: &OwnedFd, tell: &mpsc::Sender<()>) {
    let mut buffer = [MaybeUninit::<u8>::uninit(); 4096];
    let mut reader = inotify::Reader::new(inotify, &mut buffer);
    loop {
        match reader.next() {
            Ok(noticed) if noticed.events().contains(inotify::ReadFlags::IGNORED) => break,
            Ok(_) => match tell.try_send(()) {
                Ok(()) | Err(mpsc::error::TrySendError::Full(())) => {}
                Err(mpsc::error::TrySendError::Closed(())) => break,
            },
            Err(Errno::INTR) => {}
            Err(error) => {
                tracing::debug!(%error, "stopped following a session's record");
                break;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn expected() -> Expected {
        Expected {
            pid: 4242,
            started: "123456".to_owned(),
            id: SessionId::try_from("11111111-2222-4333-8444-555555555555".to_owned())
                .expect("an id"),
            cwd: PathBuf::from("/work/project"),
            socket: PathBuf::from("/run/user/1000/cc-socks/4242.sock"),
        }
    }

    fn record(changes: &[(&str, serde_json::Value)]) -> String {
        let mut record = serde_json::json!({
            "pid": 4242,
            "sessionId": "11111111-2222-4333-8444-555555555555",
            "cwd": "/work/project",
            "procStart": "123456",
            "messagingSocketPath": "/run/user/1000/cc-socks/4242.sock",
            "name": "edualc-project-calm-otter-07",
            "status": "idle",
            "peerFeatures": ["notify_idle"],
            "asksForConvertedSkills": false,
        });
        for (key, value) in changes {
            if let Some(fields) = record.as_object_mut() {
                fields.insert((*key).to_owned(), value.clone());
            }
        }
        record.to_string()
    }

    #[test]
    fn the_sessions_own_record_crosses_in_the_fields_others_read() {
        let crossed = checked_record(&record(&[]), &expected()).expect("crosses");
        let text = serde_json::to_string(&crossed).expect("json");
        assert!(text.contains("\"messagingSocketPath\":\"/run/user/1000/cc-socks/4242.sock\""));
        assert!(text.contains("\"name\":\"edualc-project-calm-otter-07\""));
        assert!(!text.contains("asksForConvertedSkills"), "{text}");
    }

    /// A record that would send messages elsewhere, or speak for another session, stays.
    #[test]
    fn a_record_not_the_sessions_own_stays() {
        for change in [
            (
                "messagingSocketPath",
                serde_json::json!("/run/user/1000/cc-socks/1.sock"),
            ),
            (
                "messagingSocketPath",
                serde_json::json!("/tmp/elsewhere.sock"),
            ),
            ("pid", serde_json::json!(4243)),
            ("procStart", serde_json::json!("999")),
            (
                "sessionId",
                serde_json::json!("11111111-2222-4333-8444-666666666666"),
            ),
            ("cwd", serde_json::json!("/elsewhere")),
            ("name", serde_json::json!("x".repeat(FIELD_CHARS + 1))),
            ("status", serde_json::json!("idle\u{1b}[2J")),
        ] {
            assert_eq!(
                checked_record(&record(std::slice::from_ref(&change)), &expected()),
                None,
                "{change:?}"
            );
        }
    }

    #[test]
    fn only_a_key_named_for_the_session_with_its_start_crosses() {
        let hash = "a".repeat(64);
        let key =
            r#"{"peerToken":"abc_DEF-123","procStart":"123456","pidDomain":"linux:x:pid:[1]"}"#;
        let name = format!("4242.{hash}.key");
        assert!(checked_key(OsStr::new(&name), key, &expected()).is_some());
        let other = format!("4243.{hash}.key");
        assert!(checked_key(OsStr::new(&other), key, &expected()).is_none());
        assert!(checked_key(OsStr::new("4242.short.key"), key, &expected()).is_none());
        let later = key.replace("123456", "999");
        assert!(checked_key(OsStr::new(&name), &later, &expected()).is_none());
        let odd = key.replace("abc_DEF-123", "abc def");
        assert!(checked_key(OsStr::new(&name), &odd, &expected()).is_none());
    }

    /// The user's copy follows the session's: written while it checks out, gone when it
    /// changes for the worse, and when the mirroring ends.
    #[test]
    fn the_users_copy_follows_and_goes() {
        let scratch = std::env::temp_dir().join(format!("edualc-mirror-{}", std::process::id()));
        let own_path = scratch.join("own");
        let users = scratch.join("users");
        std::fs::create_dir_all(&own_path).expect("own");
        std::fs::create_dir_all(&users).expect("users");
        let own = rustix::fs::open(
            &own_path,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .expect("opened");
        let key_name = format!("4242.{}.key", "b".repeat(64));
        std::fs::write(own_path.join("4242.json"), record(&[])).expect("a record");
        std::fs::write(
            own_path.join(&key_name),
            r#"{"peerToken":"tok","procStart":"123456"}"#,
        )
        .expect("a key");
        let mut records = Records {
            own,
            users: users.clone(),
            expected: expected(),
            written: Vec::new(),
        };
        records.refresh();
        assert!(users.join("4242.json").is_file(), "the record crossed");
        assert!(users.join(&key_name).is_file(), "so did its key");
        std::fs::write(
            own_path.join("4242.json"),
            record(&[("messagingSocketPath", serde_json::json!("/tmp/x.sock"))]),
        )
        .expect("a changed record");
        records.refresh();
        assert!(
            !users.join("4242.json").exists(),
            "a record gone wrong is taken back"
        );
        assert!(!users.join(&key_name).exists(), "and its key");
        std::fs::write(own_path.join("4242.json"), record(&[])).expect("the record again");
        records.refresh();
        assert!(users.join("4242.json").is_file());
        drop(records);
        assert!(
            !users.join("4242.json").exists(),
            "the mirroring's end takes it"
        );
        assert!(!users.join(&key_name).exists());
        std::fs::remove_dir_all(&scratch).expect("the scratch folder goes");
    }
}
