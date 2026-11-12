//! One Claude Code session's process, spoken to in the stream-json format the Agent SDKs use:
//! user messages and control requests go in, messages, stream events and permission requests
//! come out, one JSON object per line. The daemon runs the process (see `daemon`); this is the
//! window's handle on it, and what its lines mean.

use std::path::PathBuf;

use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;

use crate::decode::Picture;
use crate::external::wire::{
    ControlRequest, ImageBlock, Input, Message, PermissionResult, Reply, RequestId, SessionId,
};
use crate::ipc::{Generation, HostedId, StartFailure, ToDaemon};
use crate::link::{self, Link};
use crate::sandbox::Enforcement;

/// The `claude` to run: `$CLAUDE_BIN` as the daemon was started with, or the one on `PATH`.
pub(crate) fn program() -> std::ffi::OsString {
    std::env::var_os("CLAUDE_BIN").unwrap_or_else(|| "claude".into())
}

/// What the process says, or the daemon about it.
#[derive(Debug)]
pub(crate) enum Output {
    /// The daemon started it, or it runs already as the window attaches.
    Started {
        pid: u32,
        enforcement: Option<Enforcement>,
        /// Turns under way, as the daemon counted them.
        turns: usize,
    },
    /// The daemon couldn't start it, or refused its input.
    Failed(StartFailure),
    Message(Message),
    /// A line of stderr, or of stdout that isn't a message.
    Log(String),
    /// It ended, with its exit code if it exited on its own.
    Ended(Option<i32>),
    /// Its transcript went two ways in and out of the sandbox; this copy, which it doesn't go
    /// on from, keeps what only it has.
    Diverged(PathBuf),
}

impl Output {
    /// A line the process wrote on stdout.
    pub(crate) fn of_line(line: String) -> Self {
        match serde_json::from_str(&line) {
            Ok(message) => Self::Message(message),
            Err(error) => {
                tracing::debug!(%error, "a line from claude that isn't a message");
                Self::Log(line)
            }
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum Error {
    #[error("couldn't put a message for claude into JSON: {0}")]
    Encode(#[source] serde_json::Error),
    #[error("lost the edualc daemon, which runs Claude Code")]
    Lost,
}

/// The program a session talks to: Claude Code, or Google's Antigravity CLI, which edualc's
/// adapter (see `agy`) speaks to over the Agent Client Protocol and translates for the window.
#[derive(
    Debug,
    Clone,
    Copy,
    Default,
    PartialEq,
    Eq,
    Hash,
    serde::Serialize,
    serde::Deserialize,
    strum::EnumString,
    strum::IntoStaticStr,
    strum::EnumIter,
)]
#[strum(serialize_all = "lowercase")]
#[serde(rename_all = "lowercase")]
pub(crate) enum Backend {
    #[default]
    Claude,
    Agy,
}

impl Backend {
    /// Who answers and asks, as the window names it.
    pub(crate) fn agent(self) -> &'static str {
        match self {
            Self::Claude => "Claude",
            Self::Agy => "agy",
        }
    }

    /// The program the session runs as.
    pub(crate) fn program(self) -> &'static str {
        match self {
            Self::Claude => "Claude Code",
            Self::Agy => "agy",
        }
    }
}

/// What a session's process is started with; the daemon starts it.
#[derive(Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Launch {
    pub(crate) backend: Backend,
    pub(crate) cwd: PathBuf,
    pub(crate) conversation: Conversation,
    /// A model or alias, as the backend takes it; the backend's default if none.
    pub(crate) model: Option<String>,
    /// Sandboxed, and with what let in, unless `None`.
    pub(crate) sandbox: Option<SandboxWish>,
    /// Whether the folder's own configuration is read.
    pub(crate) folder_configuration: FolderConfiguration,
}

/// Whether Claude Code reads what a folder configures (`.claude/settings*.json`, `.mcp.json`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) enum FolderConfiguration {
    /// As for a trusted folder: its hooks, environment, helpers and MCP servers apply.
    #[default]
    Read,
    /// Not trusted, and started anyway without it: only the user's own settings apply, and no
    /// MCP servers from configuration at all, the user's and claude.ai connectors included
    /// (`--setting-sources user --strict-mcp-config`), as Claude Code's own trust dialog lets
    /// one go on without them. Servers the host serves in `initialize` stay (checked with CLI
    /// 2.1.316).
    Skipped,
}

/// The sandbox asked for; the daemon makes the policy from it.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SandboxWish {
    pub(crate) desktop: bool,
    /// USB devices and serial ports, for debug probes and boards.
    pub(crate) hardware: bool,
    /// Its messages to other sessions go without the user being asked about each.
    pub(crate) messages_unasked: bool,
    pub(crate) read: Vec<PathBuf>,
    pub(crate) write: Vec<PathBuf>,
}

/// A saved session for a process to go on with.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Resume {
    /// Continues the session itself.
    Continue(SessionId),
    /// Continues a copy under a new id, as the session runs elsewhere too.
    Fork(SessionId),
}

impl Resume {
    /// The conversation a process runs for `resume` (none: a new one), the ids of new
    /// sessions chosen here.
    pub(crate) fn conversation(resume: Option<Self>) -> Conversation {
        match resume {
            None => Conversation::New(SessionId::new_random()),
            Some(Self::Continue(id)) => Conversation::Continue(id),
            Some(Self::Fork(of)) => Conversation::Fork {
                of,
                id: SessionId::new_random(),
            },
        }
    }
}

