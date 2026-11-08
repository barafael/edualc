//! Reaching the daemon from a window or the page server: connected to if it runs, started
//! (`edualcd --daemon`) if not.

use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use super::{Error, Folder, connect};

/// How long a daemon just started gets to listen.
const DAEMON_START: Duration = Duration::from_secs(3);

#[derive(Debug, thiserror::Error)]
pub enum ReachError {
    #[error(transparent)]
    Ipc(#[from] Error),
    #[error("couldn't start the edualc daemon (edualcd beside this program): {0}")]
    Start(#[source] std::io::Error),
    #[error("the edualc daemon didn't come up")]
    NoDaemon,
}

/// A connection to the daemon, checked to be this user's; the daemon is started if none runs,
/// as `program --daemon` (`edualcd`, which is the page server too).
pub fn reach(program: &Path) -> Result<UnixStream, ReachError> {
    let folder = Folder::open()?;
    match connect(&folder) {
        Ok(stream) => Ok(stream),
        Err(error) => {
            tracing::info!(%error, "starting the edualc daemon");
            start_daemon(&folder, program)?;
            wait_for_daemon(&folder)
        }
    }
}

/// Where edualcd's server takes the token its pages must show. The daemon is started without
/// it: nothing it runs (Claude Code, and what Claude Code runs) is to reach the server's pages.
pub const TOKEN_VARIABLE: &str = "EDUALC_TOKEN";

/// Starts the daemon, `program --daemon`, in a process group of its own (a terminal's hangup
/// doesn't reach it), its log in the private folder, without the server's token.
fn start_daemon(folder: &Folder, program: &Path) -> Result<(), ReachError> {
    let log = folder.log_file()?;
    let mut daemon = Command::new(program)
        .arg("--daemon")
        .env_remove(TOKEN_VARIABLE)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(log)
        .process_group(0)
        .spawn()
        .map_err(ReachError::Start)?;
    // Reaped when it ends, which is after this process, as a rule.
    std::thread::spawn(move || {
        if let Err(error) = daemon.wait() {
            tracing::debug!(%error, "couldn't wait for the daemon");
        }
    });
    Ok(())
}

fn wait_for_daemon(folder: &Folder) -> Result<UnixStream, ReachError> {
    let deadline = Instant::now() + DAEMON_START;
    loop {
        match connect(folder) {
            Ok(stream) => return Ok(stream),
            Err(error) if Instant::now() > deadline => {
                tracing::warn!(%error, "the daemon didn't come up");
                return Err(ReachError::NoDaemon);
            }
            Err(_not_yet) => std::thread::sleep(Duration::from_millis(20)),
        }
    }
}
