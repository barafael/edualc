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
//! - Each peer is checked as it connects: a window, to be the same user running this same
//!   program; the daemon, which makes itself non-dumpable (its `/proc` entries closed to the
//!   user's other processes), to be the same user, in the place only the daemon binds.
//! - Messages are postcard (compact serde), each COBS-framed by postcard: a frame longer than
//!   one of [`MAX_MESSAGE`] ends the connection once the reader's fixed buffer is full, as does
//!   a message that doesn't parse. Nothing in a message makes the reader hold more than that.

use std::collections::VecDeque;
use std::io::{Read, Write};
use std::os::fd::{AsFd, OwnedFd};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};

use postcard::accumulator::{CobsAccumulator, FeedResult};
use rustix::fs::{AtFlags, FlockOperation, Mode, OFlags};
use rustix::io::Errno;
use rustix::net::{AddressFamily, SocketAddrUnix, SocketFlags, SocketType};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::buses::Buses;
use crate::claude::Launch;
use crate::decode::{Picture, Refusal};
use crate::desktop::{Opener, Program, Running};
use crate::external::wire::SessionId;
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
    /// postcard's accumulator says no more than that.
    #[error("a message that doesn't parse")]
    Unreadable,
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
    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    /// `$XDG_RUNTIME_DIR/edualc/protocol-<PROTOCOL>`, made if it isn't there, and checked to be
    /// the user's alone. Each protocol has a folder of its own, so a daemon of an earlier build
    /// runs on beside this build's until its sessions end, instead of keeping its windows out.
    pub(crate) fn open() -> Result<Self, Error> {
        let runtime =
            PathBuf::from(std::env::var_os("XDG_RUNTIME_DIR").ok_or(Error::NoRuntimeDir)?);
        let runtime_fd = open_folder(rustix::fs::CWD, &runtime, &runtime)?;
        let edualc = runtime.join(FOLDER);
        let edualc_fd = made_folder(&runtime_fd, FOLDER, &edualc)?;
        let name = format!("protocol-{PROTOCOL}");
        let path = edualc.join(&name);
        let fd = made_folder(&edualc_fd, &name, &path)?;
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

/// The folder `name` in `at` (found at `path`), made if it isn't there, then opened as
/// [`open_folder`] opens one.
fn made_folder(at: impl AsFd, name: &str, path: &Path) -> Result<OwnedFd, Error> {
    match rustix::fs::mkdirat(&at, name, Mode::RWXU) {
        Ok(()) | Err(Errno::EXIST) => {}
        Err(source) => {
            return Err(Error::Open {
                path: path.to_owned(),
                source,
            });
        }
    }
    open_folder(at, Path::new(name), path)
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

/// The daemon a window reached is this user's. Its program isn't compared: the daemon makes
/// itself non-dumpable, which closes its `/proc` entries to the window; that it is the daemon
/// the socket's place says, a folder of the user's alone that only the lock's holder binds in.
fn check_daemon(fd: impl AsFd) -> Result<(), Error> {
    let (uid, pid) = peer(fd)?;
    if uid == rustix::process::getuid().as_raw() {
        Ok(())
    } else {
        Err(Error::Stranger { uid, pid })
    }
}

/// A window is this user's, running this program: a build that replaced the daemon's own on
/// disk counts too, and the hello's protocol says whether the two agree.
fn check_window(fd: impl AsFd) -> Result<(), Error> {
    let (uid, pid) = peer(fd)?;
    let stranger = Error::Stranger { uid, pid };
    if uid != rustix::process::getuid().as_raw() {
        return Err(stranger);
    }
    let theirs = std::fs::read_link(format!("/proc/{pid}/exe"));
    let ours = std::fs::read_link("/proc/self/exe");
    match (theirs, ours) {
        (Ok(theirs), Ok(ours)) if same_program(&theirs, &ours) => Ok(()),
        _ => Err(stranger),
    }
}

fn peer(fd: impl AsFd) -> Result<(u32, i32), Error> {
    let peer = rustix::net::sockopt::socket_peercred(fd).map_err(Error::Socket)?;
    Ok((peer.uid.as_raw(), peer.pid.as_raw_nonzero().get()))
}

/// Two programs' paths name the same program, the kernel's " (deleted)" aside: it names a
/// program so once a new build replaced it on disk.
fn same_program(theirs: &Path, ours: &Path) -> bool {
    use std::os::unix::ffi::OsStrExt;
    let path = |program: &Path| {
        let bytes = program.as_os_str().as_bytes();
        bytes.strip_suffix(b" (deleted)").unwrap_or(bytes).to_vec()
    };
    path(theirs) == path(ours)
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
        check_daemon(&fd)?;
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
        deframer: &mut Deframer<T>,
    ) -> Result<T, Error> {
        let mut bytes = [0u8; READ];
        loop {
            if let Some(message) = deframer.next() {
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
        check_window(&stream)?;
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
    deframer: &mut Deframer<T>,
) -> Result<T, Error> {
    let mut bytes = [0u8; READ];
    loop {
        if let Some(message) = deframer.next() {
            return Ok(message);
        }
        let read = reading.read(&mut bytes).await.map_err(closed_or)?;
        deframer.push(bytes.get(..read).ok_or(Error::Closed)?)?;
    }
}

/// What is read from a connection at once.
const READ: usize = 16 * 1024;

/// The largest frame: a message COBS-encoded, at most one byte more per 254, and its `0`.
const MAX_FRAME: usize = MAX_MESSAGE + MAX_MESSAGE / 254 + 2;

/// A connection's bytes, as they come, made into messages (sans-io: the window's blocking reads
/// and the daemon's tokio reads feed the same one). Frames are postcard's COBS, each ended by a
/// `0`, taken apart by postcard's own accumulator, which never holds more than [`MAX_FRAME`]: a
/// longer frame, or one that doesn't parse, ends the connection (the accumulator would go on
/// with the next frame, as a lossy serial line needs; a peer here sending one is broken or
/// hostile).
pub(crate) struct Deframer<Message> {
    accumulator: CobsAccumulator<MAX_FRAME>,
    /// Whole messages a read brought, not yet taken.
    ready: VecDeque<Message>,
}

impl<Message> Default for Deframer<Message> {
    fn default() -> Self {
        Self {
            accumulator: CobsAccumulator::new(),
            ready: VecDeque::new(),
        }
    }
}

impl<Message> std::fmt::Debug for Deframer<Message> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Deframer")
            .field("ready", &self.ready.len())
            .finish_non_exhaustive()
    }
}

impl<Message: DeserializeOwned> Deframer<Message> {
    /// Bytes read; an empty read is the other side hanging up.
    pub(crate) fn push(&mut self, bytes: &[u8]) -> Result<(), Error> {
        if bytes.is_empty() {
            return Err(Error::Closed);
        }
        let mut rest = bytes;
        while !rest.is_empty() {
            rest = match self.accumulator.feed::<Message>(rest) {
                FeedResult::Consumed => break,
                FeedResult::Success { data, remaining } => {
                    self.ready.push_back(data);
                    remaining
                }
                FeedResult::OverFull(_) => return Err(Error::Oversized),
                FeedResult::DeserError(_) => return Err(Error::Unreadable),
            };
        }
        Ok(())
    }

    /// The next whole message, if one came.
    pub(crate) fn next(&mut self) -> Option<Message> {
        self.ready.pop_front()
    }
}

/// A message as a frame: postcard's COBS, ended by a `0`.
pub(crate) fn frame(message: &impl Serialize) -> Result<Vec<u8>, Error> {
    let frame = postcard::to_stdvec_cobs(message).map_err(Error::Encode)?;
    if frame.len() > MAX_FRAME {
        return Err(Error::Oversized);
    }
    Ok(frame)
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

/// Bumped with any change to the messages below, or to their framing: postcard isn't
/// self-describing, so both ends must agree on every type.
pub(crate) const PROTOCOL: u32 = 11;

/// A window's key for a process it shows, new for each one: what an earlier one still says is
/// dropped.
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
)]
pub(crate) struct Generation(u64);

impl Generation {
    pub(crate) fn next(self) -> Self {
        Self(self.0 + 1)
    }
}

/// The daemon's id for a session it runs, which a window attaches to.
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
)]
pub(crate) struct HostedId(u64);

