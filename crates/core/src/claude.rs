//! One Claude Code session's process, spoken to in the stream-json format the Agent SDKs use:
//! user messages and control requests go in, messages, stream events and permission requests
//! come out, one JSON object per line. The daemon (edualcd) runs the process; this is the
//! window's handle on it, and what its lines mean.

use std::path::PathBuf;

use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;

use crate::decode::Picture;
use crate::external::wire::{
    ControlPeek, ControlRequest, ImageBlock, Input, Message, PeekedKind, PermissionResult, Reply,
    RequestId, SessionId,
};
use crate::ipc::{Generation, HostedId, StartFailure, ToDaemon};
use crate::link::{self, Link};

/// The `claude` to run: `$CLAUDE_BIN` as the daemon was started with, or the one on `PATH`.
pub fn program() -> std::ffi::OsString {
    std::env::var_os("CLAUDE_BIN").unwrap_or_else(|| "claude".into())
}

/// What the process says, or the daemon about it.
#[derive(Debug)]
pub enum Output {
    /// The daemon started it, or it runs already as the window attaches.
    Started {
        pid: u32,
        /// Turns under way, as the daemon counted them.
        turns: usize,
    },
    /// The daemon couldn't start it, or refused its input.
    Failed(StartFailure),
    Message(Message),
    /// A line on stdout that should have been a turn's result or a request, which the window
    /// can't read: a turn would wait for it forever otherwise.
    Unreadable {
        lost: Lost,
        error: serde_json::Error,
    },
    /// A line of stderr, or of stdout that isn't a message.
    Log(String),
    /// It ended, with its exit code if it exited on its own.
    Ended(Option<i32>),
}

impl Output {
    /// A line the process wrote on stdout.
    pub fn of_line(line: String) -> Self {
        let error = match serde_json::from_str(&line) {
            Ok(message) => return Self::Message(message),
            Err(error) => error,
        };
        let lost = serde_json::from_str::<ControlPeek>(&line)
            .ok()
            .and_then(|peek| match (peek.kind, peek.request_id) {
                (PeekedKind::Result, _) => Some(Lost::Result),
                (PeekedKind::ControlRequest, Some(request)) => {
                    Some(Lost::Request(RequestId::from(request)))
                }
                (PeekedKind::ControlRequest, None)
                | (
                    PeekedKind::System
                    | PeekedKind::ControlCancelRequest
                    | PeekedKind::ControlResponse
                    | PeekedKind::User
                    | PeekedKind::Other,
                    _,
                ) => None,
            });
        match lost {
            Some(lost) => {
                tracing::warn!(%error, ?lost, "a line from claude the window can't read");
                Self::Unreadable { lost, error }
            }
            None => {
                tracing::debug!(%error, "a line from claude that isn't a message");
                Self::Log(line)
            }
        }
    }
}

/// What a line the window can't read was, as far as it can tell.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Lost {
    /// A turn's end.
    Result,
    /// A request, which Claude Code waits for an answer to.
    Request(RequestId),
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("couldn't put a message for claude into JSON: {0}")]
    Encode(#[source] serde_json::Error),
    #[error("lost the edualc daemon, which runs Claude Code")]
    Lost,
}

/// The program a session talks to: Claude Code, Google's Antigravity CLI, or opencode. edualcd's
/// ACP adapter speaks to the last two over the Agent Client Protocol and translates for the
/// window.
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
pub enum Backend {
    #[default]
    Claude,
    Agy,
    Opencode,
}

impl Backend {
    /// Who answers and asks, as the window names it.
    pub fn agent(self) -> &'static str {
        match self {
            Self::Claude => "Claude",
            Self::Agy => "agy",
            Self::Opencode => "opencode",
        }
    }

    /// The program the session runs as.
    pub fn program(self) -> &'static str {
        match self {
            Self::Claude => "Claude Code",
            Self::Agy => "agy",
            Self::Opencode => "opencode",
        }
    }

    /// Whether edualcd's ACP adapter runs it.
    pub fn is_acp(self) -> bool {
        match self {
            Self::Claude => false,
            Self::Agy | Self::Opencode => true,
        }
    }
}

/// What a session's process is started with; the daemon starts it.
#[derive(Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Launch {
    pub backend: Backend,
    pub cwd: PathBuf,
    pub conversation: Conversation,
    /// A model or alias, as the backend takes it; the backend's default if none.
    pub model: Option<String>,
    /// Whether the folder's own configuration is read.
    pub folder_configuration: FolderConfiguration,
    /// How Claude Code signs in; agy signs in itself.
    pub sign_in: SignInWith,
}

