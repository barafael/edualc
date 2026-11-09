//! The desktop around the window and edualcd: notifications (shown by the window, on its
//! desktop), and the sessions running elsewhere on edualcd's machine (stopped there, to go on in
//! the window). They go through the programs' own command lines.

use std::io::Read;
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

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
    #[error("{program} didn't end within {}s", RUN_LIMIT.as_secs())]
    TimedOut { program: Program },
    #[error("{program} failed: {stderr}")]
    Failed { program: Program, stderr: String },
    #[error("process {0} was asked to stop but is still running")]
    StillRunning(u32),
    #[error("process {0} isn't that Claude Code session (any more)")]
    NotThatSession(u32),
}

/// How long a program edualc runs gets to end. Each is quick (`claude stop`, git's
/// configuration, fonts, a notification): one that takes longer has hung, and is killed rather
/// than holding its thread, and what waits on it, forever.
const RUN_LIMIT: Duration = Duration::from_secs(30);

/// How often a running program is looked at, to see whether it ended.
const RUN_POLL: Duration = Duration::from_millis(20);

/// Runs a program to its end, for at most [`RUN_LIMIT`]; what it printed, if it succeeded.
pub fn run(program: Program, args: &[&str]) -> Result<Vec<u8>, Error> {
    let name = if program == Program::Claude {
        crate::claude::program()
    } else {
        program.to_string().into()
    };
    run_within(program, &name, args, RUN_LIMIT)
}

/// Runs `name`, as `program`, for at most `limit`.
fn run_within(
    program: Program,
    name: &std::ffi::OsStr,
    args: &[&str],
    limit: Duration,
) -> Result<Vec<u8>, Error> {
    let deadline = Instant::now() + limit;
    // edualcd's token, if it was given one so, goes to no program it runs (`claude stop`, git),
    // as it goes to no session.
    let mut child = Command::new(name)
        .args(args)
        .env_remove(crate::ipc::TOKEN_VARIABLE)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|source| Error::Run { program, source })?;
    // Read beside it, so one printing much isn't stopped by a full pipe.
    let stdout = child.stdout.take().map(read_beside);
    let stderr = child.stderr.take().map(read_beside);
    let ended = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Ok(status),
            Ok(None) if Instant::now() < deadline => std::thread::sleep(RUN_POLL),
            Ok(None) => break Err(Error::TimedOut { program }),
            Err(source) => break Err(Error::Run { program, source }),
        }
    };
    let status = match ended {
        Ok(status) => status,
        Err(error) => {
            // Its readers are left to end as the pipes close.
            if let Err(error) = child.kill() {
                tracing::debug!(%error, %program, "couldn't kill a program run");
            }
            if let Err(error) = child.wait() {
                tracing::debug!(%error, %program, "a program killed wasn't waited for");
            }
            return Err(error);
        }
    };
    // A program it started may hold its output open after it ended: not waited for past the
    // limit either.
    let printed = |reading: Option<mpsc::Receiver<Vec<u8>>>| {
        let left = deadline.saturating_duration_since(Instant::now());
        match reading.map(|reading| reading.recv_timeout(left)) {
            Some(Ok(bytes)) => Ok(bytes),
            Some(Err(_)) => Err(Error::TimedOut { program }),
            None => Ok(Vec::new()),
        }
    };
    let stdout = printed(stdout)?;
    if status.success() {
        Ok(stdout)
    } else {
        let stderr = printed(stderr).unwrap_or_default();
        Err(Error::Failed {
            program,
            stderr: String::from_utf8_lossy(&stderr).trim().to_owned(),
        })
    }
}

/// Reads all a program prints to one of its pipes, on a thread of its own.
fn read_beside(mut pipe: impl Read + Send + 'static) -> mpsc::Receiver<Vec<u8>> {
    let (read, reading) = mpsc::sync_channel(1);
    std::thread::spawn(move || {
        let mut bytes = Vec::new();
        if let Err(error) = pipe.read_to_end(&mut bytes) {
            tracing::debug!(%error, "a program's output wasn't read to its end");
        }
        if read.send(bytes).is_err() {
            // Nobody waits for it any more: it took too long.
        }
    });
    reading
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

#[cfg(test)]
mod tests {
    use super::*;

    /// A program that ends in time gives what it printed, or why it failed.
    #[test]
    fn a_program_run_gives_what_it_printed() {
        let said = run_within(Program::Git, "echo".as_ref(), &["hello"], RUN_LIMIT);
        assert_eq!(said.ok(), Some(b"hello\n".to_vec()));
        let failed = run_within(
            Program::Git,
            "sh".as_ref(),
            &["-c", "echo no >&2; exit 3"],
            RUN_LIMIT,
        );
        assert!(matches!(failed, Err(Error::Failed { stderr, .. }) if stderr == "no"));
    }

    /// One that hangs is killed at its limit; so is waiting on a program it left holding the
    /// output open.
    #[test]
    fn a_program_that_hangs_is_given_up() {
        let limit = Duration::from_millis(300);
        let started = Instant::now();
        let hung = run_within(Program::Claude, "sleep".as_ref(), &["30"], limit);
        assert!(matches!(
            hung,
            Err(Error::TimedOut {
                program: Program::Claude
            })
        ));
        let left_open = run_within(Program::Git, "sh".as_ref(), &["-c", "sleep 30 &"], limit);
        assert!(matches!(left_open, Err(Error::TimedOut { .. })));
        assert!(started.elapsed() < Duration::from_secs(5));
    }
}