impl HostedId {
    pub(crate) fn next(self) -> Self {
        Self(self.0 + 1)
    }
}

/// A window's id for an [`Act`] it asked for, which the daemon's answer carries.
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
)]
pub(crate) struct ActId(u64);

impl ActId {
    pub(crate) fn next(self) -> Self {
        Self(self.0 + 1)
    }
}

/// What the window asks of the daemon. Every field is checked where it is taken.
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) enum ToDaemon {
    /// The first message, and only it.
    Hello { protocol: u32 },
    /// Starts Claude Code for a session, known by `key` from then on.
    Start { key: Generation, launch: Launch },
    /// A piece of a line for the session's input; the last piece ends the line.
    Input {
        key: Generation,
        text: String,
        last: bool,
    },
    /// Ends the session's process: its input closes, and what doesn't end is killed.
    End { key: Generation },
    /// Shows a session the daemon runs in this window, known by `key` here from then on: what
    /// it asks and wasn't answered comes again, then what it says.
    Attach { key: Generation, hosted: HostedId },
    /// Stops showing the session, which runs on (the window closes).
    Detach { key: Generation },
    /// Claude Code's id for the session, as its process told the window: for the list.
    Identify { key: Generation, session: SessionId },
    /// Something for the desktop, answered by `Done` with the same `request`.
    Act { request: ActId, act: Act },
}

