//! A native window's line to edualcd over the network (the Android app's), which runs Claude
//! Code on edualcd's machine and does what the window can't. The line is a WebSocket over TLS,
//! edualcd's certificate pinned (a demo's is plain; see `ipc::ws`), carried by a thread of its
//! own with a small runtime: it writes what the window asks, in order, and hands what edualcd
//! says to the app as events (`link::Inbox`).
//!
//! A line the network loses is reached again, its name looked up anew each time (a phone
//! changes networks), waiting longer each time it fails; the app is told it was lost
//! (`Event::LineLost`) and reached again (`Event::Reconnected`), and what it asked in between
//! is dropped: it opens its session again as the line comes back. The sessions it ran there
//! run on meanwhile, and are attached to again (see the daemon's hub).

use std::net::SocketAddr;
use std::time::Duration;

use async_channel::{Receiver, Sender};
use futures_util::SinkExt as _;
use tokio_tungstenite::tungstenite::Message;

use crate::app::Event;
use crate::external::wire::SessionId;
use crate::ipc::pairing::Server;
use crate::ipc::{self, ActId, Answer, Deframer, FromDaemon, PROTOCOL, Read, ToDaemon, ws};
use crate::link::{self, Greeting, Inbox, Link, NotGreeted, QUEUE};
use crate::sessions::Saved;

