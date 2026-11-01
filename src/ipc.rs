//! The window and its daemon (`edualc --daemon`), speaking over a Unix socket that only they
//! reach.
//!
//! - The socket lives in `$XDG_RUNTIME_DIR/edualc`, a folder of the user's alone (0700), which is
//!   checked through the descriptor it was opened by, never followed through a link, and used by
//!   that descriptor from then on: binding and connecting go through `/proc/self/fd/<folder>`, so
//!   no path can be swapped in between. Never an abstract socket, which anyone may take first.
//! - Who is the daemon is decided by a lock held as long as it runs: only its holder removes a
//!   stale socket and binds anew, so two starting at once can't both bind, nor one unbind the
//!   other's.
//! - Each peer is checked as it connects: the same user, running this same program.
//! - Messages are postcard (compact serde), each after its length: a length over
//!   [`MAX_MESSAGE`] ends the connection before anything of it is read or kept, as does a
//!   message that doesn't parse. Nothing in a message makes the reader hold more than it was.

use std::io::{Read, Write};
use std::os::fd::{AsFd, OwnedFd};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};

use rustix::fs::{AtFlags, FlockOperation, Mode, OFlags};
use rustix::io::Errno;
use rustix::net::{AddressFamily, SocketAddrUnix, SocketFlags, SocketType};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::claude::Launch;
use crate::desktop::{Opener, Program, Running};
use crate::sandbox::Enforcement;

/// The largest message either side sends: more is a broken or hostile peer.
pub(crate) const MAX_MESSAGE: usize = 64 * 1024;
/// Text sent in one message at most (a long line of Claude's output goes in several), with
/// room left for what the message says besides.
pub(crate) const MAX_CHUNK: usize = MAX_MESSAGE - 1024;
/// Connections waiting to be taken.
const BACKLOG: i32 = 8;

const FOLDER: &str = "edualc";
const SOCKET: &str = "edualcd.sock";
const LOCK: &str = "edualcd.lock";

