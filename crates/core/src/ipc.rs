//! The windows and edualcd: what they say to each other, and the line it goes on.
//!
//! - A window (the native one, or a page) reaches edualcd over the network, at `/api/line`,
//!   the same way whether edualcd runs on this machine or another: WebSocket over TLS (`ws`,
//!   `tls`), each message one binary frame of CBOR, [`ToDaemon`] one way and [`FromDaemon`]
//!   the other. A frame longer than [`MAX_MESSAGE`] ends the line, as does a message that
//!   doesn't parse.
//! - edualcd's certificate is its own, made on its first start; a native window trusts exactly
//!   the one it was paired with, by its SHA-256 (`pairing`). A window is let in by edualcd's
//!   token, which lets it run Claude Code as the user, outside the sandbox too: it is kept as an
//!   SSH key is.
//! - The protocol's version goes in the line's address too, so that an edualcd of another
//!   version refuses a window before either reads the other's messages.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

#[cfg(not(target_arch = "wasm32"))]
pub mod pairing;
#[cfg(not(target_arch = "wasm32"))]
pub mod servers;
#[cfg(not(target_arch = "wasm32"))]
pub mod tls;
#[cfg(not(target_arch = "wasm32"))]
pub mod ws;
#[cfg(not(target_arch = "wasm32"))]
pub use ws::Error;

/// Where edualcd takes the token windows must show, if not from the file it keeps it in.
pub const TOKEN_VARIABLE: &str = "EDUALC_TOKEN";

use crate::claude::Launch;
use crate::desktop::{Program, Running};
use crate::external::wire::{SessionId, TaskId};
use crate::sandbox::Enforcement;
use crate::sessions::{LiveSession, ProjectInfo, Saved, SessionInfo, Worktree};
use crate::settings::Settings;

/// The largest message either side sends: more is a broken or hostile peer. Room for the
/// longest line kept, and what the message says besides.
pub const MAX_MESSAGE: usize = MAX_LINE + 1024 * 1024;
/// The longest line kept, of a process's output or a window's input: a tool's result can be
/// long, but not without end.
pub const MAX_LINE: usize = 32 * 1024 * 1024;

/// A byte stream (a process's output) made into lines, as it is read (sans-io). A line longer
/// than [`MAX_LINE`] is dropped, said so in its place.
#[cfg(not(target_arch = "wasm32"))]
#[derive(Debug, Default)]
pub struct LineSplitter {
    buffer: Vec<u8>,
    /// Bytes of a line too long to keep, being passed over up to its end.
    skipping: bool,
}

#[cfg(not(target_arch = "wasm32"))]
impl LineSplitter {
    /// The whole lines `bytes` complete.
    pub fn push(&mut self, bytes: &[u8]) -> Vec<String> {
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
    pub fn finish(&mut self) -> Option<String> {
        let rest = std::mem::take(&mut self.buffer);
        (!rest.is_empty()).then(|| String::from_utf8_lossy(&rest).into_owned())
    }
}

/// A message as it goes on the line: CBOR.
pub fn encode(message: &impl Serialize) -> Result<Vec<u8>, EncodeError> {
    let mut bytes = Vec::new();
    ciborium::into_writer(message, &mut bytes).map_err(|error| EncodeError(error.to_string()))?;
    Ok(bytes)
}

/// A message as it came on the line.
pub fn decode<T: serde::de::DeserializeOwned>(bytes: &[u8]) -> Result<T, DecodeError> {
    ciborium::from_reader(bytes).map_err(|error| DecodeError(error.to_string()))
}

#[derive(Debug, thiserror::Error)]
#[error("couldn't encode a message: {0}")]
pub struct EncodeError(String);

#[derive(Debug, thiserror::Error)]
#[error("a message that doesn't parse: {0}")]
pub struct DecodeError(String);

/// Bumped with any change to the messages below, or to their framing: both ends must agree on
/// every type, which are checked strictly (unknown fields refused).
pub const PROTOCOL: u32 = 16;

/// A window's key for a process it shows, new for each one: what an earlier one still says is
/// dropped.
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
)]
pub struct Generation(u64);

impl Generation {
    pub fn next(self) -> Self {
        Self(self.0 + 1)
    }
}

/// The daemon's id for a session it runs, which a window attaches to.
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
)]
pub struct HostedId(u64);

