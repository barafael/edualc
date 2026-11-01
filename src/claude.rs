//! One Claude Code session's process, spoken to in the stream-json format the Agent SDKs use:
//! user messages and control requests go in, messages, stream events and permission requests
//! come out, one JSON object per line. The daemon runs the process (see `daemon`); this is the
//! window's handle on it, and what its lines mean.

use std::path::PathBuf;

use crate::ipc::{StartFailure, ToDaemon};
use crate::link::{self, Link};
use crate::sandbox::Enforcement;
use crate::wire::{ControlRequest, Input, Message, PermissionResult, Reply, RequestId, SessionId};

/// What the process says, or the daemon about it.
#[derive(Debug)]
pub(crate) enum Output {
    /// The daemon started it.
    Started {
        pid: u32,
        enforcement: Option<Enforcement>,
    },
    /// The daemon couldn't start it, or refused its input.
    Failed(StartFailure),
    Message(Message),
    /// A line of stderr, or of stdout that isn't a message.
    Log(String),
    /// It ended, with its exit code if it exited on its own.
    Ended(Option<i32>),
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

/// What a session's process is started with; the daemon starts it.
#[derive(Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct Launch {
    pub(crate) cwd: PathBuf,
    pub(crate) resume: Option<Resume>,
    /// A model or alias for `--model`; the user's default if none.
    pub(crate) model: Option<String>,
    /// Sandboxed, and with what let in, unless `None`.
    pub(crate) sandbox: Option<SandboxWish>,
}

/// The sandbox asked for; the daemon makes the policy from it.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct SandboxWish {
    pub(crate) desktop: bool,
    pub(crate) read: Vec<PathBuf>,
    pub(crate) write: Vec<PathBuf>,
}

#[derive(Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) enum Resume {
    /// Continues the session itself.
    Continue(SessionId),
    /// Continues a copy under a new id, as the session runs elsewhere too.
    Fork(SessionId),
}

/// The window's handle on a session's process. Dropped, it ends the process: what runs is
/// interrupted, and its input closed.
#[derive(Debug)]
pub(crate) struct Claude {
    /// This window's name for the session, which everything the daemon says about it carries.
    key: u64,
    link: Link,
    next_request: u64,
    pid: Option<u32>,
    enforcement: Option<Enforcement>,
}

impl Claude {
    /// Asks the daemon to start the process; [`Output::Started`] or [`Output::Failed`] says how
    /// that went.
    pub(crate) fn start(link: &Link, key: u64, launch: Launch) -> Result<Self, Error> {
        link::send(link, ToDaemon::Start { key, launch }).map_err(|_lost| Error::Lost)?;
        Ok(Self {
            key,
            link: link.clone(),
            next_request: 0,
            pid: None,
            enforcement: None,
        })
    }

    /// The daemon started it.
    pub(crate) fn started(&mut self, pid: u32, enforcement: Option<Enforcement>) {
        self.pid = Some(pid);
        self.enforcement = enforcement;
    }

    /// Writes one line of JSON.
    fn send(&self, message: &Input<'_>) -> Result<(), Error> {
        let line = serde_json::to_string(message).map_err(Error::Encode)?;
        link::send_input(&self.link, self.key, &line).map_err(|_lost| Error::Lost)
    }

    pub(crate) fn prompt(&self, text: &str) -> Result<(), Error> {
        self.send(&Input::prompt(text))
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
        if let Err(error) = self.request(&ControlRequest::Interrupt) {
            tracing::debug!(%error, "couldn't interrupt claude before ending it");
        }
        if link::send(&self.link, ToDaemon::End { key: self.key }).is_err() {
            tracing::debug!("the daemon was gone as a session ended");
        }
    }
}