/// The conversation a process runs, and the id it runs it under, which its `init` tells. A new
/// session's or a copy's is chosen here (`--session-id`), so it is known from the start, and
/// the session's name for other sessions with it.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) enum Conversation {
    New(SessionId),
    Continue(SessionId),
    /// A copy of `of`, under `id`.
    Fork {
        of: SessionId,
        id: SessionId,
    },
}

impl Conversation {
    pub(crate) fn id(&self) -> &SessionId {
        match self {
            Self::New(id) | Self::Continue(id) | Self::Fork { id, .. } => id,
        }
    }
}

/// The window's handle on a session's process, which the daemon runs. Dropped, it ends the
/// process (what runs is interrupted, and its input closed), unless it was let go of
/// ([`Claude::detach`]): then the process runs on, for a window to attach to later.
#[derive(Debug)]
pub(crate) struct Claude {
    key: Generation,
    link: Link,
    next_request: u64,
    pid: Option<u32>,
    leaving: Leaving,
}

/// What a handle going away does to its process.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Leaving {
    /// Interrupts it and ends it.
    End,
    /// Lets it run on, shown nowhere.
    LetGo,
}

impl Claude {
    /// Asks the daemon to start the process; [`Output::Started`] or [`Output::Failed`] says how
    /// that went.
    pub(crate) fn start(link: &Link, key: Generation, launch: Launch) -> Result<Self, Error> {
        link::send(link, ToDaemon::Start { key, launch }).map_err(|_lost| Error::Lost)?;
        Ok(Self::new(link, key))
    }

    /// Shows a process the daemon runs under `key`; what it asked meanwhile comes again, then
    /// what it says.
    pub(crate) fn attach(link: &Link, key: Generation, hosted: HostedId) -> Result<Self, Error> {
        link::send(link, ToDaemon::Attach { key, hosted }).map_err(|_lost| Error::Lost)?;
        Ok(Self::new(link, key))
    }

    fn new(link: &Link, key: Generation) -> Self {
        Self {
            key,
            link: link.clone(),
            next_request: 0,
            pid: None,
            leaving: Leaving::End,
        }
    }

    /// The daemon started it (or showed it here).
    pub(crate) fn started(&mut self, pid: u32) {
        self.pid = Some(pid);
    }

    /// Tells the daemon Claude Code's id for the session, for its list.
    pub(crate) fn identify(&self, session: &SessionId) -> Result<(), Error> {
        let identify = ToDaemon::Identify {
            key: self.key,
            session: session.clone(),
        };
        link::send(&self.link, identify).map_err(|_lost| Error::Lost)
    }

    /// Lets go of the process, which runs on (the window closes).
    pub(crate) fn detach(mut self) {
        self.leaving = Leaving::LetGo;
    }

    /// Writes one line of JSON.
    fn send(&self, message: &Input<'_>) -> Result<(), Error> {
        let line = serde_json::to_string(message).map_err(Error::Encode)?;
        link::send_input(&self.link, self.key, &line).map_err(|_lost| Error::Lost)
    }

    /// A prompt, after the rows of the `!` commands run since the last (see
    /// [`Input::prompt_with`]), with images as the confined helper encoded them (see `decode`).
    pub(crate) fn prompt(
        &self,
        shell: &[String],
        images: &[Picture],
        text: &str,
    ) -> Result<(), Error> {
        let blocks = images
            .iter()
            .map(|picture| ImageBlock::base64(picture.media_type, BASE64.encode(&picture.bytes)))
            .collect();
        self.send(&Input::prompt_with(shell, blocks, text))
    }

    /// A `!` command, for the CLI to run (see [`Input::BashCommand`]).
    pub(crate) fn run_shell(&self, command: &str) -> Result<(), Error> {
        self.send(&Input::bash_command(command))
    }

    /// Sends a control request; its answer comes back with the returned id.
    pub(crate) fn request(&mut self, request: &ControlRequest) -> Result<RequestId, Error> {
        self.next_request += 1;
        let id = RequestId::from(format!("gui_{}", self.next_request));
        self.send(&Input::ControlRequest {
            request_id: &id,
            request,
        })?;
        Ok(id)
    }

    /// Answers one of the process's control requests (a permission request).
    pub(crate) fn answer(
        &self,
        request: &RequestId,
        response: &PermissionResult,
    ) -> Result<(), Error> {
        self.send(&Input::ControlResponse {
            response: Reply::Success {
                request_id: request,
                response,
            },
        })
    }

    /// Turns down a control request we don't handle (hook callbacks, SDK MCP servers), so the
    /// process doesn't wait for it.
    pub(crate) fn refuse(&self, request: &RequestId, error: &str) -> Result<(), Error> {
        self.send(&Input::ControlResponse {
            response: Reply::Error {
                request_id: request,
                error,
            },
        })
    }

    /// The process's id, once it runs: it lists itself among running sessions, like any other.
    pub(crate) fn pid(&self) -> Option<u32> {
        self.pid
    }
}

impl Drop for Claude {
    fn drop(&mut self) {
        let leave = match self.leaving {
            Leaving::End => {
                if let Err(error) = self.request(&ControlRequest::Interrupt) {
                    tracing::debug!(%error, "couldn't interrupt claude before ending it");
                }
                ToDaemon::End { key: self.key }
            }
            Leaving::LetGo => ToDaemon::Detach { key: self.key },
        };
        if link::send(&self.link, leave).is_err() {
            tracing::debug!("the daemon was gone as a window let go of a session");
        }
    }
}
