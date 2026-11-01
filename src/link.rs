//! The window's line to its daemon, which runs Claude Code and does what the window's sandbox
//! keeps it from. The daemon is started if none runs, and checked as it checks the window. Two
//! threads carry the line: one writes what the window asks, in order; the other reads what the
//! daemon says, puts lines together again (sans-io, `ipc::Assembler`) and hands them to the app.

use std::os::unix::process::CommandExt;
use std::process::{Command, Stdio};
use std::sync::mpsc::{SyncSender, TrySendError};
use std::time::{Duration, Instant};

use async_channel::Sender;

use crate::app::{Event, Generation};
use crate::buses::Buses;
use crate::claude::Output;
use crate::ipc::{
    self, Assembler, Channel, Deframer, Folder, FromDaemon, PROTOCOL, Stream, ToDaemon,
};

/// What the window asks of the daemon goes this way, in order. Bounded: a daemon that stopped
/// taking messages loses the line rather than the window its memory.
pub(crate) type Link = SyncSender<ToDaemon>;

/// Messages waiting to be written to the daemon.
const QUEUE: usize = 1024;
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
}

/// The line was lost: the daemon ended, or stopped taking messages.
#[derive(Debug)]
pub(crate) struct Lost;

/// Sends a message the window doesn't wait on.
pub(crate) fn send(link: &Link, message: ToDaemon) -> Result<(), Lost> {
    match link.try_send(message) {
        Ok(()) => Ok(()),
        Err(TrySendError::Full(_) | TrySendError::Disconnected(_)) => Err(Lost),
    }
}

/// A line for a session's input, in pieces that each fit a message.
pub(crate) fn send_input(link: &Link, key: u64, line: &str) -> Result<(), Lost> {
    let mut pieces = ipc::chunks(line).peekable();
    while let Some(text) = pieces.next() {
        let last = pieces.peek().is_none();
        send(
            link,
            ToDaemon::Input {
                key,
                text: text.to_owned(),
                last,
            },
        )?;
    }
    Ok(())
}

/// A line to the daemon, greeted, its threads not started yet.
#[derive(Debug)]
pub(crate) struct Connected {
    channel: Channel,
    deframer: Deframer,
    /// The D-Bus the window is to use.
    pub(crate) buses: Buses,
}

/// Connects to the daemon, starting it if none runs: before the window is confined, which the
/// daemon must not be.
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
    let buses = match channel.recv(&mut deframer)? {
        FromDaemon::Hello { protocol, buses } if protocol == PROTOCOL => buses,
        FromDaemon::Hello { .. }
        | FromDaemon::Started { .. }
        | FromDaemon::Failed { .. }
        | FromDaemon::Output { .. }
        | FromDaemon::Ended { .. }
        | FromDaemon::Done { .. }
        | FromDaemon::Picked { .. } => return Err(Error::Version),
    };
    Ok(Connected {
        channel,
        deframer,
        buses,
    })
}

/// Starts the threads that carry the line; what the daemon says comes to the app as `events`.
pub(crate) fn run(connected: Connected, events: Sender<Event>) -> Result<Link, Error> {
    let Connected {
        mut channel,
        deframer,
        ..
    } = connected;
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

/// Starts `edualc --daemon` beside this window, in a process group of its own (a terminal's
/// hangup doesn't reach it), its log in the private folder.
fn start_daemon(folder: &Folder) -> Result<(), Error> {
    let program = std::env::current_exe().map_err(Error::Start)?;
    let log = folder.log_file()?;
    let mut daemon = Command::new(program)
        .arg("--daemon")
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
fn read(mut channel: Channel, mut deframer: Deframer, events: &Sender<Event>) {
    let mut lines: Assembler<(u64, Stream)> = Assembler::default();
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
        let event = match message {
            FromDaemon::Started {
                key,
                pid,
                enforcement,
            } => Some(claude(key, Output::Started { pid, enforcement })),
            FromDaemon::Failed { key, failure } => Some(claude(key, Output::Failed(failure))),
            FromDaemon::Output {
                key,
                stream,
                text,
                last,
            } => match lines.push((key, stream), &text, last) {
                Ok(Some(line)) => Some(claude(
                    key,
                    match stream {
                        Stream::Out => Output::of_line(line),
                        Stream::Err => Output::Log(line),
                    },
                )),
                Ok(None) => None,
                Err(error) => {
                    tracing::warn!(%error, "a line from claude too long to keep");
                    None
                }
            },
            FromDaemon::Ended { key, code } => {
                lines.forget(&(key, Stream::Out));
                lines.forget(&(key, Stream::Err));
                Some(claude(key, Output::Ended(code)))
            }
            FromDaemon::Done { request, result } => Some(Event::Acted { request, result }),
            FromDaemon::Picked { request, folder } => Some(Event::Picked { request, folder }),
            FromDaemon::Hello { .. } => None,
        };
        if let Some(event) = event
            && events.send_blocking(event).is_err()
        {
            return;
        }
    }
}

fn claude(key: u64, output: Output) -> Event {
    Event::Claude {
        generation: Generation(key),
        output,
    }
}
