//! A native window's line to its edualcd (the Slint window's, the Android app's), which runs
//! Claude Code and does what the window can't: on this machine or another, the same way (see
//! `ipc`). The line is a WebSocket over TLS, edualcd's certificate pinned (a demo's is plain),
//! carried by a thread of its own with a small runtime: it writes what the window asks, in
//! order, and hands what edualcd says to the app as events (`link::Inbox`).
//!
//! A line the network loses is reached again, at the addresses looked up before the window was
//! confined (or looked up anew, for a window that isn't: see `Lookup`), waiting longer each
//! time it fails; the app is told it was lost
//! (`Event::DaemonLost`) and reached again (`Event::Reconnected`), and what it asked in between
//! is dropped: it opens its session again as the line comes back.

use std::net::SocketAddr;
use std::time::Duration;

use async_channel::{Receiver, Sender};
use futures_util::SinkExt as _;
use tokio_tungstenite::tungstenite::Message;

use crate::app::Event;
use crate::external::wire::SessionId;
use crate::ipc::pairing::Server;
use crate::ipc::{self, ActId, Answer, FromDaemon, PROTOCOL, Read, ToDaemon, WindowKey, ws};
use crate::link::{self, Greeting, Inbox, Link, QUEUE, Ungreeted};
use crate::sessions::Saved;