#[derive(Debug, thiserror::Error)]
pub(crate) enum Error {
    #[error("XDG_RUNTIME_DIR isn't set: there is no private place for the daemon's socket")]
    NoRuntimeDir,
    #[error("{0} isn't the user's alone (it must be a folder they own, closed to others)")]
    NotPrivate(PathBuf),
    #[error("couldn't open {path}: {source}")]
    Open {
        path: PathBuf,
        #[source]
        source: Errno,
    },
    #[error("the daemon's socket: {0}")]
    Socket(#[source] Errno),
    #[error("the daemon's socket: {0}")]
    Io(#[source] std::io::Error),
    #[error("a message bigger than {MAX_MESSAGE} bytes")]
    Oversized,
    #[error("a message that doesn't parse: {0}")]
    Unreadable(#[source] postcard::Error),
    #[error("couldn't encode a message: {0}")]
    Encode(#[source] postcard::Error),
    #[error("the other side hung up")]
    Closed,
    #[error("a peer that isn't this user's edualc (uid {uid}, pid {pid})")]
    Stranger { uid: u32, pid: i32 },
}

/// The private folder, open: what is done in it is done through this descriptor.
#[derive(Debug)]
pub(crate) struct Folder {
    fd: OwnedFd,
    path: PathBuf,
}

impl Folder {
    /// `$XDG_RUNTIME_DIR/edualc`, made if it isn't there, and checked to be the user's alone.
    pub(crate) fn open() -> Result<Self, Error> {
        let runtime =
            PathBuf::from(std::env::var_os("XDG_RUNTIME_DIR").ok_or(Error::NoRuntimeDir)?);
        let runtime_fd = open_folder(rustix::fs::CWD, &runtime, &runtime)?;
        let path = runtime.join(FOLDER);
        match rustix::fs::mkdirat(&runtime_fd, FOLDER, Mode::RWXU) {
            Ok(()) | Err(Errno::EXIST) => {}
            Err(source) => return Err(Error::Open { path, source }),
        }
        let fd = open_folder(&runtime_fd, Path::new(FOLDER), &path)?;
        Ok(Self { fd, path })
    }

    /// The daemon's log, begun anew, the user's alone.
    pub(crate) fn log_file(&self) -> Result<std::fs::File, Error> {
        let flags =
            OFlags::CREATE | OFlags::TRUNC | OFlags::WRONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC;
        let fd = rustix::fs::openat(&self.fd, "edualcd.log", flags, Mode::RUSR | Mode::WUSR)
            .map_err(|source| Error::Open {
                path: self.path.join("edualcd.log"),
                source,
            })?;
        Ok(std::fs::File::from(fd))
    }

    /// A name in the folder as a socket address that resolves through the folder's descriptor,
    /// not through the path it was found at.
    fn address(&self, name: &str) -> Result<SocketAddrUnix, Error> {
        use std::os::fd::AsRawFd;
        let through = format!("/proc/self/fd/{}/{name}", self.fd.as_raw_fd());
        SocketAddrUnix::new(through).map_err(Error::Socket)
    }
}

/// Opens a folder without following a link to it, and checks it is the user's alone; one of
/// theirs with looser permissions is closed to others first.
fn open_folder(at: impl AsFd, name: &Path, path: &Path) -> Result<OwnedFd, Error> {
    let flags = OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC | OFlags::RDONLY;
    let fd = rustix::fs::openat(at, name, flags, Mode::empty()).map_err(|source| Error::Open {
        path: path.to_owned(),
        source,
    })?;
    let stat = rustix::fs::fstat(&fd).map_err(|source| Error::Open {
        path: path.to_owned(),
        source,
    })?;
    if stat.st_uid != rustix::process::getuid().as_raw() {
        return Err(Error::NotPrivate(path.to_owned()));
    }
    if Mode::from_raw_mode(stat.st_mode).intersects(Mode::RWXG | Mode::RWXO) {
        rustix::fs::fchmod(&fd, Mode::RWXU).map_err(|source| Error::Open {
            path: path.to_owned(),
            source,
        })?;
    }
    Ok(fd)
}

/// The daemon's end: the lock it holds, and the socket it listens on.
#[derive(Debug)]
pub(crate) struct Listener {
    socket: UnixListener,
    /// Held as long as the daemon runs; the kernel lets go of it when the process ends.
    lock: OwnedFd,
}

impl Listener {
    /// Becomes the daemon: `None` if another one is.
    pub(crate) fn bind(folder: &Folder) -> Result<Option<Self>, Error> {
        let flags = OFlags::CREATE | OFlags::RDWR | OFlags::NOFOLLOW | OFlags::CLOEXEC;
        let lock = rustix::fs::openat(&folder.fd, LOCK, flags, Mode::RUSR | Mode::WUSR).map_err(
            |source| Error::Open {
                path: folder.path.join(LOCK),
                source,
            },
        )?;
        match rustix::fs::flock(&lock, FlockOperation::NonBlockingLockExclusive) {
            Ok(()) => {}
            Err(Errno::WOULDBLOCK) => return Ok(None),
            Err(source) => {
                return Err(Error::Open {
                    path: folder.path.join(LOCK),
                    source,
                });
            }
        }
        // Holding the lock: whatever socket is there is a dead daemon's.
        match rustix::fs::unlinkat(&folder.fd, SOCKET, AtFlags::empty()) {
            Ok(()) | Err(Errno::NOENT) => {}
            Err(source) => return Err(Error::Socket(source)),
        }
        let socket = rustix::net::socket_with(
            AddressFamily::UNIX,
            SocketType::STREAM,
            SocketFlags::CLOEXEC,
            None,
        )
        .map_err(Error::Socket)?;
        rustix::net::bind(&socket, &folder.address(SOCKET)?).map_err(Error::Socket)?;
        rustix::fs::chmodat(
            &folder.fd,
            SOCKET,
            Mode::RUSR | Mode::WUSR,
            AtFlags::empty(),
        )
        .map_err(Error::Socket)?;
        rustix::net::listen(&socket, BACKLOG).map_err(Error::Socket)?;
        Ok(Some(Self {
            socket: UnixListener::from(socket),
            lock,
        }))
    }

    /// For tokio: the socket, and the lock to hold as long as it is listened on.
    pub(crate) fn into_tokio(self) -> Result<(tokio::net::UnixListener, OwnedFd), Error> {
        self.socket.set_nonblocking(true).map_err(Error::Io)?;
        let socket = tokio::net::UnixListener::from_std(self.socket).map_err(Error::Io)?;
        Ok((socket, self.lock))
    }
}

/// The peer is this user, running this same program.
pub(crate) fn check_peer(fd: impl AsFd) -> Result<(), Error> {
    let peer = rustix::net::sockopt::socket_peercred(fd).map_err(Error::Socket)?;
    let uid = peer.uid.as_raw();
    let pid = peer.pid.as_raw_nonzero().get();
    let stranger = Error::Stranger { uid, pid };
    if uid != rustix::process::getuid().as_raw() {
        return Err(stranger);
    }
    let theirs = std::fs::read_link(format!("/proc/{pid}/exe"));
    let ours = std::fs::read_link("/proc/self/exe");
    match (theirs, ours) {
        (Ok(theirs), Ok(ours)) if theirs == ours => Ok(()),
        _ => Err(stranger),
    }
}

/// One end of a connection, sending and taking whole messages (the window's: it waits).
#[derive(Debug)]
pub(crate) struct Channel {
    stream: UnixStream,
}

impl Channel {
    /// Connects to the daemon, if one is listening.
    pub(crate) fn connect(folder: &Folder) -> Result<Self, Error> {
        let fd = rustix::net::socket_with(
            AddressFamily::UNIX,
            SocketType::STREAM,
            SocketFlags::CLOEXEC,
            None,
        )
        .map_err(Error::Socket)?;
        rustix::net::connect(&fd, &folder.address(SOCKET)?).map_err(Error::Socket)?;
        // The daemon is checked as it checks us: this user, this program.
        check_peer(&fd)?;
        Ok(Self {
            stream: UnixStream::from(fd),
        })
    }

    /// A second end for the same connection: one thread sends, another takes.
    pub(crate) fn try_clone(&self) -> Result<Self, Error> {
        Ok(Self {
            stream: self.stream.try_clone().map_err(Error::Io)?,
        })
    }

    pub(crate) fn send(&mut self, message: &impl Serialize) -> Result<(), Error> {
        let frame = frame(message)?;
        self.stream.write_all(&frame).map_err(closed_or)
    }

    /// The next message; waits for it.
    pub(crate) fn recv<T: DeserializeOwned>(
        &mut self,
        deframer: &mut Deframer,
    ) -> Result<T, Error> {
        let mut bytes = [0u8; READ];
        loop {
            if let Some(message) = deframer.next()? {
                return Ok(message);
            }
            let read = self.stream.read(&mut bytes).map_err(closed_or)?;
            deframer.push(bytes.get(..read).ok_or(Error::Closed)?)?;
        }
    }
}

/// A connection's end in the daemon, waited on with tokio.
#[derive(Debug)]
pub(crate) struct AsyncChannel {
    stream: tokio::net::UnixStream,
}

/// The halves of a connection in the daemon: one reads requests, another writes answers.
pub(crate) type Reading = tokio::net::unix::OwnedReadHalf;
pub(crate) type Writing = tokio::net::unix::OwnedWriteHalf;

impl AsyncChannel {
    /// The next peer of `listener`, once it is checked to be this user's edualc.
    pub(crate) async fn accept(listener: &tokio::net::UnixListener) -> Result<Self, Error> {
        let (stream, _) = listener.accept().await.map_err(Error::Io)?;
        check_peer(&stream)?;
        Ok(Self { stream })
    }

    pub(crate) fn split(self) -> (Reading, Writing) {
        self.stream.into_split()
    }
}

pub(crate) async fn send(writing: &mut Writing, message: &impl Serialize) -> Result<(), Error> {
    let frame = frame(message)?;
    writing.write_all(&frame).await.map_err(closed_or)
}

pub(crate) async fn recv<T: DeserializeOwned>(
    reading: &mut Reading,
    deframer: &mut Deframer,
) -> Result<T, Error> {
    let mut bytes = [0u8; READ];
    loop {
        if let Some(message) = deframer.next()? {
            return Ok(message);
        }
        let read = reading.read(&mut bytes).await.map_err(closed_or)?;
        deframer.push(bytes.get(..read).ok_or(Error::Closed)?)?;
    }
}

/// What is read from a connection at once.
const READ: usize = 16 * 1024;

/// A connection's bytes, as they come, made into messages (sans-io: the window's blocking reads
/// and the daemon's tokio reads feed the same one). It never holds more than one message and a
/// read: a length over [`MAX_MESSAGE`] is refused as soon as it is read.
#[derive(Debug, Default)]
pub(crate) struct Deframer {
    buffer: Vec<u8>,
}

impl Deframer {
    /// Bytes read; an empty read is the other side hanging up.
    pub(crate) fn push(&mut self, bytes: &[u8]) -> Result<(), Error> {
        if bytes.is_empty() {
            return Err(Error::Closed);
        }
        self.buffer.extend_from_slice(bytes);
        Ok(())
    }

    /// The next whole message, if it is all there.
    pub(crate) fn next<T: DeserializeOwned>(&mut self) -> Result<Option<T>, Error> {
        let Some(length) = self.buffer.first_chunk::<4>().copied() else {
            return Ok(None);
        };
        let length = checked_length(length)?;
        let Some(message) = self.buffer.get(4..4 + length) else {
            return Ok(None);
        };
        let message = postcard::from_bytes(message).map_err(Error::Unreadable)?;
        self.buffer.drain(..4 + length);
        Ok(Some(message))
    }
}

/// A message after its length.
pub(crate) fn frame(message: &impl Serialize) -> Result<Vec<u8>, Error> {
    let bytes = postcard::to_stdvec(message).map_err(Error::Encode)?;
    if bytes.len() > MAX_MESSAGE {
        return Err(Error::Oversized);
    }
    let length = u32::try_from(bytes.len()).map_err(|_overflow| Error::Oversized)?;
    let mut frame = Vec::with_capacity(bytes.len() + 4);
    frame.extend_from_slice(&length.to_le_bytes());
    frame.extend_from_slice(&bytes);
    Ok(frame)
}

/// A message's length as read, refused before anything is kept for it if it's too long.
fn checked_length(length: [u8; 4]) -> Result<usize, Error> {
    let length =
        usize::try_from(u32::from_le_bytes(length)).map_err(|_overflow| Error::Oversized)?;
    if length > MAX_MESSAGE {
        return Err(Error::Oversized);
    }
    Ok(length)
}

fn closed_or(error: std::io::Error) -> Error {
    let hung_up = matches!(
        error.kind(),
        std::io::ErrorKind::UnexpectedEof
            | std::io::ErrorKind::BrokenPipe
            | std::io::ErrorKind::ConnectionReset
    );
    if hung_up {
        Error::Closed
    } else {
        Error::Io(error)
    }
}

/// The longest line kept, of a process's output or a window's input: a tool's result can be
/// long, but not without end.
pub(crate) const MAX_LINE: usize = 32 * 1024 * 1024;

/// A byte stream (a process's output) made into lines, as it is read (sans-io). A line longer
/// than [`MAX_LINE`] is dropped, said so in its place.
#[derive(Debug, Default)]
pub(crate) struct LineSplitter {
    buffer: Vec<u8>,
    /// Bytes of a line too long to keep, being passed over up to its end.
    skipping: bool,
}

impl LineSplitter {
    /// The whole lines `bytes` complete.
    pub(crate) fn push(&mut self, bytes: &[u8]) -> Vec<String> {
        let mut lines = Vec::new();
        for piece in bytes.split_inclusive(|&byte| byte == b'\n') {
            let ends = piece.last() == Some(&b'\n');
            if !self.skipping {
                self.buffer.extend_from_slice(piece);
                if self.buffer.len() > MAX_LINE {
                    self.buffer.clear();
                    self.skipping = true;
                }
            }
            if ends {
                if self.skipping {
                    self.skipping = false;
                    lines.push(format!("[a line over {MAX_LINE} bytes, left out]"));
                } else {
                    let line = std::mem::take(&mut self.buffer);
                    lines.push(
                        String::from_utf8_lossy(&line)
                            .trim_end_matches('\n')
                            .to_owned(),
                    );
                }
            }
        }
        lines
    }

    /// What is left at the end of the stream, as a last line.
    pub(crate) fn finish(&mut self) -> Option<String> {
        let rest = std::mem::take(&mut self.buffer);
        (!rest.is_empty()).then(|| String::from_utf8_lossy(&rest).into_owned())
    }
}

/// Lines put together again from the pieces they were sent in (sans-io), up to [`MAX_LINE`].
#[derive(Debug)]
pub(crate) struct Assembler<K: Eq + std::hash::Hash> {
    partial: std::collections::HashMap<K, String>,
}

impl<K: Eq + std::hash::Hash> Default for Assembler<K> {
    fn default() -> Self {
        Self {
            partial: std::collections::HashMap::new(),
        }
    }
}

impl<K: Eq + std::hash::Hash> Assembler<K> {
    /// The whole line, once its last piece came; an error for one too long.
    pub(crate) fn push(
        &mut self,
        key: K,
        piece: &str,
        last: bool,
    ) -> Result<Option<String>, Error> {
        let line = self.partial.entry(key).or_default();
        if line.len() + piece.len() > MAX_LINE {
            line.clear();
            return Err(Error::Oversized);
        }
        line.push_str(piece);
        Ok(last.then(|| std::mem::take(line)))
    }

    /// Forgets the pieces of a session that ended.
    pub(crate) fn forget(&mut self, key: &K) {
        self.partial.remove(key);
    }
}

/// Text cut into pieces that each fit a message, on character boundaries.
pub(crate) fn chunks(text: &str) -> impl Iterator<Item = &str> {
    let mut rest = text;
    let mut first = true;
    std::iter::from_fn(move || {
        if rest.is_empty() && !std::mem::take(&mut first) {
            return None;
        }
        first = false;
        let mut end = rest.len().min(MAX_CHUNK);
        while !rest.is_char_boundary(end) {
            end -= 1;
        }
        let (piece, after) = rest.split_at(end);
        rest = after;
        Some(piece)
    })
}

/// Bumped with any change to the messages below: postcard isn't self-describing, so both ends
/// must agree on every type.
pub(crate) const PROTOCOL: u32 = 1;

/// What the window asks of the daemon. Every field is checked where it is taken.
#[derive(Debug, Serialize, Deserialize)]
pub(crate) enum ToDaemon {
    /// The first message, and only it.
    Hello { protocol: u32 },
    /// Starts Claude Code for a session, known by `key` (this window's) from then on.
    Start { key: u64, launch: Launch },
    /// A piece of a line for the session's input; the last piece ends the line.
    Input { key: u64, text: String, last: bool },
    /// Ends the session's process: its input closes, and what doesn't end is killed.
    End { key: u64 },
    /// Something for the desktop, answered by `Done` with the same `request`.
    Act { request: u64, act: Act },
}

/// What the daemon tells the window.
#[derive(Debug, Serialize, Deserialize)]
pub(crate) enum FromDaemon {
    Hello {
        protocol: u32,
    },
    Started {
        key: u64,
        pid: u32,
        enforcement: Option<Enforcement>,
    },
    Failed {
        key: u64,
        failure: StartFailure,
    },
    /// A piece of a line the process wrote; the last piece ends the line.
    Output {
        key: u64,
        stream: Stream,
        text: String,
        last: bool,
    },
    /// The process ended, with its exit code if it exited on its own.
    Ended {
        key: u64,
        code: Option<i32>,
    },
    Done {
        request: u64,
        result: Result<(), ActFailure>,
    },
    /// The folder picked, if one was.
    Picked {
        request: u64,
        folder: Option<PathBuf>,
    },
}

/// Why a session's process didn't start, or its input was refused, as the window is told.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, thiserror::Error)]
pub(crate) enum StartFailure {
    #[error("{0} isn't a folder to work in")]
    NoFolder(PathBuf),
    #[error("{0:?} isn't a model's name")]
    NoModel(String),
    #[error("{0} isn't a full path")]
    NotAbsolute(PathBuf),
    #[error(
        "couldn't start claude: {0}. Is Claude Code installed and `claude` on PATH? CLAUDE_BIN can name it."
    )]
    Start(String),
    #[error("couldn't set up the sandbox: {0}")]
    Sandbox(String),
    #[error("a session with that key runs already")]
    KeyInUse,
    #[error("a line too long for claude's input")]
    LineTooLong,
    #[error("the daemon failed starting it")]
    Crashed,
}

/// Why the desktop didn't do what it was asked, as the window is told.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, thiserror::Error)]
pub(crate) enum ActFailure {
    #[error("couldn't run {program}: {reason}")]
    Run { program: Program, reason: String },
    #[error("{program} failed: {stderr}")]
    Failed { program: Program, stderr: String },
    #[error("niri's window list doesn't parse: {0}")]
    Windows(String),
    #[error(
        "no window holds process {0}; it may run in the background or in a terminal multiplexer"
    )]
    NoWindow(u32),
    #[error("finding a session's window needs niri (NIRI_SOCKET is unset)")]
    NoNiri,
    #[error("process {0} was asked to stop but is still running")]
    StillRunning(u32),
    #[error("process {0} isn't that Claude Code session (any more)")]
    NotThatSession(u32),
    #[error("{0} isn't a folder")]
    NotAFolder(PathBuf),
    #[error("not opened: {0}\nOnly web links open from here; copy it to open it yourself.")]
    NotAWebLink(String),
    #[error("couldn't open {link}: {reason}")]
    Open { link: String, reason: String },
    #[error("the daemon failed doing it")]
    Crashed,
}

