//! The window's line to its daemon, which runs Claude Code and does what the window's sandbox
//! keeps it from. The daemon is started if none runs, and checked as it checks the window. The
//! line is a WebSocket on the daemon's Unix socket (see `ipc`), carried by a thread of its own
//! with a small runtime: it writes what the window asks, in order, and hands what the daemon
//! says to the app as events (`event_of`).

use std::os::unix::process::CommandExt;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use async_channel::{Receiver, Sender, TrySendError};
use tokio_tungstenite::WebSocketStream;

use crate::app::Event;
use crate::claude::Output;
use crate::ipc::{
    self, Buses, Folder, FromDaemon, Generation, Hosted, PROTOCOL, Stream, ToDaemon, ws,
};

/// What the window asks of the daemon goes this way, in order. Bounded: a daemon that stopped
/// taking messages loses the line rather than the window its memory.
pub(crate) type Link = Sender<ToDaemon>;

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
        Err(TrySendError::Full(_) | TrySendError::Closed(_)) => Err(Lost),
    }
}

/// A line for a session's input.
pub(crate) fn send_input(link: &Link, key: Generation, line: &str) -> Result<(), Lost> {
    let line = line.to_owned();
    send(link, ToDaemon::Input { key, line })
}

type Socket = WebSocketStream<tokio::net::UnixStream>;

/// A line to the daemon, greeted, its thread not started yet.
#[derive(Debug)]
pub(crate) struct Connected {
    runtime: tokio::runtime::Runtime,
    socket: Socket,
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
    let stream = match ipc::connect(&folder) {
        Ok(stream) => stream,
        Err(error) => {
            tracing::info!(%error, "starting the edualc daemon");
            start_daemon(&folder)?;
            wait_for_daemon(&folder)?
        }
    };
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(Error::Start)?;
    let (socket, buses, hosted) = runtime.block_on(greet(stream))?;
    Ok(Connected {
        runtime,
        socket,
        buses,
        hosted,
    })
}

/// The handshake and hello, then the sessions the daemon runs, which come first.
async fn greet(
    stream: std::os::unix::net::UnixStream,
) -> Result<(Socket, Buses, Vec<Hosted>), Error> {
    stream.set_nonblocking(true).map_err(ipc::Error::Io)?;
    let stream = tokio::net::UnixStream::from_std(stream).map_err(ipc::Error::Io)?;
    let mut socket = ws::connect(stream).await?;
    ws::send(&mut socket, &ToDaemon::Hello { protocol: PROTOCOL }).await?;
    let greeting = match ws::recv::<_, FromDaemon>(&mut socket).await {
        Ok(greeting) => greeting,
        // A daemon of another version may speak otherwise, and hang up on ours.
        Err(ipc::Error::Closed | ipc::Error::Unreadable(_) | ipc::Error::WebSocket(_)) => {
            return Err(Error::Version);
        }
        Err(error) => return Err(error.into()),
    };
    let FromDaemon::Hello { protocol, buses } = greeting else {
        return Err(Error::Version);
    };
    if protocol != PROTOCOL {
        return Err(Error::Version);
    }
    let FromDaemon::Hosted(hosted) = ws::recv::<_, FromDaemon>(&mut socket).await? else {
        return Err(Error::OutOfTurn);
    };
    Ok((socket, buses, hosted))
}

/// Starts the thread that carries the line; what the daemon says comes to the app as `events`.
pub(crate) fn run(connected: Connected, events: Sender<Event>) -> Result<Link, Error> {
    let Connected {
        runtime, socket, ..
    } = connected;
    let (link, queue) = async_channel::bounded(QUEUE);
    std::thread::spawn(move || runtime.block_on(pump(socket, &queue, &events)));
    Ok(link)
}

/// Writes what is queued and reads what the daemon says, until the line is lost or the window
/// closes.
async fn pump(mut socket: Socket, queue: &Receiver<ToDaemon>, events: &Sender<Event>) {
    loop {
        tokio::select! {
            message = queue.recv() => match message {
                Ok(message) => {
                    if let Err(error) = ws::send(&mut socket, &message).await {
                        tracing::warn!(%error, "lost the daemon");
                        break;
                    }
                }
                // The window closed.
                Err(_closed) => return,
            },
            message = ws::recv::<_, FromDaemon>(&mut socket) => match message {
                Ok(message) => {
                    if let Some(event) = event_of(message)
                        && events.send(event).await.is_err()
                    {
                        return;
                    }
                }
                Err(error) => {
                    tracing::warn!(%error, "lost the daemon");
                    break;
                }
            },
        }
    }
    if events.send(Event::DaemonLost).await.is_err() {
        tracing::debug!("the window closed with the daemon");
    }
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

fn wait_for_daemon(folder: &Folder) -> Result<std::os::unix::net::UnixStream, Error> {
    let deadline = Instant::now() + DAEMON_START;
    loop {
        match ipc::connect(folder) {
            Ok(stream) => return Ok(stream),
            Err(error) if Instant::now() > deadline => {
                tracing::warn!(%error, "the daemon didn't come up");
                return Err(Error::NoDaemon);
            }
            Err(_not_yet) => std::thread::sleep(Duration::from_millis(20)),
        }
    }
}

/// What the daemon says, as the app's event.
pub(crate) fn event_of(message: FromDaemon) -> Option<Event> {
    Some(match message {
        FromDaemon::Started {
            key,
            pid,
            enforcement,
            turns,
        } => claude(
            key,
            Output::Started {
                pid,
                enforcement,
                turns,
            },
        ),
        FromDaemon::Failed { key, failure } => claude(key, Output::Failed(failure)),
        FromDaemon::Output { key, stream, line } => claude(
            key,
            match stream {
                Stream::Out => Output::of_line(line),
                Stream::Err => Output::Log(line),
            },
        ),
        FromDaemon::Ended { key, code } => claude(key, Output::Ended(code)),
        FromDaemon::Diverged { key, left } => claude(key, Output::Diverged(left)),
        FromDaemon::Done { request, result } => Event::Acted { request, result },
        FromDaemon::Picked { request, folder } => Event::Picked { request, folder },
        FromDaemon::Trusted {
            request,
            accepted,
            runs,
        } => Event::Trusted {
            request,
            accepted,
            runs,
        },
        FromDaemon::Hosted(sessions) => Event::Hosted(sessions),
        FromDaemon::Pasted { request, pasted } => Event::Pasted { request, pasted },
        FromDaemon::Hello { .. } => return None,
    })
}

fn claude(generation: Generation, output: Output) -> Event {
    Event::Claude { generation, output }
}