/// How often the window pings edualcd, and how long a line that says nothing, not even a pong
/// to edualcd's pings, is taken to be alive.
const PING: Duration = Duration::from_secs(15);
const SILENT: Duration = Duration::from_secs(45);
/// How long edualcd may take to greet a window once its line is open.
const GREETING: Duration = Duration::from_secs(10);
/// How long the window waits before reaching edualcd again, first and at most.
const FIRST_RETRY: Duration = Duration::from_secs(1);
const LAST_RETRY: Duration = Duration::from_secs(30);

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(transparent)]
    Line(#[from] ipc::Error),
    #[error("couldn't start the window's line to edualcd: {0}")]
    Runtime(#[source] std::io::Error),
    #[error(transparent)]
    Ungreeted(#[from] Ungreeted),
    #[error("edualcd answered another question")]
    Unanswered,
}

impl Error {
    /// Whether reaching edualcd again could go otherwise: not if it refused the window's token
    /// or version, nor shows another certificate.
    fn passes(&self) -> bool {
        match self {
            Self::Line(ipc::Error::Token | ipc::Error::Version(_) | ipc::Error::Certificate(_))
            | Self::Ungreeted(_) => false,
            Self::Line(_) | Self::Runtime(_) | Self::Unanswered => true,
        }
    }
}

/// Where the window reaches edualcd again, as its line is lost.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lookup {
    /// At the addresses looked up as it first connected: the Slint window, which is confined
    /// after that and can't ask DNS.
    Once,
    /// At those its name has then, looked up again each time (the old ones if it isn't
    /// found): the Android app, which isn't confined, on a phone that changes networks.
    EachTime,
}

/// A line to edualcd, greeted, its thread not started yet.
#[derive(Debug)]
pub struct Connected {
    runtime: tokio::runtime::Runtime,
    socket: ws::Line,
    /// What edualcd tells as the window connects.
    pub greeting: Greeting,
    /// Where edualcd was reached, looked up before the window was confined.
    pub addresses: Vec<SocketAddr>,
    server: Server,
    lookup: Lookup,
    /// Which window this is, the same each time its line is reached again.
    key: WindowKey,
    /// What edualcd said while the window waited for an answer (`ask`), for the app, in order.
    early: Vec<FromDaemon>,
}

impl Connected {
    /// Where edualcd keeps the session `id`'s transcript, as it says: the folder it was started
    /// in, if that is known, for a window that goes on with it.
    pub fn saved(&mut self, id: &SessionId) -> Result<Saved, Error> {
        let Answer::Saved(saved) = self.ask(Read::Saved(id.clone()))? else {
            return Err(Error::Unanswered);
        };
        Ok(saved)
    }

    /// Asks edualcd to read something, and waits for its answer: before the app runs, for
    /// where it is to work, which is on edualcd's machine.
    pub fn ask(&mut self, read: Read) -> Result<Answer, Error> {
        let request = ActId::default();
        let Self {
            runtime,
            socket,
            early,
            ..
        } = self;
        runtime.block_on(async {
            ws::send(socket, &ToDaemon::Read { request, read }).await?;
            loop {
                match ws::recv::<_, FromDaemon>(socket).await? {
                    FromDaemon::Answer {
                        request: answered,
                        answer,
                    } if answered == request => return Ok(answer),
                    message @ (FromDaemon::Hello { .. }
                    | FromDaemon::Started { .. }
                    | FromDaemon::Failed { .. }
                    | FromDaemon::Output { .. }
                    | FromDaemon::Ended { .. }
                    | FromDaemon::Diverged { .. }
                    | FromDaemon::Done { .. }
                    | FromDaemon::Trusted { .. }
                    | FromDaemon::Hosted(_)
                    | FromDaemon::Answer { .. }
                    | FromDaemon::Lines { .. }
                    | FromDaemon::Changed(_)) => early.push(message),
                }
            }
        })
    }
}

/// Connects to `server`: before the window is confined, which then can't ask DNS.
pub fn connect(server: &Server, lookup: Lookup) -> Result<Connected, Error> {
    let addresses = ws::resolve(server)?;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(Error::Runtime)?;
    let key = WindowKey::new();
    let (socket, greeting) = runtime.block_on(greet(server, &addresses, key))?;
    Ok(Connected {
        runtime,
        socket,
        greeting,
        addresses,
        server: server.clone(),
        lookup,
        key,
        early: Vec::new(),
    })
}

/// The handshakes and hello, then the sessions edualcd runs, which come first; each bounded
/// in time, so that a line that opens and then says nothing (the network changing just then,
/// say) is given up rather than waited on.
async fn greet(
    server: &Server,
    addresses: &[SocketAddr],
    key: WindowKey,
) -> Result<(ws::Line, Greeting), Error> {
    let mut socket = ws::connect(server, addresses).await?;
    let greeted = async {
        let hello = ToDaemon::Hello {
            protocol: PROTOCOL,
            window: key,
        };
        ws::send(&mut socket, &hello).await?;
        let hello = match ws::recv::<_, FromDaemon>(&mut socket).await {
            Ok(hello) => hello,
            // An edualcd of another version may speak otherwise, and hang up on ours.
            Err(ipc::Error::Closed | ipc::Error::Unreadable(_) | ipc::Error::WebSocket(_)) => {
                return Err(Ungreeted::Version.into());
            }
            Err(error) => return Err(error.into()),
        };
        let hosted = ws::recv::<_, FromDaemon>(&mut socket).await?;
        Ok::<_, Error>(link::greeted(hello, hosted)?)
    };
    let greeting = tokio::time::timeout(GREETING, greeted)
        .await
        .map_err(|_late| ipc::Error::Slow(server.authority()))??;
    Ok((socket, greeting))
}

/// Starts the thread that carries the line; what edualcd says comes to the app as `events`.
pub fn run(connected: Connected, events: Sender<Event>) -> (Link, Greeting) {
    let Connected {
        runtime,
        socket,
        greeting,
        mut addresses,
        server,
        lookup,
        key,
        early,
    } = connected;
    let (link, queue) = async_channel::bounded(QUEUE);
    std::thread::spawn(move || {
        runtime.block_on(async {
            let mut socket = socket;
            let mut early = early;
            loop {
                if !carry(socket, early, &queue, &events).await {
                    return;
                }
                if events.send(Event::DaemonLost).await.is_err() {
                    return;
                }
                let Some((again, greeting)) =
                    reach_again(&server, &mut addresses, lookup, key, &queue, &events).await
                else {
                    return;
                };
                let reconnected = Event::Reconnected {
                    hosted: greeting.hosted,
                    live: greeting.live,
                };
                if events.send(reconnected).await.is_err() {
                    return;
                }
                socket = again;
                early = Vec::new();
            }
        });
    });
    (link, greeting)
}

/// Reaches edualcd again, waiting longer after each failure, and dropping what the window asks
/// meanwhile; none once the window closed, or edualcd refuses it for good (the app is told
/// why).
async fn reach_again(
    server: &Server,
    addresses: &mut Vec<SocketAddr>,
    lookup: Lookup,
    key: WindowKey,
    queue: &Receiver<ToDaemon>,
    events: &Sender<Event>,
) -> Option<(ws::Line, Greeting)> {
    let mut wait = FIRST_RETRY;
    loop {
        let waited = tokio::time::sleep(wait);
        tokio::pin!(waited);
        loop {
            tokio::select! {
                () = &mut waited => break,
                asked = queue.recv() => if asked.is_err() {
                    // The window closed.
                    return None;
                },
            }
        }
        if lookup == Lookup::EachTime {
            match ws::lookup(server).await {
                Ok(found) if !found.is_empty() => *addresses = found,
                Ok(_) => {}
                Err(error) => tracing::info!(%error, "edualcd's name wasn't found again"),
            }
        }
        match greet(server, addresses, key).await {
            Ok(reached) => {
                tracing::info!("reached edualcd again");
                return Some(reached);
            }
            Err(error) if error.passes() => {
                tracing::info!(%error, ?wait, "edualcd not reached again yet");
                wait = (wait * 2).min(LAST_RETRY);
            }
            Err(error) => {
                tracing::warn!(%error, "edualcd refuses the window now");
                if events
                    .send(Event::SeatFailed(format!(
                        "edualcd isn't reached again: {error}"
                    )))
                    .await
                    .is_err()
                {
                    tracing::debug!("the window closed");
                }
                return None;
            }
        }
    }
}

/// Writes what is queued and reads what edualcd says, until the line is lost (true) or the
/// window closes (false). Each way is its own loop, the two polled together: a message waiting
/// for edualcd to read doesn't stop the window reading, which would leave both ends writing and
/// neither reading. The window pings edualcd, and takes a line silent for [`SILENT`] as lost.
async fn carry(
    socket: ws::Line,
    early: Vec<FromDaemon>,
    queue: &Receiver<ToDaemon>,
    events: &Sender<Event>,
) -> bool {
    let (mut writing, mut reading) = futures_util::StreamExt::split(socket);
    let write = async {
        let mut ping = tokio::time::interval(PING);
        loop {
            let sent = tokio::select! {
                message = queue.recv() => match message {
                    Ok(message) => ws::send(&mut writing, &message).await,
                    // The window closed.
                    Err(_closed) => return false,
                },
                _ = ping.tick() => writing
                    .send(Message::Ping(Vec::new().into()))
                    .await
                    .map_err(ipc::Error::from),
            };
            if let Err(error) = sent {
                tracing::warn!(%error, "lost edualcd");
                return true;
            }
        }
    };
    let read = async {
        let mut inbox = Inbox::default();
        for message in early {
            if let Some(event) = inbox.take(message)
                && events.send(event).await.is_err()
            {
                return false;
            }
        }
        loop {
            match tokio::time::timeout(SILENT, ws::recv::<_, FromDaemon>(&mut reading)).await {
                Ok(Ok(message)) => {
                    if let Some(event) = inbox.take(message)
                        && events.send(event).await.is_err()
                    {
                        return false;
                    }
                }
                Ok(Err(error)) => {
                    tracing::warn!(%error, "lost edualcd");
                    return true;
                }
                Err(_silent) => {
                    tracing::warn!("edualcd went silent: the line is taken as lost");
                    return true;
                }
            }
        }
    };
    tokio::select! {
        lost = write => lost,
        lost = read => lost,
    }
}
