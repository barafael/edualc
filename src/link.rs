//! The window's line to its daemon, which runs Claude Code and does what the window's sandbox
//! keeps it from. The daemon is started if none runs, and checked as it checks the window. Two
//! threads carry the line: one writes what the window asks, in order; the other reads what the
//! daemon says, puts lines together again (sans-io, `ipc::Assembler`) and hands them to the app.

use std::collections::BTreeMap;
use std::os::unix::process::CommandExt;
use std::process::{Command, Stdio};
use std::sync::mpsc::{SyncSender, TrySendError};
use std::time::{Duration, Instant};

use async_channel::Sender;

use crate::app::Event;
use crate::buses::Buses;
use crate::claude::Output;
use crate::decode;
use crate::ipc::{
    self, ActId, Assembler, Channel, Deframer, Folder, FromDaemon, Generation, Hosted, PROTOCOL,
    PasteFailure, Pasted, Stream, ToDaemon,
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
    #[error("the edualc daemon said something out of turn as the window connected")]
    OutOfTurn,
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
pub(crate) fn send_input(link: &Link, key: Generation, line: &str) -> Result<(), Lost> {
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
    deframer: Deframer<FromDaemon>,
    /// The D-Bus the window is to use.
    pub(crate) buses: Buses,
    /// The sessions the daemon runs as the window connected: known before the window opens
    /// one, so that one it runs is attached to rather than started a second time.
    pub(crate) hosted: Vec<Hosted>,
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
    let greeting = match channel.recv(&mut deframer) {
        Ok(greeting) => greeting,
        // A daemon of another version may frame messages otherwise, and hang up on ours.
        Err(ipc::Error::Closed | ipc::Error::Oversized | ipc::Error::Unreadable) => {
            return Err(Error::Version);
        }
        Err(error) => return Err(error.into()),
    };
    let buses = match greeting {
        FromDaemon::Hello { protocol, buses } if protocol == PROTOCOL => buses,
        FromDaemon::Hello { .. }
        | FromDaemon::Started { .. }
        | FromDaemon::Failed { .. }
        | FromDaemon::Output { .. }
        | FromDaemon::Ended { .. }
        | FromDaemon::Done { .. }
        | FromDaemon::Picked { .. }
        | FromDaemon::Hosted(_)
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
        | FromDaemon::Pasted { .. } => return Err(Error::OutOfTurn),
    };
    Ok(Connected {
        channel,
        deframer,
        buses,
        hosted,
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
fn read(mut channel: Channel, mut deframer: Deframer<FromDaemon>, events: &Sender<Event>) {
    let mut lines: Assembler<(Generation, Stream)> = Assembler::default();
    let mut pasting: BTreeMap<ActId, Joining> = BTreeMap::new();
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
            FromDaemon::Hosted(sessions) => Some(Event::Hosted(sessions)),
            FromDaemon::Pasted {
                request,
                piece,
                last,
            } => joined(&mut pasting, request, &piece, last)
                .map(|pasted| Event::Pasted { request, pasted }),
            FromDaemon::Hello { .. } => None,
        };
        if let Some(event) = event
            && events.send_blocking(event).is_err()
        {
            return;
        }
    }
}

fn claude(generation: Generation, output: Output) -> Event {
    Event::Claude { generation, output }
}

/// A pasted image's pieces, as they come.
#[derive(Debug)]
enum Joining {
    Bytes(Vec<u8>),
    /// More came than an image comes to: told once, the rest dropped.
    Overflowed,
}

/// A pasted image, once its last piece came (sans-io); `None` while pieces come, and for the
/// rest of one that overflowed.
fn joined(
    pasting: &mut BTreeMap<ActId, Joining>,
    request: ActId,
    piece: &[u8],
    last: bool,
) -> Option<Pasted> {
    let joining = pasting
        .entry(request)
        .or_insert_with(|| Joining::Bytes(Vec::new()));
    let told = match joining {
        Joining::Bytes(bytes) if bytes.len() + piece.len() > decode::MAX_ANSWER_BYTES => {
            *joining = Joining::Overflowed;
            Some(Err(PasteFailure::Decoder(
                "the image came in more pieces than an image comes to".to_owned(),
            )))
        }
        Joining::Bytes(bytes) => {
            bytes.extend_from_slice(piece);
            None
        }
        Joining::Overflowed => None,
    };
    if !last {
        return told;
    }
    match pasting.remove(&request) {
        Some(Joining::Bytes(bytes)) => Some(
            postcard::from_bytes::<Pasted>(&bytes)
                .unwrap_or_else(|error| Err(PasteFailure::Decoder(error.to_string()))),
        ),
        Some(Joining::Overflowed) | None => told,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A pasted image's pieces come together in order; too many are told of once and the
    /// rest dropped; bytes that don't parse are an error, not a panic.
    #[test]
    fn pasted_pieces_join_once_and_no_further() {
        let mut pasting = BTreeMap::new();
        let request = ActId::default().next();
        let sent: Pasted = Err(PasteFailure::NoImage);
        let bytes = postcard::to_stdvec(&sent).expect("encodes");
        let (first, rest) = bytes.split_at(1);
        assert_eq!(joined(&mut pasting, request, first, false), None);
        assert_eq!(joined(&mut pasting, request, rest, true), Some(sent));
        assert!(pasting.is_empty());

        let huge = vec![0u8; decode::MAX_ANSWER_BYTES + 1];
        assert!(matches!(
            joined(&mut pasting, request, &huge, false),
            Some(Err(PasteFailure::Decoder(_)))
        ));
        assert_eq!(joined(&mut pasting, request, b"more", false), None);
        assert_eq!(
            joined(&mut pasting, request, b"end", true),
            None,
            "told once"
        );
        assert!(pasting.is_empty());

        assert!(matches!(
            joined(&mut pasting, request, &[0xff, 0xff], true),
            Some(Err(PasteFailure::Decoder(_)))
        ));
    }
}