impl HostedId {
    pub fn next(self) -> Self {
        Self(self.0 + 1)
    }
}

/// A window's id for an [`Act`] it asked for, which the daemon's answer carries.
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
)]
pub struct ActId(u64);

impl ActId {
    pub fn next(self) -> Self {
        Self(self.0 + 1)
    }
}

/// What the window asks of the daemon. Every field is checked where it is taken.
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub enum ToDaemon {
    /// The first message, and only it: the protocol the window speaks, and which window it is,
    /// the same each time its line is reached again.
    Hello {
        protocol: u32,
        window: WindowKey,
    },
    /// Starts Claude Code for a session, known by `key` from then on.
    Start {
        key: Generation,
        launch: Launch,
    },
    /// A line for the session's input, up to [`MAX_LINE`].
    Input {
        key: Generation,
        line: String,
    },
    /// Ends the session's process: its input closes, and what doesn't end is killed.
    End {
        key: Generation,
    },
    /// Shows a session the daemon runs in this window, known by `key` here from then on: what
    /// it asks and wasn't answered comes again, then what it says.
    Attach {
        key: Generation,
        hosted: HostedId,
    },
    /// Stops showing the session, which runs on (the window closes).
    Detach {
        key: Generation,
    },
    /// Claude Code's id for the session, as its process told the window: for the list.
    Identify {
        key: Generation,
        session: SessionId,
    },
    /// Something for the desktop, answered by `Done` with the same `request`.
    Act {
        request: ActId,
        act: Act,
    },
    /// Something of Claude Code's or edualc's files, which a window doesn't read itself (a
    /// page can't, and the native window needn't), answered by `Answer` with the same
    /// `request`; a transcript's lines come before, as `Lines`.
    Read {
        request: ActId,
        read: Read,
    },
    /// Keeps the settings (`~/.config/edualc/settings.json`).
    SaveSettings(Settings),
    /// Tells the window, as `Changed`, when what `target` names changes, until `Unwatch`.
    Watch {
        watched: Watched,
        target: Target,
    },
    Unwatch(Watched),
}

/// A window, as it names itself for as long as it is open: reaching edualcd again after its line
/// was lost, it is known as the window the lost line was, whose sessions are let go of at once
/// for it to take back (that line may not be found dead for a while).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct WindowKey(uuid::Uuid);

impl WindowKey {
    pub fn new() -> Self {
        Self(uuid::Uuid::new_v4())
    }
}

impl Default for WindowKey {
    fn default() -> Self {
        Self::new()
    }
}

/// A watch, named by the window that asked for it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Watched(uuid::Uuid);

impl Watched {
    pub fn new() -> Self {
        Self(uuid::Uuid::new_v4())
    }
}

impl Default for Watched {
    fn default() -> Self {
        Self::new()
    }
}

/// What a window watches.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub enum Target {
    /// The records of the sessions running (`~/.claude/sessions`).
    Records,
    /// A transcript being written to; while it isn't there yet, the folders it will be in.
    Transcript(Place),
}

/// Where a transcript is, as a window knows it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub enum Place {
    /// Found already.
    Path(PathBuf),
    /// A session's, wherever it is: a worktree it entered may have moved it.
    Session { cwd: PathBuf, id: SessionId },
    /// A subagent's, beside its session's in the home its process writes in.
    Subagent {
        sandboxed: bool,
        cwd: PathBuf,
        id: SessionId,
        task: TaskId,
    },
}

/// What a window reads through the daemon.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub enum Read {
    /// The sessions running now: `Answer::Live`.
    Live,
    /// The folders Claude Code worked in: `Answer::Projects`.
    Projects,
    /// A folder's sessions, newest first, those `known` summed up again only if their
    /// transcript changed: `Answer::Sessions`.
    Sessions {
        cwd: PathBuf,
        limit: usize,
        known: Vec<SessionInfo>,
    },
    /// A transcript's whole lines from `from` on: `Lines`, then `Answer::Transcript`.
    Transcript { place: Place, from: u64 },
    /// The worktree a session's transcript says it entered: `Answer::Worktree`.
    WorktreeOf { cwd: PathBuf, id: SessionId },
    /// `cwd` as a git worktree, if it is a linked one: `Answer::Worktree`.
    LinkedWorktree { cwd: PathBuf },
    /// The folder a path typed names, made whole, `~` at its start the home folder:
    /// `Answer::Folder`.
    TypedFolder(String),
    /// The folder at a path (the picker's, a worktree's, a command line's), made whole, if it
    /// is one: `Answer::Folder`.
    Folder(PathBuf),
    /// Where a session to continue was started, as its transcript says: `Answer::Saved`.
    Saved(SessionId),
}