/// What a session runs, as the sidebar and the New session menu tell them apart.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Program {
    #[default]
    Claude,
    ClaudeApiKey,
    Agy,
    Opencode,
}

impl Program {
    /// agy and opencode sign in themselves, so their sign-in doesn't matter.
    pub fn of(backend: Backend, sign_in: SignInWith) -> Self {
        match (backend, sign_in) {
            (Backend::Claude, SignInWith::Account) => Self::Claude,
            (Backend::Claude, SignInWith::ApiKey) => Self::ClaudeApiKey,
            (Backend::Agy, _) => Self::Agy,
            (Backend::Opencode, _) => Self::Opencode,
        }
    }
}

/// What a Claude Code session signs in with.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum SignInWith {
    /// The user's own sign-in, as in their terminal.
    #[default]
    Account,
    /// The Anthropic API key in the desktop's keyring, billed by the token.
    ApiKey,
}

/// Whether Claude Code reads what a folder configures (`.claude/settings*.json`, `.mcp.json`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum FolderConfiguration {
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

/// A saved session for a process to go on with.
#[derive(Debug, PartialEq, Eq)]
pub enum Resume {
    /// Continues the session itself.
    Continue(SessionId),
    /// Continues a copy under a new id, as the session runs elsewhere too.
    Fork(SessionId),
}

