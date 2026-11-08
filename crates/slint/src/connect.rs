//! The window's line to its daemon, which runs Claude Code and does what the window's sandbox
//! keeps it from. The daemon (`edualcd`, beside this program) is started if none runs, and
//! checked as it checks the window. The line is a WebSocket on the daemon's Unix socket (see
//! `ipc`), carried by a thread of its own with a small runtime: it writes what the window asks,
//! in order, and hands what the daemon says to the app as events (`link::Inbox`).

use std::os::unix::process::CommandExt;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use async_channel::{Receiver, Sender};
use tokio_tungstenite::WebSocketStream;

use crate::app::Event;
use crate::ipc::{self, Buses, Folder, FromDaemon, PROTOCOL, ToDaemon, ws};
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

pub(crate) type Socket = WebSocketStream<tokio::net::UnixStream>;

/// A line to the daemon, greeted, its thread not started yet.
#[derive(Debug)]
pub(crate) struct Connected {
    pub(crate) runtime: tokio::runtime::Runtime,
    pub(crate) socket: Socket,
    /// The D-Bus the window is to use.
    pub(crate) buses: Buses,
    /// What the daemon tells as the window connects.
    pub(crate) greeting: Greeting,
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
    let (socket, buses, greeting) = runtime.block_on(greet(stream))?;
    Ok(Connected {
        runtime,
        socket,
        buses,
        greeting,
    })
}

/// The handshake and hello, then the sessions the daemon runs, which come first.
async fn greet(stream: std::os::unix::net::UnixStream) -> Result<(Socket, Buses, Greeting), Error> {
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
    let FromDaemon::Hello {
        protocol,
        buses,
        settings,
        openers,
        live,
        claude_codes_sandbox,
        finds_windows,
    } = greeting
    else {
        return Err(Error::Version);
    };
    if protocol != PROTOCOL {
        return Err(Error::Version);
    }
    let FromDaemon::Hosted(hosted) = ws::recv::<_, FromDaemon>(&mut socket).await? else {
        return Err(Error::OutOfTurn);
    };
    let greeting = Greeting {
        hosted,
        live,
        settings,
        openers,
        claude_codes_sandbox,
        finds_windows,
    };
    Ok((socket, buses, greeting))
}

/// Starts the thread that carries the line; what the daemon says comes to the app as `events`.
pub(crate) fn run(runtime: tokio::runtime::Runtime, socket: Socket, events: Sender<Event>) -> Link {
    let (link, queue) = async_channel::bounded(QUEUE);
    std::thread::spawn(move || runtime.block_on(pump(socket, &queue, &events)));
    link
}

/// Writes what is queued and reads what the daemon says, until the line is lost or the window
/// closes.
async fn pump(mut socket: Socket, queue: &Receiver<ToDaemon>, events: &Sender<Event>) {
    let mut inbox = Inbox::default();
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
                    if let Some(event) = inbox.take(message)
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