/// How often the window pings edualcd, and how long a line that says nothing, not even a pong
/// to edualcd's pings, is taken to be alive.
const PING: Duration = Duration::from_secs(15);
const SILENT: Duration = Duration::from_secs(45);
/// How long edualcd may take to greet a window once its line is open.
const GREETING: Duration = Duration::from_secs(10);
/// How long edualcd may take to answer what a window asks before it opens (`Connected::ask`).
const ANSWER: Duration = Duration::from_secs(30);
/// How long the window waits before reaching edualcd again, first and at most.
const FIRST_RETRY: Duration = Duration::from_secs(1);
const LAST_RETRY: Duration = Duration::from_secs(30);

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(transparent)]
    Line(#[from] ws::Error),
    #[error("couldn't start the window's line to edualcd: {0}")]
    Runtime(#[source] std::io::Error),
    #[error(transparent)]
    Ungreeted(#[from] NotGreeted),
    #[error("edualcd answered another question")]
    Unanswered,
}

impl Error {
    /// Whether reaching edualcd again could go otherwise: not if it refused the window's token
    /// or version, nor shows another certificate.
    fn passes(&self) -> bool {
        match self {
            Self::Line(ws::Error::Token | ws::Error::Version(_) | ws::Error::Certificate(_))
            | Self::Ungreeted(_) => false,
            Self::Line(_) | Self::Runtime(_) | Self::Unanswered => true,
        }
    }
}

/// A line to edualcd, greeted, its thread not started yet.
#[derive(Debug)]
pub struct Connected {
    runtime: tokio::runtime::Runtime,
    socket: ws::Line,
    deframer: Deframer<FromDaemon>,
    /// What edualcd tells as the window connects.
    pub greeting: Greeting,
    addresses: Vec<SocketAddr>,
    server: Server,
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

    /// Asks edualcd to read something, and waits for its answer, up to [`ANSWER`]: before the
    /// app runs, for where it is to work, which is on edualcd's machine. A line gone silent
    /// meanwhile isn't waited on for ever.
    pub fn ask(&mut self, read: Read) -> Result<Answer, Error> {
        let request = ActId::default();
        let Self {
            runtime,
            socket,
            deframer,
            early,
            server,
            ..
        } = self;
        let asked = async {
            ws::send(socket, &ToDaemon::Read { request, read }).await?;
            loop {
                match ws::recv(socket, deframer).await? {
                    FromDaemon::Answer {
                        request: answered,
                        answer,
                    } if answered == request => return Ok::<_, Error>(answer),
                    message @ (FromDaemon::Hello { .. }
                    | FromDaemon::Started { .. }
                    | FromDaemon::Failed { .. }
                    | FromDaemon::Output { .. }
                    | FromDaemon::Ended { .. }
                    | FromDaemon::Done { .. }
                    | FromDaemon::Picked { .. }
                    | FromDaemon::Trusted { .. }
                    | FromDaemon::Hosted(_)
                    | FromDaemon::ShuttingDown
                    | FromDaemon::Pasted { .. }
                    | FromDaemon::Answer { .. }
                    | FromDaemon::Lines { .. }
                    | FromDaemon::Changed(_)) => early.push(message),
                }
            }
        };
        runtime.block_on(async {
            tokio::time::timeout(ANSWER, asked)
                .await
                .map_err(|_late| Error::from(ws::Error::Slow(server.authority())))?
        })
    }
}

/// Connects to `server`. Blocking: on a thread of the window's own.
pub fn connect(server: &Server) -> Result<Connected, Error> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(Error::Runtime)?;
    let (addresses, socket, deframer, greeting) = runtime.block_on(async {
        let addresses = ws::lookup(server).await?;
        let (socket, deframer, greeting) = greet(server, &addresses).await?;
        Ok::<_, Error>((addresses, socket, deframer, greeting))
    })?;
    Ok(Connected {
        runtime,
        socket,
        deframer,
        greeting,
        addresses,
        server: server.clone(),
        early: Vec::new(),
    })
}

/// The handshakes and the token, the hello, then the sessions edualcd runs, which come first;
/// each bounded in time, so that a line that opens and then says nothing (the network changing
/// just then, say) is given up rather than waited on.
async fn greet(
    server: &Server,
    addresses: &[SocketAddr],
) -> Result<(ws::Line, Deframer<FromDaemon>, Greeting), Error> {
    let mut socket = ws::connect(server, addresses).await?;
    let mut deframer = Deframer::default();
    let greeted = async {
        ws::send(&mut socket, &ToDaemon::Hello { protocol: PROTOCOL }).await?;
        let hello = match ws::recv(&mut socket, &mut deframer).await {
            Ok(hello) => hello,
            // An edualcd of another version may speak otherwise. One that only hangs up, or a
            // line that breaks, is the network's doing (edualcd refuses another version before
            // the line opens, HTTP 409): reached again, not given up on for good.
            Err(ws::Error::Frame(ipc::Error::Unreadable)) => {
                return Err(NotGreeted::Version.into());
            }
            Err(error) => return Err(error.into()),
        };
        let hosted = ws::recv(&mut socket, &mut deframer).await?;
        Ok::<_, Error>(link::greeted(hello, hosted)?)
    };
    let greeting = tokio::time::timeout(GREETING, greeted)
        .await
        .map_err(|_late| ws::Error::Slow(server.authority()))??;
    Ok((socket, deframer, greeting))
}

/// Starts the thread that carries the line; what edualcd says comes to the app as `events`.
pub fn run(connected: Connected, events: Sender<Event>) -> (Link, Greeting) {
    let Connected {
        runtime,
        socket,
        deframer,
        greeting,
        mut addresses,
        server,
        early,
    } = connected;
    let (link, queue) = async_channel::bounded(QUEUE);
    std::thread::spawn(move || {
        runtime.block_on(async {
            let mut socket = socket;
            let mut deframer = deframer;
            let mut early = early;
            loop {
                if !carry(socket, deframer, early, &queue, &events).await {
                    return;
                }
                if events.send(Event::LineLost).await.is_err() {
                    return;
                }
                let Some((again, deframed, greeting)) =
                    reach_again(&server, &mut addresses, &queue, &events).await
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
                deframer = deframed;
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
    queue: &Receiver<ToDaemon>,
    events: &Sender<Event>,
) -> Option<(ws::Line, Deframer<FromDaemon>, Greeting)> {
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
        // Its name looked up anew: the phone may be on another network now.
        match ws::lookup(server).await {
            Ok(found) if !found.is_empty() => *addresses = found,
            Ok(_) => {}
            Err(error) => tracing::info!(%error, "edualcd's name wasn't found again"),
        }
        match greet(server, addresses).await {
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
    mut deframer: Deframer<FromDaemon>,
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
                    .map_err(ws::Error::from),
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
            // Each frame, edualcd's pings and the pongs to the window's among them, says the
            // line is alive: an idle line hears nothing else.
            match tokio::time::timeout(SILENT, ws::next(&mut reading, &mut deframer)).await {
                Ok(Ok(Some(message))) => {
                    if let Some(event) = inbox.take(message)
                        && events.send(event).await.is_err()
                    {
                        return false;
                    }
                }
                Ok(Ok(None)) => {}
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