impl From<crate::desktop::Error> for ActFailure {
    fn from(error: crate::desktop::Error) -> Self {
        use crate::desktop::Error;
        match error {
            Error::Run { program, source } => Self::Run {
                program,
                reason: source.to_string(),
            },
            Error::Failed { program, stderr } => Self::Failed { program, stderr },
            Error::Windows(source) => Self::Windows(source.to_string()),
            Error::NoWindow(pid) => Self::NoWindow(pid),
            Error::NoNiri => Self::NoNiri,
            Error::StillRunning(pid) => Self::StillRunning(pid),
            Error::NotThatSession(pid) => Self::NotThatSession(pid),
        }
    }
}

impl From<crate::spawn::Error> for StartFailure {
    fn from(error: crate::spawn::Error) -> Self {
        use crate::spawn::Error;
        match error {
            Error::NoFolder(path) => Self::NoFolder(path),
            Error::NoModel(model) => Self::NoModel(model),
            Error::NotAbsolute(path) => Self::NotAbsolute(path),
            Error::Start(source) => Self::Start(source.to_string()),
            Error::Sandbox(source) => Self::Sandbox(source.to_string()),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub(crate) enum Stream {
    Out,
    Err,
}

/// What the window can have the desktop do; it does nothing else outside its sandbox.
#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum Act {
    OpenIn {
        opener: Opener,
        folder: PathBuf,
    },
    /// A link from the conversation: only web pages and mail are opened.
    OpenUrl(String),
    /// The desktop's folder picker; answered by `Picked`.
    PickFolder,
    Notify {
        summary: String,
        body: String,
    },
    FocusWindowOf {
        pid: u32,
    },
    StopSession(Running),
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use super::*;

    /// A private folder of the test's own, standing in for `$XDG_RUNTIME_DIR/edualc`.
    fn scratch(name: &str) -> Folder {
        let path = std::env::temp_dir().join(format!("edualc-ipc-{name}-{}", std::process::id()));
        std::fs::create_dir_all(&path).expect("a scratch folder");
        let fd = open_folder(rustix::fs::CWD, &path, &path).expect("our own folder");
        Folder { fd, path }
    }

    #[test]
    fn one_daemon_binds_and_a_second_stands_back() {
        let folder = scratch("lock");
        let first = Listener::bind(&folder)
            .expect("binds")
            .expect("is the daemon");
        assert!(
            Listener::bind(&folder).expect("asks").is_none(),
            "the lock is held"
        );
        let mode = std::fs::metadata(folder.path.join(SOCKET)).expect("a socket");
        assert_eq!(mode.permissions().mode() & 0o777, 0o600);
        // The first one's socket is untouched: a client still reaches it.
        Channel::connect(&folder).expect("reaches the first");
        drop(first);
        std::fs::remove_dir_all(&folder.path).expect("the scratch folder goes");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn whole_messages_go_through_and_long_ones_end_it() {
        let folder = scratch("talk");
        let (listener, _lock) = Listener::bind(&folder)
            .expect("binds")
            .expect("is the daemon")
            .into_tokio()
            .expect("for tokio");
        let mut client = Channel::connect(&folder).expect("connects");
        let mut deframer = Deframer::default();
        let (mut reading, _writing) = AsyncChannel::accept(&listener)
            .await
            .expect("a peer of ours")
            .split();
        client.send(&("hello", 1u32)).expect("sent");
        let got: (String, u32) = recv(&mut reading, &mut deframer).await.expect("taken");
        assert_eq!(got, ("hello".to_owned(), 1));
        // Too long for a message: refused on the way out.
        let long = "x".repeat(MAX_MESSAGE);
        assert!(matches!(client.send(&long), Err(Error::Oversized)));
        // Said to be longer than that: refused on the way in, before anything is kept.
        let claimed = u32::try_from(MAX_MESSAGE + 1).expect("fits");
        client
            .stream
            .write_all(&claimed.to_le_bytes())
            .expect("raw");
        assert!(matches!(
            recv::<String>(&mut reading, &mut deframer).await,
            Err(Error::Oversized)
        ));
        // Refused, the connection ends: what is left of it is read no more.
        drop(client);
        assert!(matches!(
            recv::<String>(&mut reading, &mut Deframer::default()).await,
            Err(Error::Closed)
        ));
        std::fs::remove_dir_all(&folder.path).expect("the scratch folder goes");
    }

    #[test]
    fn a_folder_open_to_others_is_closed_and_a_link_refused() {
        let path = std::env::temp_dir().join(format!("edualc-ipc-mode-{}", std::process::id()));
        std::fs::create_dir_all(&path).expect("a folder");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o777)).expect("opened");
        open_folder(rustix::fs::CWD, &path, &path).expect("ours, closed again");
        let mode = std::fs::metadata(&path)
            .expect("there")
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o700);
        let link = path.with_extension("link");
        std::os::unix::fs::symlink(&path, &link).expect("a link");
        assert!(
            open_folder(rustix::fs::CWD, &link, &link).is_err(),
            "not followed"
        );
        std::fs::remove_file(&link).expect("the link goes");
        std::fs::remove_dir_all(&path).expect("the folder goes");
    }

    #[test]
    fn long_text_goes_in_pieces_that_fit() {
        let text = "é".repeat(MAX_CHUNK);
        let pieces: Vec<&str> = chunks(&text).collect();
        assert!(pieces.iter().all(|piece| piece.len() <= MAX_CHUNK));
        assert_eq!(pieces.concat(), text);
        assert_eq!(chunks("").collect::<Vec<_>>(), [""]);
    }
}
