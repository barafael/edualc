//! A native window's line to edualcd over the network (the Android app's): WebSocket over TLS
//! (see `tls`), to edualcd's `/api/line`, as a page's is. Each message is one binary frame of
//! what the Unix socket carries (postcard's COBS; see the parent module), and the first frame
//! shows the token (`Auth`). A demo's line is plain (see `pairing::Demo`).

use std::io;
use std::net::SocketAddr;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use serde::Serialize;
use serde::de::DeserializeOwned;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::http::StatusCode;
use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;
use tokio_tungstenite::{Connector, MaybeTlsStream, WebSocketStream};

use super::pairing::Server;
use super::{Auth, Deframer, MAX_FRAME};

/// How long reaching edualcd may take: the connection, TLS and the WebSocket's handshake.
const REACH: Duration = Duration::from_secs(10);
/// The close code edualcd ends a line with that showed a wrong token.
const REFUSED: u16 = 4401;

/// A window's line to edualcd, its handshakes done: over TLS, or, to a demo, over the
/// connection itself.
pub type Line = WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("couldn't reach edualcd at {address}: {source}")]
    Unreachable {
        address: String,
        #[source]
        source: std::io::Error,
    },
    #[error("edualcd at {0} didn't answer in time")]
    Slow(String),
    #[error(
        "edualcd at {0} shows another certificate than the one paired with: if it was made anew, \
         pair again with the address `edualcd --pair` prints; if not, something stands between"
    )]
    Certificate(String),
    #[error("edualcd refused the token: pair again with the address `edualcd --pair` prints")]
    Token,
    #[error("edualcd runs another version of edualc ({0})")]
    Version(String),
    /// Something other than edualcd's line answered (a proxy in front of it, say, while it
    /// restarts): reached again, as the network's doing, not given up on as another version.
    #[error("{address} answered {status} in place of edualcd's line: {said}")]
    Answered {
        address: String,
        status: StatusCode,
        said: String,
    },
    #[error("the line's TLS: {0}")]
    Tls(#[from] rustls::Error),
    #[error("the line: {0}")]
    Io(#[source] std::io::Error),
    #[error(transparent)]
    WebSocket(#[from] tokio_tungstenite::tungstenite::Error),
    #[error(transparent)]
    Frame(#[from] super::Error),
    #[error("the other side hung up")]
    Closed,
}

/// How big a message and a frame may be: a peer sending more is broken or hostile.
fn config() -> WebSocketConfig {
    WebSocketConfig::default()
        .max_message_size(Some(MAX_FRAME))
        .max_frame_size(Some(MAX_FRAME))
}

/// The addresses `server` names, looked up on the runtime's blocking threads, which keeps what
/// else runs there going meanwhile.
pub async fn lookup(server: &Server) -> Result<Vec<SocketAddr>, Error> {
    let authority = server.authority();
    let found = tokio::time::timeout(
        REACH,
        tokio::net::lookup_host((server.host(), server.port())),
    )
    .await
    .map_err(|_late| Error::Slow(authority.clone()))?
    .map_err(|source| Error::Unreachable {
        address: authority,
        source,
    })?;
    Ok(found.collect())
}

/// The window's end of the line to `server`, at one of `addresses`, its token shown.
pub async fn connect(server: &Server, addresses: &[SocketAddr]) -> Result<Line, Error> {
    let authority = server.authority();
    tokio::time::timeout(REACH, reach(server, addresses))
        .await
        .map_err(|_elapsed| Error::Slow(authority))?
}

async fn reach(server: &Server, addresses: &[SocketAddr]) -> Result<Line, Error> {
    let authority = server.authority();
    let stream = tokio::net::TcpStream::connect(addresses)
        .await
        .map_err(|source| Error::Unreachable {
            address: authority.clone(),
            source,
        })?;
    stream.set_nodelay(true).map_err(Error::Io)?;
    // TLS trusting the certificate paired with alone, by the name in the line's address.
    let tls = match server {
        Server::Paired(pairing) => {
            Connector::Rustls(super::tls::client(pairing.fingerprint)?.into())
        }
        Server::Demo(_) => Connector::Plain,
    };
    let opened = tokio_tungstenite::client_async_tls_with_config(
        server.line_url(),
        stream,
        Some(config()),
        Some(tls),
    )
    .await;
    let mut socket = match opened {
        Ok((socket, _response)) => socket,
        // The TLS handshake's, or the connection's.
        Err(tokio_tungstenite::tungstenite::Error::Io(error)) => {
            return Err(refused_certificate(error, &authority));
        }
        Err(tokio_tungstenite::tungstenite::Error::Http(response)) => {
            let said = response
                .body()
                .as_deref()
                .map(|body| String::from_utf8_lossy(body).into_owned())
                .unwrap_or_default();
            // Only edualcd's own refusal of another version is that; whatever else answers may
            // answer otherwise as the line is tried again.
            return Err(match response.status() {
                StatusCode::CONFLICT => Error::Version(said),
                status => Error::Answered {
                    address: authority,
                    status,
                    said,
                },
            });
        }
        Err(error) => return Err(error.into()),
    };
    // The token is in no address, which proxies and servers log: the line's first frame shows
    // it.
    send(
        &mut socket,
        &Auth {
            token: server.token(),
        },
    )
    .await?;
    Ok(socket)
}

/// A TLS handshake's failure, the pinned certificate's mismatch named.
fn refused_certificate(error: io::Error, authority: &str) -> Error {
    let mismatch = error
        .get_ref()
        .and_then(|inner| inner.downcast_ref::<rustls::Error>())
        .is_some_and(|tls| {
            matches!(
                tls,
                rustls::Error::InvalidCertificate(
                    rustls::CertificateError::ApplicationVerificationFailure
                )
            )
        });
    if mismatch {
        Error::Certificate(authority.to_owned())
    } else {
        Error::Unreachable {
            address: authority.to_owned(),
            source: error,
        }
    }
}

/// Sends a message, in a frame of its own.
pub async fn send<S, T>(socket: &mut S, message: &T) -> Result<(), Error>
where
    S: SinkExt<Message, Error = tokio_tungstenite::tungstenite::Error> + Unpin,
    T: Serialize,
{
    let frame = super::frame(message)?;
    socket.send(Message::Binary(frame.into())).await?;
    Ok(())
}

/// The next message; pings, pongs and text frames between are passed over (tungstenite answers
/// pings itself).
pub async fn recv<S, T>(socket: &mut S, deframer: &mut Deframer<T>) -> Result<T, Error>
where
    S: StreamExt<Item = Result<Message, tokio_tungstenite::tungstenite::Error>> + Unpin,
    T: DeserializeOwned,
{
    loop {
        if let Some(message) = next(socket, deframer).await? {
            return Ok(message);
        }
    }
}

/// What the next frame brings: a message, or none for a ping, a pong or a text frame, which
/// say only that the other side is there (tungstenite answers pings itself). A line watched for
/// silence counts each frame, not each message. A line closed for its token says so.
pub async fn next<S, T>(socket: &mut S, deframer: &mut Deframer<T>) -> Result<Option<T>, Error>
where
    S: StreamExt<Item = Result<Message, tokio_tungstenite::tungstenite::Error>> + Unpin,
    T: DeserializeOwned,
{
    if let Some(message) = deframer.take() {
        return Ok(Some(message));
    }
    match socket.next().await {
        Some(Ok(Message::Binary(bytes))) => {
            deframer.push(&bytes)?;
            Ok(deframer.take())
        }
        Some(Ok(Message::Close(Some(close)))) if close.code == CloseCode::from(REFUSED) => {
            Err(Error::Token)
        }
        Some(Ok(Message::Close(_))) | None => Err(Error::Closed),
        Some(Ok(Message::Text(_) | Message::Ping(_) | Message::Pong(_) | Message::Frame(_))) => {
            Ok(None)
        }
        Some(Err(error)) => Err(error.into()),
    }
}

#[cfg(test)]
mod tests {
    use tokio::io::{AsyncRead, AsyncWrite};

    use super::*;
    use crate::ipc::{FromDaemon, PROTOCOL, ToDaemon, Token};

    /// The other end of a line, its handshake done: the tests' edualcd.
    async fn accept<S>(stream: S) -> WebSocketStream<S>
    where
        S: AsyncRead + AsyncWrite + Unpin,
    {
        tokio_tungstenite::accept_async_with_config(stream, Some(config()))
            .await
            .expect("handshake")
    }

    /// The window's end over `stream`, its handshake done.
    async fn client<S>(stream: S) -> WebSocketStream<S>
    where
        S: AsyncRead + AsyncWrite + Unpin,
    {
        tokio_tungstenite::client_async_with_config("ws://edualcd/", stream, Some(config()))
            .await
            .expect("handshake")
            .0
    }

    /// A demo is reached in plain, at its line, speaking its protocol, and shows an empty token
    /// first.
    #[tokio::test]
    async fn a_demo_is_reached_without_tls() {
        use crate::ipc::pairing::Demo;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("a port");
        let address = listener.local_addr().expect("its address");
        let demo = tokio::spawn(async move {
            let (stream, _peer) = listener.accept().await.expect("a connection");
            let mut asked = None;
            let mut socket = tokio_tungstenite::accept_hdr_async(
                stream,
                |request: &tokio_tungstenite::tungstenite::handshake::server::Request, response| {
                    asked = request.uri().query().map(str::to_owned);
                    Ok(response)
                },
            )
            .await
            .expect("handshake");
            let auth: Auth = recv(&mut socket, &mut Deframer::default())
                .await
                .expect("the token");
            (asked, auth.token.is_empty())
        });
        let server = Server::Demo(Demo {
            host: "127.0.0.1".to_owned(),
            port: address.port(),
        });
        connect(&server, &[address]).await.expect("reached");
        assert_eq!(
            demo.await.expect("the demo ran"),
            (Some(format!("protocol={PROTOCOL}")), true)
        );
    }

    /// A ping comes as no message, but comes: a line watched for silence hears an idle edualcd's
    /// pings; what is said after it comes as said.
    #[tokio::test]
    async fn a_ping_is_heard_as_no_message() {
        let (window_end, daemon_end) = tokio::net::UnixStream::pair().expect("a socket pair");
        let daemon = tokio::spawn(async move {
            let mut socket = accept(daemon_end).await;
            socket
                .send(Message::Ping(Vec::new().into()))
                .await
                .expect("pinged");
            send(&mut socket, &FromDaemon::Hosted(Vec::new()))
                .await
                .expect("sent");
            socket
        });
        let mut socket = client(window_end).await;
        let mut deframer = Deframer::<FromDaemon>::default();
        let pinged = next(&mut socket, &mut deframer).await.expect("a frame");
        assert!(pinged.is_none(), "{pinged:?}");
        let said = next(&mut socket, &mut deframer).await.expect("a frame");
        assert!(matches!(said, Some(FromDaemon::Hosted(_))), "{said:?}");
        drop(daemon.await.expect("the daemon's end ran"));
    }

    /// A line closed for its token is told as refused, not as hung up.
    #[tokio::test]
    async fn a_refused_token_is_told() {
        let (window_end, daemon_end) = tokio::net::UnixStream::pair().expect("a socket pair");
        let daemon = tokio::spawn(async move {
            let mut socket = accept(daemon_end).await;
            let _auth: Auth = recv(&mut socket, &mut Deframer::default())
                .await
                .expect("the token");
            socket
                .close(Some(tokio_tungstenite::tungstenite::protocol::CloseFrame {
                    code: CloseCode::from(REFUSED),
                    reason: "refused".into(),
                }))
                .await
                .expect("closed");
        });
        let mut socket = client(window_end).await;
        send(
            &mut socket,
            &Auth {
                token: Token::new("wrong".to_owned()),
            },
        )
        .await
        .expect("sent");
        let said = recv::<_, FromDaemon>(&mut socket, &mut Deframer::default()).await;
        assert!(matches!(said, Err(Error::Token)), "{said:?}");
        daemon.await.expect("the daemon's end ran");
    }

    /// Messages go both ways whole over a socket pair.
    #[tokio::test]
    async fn whole_messages_go_through() {
        let (window_end, daemon_end) = tokio::net::UnixStream::pair().expect("a socket pair");
        let daemon = tokio::spawn(async move {
            let mut socket = accept(daemon_end).await;
            let hello: ToDaemon = recv(&mut socket, &mut Deframer::default())
                .await
                .expect("a hello");
            assert!(matches!(hello, ToDaemon::Hello { protocol: PROTOCOL }));
            send(&mut socket, &FromDaemon::Hosted(Vec::new()))
                .await
                .expect("sent");
        });
        let mut socket = client(window_end).await;
        send(&mut socket, &ToDaemon::Hello { protocol: PROTOCOL })
            .await
            .expect("sent");
        let said: FromDaemon = recv(&mut socket, &mut Deframer::default())
            .await
            .expect("an answer");
        assert!(matches!(said, FromDaemon::Hosted(_)));
        daemon.await.expect("the daemon's end ran");
    }
}
