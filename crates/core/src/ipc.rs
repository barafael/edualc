//! The window and its daemon (`edualcd`), speaking over a Unix socket that only they reach.
//!
//! - The socket lives in `$XDG_RUNTIME_DIR/edualc`, a folder of the user's alone (0700), which is
//!   checked through the descriptor it was opened by, never followed through a link, and used by
//!   that descriptor from then on: binding and connecting go through `/proc/self/fd/<folder>`, so
//!   no path can be swapped in between. Never an abstract socket, which anyone may take first.
//! - Who is the daemon is decided by a lock held as long as it runs: only its holder removes a
//!   stale socket and binds anew, so two starting at once can't both bind, nor one unbind the
//!   other's.
//! - Each peer is checked as it connects: a window, to be the same user running a program
//!   installed beside the daemon (`edualc`); the daemon, which makes itself non-dumpable (its `/proc` entries closed to the
//!   user's other processes), to be the same user, in the place only the daemon binds.
//! - Messages are postcard (compact serde), each COBS-framed by postcard: a frame longer than
//!   one of [`MAX_MESSAGE`] ends the connection once the reader's fixed buffer is full, as does
//!   a message that doesn't parse. Nothing in a message makes the reader hold more than that.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

mod frame;
pub use frame::{Deframer, Error, MAX_FRAME, deframe, frame};
#[cfg(not(target_arch = "wasm32"))]
mod socket;
#[cfg(not(target_arch = "wasm32"))]
pub use socket::{AsyncChannel, Channel, Folder, Listener, Writing, recv, send};

pub mod pairing;
#[cfg(not(target_arch = "wasm32"))]
pub mod tls;
mod token;
pub use token::{Auth, Token};

/// Where edualcd takes the token windows must show, if not from the file it keeps it in.
pub const TOKEN_VARIABLE: &str = "EDUALC_TOKEN";

use crate::claude::Launch;
use crate::decode::{Picture, Refusal};
use crate::desktop::{Opener, Program, Running};
use crate::external::wire::{SessionId, TaskId};
use crate::sessions::{LiveSession, ProjectInfo, SessionInfo, Worktree};
use crate::settings::Settings;

/// The largest message either side sends: more is a broken or hostile peer. Text is cut to fit
/// (see [`chunks`]); what the daemon reads for a window (the sessions listed, the folders
/// worked in) is one message, which a thousand sessions fill well past 64 KiB.
pub const MAX_MESSAGE: usize = 1024 * 1024;
/// Text sent in one message at most (a long line of Claude's output goes in several), with
/// room left for what the message says besides.
pub const MAX_CHUNK: usize = MAX_MESSAGE - 1024;
/// The longest line kept, of a process's output or a window's input: a tool's result can be
/// long, but not without end.
pub const MAX_LINE: usize = 32 * 1024 * 1024;

/// A byte stream (a process's output) made into lines, as it is read (sans-io). A line longer
/// than [`MAX_LINE`] is dropped, said so in its place.
#[derive(Debug, Default)]
pub struct LineSplitter {
    buffer: Vec<u8>,
    /// Bytes of a line too long to keep, being passed over up to its end.
    skipping: bool,
}

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

/// Lines put together again from the pieces they were sent in (sans-io), up to [`MAX_LINE`].
#[derive(Debug)]
pub struct Assembler<K: Eq + std::hash::Hash> {
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
    pub fn push(&mut self, key: K, piece: &str, last: bool) -> Result<Option<String>, Error> {
        let line = self.partial.entry(key).or_default();
        if line.len() + piece.len() > MAX_LINE {
            line.clear();
            return Err(Error::Oversized);
        }
        line.push_str(piece);
        Ok(last.then(|| std::mem::take(line)))
    }

    /// Forgets the pieces of a session that ended.
    pub fn forget(&mut self, key: &K) {
        self.partial.remove(key);
    }
}