impl Resume {
    /// The conversation a process runs for `resume` (none: a new one), the ids of new
    /// sessions chosen here.
    pub fn conversation(resume: Option<Self>) -> Conversation {
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
pub enum Conversation {
    New(SessionId),
    Continue(SessionId),
    /// A copy of `of`, under `id`.
    Fork {
        of: SessionId,
        id: SessionId,
    },
}

impl Conversation {
    pub fn id(&self) -> &SessionId {
        match self {
            Self::New(id) | Self::Continue(id) | Self::Fork { id, .. } => id,
        }
    }
}

/// The window's handle on a session's process, which the daemon runs. Dropped, it ends the
/// process (what runs is interrupted, and its input closed), unless it was let go of
/// ([`Claude::detach`]): then the process runs on, for a window to attach to later.
#[derive(Debug)]
pub struct Claude {
    key: Generation,
    link: Link,
    next_request: u64,
    pid: Option<u32>,
    leaving: Leaving,
    use_: Use,
}

/// Whether the user has used the session through this handle, which decides whether leaving it
/// ends its process (see [`Claude::used`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Use {
    /// Opened to read: leaving it ends the process, and its transcript continues it later.
    Opened,
    /// Prompted, given a `!` command, or attached to after it was kept running.
    Used,
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
    pub fn start(link: &Link, key: Generation, launch: Launch) -> Result<Self, Error> {
        link::send(link, ToDaemon::Start { key, launch }).map_err(|_lost| Error::Lost)?;
        Ok(Self::new(link, key, Use::Opened))
    }

    /// Shows a process the daemon runs under `key`; what it asked meanwhile comes again, then
    /// what it says. It runs because it was kept running, so leaving it again keeps it too.
    pub fn attach(link: &Link, key: Generation, hosted: HostedId) -> Result<Self, Error> {
        link::send(link, ToDaemon::Attach { key, hosted }).map_err(|_lost| Error::Lost)?;
        Ok(Self::new(link, key, Use::Used))
    }

    fn new(link: &Link, key: Generation, use_: Use) -> Self {
        Self {
            key,
            link: link.clone(),
            next_request: 0,
            pid: None,
            leaving: Leaving::End,
            use_,
        }
    }

    /// Whether the user used the session here (a prompt, a `!` command) or it was kept running
    /// before: leaving such a session keeps its process running.
    pub fn used(&self) -> bool {
        self.use_ == Use::Used
    }

    /// The daemon started it (or showed it here).
    pub fn started(&mut self, pid: u32) {
        self.pid = Some(pid);
    }

    /// Tells the daemon Claude Code's id for the session, for its list.
    pub fn identify(&self, session: &SessionId) -> Result<(), Error> {
        let identify = ToDaemon::Identify {
            key: self.key,
            session: session.clone(),
        };
        link::send(&self.link, identify).map_err(|_lost| Error::Lost)
    }

    /// Lets go of the process, which runs on (the window closes).
    pub fn detach(mut self) {
        self.leaving = Leaving::LetGo;
    }

    /// Writes one line of JSON.
    fn send(&self, message: &Input) -> Result<(), Error> {
        let line = serde_json::to_string(message).map_err(Error::Encode)?;
        link::send_input(&self.link, self.key, &line).map_err(|_lost| Error::Lost)
    }

    /// A prompt, after the rows of the `!` commands run since the last (see
    /// [`Input::prompt_with`]), with images as the confined helper encoded them (see
    /// [`crate::decode`]).
    pub fn prompt(
        &mut self,
        shell: &[String],
        images: &[Picture],
        text: &str,
    ) -> Result<(), Error> {
        let blocks = images
            .iter()
            .map(|picture| ImageBlock::base64(picture.media_type, BASE64.encode(&picture.bytes)))
            .collect();
        self.use_ = Use::Used;
        self.send(&Input::prompt_with(shell, blocks, text))
    }

    /// A `!` command, for the CLI to run (see [`Input::BashCommand`]).
    pub fn run_shell(&mut self, command: &str) -> Result<(), Error> {
        self.use_ = Use::Used;
        self.send(&Input::bash_command(command))
    }

    /// Sends a control request; its answer comes back with the returned id.
    pub fn request(&mut self, request: ControlRequest) -> Result<RequestId, Error> {
        self.next_request += 1;
        let id = RequestId::from(format!("gui_{}", self.next_request));
        self.send(&Input::ControlRequest {
            request_id: id.clone(),
            request,
        })?;
        Ok(id)
    }

    /// Answers one of the process's control requests (a permission request).
    pub fn answer(&self, request: &RequestId, response: PermissionResult) -> Result<(), Error> {
        self.send(&Input::ControlResponse {
            response: Reply::Success {
                request_id: request.clone(),
                response,
            },
        })
    }

    /// Turns down a control request we don't handle (hook callbacks, SDK MCP servers), so the
    /// process doesn't wait for it.
    pub fn refuse(&self, request: &RequestId, error: &str) -> Result<(), Error> {
        self.send(&Input::ControlResponse {
            response: Reply::Error {
                request_id: request.clone(),
                error: error.to_owned(),
            },
        })
    }

    /// The process's id, once it runs: it lists itself among running sessions, like any other.
    pub fn pid(&self) -> Option<u32> {
        self.pid
    }
}

impl Drop for Claude {
    fn drop(&mut self) {
        let leave = match self.leaving {
            Leaving::End => {
                if let Err(error) = self.request(ControlRequest::Interrupt) {
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

#[cfg(all(test, not(target_arch = "wasm32")))]
mod tests {
    use super::*;

    /// A turn's result or a request the window can't read is told as lost, with the request's
    /// id to answer it by; any other line it can't read is the process's log.
    #[test]
    fn a_result_or_request_the_window_cant_read_is_told_as_lost() {
        let lost = |line: &str| match Output::of_line(line.to_owned()) {
            Output::Unreadable { lost, .. } => Some(lost),
            Output::Log(_) => None,
            other @ (Output::Started { .. }
            | Output::Failed(_)
            | Output::Message(_)
            | Output::Ended(_)) => panic!("{line} read as {other:?}"),
        };
        assert_eq!(
            lost(r#"{"type":"result","subtype":"success","is_error":"no"}"#),
            Some(Lost::Result)
        );
        assert_eq!(
            lost(
                r#"{"type":"control_request","request_id":"r1","request":{"subtype":"can_use_tool","tool_name":7}}"#
            ),
            Some(Lost::Request(RequestId::from("r1".to_owned())))
        );
        assert_eq!(lost(r#"{"type":"assistant","message":5}"#), None);
        assert_eq!(lost("not JSON at all"), None);
        assert!(matches!(
            Output::of_line(
                r#"{"type":"result","subtype":"success","result":"ok","total_cost_usd":null}"#
                    .to_owned()
            ),
            Output::Message(Message::Result(_))
        ));
    }

    /// A session started here counts as used once the user prompts it or gives it a `!`
    /// command, and leaving it then keeps it running; one only opened to read doesn't. One
    /// attached to was kept running before, so it counts as used from the start.
    #[test]
    fn a_session_is_used_once_prompted_or_given_a_command() {
        let (link, _daemon) = std::sync::mpsc::sync_channel(link::QUEUE);
        let launch = || Launch {
            backend: Backend::Claude,
            cwd: PathBuf::from("/work"),
            conversation: Conversation::New(SessionId::new_random()),
            model: None,
            folder_configuration: FolderConfiguration::Read,
            sign_in: SignInWith::Account,
        };

        let mut prompted = Claude::start(&link, Generation::default(), launch()).expect("sent");
        assert!(!prompted.used(), "only opened");
        prompted.prompt(&[], &[], "go on").expect("sent");
        assert!(prompted.used());

        let mut commanded = Claude::start(&link, Generation::default(), launch()).expect("sent");
        commanded.run_shell("ls").expect("sent");
        assert!(commanded.used());

        let attached = Claude::attach(&link, Generation::default(), HostedId::default());
        assert!(attached.expect("sent").used());
    }
}