/// What the daemon tells the window.
#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) enum FromDaemon {
    /// With the D-Bus the window is to use (see `buses`). `Hosted` follows, before anything
    /// else.
    Hello { protocol: u32, buses: Buses },
    /// The process runs: started for the window, or running already as it attaches.
    Started {
        key: Generation,
        pid: u32,
        enforcement: Option<Enforcement>,
        /// Turns under way: prompts written and not yet ended by a result (none for a process
        /// just started).
        turns: usize,
    },
    Failed {
        key: Generation,
        failure: StartFailure,
    },
    /// A piece of a line the process wrote; the last piece ends the line.
    Output {
        key: Generation,
        stream: Stream,
        text: String,
        last: bool,
    },
    /// The process ended, with its exit code if it exited on its own.
    Ended { key: Generation, code: Option<i32> },
    /// The session's transcript went two ways in and out of the sandbox, found as it was
    /// resumed or as its transcript was copied back: `left` is the copy it doesn't go on from,
    /// with what only that copy has.
    Diverged { key: Generation, left: PathBuf },
    Done {
        request: ActId,
        result: Result<(), ActFailure>,
    },
    /// The folder picked, if one was.
    Picked {
        request: ActId,
        folder: Option<PathBuf>,
    },
    /// Whether the terminal's trust dialog was accepted for the folder asked about; if not, what
    /// the folder's own configuration would have Claude Code run there.
    Trusted {
        request: ActId,
        accepted: bool,
        runs: Vec<Runs>,
    },
    /// The sessions the daemon runs, anew whenever that changes.
    Hosted(Vec<Hosted>),
    /// The clipboard's image, or why there is none: a [`Pasted`], postcard-encoded, in pieces
    /// that fit messages; the last piece ends it.
    Pasted {
        request: ActId,
        piece: Vec<u8>,
        last: bool,
    },
}

/// The clipboard's image, decoded in the confined helper (see `decode`), or why there is none.
pub(crate) type Pasted = Result<Picture, PasteFailure>;

/// Something a folder's own configuration (`.claude/settings.json`, `settings.local.json`,
/// `.mcp.json`) would have Claude Code run or set there, read before the folder is trusted, for
/// the user to decide on. Plain text, cut short, without control characters (see `trust`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) enum Runs {
    /// A command run on one of Claude Code's events, for the tools `matcher` names (all, if
    /// empty).
    Hook {
        event: String,
        matcher: String,
        command: String,
    },
    /// An MCP server: the command line started, or the URL reached.
    McpServer { name: String, runs: String },
    /// A command whose output is used as the API key.
    ApiKeyHelper(String),
    /// A command run to draw the status line.
    StatusLine(String),
    /// A plugin turned on, which brings hooks and MCP servers of its own.
    Plugin(String),
    /// Environment variables set, by name; their values aren't shown.
    Environment(Vec<String>),
    /// A file that couldn't be read, and why: what it would run is unknown.
    Unreadable { file: String, reason: String },
}

/// Why the clipboard gave no image, as the window is told.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, thiserror::Error)]
pub(crate) enum PasteFailure {
    /// The clipboard holds something else (text, say): nothing to tell.
    #[error("the clipboard holds no PNG, JPEG, GIF or WebP image")]
    NoImage,
    #[error("couldn't read the clipboard: {0}")]
    Clipboard(String),
    #[error("the image wasn't taken: {0}")]
    Refused(Refusal),
    #[error("{0}")]
    Decoder(String),
}

