//! The desktop around the window: notifications, the windows other programs run sessions in,
//! and the editors, terminals and file manager a folder can be opened in. They go through the
//! programs' own command lines, and niri through its socket.

use std::path::{Path, PathBuf};
use std::process::Command;

use crate::external::niri;
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
pub(crate) enum Program {
    NotifySend,
    XdgOpen,
    /// Zed, as Arch and others name it.
    Zeditor,
    Zed,
    Code,
    Codium,
    Ghostty,
    Foot,
    Alacritty,
    Kitty,
    Wezterm,
    Konsole,
    GnomeTerminal,
    Kill,
    Claude,
    FcList,
    FcMatch,
    Gsettings,
    Git,
}

/// Something a folder can be opened in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) enum Opener {
    Editor(Program),
    Terminal(Program),
    Files,
}

impl Opener {
    pub(crate) fn label(self) -> String {
        match self {
            Self::Editor(Program::Zeditor | Program::Zed) => "Zed".into(),
            Self::Editor(Program::Code) => "VS Code".into(),
            Self::Editor(Program::Codium) => "VSCodium".into(),
            Self::Editor(program) | Self::Terminal(program) => program.to_string(),
            Self::Files => "File manager".into(),
        }
    }
}

/// What this machine has to open a folder in: Zed, VS Code, a terminal, the file manager.
pub(crate) fn openers() -> Vec<Opener> {
    let first =
        |candidates: &[Program]| candidates.iter().copied().find(|program| on_path(*program));
    let editors = [
        &[Program::Zeditor, Program::Zed][..],
        &[Program::Code, Program::Codium][..],
    ];
    let terminals = [
        Program::Ghostty,
        Program::Foot,
        Program::Alacritty,
        Program::Kitty,
        Program::Wezterm,
        Program::Konsole,
        Program::GnomeTerminal,
    ];
    let mut found: Vec<Opener> = editors
        .iter()
        .filter_map(|names| first(names))
        .map(Opener::Editor)
        .collect();
    found.extend(first(&terminals).map(Opener::Terminal));
    if on_path(Program::XdgOpen) {
        found.push(Opener::Files);
    }
    found
}

fn on_path(program: Program) -> bool {
    let name = program.to_string();
    std::env::var_os("PATH").is_some_and(|path| {
        std::env::split_paths(&path).any(|dir: PathBuf| dir.join(&name).is_file())
    })
}

/// Opens `folder` in `opener`, and waits for the program so it is reaped when it ends.
pub(crate) fn open_in(opener: Opener, folder: &Path) -> Result<(), Error> {
    let program = match opener {
        Opener::Editor(program) | Opener::Terminal(program) => program,
        Opener::Files => Program::XdgOpen,
    };
    let mut command = Command::new(program.to_string());
    match opener {
        // Terminals start where they are started.
        Opener::Terminal(_) => command.current_dir(folder),
        Opener::Editor(_) | Opener::Files => command.arg(folder),
    };
    let mut child = command
        .spawn()
        .map_err(|source| Error::Run { program, source })?;
    match child.wait() {
        Ok(status) if status.success() => Ok(()),
        Ok(status) => Err(Error::Failed {
            program,
            stderr: format!("ended with {status}"),
        }),
        Err(source) => Err(Error::Run { program, source }),
    }
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum Error {
    #[error("couldn't run {program}: {source}")]
    Run {
        program: Program,
        #[source]
        source: std::io::Error,
    },
    #[error("{program} failed: {stderr}")]
    Failed { program: Program, stderr: String },
    #[error(transparent)]
    Niri(#[from] niri::Error),
    #[error(
        "no window holds process {0}; it may run in the background or in a terminal multiplexer"
    )]
    NoWindow(u32),
    #[error("process {0} was asked to stop but is still running")]
    StillRunning(u32),
    #[error("process {0} isn't that Claude Code session (any more)")]
    NotThatSession(u32),
}

/// Runs a program to its end; what it printed, if it succeeded.
pub(crate) fn run(program: Program, args: &[&str]) -> Result<Vec<u8>, Error> {
    let name = if program == Program::Claude {
        crate::spawn::program()
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
pub(crate) const APP_ID: &str = "edualc";

/// Shows a desktop notification. Its texts come from other sessions' records: they are taken
/// as text, never as options or markup (which notification daemons render in the body).
pub(crate) fn notify(summary: &str, body: &str) -> Result<(), Error> {
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

/// Focuses the window that process `pid` runs in: the one whose process is `pid` or one of
/// its ancestors (a terminal, an editor). Only niri is asked.
pub(crate) fn focus_window_of(pid: u32) -> Result<(), Error> {
    let windows = match niri::ask(&niri::Request::Windows)? {
        niri::Response::Windows(windows) => windows,
        niri::Response::Handled => Vec::new(),
    };
    let window = ancestors(pid)
        .into_iter()
        .find_map(|ancestor| {
            windows
                .iter()
                .find(|window| window.pid.and_then(|pid| u32::try_from(pid).ok()) == Some(ancestor))
        })
        .ok_or(Error::NoWindow(pid))?;
    let focus = niri::Request::Action(niri::Action::FocusWindow { id: window.id });
    niri::ask(&focus).map(drop).map_err(Error::Niri)
}

/// How long a stopped session gets to end (it writes its transcript and tidies up first).
const STOP_GRACE_STEPS: u32 = 100;

/// A session running elsewhere, as far as stopping it takes: which process, started when (to
/// tell it from a later one with the pid), and how it runs.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Running {
    pub(crate) pid: u32,
    pub(crate) started: String,
    pub(crate) kind: LiveKind,
    pub(crate) id: crate::external::wire::SessionId,
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
pub(crate) fn stop_session(session: &Running) -> Result<(), Error> {
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

/// `pid` and its parents, nearest first, up to (not including) init.
fn ancestors(pid: u32) -> Vec<u32> {
    let mut chain = vec![pid];
    let mut current = pid;
    // Deep enough for any real chain; a cycle in a racing /proc can't loop forever.
    for _ in 0..64 {
        let Some(parent) = parent_of(current).filter(|&parent| parent > 1) else {
            break;
        };
        chain.push(parent);
        current = parent;
    }
    chain
}

fn parent_of(pid: u32) -> Option<u32> {
    crate::process::stat(pid).map(|stat| stat.parent)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_process_has_ancestors() {
        let chain = ancestors(std::process::id());
        assert_eq!(chain.first(), Some(&std::process::id()));
        assert!(chain.len() > 1, "the test runner has a parent");
    }
}