/// Text cut into pieces that each fit a message, on character boundaries.
pub fn chunks(text: &str) -> impl Iterator<Item = &str> {
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
pub const PROTOCOL: u32 = 20;

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
    /// The first message, and only it.
    Hello {
        protocol: u32,
    },
    /// Starts Claude Code for a session, known by `key` from then on.
    Start {
        key: Generation,
        launch: Launch,
    },
    /// A piece of a line for the session's input; the last piece ends the line.
    Input {
        key: Generation,
        text: String,
        last: bool,
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
    /// Ends every session the daemon runs, in every window, and then the daemon: the user
    /// asked, to have the next window start the edualc installed now.
    ShutDown,
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
    /// A subagent's, beside its session's.
    Subagent {
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
    /// The folder at a path (the picker's, a worktree's), made whole, if it is one:
    /// `Answer::Folder`.
    Folder(PathBuf),
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
}

/// What the daemon tells the window.
#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub enum FromDaemon {
    /// With the settings (or why they can't be read), what folders open in here, and the
    /// sessions running now. `Hosted` follows, before anything else.
    Hello {
        protocol: u32,
        settings: Result<Settings, String>,
        openers: Vec<Opener>,
        live: Vec<LiveSession>,
        /// Other programs' windows can be found (niri runs).
        finds_windows: bool,
        /// The window is on the daemon's machine, by its Unix socket: the daemon does what the
        /// desktop is asked for it (links, notifications, the clipboard, editors, the folder
        /// picker, finding windows). A line over the network is no such window's: what the
        /// desktop is asked there is the window's own to do, and the daemon refuses it.
        local: bool,
        /// The daemon's user's home folder, where a window with no folder of its own opens.
        home: PathBuf,
    },
    /// The process runs: started for the window, or running already as it attaches.
    Started {
        key: Generation,
        pid: u32,
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
    /// A window asked the daemon to shut down: its sessions end, and then it.
    ShuttingDown,
    /// The clipboard's image, or why there is none: a [`Pasted`], postcard-encoded, in pieces
    /// that fit messages; the last piece ends it.
    Pasted {
        request: ActId,
        piece: Vec<u8>,
        last: bool,
    },
    /// What was read, as asked by `Read`.
    Answer { request: ActId, answer: Answer },
    /// A transcript's lines, as asked by `Read::Transcript`: its text, a piece at a time that
    /// fits a message (a piece ends where it ends, not at a line); its `Answer` follows the
    /// last.
    Lines { request: ActId, text: String },
    /// What a watch names changed.
    Changed(Watched),
}

/// The clipboard's image, decoded in the confined helper (see `decode`), or why there is none.
pub type Pasted = Result<Picture, PasteFailure>;

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

/// Why the clipboard gave no image, as the window is told.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, thiserror::Error)]
pub enum PasteFailure {
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
    #[error(
        "couldn't start claude: {0}. Is Claude Code installed and `claude` on PATH? CLAUDE_BIN can name it."
    )]
    Start(String),
    #[error("a session with that key runs already")]
    KeyInUse,
    #[error("a line too long for claude's input")]
    LineTooLong,
    #[error("that session isn't run here, or is shown in another window")]
    NotHosted,
    #[error("the daemon failed starting it")]
    Crashed,
    #[error("{0}")]
    Agy(String),
}

/// Why the desktop didn't do what it was asked, as the window is told.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, thiserror::Error)]
#[serde(deny_unknown_fields)]
pub enum ActFailure {
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
    #[error("the daemon is on another machine: its desktop isn't yours")]
    NotHere,
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
            // Told as a program that couldn't run, which every window reads, not as a reason
            // of its own.
            Error::TimedOut { program } => Self::Run {
                program,
                reason: format!(
                    "it didn't end within {}s",
                    crate::desktop::RUN_LIMIT.as_secs()
                ),
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

/// What the window can have the desktop do, through the daemon.
#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub enum Act {
    OpenIn {
        opener: Opener,
        folder: PathBuf,
    },
    /// A link from the conversation: only web pages and mail are opened.
    OpenUrl(String),
    /// The desktop's folder picker, for a folder to open; answered by `Picked`.
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

impl Act {
    /// Whether it is done on the desktop of the daemon's machine, which only a window there
    /// (its Unix socket) has any say in.
    #[must_use]
    pub fn needs_its_desktop(&self) -> bool {
        match self {
            Self::OpenIn { .. }
            | Self::OpenUrl(_)
            | Self::PickFolder
            | Self::PasteImage
            | Self::Notify { .. }
            | Self::FocusWindowOf { .. } => true,
            Self::CheckTrust { .. } | Self::StopSession(_) => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn long_text_goes_in_pieces_that_fit() {
        let text = "é".repeat(MAX_CHUNK);
        let pieces: Vec<&str> = chunks(&text).collect();
        assert!(pieces.iter().all(|piece| piece.len() <= MAX_CHUNK));
        assert_eq!(pieces.concat(), text);
        assert_eq!(chunks("").collect::<Vec<_>>(), [""]);
    }
}
