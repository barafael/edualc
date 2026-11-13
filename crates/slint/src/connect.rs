//! The window's line to its daemon, which runs Claude Code and reads its files for the window.
//! The daemon (`edualcd`, beside this program) is started if none runs, and
//! checked as it checks the window. Two threads carry the line over its Unix socket: one writes
//! what the window asks, in order; the other reads what the daemon says and hands it to the app
//! as events (`link::Inbox`).

use std::os::unix::process::CommandExt;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use async_channel::Sender;

use crate::app::Event;
use crate::ipc::{self, Channel, Deframer, Folder, FromDaemon, PROTOCOL, ToDaemon};
use crate::link::{Greeting, Inbox, Link, QUEUE};

/// How long a daemon just started gets to listen.
const DAEMON_START: Duration = Duration::from_secs(3);

#[derive(Debug, thiserror::Error)]
pub(crate) enum Error {
    #[error(transparent)]
    Ipc(#[from] ipc::Error),
    #[error("couldn't start the edualc daemon: {0}")]
    Start(#[source] std::io::Error),
    #[error("the edualc daemon didn't come up")]
    NoDaemon,
    #[error(
        "the edualc daemon running is another version; it ends once its windows and sessions do"
    )]
    Version,
    #[error("the edualc daemon said something out of turn as the window connected")]
    OutOfTurn,
}

/// A line to the daemon, greeted, its threads not started yet.
#[derive(Debug)]
pub(crate) struct Connected {
    pub(crate) line: Line,
    /// What the daemon tells as the window connects.
    pub(crate) greeting: Greeting,
}

/// The line itself, greeted, its threads not started yet.
#[derive(Debug)]
pub(crate) struct Line {
    channel: Channel,
    deframer: Deframer<FromDaemon>,
}

/// Connects to the daemon, starting it if none runs.
pub(crate) fn connect() -> Result<Connected, Error> {
    let folder = Folder::open()?;
    let mut channel = match Channel::connect(&folder) {
        Ok(channel) => channel,
        Err(error) => {
            tracing::info!(%error, "starting the edualc daemon");
            start_daemon(&folder)?;
            wait_for_daemon(&folder)?
        }
    };
    let mut deframer = Deframer::default();
    channel.send(&ToDaemon::Hello { protocol: PROTOCOL })?;
    let greeting = match channel.recv(&mut deframer) {
        Ok(greeting) => greeting,
        // A daemon of another version may frame messages otherwise, and hang up on ours.
        Err(ipc::Error::Closed | ipc::Error::Oversized | ipc::Error::Unreadable) => {
            return Err(Error::Version);
        }
        Err(error) => return Err(error.into()),
    };
    let (settings, openers, live, finds_windows, local) = match greeting {
        FromDaemon::Hello {
            protocol,
            settings,
            openers,
            live,
            finds_windows,
            local,
        } if protocol == PROTOCOL => (settings, openers, live, finds_windows, local),
        FromDaemon::Hello { .. }
        | FromDaemon::Started { .. }
        | FromDaemon::Failed { .. }
        | FromDaemon::Output { .. }
        | FromDaemon::Ended { .. }
        | FromDaemon::Done { .. }
        | FromDaemon::Picked { .. }
        | FromDaemon::Trusted { .. }
        | FromDaemon::Hosted(_)
        | FromDaemon::ShuttingDown
        | FromDaemon::Answer { .. }
        | FromDaemon::Lines { .. }
        | FromDaemon::Changed(_)
        | FromDaemon::Pasted { .. } => return Err(Error::Version),
    };
    let hosted = match channel.recv(&mut deframer)? {
        FromDaemon::Hosted(hosted) => hosted,
        FromDaemon::Hello { .. }
        | FromDaemon::Started { .. }
        | FromDaemon::Failed { .. }
        | FromDaemon::Output { .. }
        | FromDaemon::Ended { .. }
        | FromDaemon::Done { .. }
        | FromDaemon::Picked { .. }
        | FromDaemon::Trusted { .. }
        | FromDaemon::ShuttingDown
        | FromDaemon::Answer { .. }
        | FromDaemon::Lines { .. }
        | FromDaemon::Changed(_)
        | FromDaemon::Pasted { .. } => return Err(Error::OutOfTurn),
    };
    let greeting = Greeting {
        hosted,
        live,
        settings,
        openers,
        finds_windows,
        local,
    };
    Ok(Connected {
        line: Line { channel, deframer },
        greeting,
    })
}

/// Starts the threads that carry the line; what the daemon says comes to the app as `events`.
pub(crate) fn run(line: Line, events: Sender<Event>) -> Result<Link, Error> {
    let Line {
        mut channel,
        deframer,
    } = line;
    let reading = channel.try_clone()?;
    let (link, queue) = std::sync::mpsc::sync_channel(QUEUE);
    std::thread::spawn(move || {
        for message in queue {
            if let Err(error) = channel.send(&message) {
                tracing::warn!(%error, "lost the daemon");
                break;
            }
        }
    });
    std::thread::spawn(move || read(reading, deframer, &events));
    Ok(link)
}

/// Starts the daemon, `edualcd` beside this program, in a process group of its own (a
/// terminal's hangup doesn't reach it), its log in the private folder.
fn start_daemon(folder: &Folder) -> Result<(), Error> {
    let program = std::env::current_exe()
        .map_err(Error::Start)?
        .with_file_name("edualcd");
    let log = folder.log_file()?;
    let mut daemon = Command::new(program)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(log)
        .process_group(0)
        .spawn()
        .map_err(Error::Start)?;
    // Reaped when it ends, which is after this window, as a rule.
    std::thread::spawn(move || {
        if let Err(error) = daemon.wait() {
            tracing::debug!(%error, "couldn't wait for the daemon");
        }
    });
    Ok(())
}

fn wait_for_daemon(folder: &Folder) -> Result<Channel, Error> {
    let deadline = Instant::now() + DAEMON_START;
    loop {
        match Channel::connect(folder) {
            Ok(channel) => return Ok(channel),
            Err(error) if Instant::now() > deadline => {
                tracing::warn!(%error, "the daemon didn't come up");
                return Err(Error::NoDaemon);
            }
            Err(_not_yet) => std::thread::sleep(Duration::from_millis(20)),
        }
    }
}

/// What the daemon says, as the app's events, until the line is lost.
fn read(mut channel: Channel, mut deframer: Deframer<FromDaemon>, events: &Sender<Event>) {
    let mut inbox = Inbox::default();
    loop {
        let message = match channel.recv(&mut deframer) {
            Ok(message) => message,
            Err(error) => {
                tracing::warn!(%error, "lost the daemon");
                if events.send_blocking(Event::DaemonLost).is_err() {
                    tracing::debug!("the window closed with the daemon");
                }
                return;
            }
        };
        if let Some(event) = inbox.take(message)
            && events.send_blocking(event).is_err()
        {
            return;
        }
    }
}
