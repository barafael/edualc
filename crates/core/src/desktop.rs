//! The desktop around the window and edualcd: notifications (shown by the window, on its
//! desktop), and the sessions running elsewhere on edualcd's machine (stopped there, to go on in
//! the window). They go through the programs' own command lines.

use std::process::Command;

use crate::sessions::{LiveKind, LiveSession};

#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    strum::Display,
    strum::EnumIter,
    serde::Serialize,
    serde::Deserialize,
)]
#[strum(serialize_all = "kebab-case")]
pub enum Program {
    NotifySend,
    Kill,
    Claude,
    FcList,
    FcMatch,
    Gsettings,
    Git,
}

/// Whether a link Claude wrote may open: a web page's, or mail. A file or another app's scheme
/// could run what it wrote.
pub fn is_web_link(link: &str) -> bool {
    ["https://", "http://", "mailto:"].iter().any(|scheme| {
        link.get(..scheme.len())
            .is_some_and(|start| start.eq_ignore_ascii_case(scheme))
    })
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("couldn't run {program}: {source}")]
    Run {
        program: Program,
        #[source]
        source: std::io::Error,
    },
    #[error("{program} failed: {stderr}")]
    Failed { program: Program, stderr: String },
    #[error("process {0} was asked to stop but is still running")]
    StillRunning(u32),
    #[error("process {0} isn't that Claude Code session (any more)")]
    NotThatSession(u32),
}

/// Runs a program to its end; what it printed, if it succeeded.
pub fn run(program: Program, args: &[&str]) -> Result<Vec<u8>, Error> {
    let name = if program == Program::Claude {
        crate::claude::program()
    } else {
        program.to_string().into()
    };
    let output = Command::new(name)
        .args(args)
        .output()
        .map_err(|source| Error::Run { program, source })?;
    if output.status.success() {
        Ok(output.stdout)
    } else {
        Err(Error::Failed {
            program,
            stderr: String::from_utf8_lossy(&output.stderr).trim().to_owned(),
        })
    }
}

/// What the desktop knows edualc by: the window's Wayland app id, and the name of its desktop
/// entry (`assets/edualc.desktop`) and of its icon, which the entry's `Icon` names.
pub const APP_ID: &str = "edualc";

/// Shows a desktop notification. Its texts come from other sessions' records: they are taken
/// as text, never as options or markup (which notification daemons render in the body).
pub fn notify(summary: &str, body: &str) -> Result<(), Error> {
    let body = body
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;");
    let desktop_entry = format!("string:desktop-entry:{APP_ID}");
    run(
        Program::NotifySend,
        &[
            "--app-name",
            APP_ID,
            "--icon",
            APP_ID,
            "--hint",
            &desktop_entry,
            "--",
            summary,
            &body,
        ],
    )
    .map(drop)
}

/// How long a stopped session gets to end (it writes its transcript and tidies up first).
const STOP_GRACE_STEPS: u32 = 100;

/// A session running elsewhere, as far as stopping it takes: which process, started when (to
/// tell it from a later one with the pid), and how it runs.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Running {
    pub pid: u32,
    pub started: String,
    pub kind: LiveKind,
    pub id: crate::external::wire::SessionId,
}

impl From<&LiveSession> for Running {
    fn from(session: &LiveSession) -> Self {
        Self {
            pid: session.pid,
            started: session.started.clone(),
            kind: session.kind,
            id: session.id.clone(),
        }
    }
}

/// Stops a Claude Code session where it runs, and waits until it has ended: a background
/// session as `claude stop` stops it (its conversation is kept), one in a terminal as closing
/// the terminal would (SIGTERM). Its transcript is complete afterwards, to be resumed.
pub fn stop_session(session: &Running) -> Result<(), Error> {
    // The record says which process; it must still be that one, and Claude Code, right before
    // it is signalled: records are files anyone running as the user can write.
    let still_it = || crate::process::is_running(session.pid, &session.started);
    if !still_it() || !crate::process::runs_claude(session.pid) {
        return Err(Error::NotThatSession(session.pid));
    }
    let pid = session.pid.to_string();
    match session.kind {
        LiveKind::Bg => run(Program::Claude, &["stop", "--", session.id.as_str()])?,
        LiveKind::Interactive | LiveKind::Other => run(Program::Kill, &["-TERM", &pid])?,
    };
    for _ in 0..STOP_GRACE_STEPS {
        if !still_it() {
            return Ok(());
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    Err(Error::StillRunning(session.pid))
}