/// What the daemon read for a window.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub enum Answer {
    Live(Vec<LiveSession>),
    Projects(Vec<ProjectInfo>),
    Sessions(Vec<SessionInfo>),
    /// The end of a transcript's lines: where it was found, and the offset after its last
    /// whole line, or why it couldn't be read.
    Transcript {
        path: PathBuf,
        read: Result<u64, String>,
    },
    Worktree(Option<Worktree>),
    Folder(Result<PathBuf, String>),
    Saved(Saved),
}

/// What the daemon tells the window.
#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub enum FromDaemon {
    /// The settings (or why they can't be read), and the sessions running now. `Hosted`
    /// follows, before anything else.
    Hello {
        protocol: u32,
        settings: Result<Settings, String>,
        live: Vec<LiveSession>,
        /// Claude Code's own sandbox is on in its settings: edualc's stays off.
        claude_codes_sandbox: bool,
        /// The user's home folder, where a page opens unless it names a folder.
        home: Option<PathBuf>,
    },
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
    /// A line the process wrote.
    Output {
        key: Generation,
        stream: Stream,
        line: String,
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
    /// Whether the terminal's trust dialog was accepted for the folder asked about; if not, what
    /// the folder's own configuration would have Claude Code run there.
    Trusted {
        request: ActId,
        accepted: bool,
        runs: Vec<Runs>,
    },
    /// The sessions the daemon runs, anew whenever that changes.
    Hosted(Vec<Hosted>),
    /// What was read, as asked by `Read`.
    Answer { request: ActId, answer: Answer },
    /// Whole lines of a transcript read, as asked by `Read::Transcript`, in pieces that fit a
    /// message; its `Answer` follows the last.
    Lines { request: ActId, lines: Vec<String> },
    /// What a watch names changed.
    Changed(Watched),
}

/// Something a folder's own configuration (`.claude/settings.json`, `settings.local.json`,
/// `.mcp.json`) would have Claude Code run or set there, read before the folder is trusted, for
/// the user to decide on. Plain text, cut short, without control characters (see `trust`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub enum Runs {
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

/// A session the daemon runs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Hosted {
    pub id: HostedId,
    /// Claude Code's, once known.
    pub session: Option<SessionId>,
    pub cwd: PathBuf,
    /// Shown in a window now.
    pub attached: bool,
}

/// Why a session's process didn't start, or its input was refused, as the window is told.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, thiserror::Error)]
pub enum StartFailure {
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
pub enum ActFailure {
    #[error("couldn't run {program}: {reason}")]
    Run { program: Program, reason: String },
    #[error("{program} failed: {stderr}")]
    Failed { program: Program, stderr: String },
    #[error("process {0} was asked to stop but is still running")]
    StillRunning(u32),
    #[error("process {0} isn't that Claude Code session (any more)")]
    NotThatSession(u32),
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
            // Told as a failure, which every window reads, not as a reason of its own.
            timed_out @ Error::TimedOut { program } => Self::Failed {
                program,
                stderr: timed_out.to_string(),
            },
            Error::StillRunning(pid) => Self::StillRunning(pid),
            Error::NotThatSession(pid) => Self::NotThatSession(pid),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Stream {
    Out,
    Err,
}

/// How far the sandbox lets Claude into a folder the user added (`Settings::sandbox_read`,
/// `Settings::sandbox_write`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SandboxAccess {
    Read,
    Write,
}

/// What the window can have edualcd do on its machine, besides running sessions; the window's
/// own desktop (links, notifications, the clipboard) is the window's to use.
#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub enum Act {
    /// Whether the terminal's trust dialog was accepted for the folder, or one it is in, which
    /// the window can't read (see `trust`); answered by `Trusted`.
    CheckTrust { folder: PathBuf },
    /// Stops a session running elsewhere on edualcd's machine, to continue it in the window.
    StopSession(Running),
}
