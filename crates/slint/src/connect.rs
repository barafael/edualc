//! The window's line to its daemon, which runs Claude Code and does what the window's sandbox
//! keeps it from. The daemon (`edualcd`, beside this program) is started if none runs, and
//! checked as it checks the window (`ipc::reach`). The line is a WebSocket on the daemon's Unix
//! socket (see `ipc`), carried by a thread of its own with a small runtime: it writes what the
//! window asks, in order, and hands what the daemon says to the app as events (`link::Inbox`).

use async_channel::{Receiver, Sender};
use tokio_tungstenite::WebSocketStream;

use crate::app::Event;
use crate::ipc::{self, Buses, FromDaemon, PROTOCOL, ReachError, ToDaemon, ws};
use crate::link::{self, Greeting, Inbox, Link, QUEUE, Ungreeted};

#[derive(Debug, thiserror::Error)]
pub(crate) enum Error {
    #[error(transparent)]
    Reach(#[from] ReachError),
    #[error(transparent)]
    Ipc(#[from] ipc::Error),
    #[error("couldn't start the window's line to the daemon: {0}")]
    Runtime(#[source] std::io::Error),
    #[error(transparent)]
    Ungreeted(#[from] Ungreeted),
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
    // `edualcd`, installed beside the window.
    let program = std::env::current_exe()
        .map_err(Error::Runtime)?
        .with_file_name("edualcd");
    let stream = ipc::reach(&program)?;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(Error::Runtime)?;
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
    let hello = match ws::recv::<_, FromDaemon>(&mut socket).await {
        Ok(hello) => hello,
        // A daemon of another version may speak otherwise, and hang up on ours.
        Err(ipc::Error::Closed | ipc::Error::Unreadable(_) | ipc::Error::WebSocket(_)) => {
            return Err(Ungreeted::Version.into());
        }
        Err(error) => return Err(error.into()),
    };
    let hosted = ws::recv::<_, FromDaemon>(&mut socket).await?;
    let (buses, greeting) = link::greeted(hello, hosted)?;
    Ok((socket, buses, greeting))
}

/// Starts the thread that carries the line; what the daemon says comes to the app as `events`.
pub(crate) fn run(runtime: tokio::runtime::Runtime, socket: Socket, events: Sender<Event>) -> Link {
    let (link, queue) = async_channel::bounded(QUEUE);
    std::thread::spawn(move || runtime.block_on(pump(socket, &queue, &events)));
    link
}

/// Writes what is queued and reads what the daemon says, until the line is lost or the window
/// closes. Each way is its own loop, the two polled together: a message waiting for the daemon
/// to read doesn't stop the window reading, which would leave both ends writing and neither
/// reading.
async fn pump(socket: Socket, queue: &Receiver<ToDaemon>, events: &Sender<Event>) {
    let (mut writing, mut reading) = futures_util::StreamExt::split(socket);
    // Each says whether the daemon was lost (rather than the window closed).
    let write = async {
        loop {
            match queue.recv().await {
                Ok(message) => {
                    if let Err(error) = ws::send(&mut writing, &message).await {
                        tracing::warn!(%error, "lost the daemon");
                        return true;
                    }
                }
                // The window closed.
                Err(_closed) => return false,
            }
        }
    };
    let read = async {
        let mut inbox = Inbox::default();
        loop {
            match ws::recv::<_, FromDaemon>(&mut reading).await {
                Ok(message) => {
                    if let Some(event) = inbox.take(message)
                        && events.send(event).await.is_err()
                    {
                        return false;
                    }
                }
                Err(error) => {
                    tracing::warn!(%error, "lost the daemon");
                    return true;
                }
            }
        }
    };
    let lost = tokio::select! {
        lost = write => lost,
        lost = read => lost,
    };
    if lost && events.send(Event::DaemonLost).await.is_err() {
        tracing::debug!("the window closed with the daemon");
    }
}