/// A session the daemon runs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Hosted {
    pub(crate) id: HostedId,
    /// Claude Code's, once known.
    pub(crate) session: Option<SessionId>,
    pub(crate) cwd: PathBuf,
    /// Shown in a window now.
    pub(crate) attached: bool,
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
    #[error("that session isn't run here, or is shown in another window")]
    NotHosted,
    #[error("the daemon failed starting it")]
    Crashed,
}

/// Why the desktop didn't do what it was asked, as the window is told.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, thiserror::Error)]
#[serde(deny_unknown_fields)]
pub(crate) enum ActFailure {
    #[error("couldn't run {program}: {reason}")]
    Run { program: Program, reason: String },
    #[error("{program} failed: {stderr}")]
    Failed { program: Program, stderr: String },
    #[error("{0}")]
    Niri(String),
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
        use crate::external::niri;
        match error {
            Error::Run { program, source } => Self::Run {
                program,
                reason: source.to_string(),
            },
            Error::Failed { program, stderr } => Self::Failed { program, stderr },
            Error::Niri(niri::Error::NotRunning) => Self::NoNiri,
            Error::Niri(
                error @ (niri::Error::Socket(_)
                | niri::Error::Request(_)
                | niri::Error::Answer(_)
                | niri::Error::Refused(_)),
            ) => Self::Niri(error.to_string()),
            Error::NoWindow(pid) => Self::NoWindow(pid),
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
            error @ (Error::Sandbox(_)
            | Error::Home(_)
            | Error::SignIn(_)
            | Error::SignInPipeShared(_)) => Self::Sandbox(error.to_string()),
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
#[serde(deny_unknown_fields)]
pub(crate) enum Act {
    OpenIn {
        opener: Opener,
        folder: PathBuf,
    },
    /// A link from the conversation: only web pages and mail are opened.
    OpenUrl(String),
    /// The desktop's folder picker; answered by `Picked`.
    PickFolder,
    /// The clipboard's image, if it holds one; answered by `Pasted`.
    PasteImage,
    /// Whether the terminal's trust dialog was accepted for the folder, or one it is in, which
    /// the window can't read (see `trust`); answered by `Trusted`.
    CheckTrust {
        folder: PathBuf,
    },
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

    /// A new build replacing the daemon's program on disk is still this program; another
    /// program, or another place, isn't.
    #[test]
    fn a_rebuilt_program_is_the_same_program() {
        let ours = Path::new("/home/someone/edualc/target/release/edualc");
        let replaced = Path::new("/home/someone/edualc/target/release/edualc (deleted)");
        assert!(same_program(ours, ours));
        assert!(same_program(ours, replaced));
        assert!(same_program(replaced, ours));
        assert!(!same_program(Path::new("/usr/bin/python3"), ours));
        assert!(!same_program(
            Path::new("/home/someone/edualc/target/debug/edualc"),
            ours
        ));
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

    /// Bytes cut where `cuts` say (offsets into them, in any order), as reads might bring them.
    fn reads<'bytes>(bytes: &'bytes [u8], cuts: &[usize]) -> Vec<&'bytes [u8]> {
        let mut offsets: Vec<usize> = cuts.iter().map(|cut| cut % (bytes.len() + 1)).collect();
        offsets.extend([0, bytes.len()]);
        offsets.sort_unstable();
        offsets.dedup();
        offsets
            .windows(2)
            .filter_map(|pair| bytes.get(pair.first().copied()?..pair.last().copied()?))
            .collect()
    }

    /// Feeds `bytes`, as cut, to a deframer of `Message`: the messages taken, and the error that
    /// ended it, if one did.
    fn fed<Message: DeserializeOwned>(
        bytes: &[u8],
        cuts: &[usize],
    ) -> (Vec<Message>, Option<Error>) {
        let mut deframer = Deframer::<Message>::default();
        let mut taken = Vec::new();
        for read in reads(bytes, cuts) {
            let pushed = deframer.push(read);
            taken.extend(std::iter::from_fn(|| deframer.next()));
            if let Err(error) = pushed {
                return (taken, Some(error));
            }
        }
        (taken, None)
    }

    proptest::proptest! {
        /// Whatever the reads, the messages sent come out whole, in order, and nothing else.
        #[test]
        fn frames_come_out_as_sent_however_reads_cut_them(
            sent in proptest::collection::vec((".{0,300}", proptest::num::u32::ANY), 0..8),
            cuts in proptest::collection::vec(proptest::num::usize::ANY, 0..40),
        ) {
            let mut bytes = Vec::new();
            for message in &sent {
                bytes.extend(frame(message).expect("framed"));
            }
            let (taken, error) = fed::<(String, u32)>(&bytes, &cuts);
            proptest::prop_assert!(error.is_none(), "{error:?}");
            proptest::prop_assert_eq!(taken, sent);
        }

        /// Whatever a peer sends, as either side's messages, it is taken or refused: never a
        /// panic, never more messages than frames.
        #[test]
        fn any_bytes_are_taken_or_refused(
            bytes in proptest::collection::vec(proptest::num::u8::ANY, 0..4096),
            cuts in proptest::collection::vec(proptest::num::usize::ANY, 0..40),
        ) {
            let frames = bytes.iter().filter(|&&byte| byte == 0).count();
            let (to_daemon, error) = fed::<ToDaemon>(&bytes, &cuts);
            proptest::prop_assert!(to_daemon.len() <= frames);
            proptest::prop_assert!(matches!(error, None | Some(Error::Oversized | Error::Unreadable)));
            let (from_daemon, error) = fed::<FromDaemon>(&bytes, &cuts);
            proptest::prop_assert!(from_daemon.len() <= frames);
            proptest::prop_assert!(matches!(error, None | Some(Error::Oversized | Error::Unreadable)));
        }

        /// A frame longer than any message is refused however it comes, unended or not, and
        /// whatever came before it.
        #[test]
        fn a_frame_longer_than_any_message_is_refused(
            filler in 1u8..=u8::MAX,
            extra in 1..MAX_FRAME,
            ended in proptest::bool::ANY,
            cuts in proptest::collection::vec(proptest::num::usize::ANY, 0..40),
        ) {
            let mut bytes = frame(&("before", 1u32)).expect("framed");
            bytes.extend(std::iter::repeat_n(filler, MAX_FRAME + extra));
            if ended {
                bytes.push(0);
            }
            let (taken, error) = fed::<(String, u32)>(&bytes, &cuts);
            proptest::prop_assert_eq!(taken, [("before".to_owned(), 1)]);
            proptest::prop_assert!(matches!(error, Some(Error::Oversized)), "{error:?}");
        }
    }

    /// Frames come together however reads split them; one that doesn't parse is refused.
    #[test]
    fn frames_are_taken_byte_by_byte_and_junk_refused() {
        let mut deframer = Deframer::<(String, u32)>::default();
        let mut bytes = frame(&("one", 1u32)).expect("framed");
        bytes.extend(frame(&("two", 2u32)).expect("framed"));
        let mut taken = Vec::new();
        for byte in &bytes {
            deframer.push(std::slice::from_ref(byte)).expect("fed");
            taken.extend(deframer.next());
        }
        assert_eq!(taken, [("one".to_owned(), 1), ("two".to_owned(), 2)]);
        assert!(deframer.next().is_none());
        // A frame ended by its 0, but not a message of this kind.
        assert!(matches!(
            Deframer::<(String, u32)>::default().push(&[2, 0xff, 0]),
            Err(Error::Unreadable)
        ));
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
        client.send(&("again", 2u32)).expect("sent");
        let got: (String, u32) = recv(&mut reading, &mut deframer).await.expect("taken");
        assert_eq!(got, ("hello".to_owned(), 1));
        let got: (String, u32) = recv(&mut reading, &mut deframer).await.expect("taken");
        assert_eq!(
            got,
            ("again".to_owned(), 2),
            "two frames in one read, in order"
        );
        // Too long for a message: refused on the way out.
        let long = "x".repeat(MAX_MESSAGE);
        assert!(matches!(client.send(&long), Err(Error::Oversized)));
        // A frame that doesn't end in time: refused on the way in, holding no more than one.
        client
            .stream
            .write_all(&vec![1u8; MAX_FRAME + 1])
            .expect("raw");
        let mut taking = Deframer::<String>::default();
        assert!(matches!(
            recv::<String>(&mut reading, &mut taking).await,
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
