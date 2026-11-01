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
#[serde(deny_unknown_fields)]
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
#[serde(deny_unknown_fields)]
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

    pub(crate) fn prompt(&self, text: &str) -> Result<(), Error> {
        self.send(&Input::prompt(text))
    }

    /// A prompt with images, as the confined helper encoded them (see `decode`).
    pub(crate) fn prompt_with_images(&self, text: &str, images: &[Picture]) -> Result<(), Error> {
        let blocks = images
            .iter()
            .map(|picture| ImageBlock::base64(picture.media_type, BASE64.encode(&picture.bytes)))
            .collect();
        self.send(&Input::prompt_with_images(text, blocks))
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
