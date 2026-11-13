//! A window's line over the network, at `/api/line?protocol=…`: a WebSocket, each message one
//! binary frame of what the Unix socket carries (postcard's COBS; see `ipc`).
//!
//! A line is let in by its first frame alone: the token (`ipc::Auth`), in a frame of at most
//! [`MOST_AUTH`] bytes, within [`AUTH_WITHIN`], or the line is closed, with nothing else read.
//! The token is in no address, which proxies and servers log. Only a line of the protocol this
//! edualcd speaks is taken (HTTP 409 before the WebSocket opens), so that neither end reads
//! messages the other's version made.
//!
//! A line is pinged every [`PING`], and let go of after [`SILENT`] in which nothing came from
//! it (a pong does); what the server sends does not count as a sign of life.

use std::time::Duration;

use axum::Router;
use axum::extract::Query;
use axum::extract::ws::{CloseCode, CloseFrame, Message, WebSocket, WebSocketUpgrade};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use tokio::sync::mpsc;
use tokio::time::Instant;

use crate::daemon::Line;
use crate::ipc::{self, Auth, Deframer, FromDaemon, MAX_FRAME, PROTOCOL, ToDaemon, Token};

/// How long a line has to show its token.
const AUTH_WITHIN: Duration = Duration::from_secs(5);
/// The most bytes of the frame that holds the token: far more than a token comes to, far less
/// than a message.
const MOST_AUTH: usize = 1024;
/// How often a line is pinged, and how long one that says nothing, not even a pong, is kept: a
/// window whose network went away is let go of, so that it can take its sessions back as it
/// comes again.
const PING: Duration = Duration::from_secs(15);
const SILENT: Duration = Duration::from_secs(45);
/// The close code of a line that shows a wrong token (an application's, in 4000–4999).
const REFUSED: CloseCode = 4401;

/// What a line is checked against, and where it goes once it passes.
#[derive(Debug, Clone)]
pub struct Access {
    token: Token,
    lines: mpsc::Sender<Line>,
}

impl Access {
    #[must_use]
    pub fn new(token: Token, lines: mpsc::Sender<Line>) -> Self {
        Self { token, lines }
    }
}

/// The line's route.
pub fn router(access: Access) -> Router {
    Router::new()
        .route("/api/line", get(line))
        .layer(axum::Extension(access))
}

/// What a line asks with.
#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct Asked {
    /// The protocol the window speaks.
    protocol: Option<u32>,
}

async fn line(
    axum::Extension(access): axum::Extension<Access>,
    Query(Asked { protocol }): Query<Asked>,
    upgrade: WebSocketUpgrade,
) -> Response {
    if protocol != Some(PROTOCOL) {
        return (
            StatusCode::CONFLICT,
            format!("edualcd speaks protocol {PROTOCOL}"),
        )
            .into_response();
    }
    upgrade
        .max_message_size(MAX_FRAME)
        .max_frame_size(MAX_FRAME)
        .on_upgrade(move |socket| carry(socket, access))
}

/// Lets a line in or closes it, then carries it.
async fn carry(mut socket: WebSocket, access: Access) {
    if !let_in(&mut socket, &access.token).await {
        tracing::info!("a line refused: it didn't show the token");
        let refused = Message::Close(Some(CloseFrame {
            code: REFUSED,
            reason: "refused".into(),
        }));
        if let Err(error) = socket.send(refused).await {
            tracing::debug!(%error, "a refused line was gone");
        }
        return;
    }
    let (line, asks, says) = Line::pair();
    if access.lines.send(line).await.is_err() {
        tracing::debug!("the daemon takes no lines");
        return;
    }
    carry_on(socket, asks, says).await;
}

/// Whether the line's first frame shows `token`, in time and in size.
async fn let_in(socket: &mut WebSocket, token: &Token) -> bool {
    let Ok(first) = tokio::time::timeout(AUTH_WITHIN, socket.recv()).await else {
        return false;
    };
    let Some(Ok(Message::Binary(frame))) = first else {
        return false;
    };
    frame.len() <= MOST_AUTH
        && ipc::deframe::<Auth>(&frame).is_ok_and(|auth| token.admits(&auth.token))
}

/// Carries what the window says to the daemon, and what the daemon says to the window, until
/// either ends.
async fn carry_on(
    mut socket: WebSocket,
    asks: mpsc::Sender<ToDaemon>,
    mut says: mpsc::Receiver<FromDaemon>,
) {
    let mut deframer = Deframer::<ToDaemon>::default();
    let mut ping = tokio::time::interval(PING);
    let mut heard = Instant::now();
    loop {
        // A message already there is taken before the line is judged silent.
        tokio::select! {
            biased;
            message = socket.recv() => match message {
                Some(Ok(Message::Binary(bytes))) => {
                    if let Err(error) = deframer.push(&bytes) {
                        tracing::info!(%error, "a line sent what can't be read");
                        return;
                    }
                    while let Some(request) = deframer.take() {
                        if asks.send(request).await.is_err() {
                            return;
                        }
                    }
                    // A line is silent from when its last request was taken, not from before:
                    // a daemon slow to take it is not the window's silence.
                    heard = Instant::now();
                }
                Some(Ok(Message::Pong(_) | Message::Ping(_))) => heard = Instant::now(),
                // Messages are binary; and a line closed, or broken, is done.
                Some(Ok(Message::Text(_) | Message::Close(_)) | Err(_)) | None => return,
            },
            answer = says.recv() => {
                let Some(answer) = answer else {
                    // The daemon is done with this window.
                    return;
                };
                match ipc::frame(&answer) {
                    Ok(frame) => {
                        if socket.send(Message::Binary(frame.into())).await.is_err() {
                            return;
                        }
                    }
                    Err(error) => tracing::warn!(%error, "a message too large to send"),
                }
            },
            _ = ping.tick() => {
                if socket.send(Message::Ping(Vec::new().into())).await.is_err() {
                    return;
                }
            },
            () = tokio::time::sleep_until(heard + SILENT) => {
                tracing::info!("a line let go of: it said nothing for a while");
                return;
            },
        }
    }
}
