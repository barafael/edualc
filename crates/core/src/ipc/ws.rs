//! The messages on a line between a window and edualcd: WebSocket over TLS (see `tls`), each
//! message one binary frame of CBOR (see the parent module), to edualcd's `/api/line`, as a
//! page's are. A demo's line is plain (see `pairing::Demo`).

use std::io;
use std::net::SocketAddr;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use serde::Serialize;
use serde::de::DeserializeOwned;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::http::StatusCode;
use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;

use super::MAX_MESSAGE;
use super::pairing::{Pairing, Server};

/// How long reaching edualcd may take: the connection, TLS and the WebSocket's handshake.
const REACH: Duration = Duration::from_secs(10);

/// A window's line to edualcd, its handshakes done.
pub type Line = WebSocketStream<Stream>;

/// What a line runs over: TLS, or, to a demo, the connection itself.
// One per line, made once and never moved after: its size costs nothing.
#[expect(clippy::large_enum_variant)]
#[derive(Debug)]
pub enum Stream {
    Tls(tokio_rustls::client::TlsStream<tokio::net::TcpStream>),
    Plain(tokio::net::TcpStream),
}

impl AsyncRead for Stream {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        match self.get_mut() {
            Self::Tls(stream) => Pin::new(stream).poll_read(cx, buf),
            Self::Plain(stream) => Pin::new(stream).poll_read(cx, buf),
        }
    }
}

impl AsyncWrite for Stream {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        match self.get_mut() {
            Self::Tls(stream) => Pin::new(stream).poll_write(cx, buf),
            Self::Plain(stream) => Pin::new(stream).poll_write(cx, buf),
        }
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match self.get_mut() {
            Self::Tls(stream) => Pin::new(stream).poll_flush(cx),
            Self::Plain(stream) => Pin::new(stream).poll_flush(cx),
        }
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match self.get_mut() {
            Self::Tls(stream) => Pin::new(stream).poll_shutdown(cx),
            Self::Plain(stream) => Pin::new(stream).poll_shutdown(cx),
        }
    }
}

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
         pair again (edualc --pair) with the address it prints; if not, something stands between"
    )]
    Certificate(String),
    #[error("edualcd refused the token: pair again (edualc --pair) with the address it prints")]
    Token,
    #[error("edualcd runs another version of edualc ({0})")]
    Version(String),
    #[error("the line's TLS: {0}")]
    Tls(#[from] rustls::Error),
    #[error("the line: {0}")]
    Io(#[source] std::io::Error),
    #[error(transparent)]
    WebSocket(#[from] tokio_tungstenite::tungstenite::Error),
    #[error(transparent)]
    Encode(#[from] super::EncodeError),
    #[error(transparent)]
    Unreadable(#[from] super::DecodeError),
    #[error("the other side hung up")]
    Closed,
}

/// How big a message and a frame may be: a peer sending more is broken or hostile.
fn config() -> WebSocketConfig {
    WebSocketConfig::default()
        .max_message_size(Some(MAX_MESSAGE))
        .max_frame_size(Some(MAX_MESSAGE))
}

/// The addresses `server` names, looked up now: a window does it before it is confined, which
/// leaves it no way to ask DNS. The system's lookup may take its time over a name no one
/// answers for: it is waited for as long as reaching edualcd is, on a thread of its own, which
/// is left to end by itself after that.
pub fn resolve(server: &Server) -> Result<Vec<SocketAddr>, Error> {
    use std::net::ToSocketAddrs as _;
    let (host, port, authority) = (server.host().to_owned(), server.port(), server.authority());
    let (found, looked_up) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let addresses = (host.as_str(), port)
            .to_socket_addrs()
            .map(Iterator::collect::<Vec<_>>);
        if found.send(addresses).is_err() {
            tracing::debug!(%host, "a name was found after it was given up on");
        }
    });
    match looked_up.recv_timeout(REACH) {
        Ok(Ok(addresses)) => Ok(addresses),
        Ok(Err(source)) => Err(Error::Unreachable {
            address: authority,
            source,
        }),
        Err(_late) => Err(Error::Slow(authority)),
    }
}

/// The addresses `server` names, looked up on the runtime's blocking threads, which keeps what
/// else runs there going meanwhile: for a window that isn't confined, reaching edualcd again.
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

/// The window's end of the line to `server`, at one of `addresses`.
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
    let stream = match server {
        Server::Paired(pairing) => Stream::Tls(secured(pairing, stream, &authority).await?),
        Server::Demo(_) => Stream::Plain(stream),
    };
    match tokio_tungstenite::client_async_with_config(server.line_url(), stream, Some(config()))
        .await
    {
        Ok((socket, _response)) => Ok(socket),
        Err(tokio_tungstenite::tungstenite::Error::Http(response)) => {
            let said = response
                .body()
                .as_deref()
                .map(|body| String::from_utf8_lossy(body).into_owned())
                .unwrap_or_default();
            Err(match response.status() {
                StatusCode::FORBIDDEN => Error::Token,
                StatusCode::CONFLICT => Error::Version(said),
                status => Error::Version(format!("it answered {status}: {said}")),
            })
        }
        Err(error) => Err(error.into()),
    }
}

/// TLS over `stream`, trusting the certificate paired with alone.
async fn secured(
    pairing: &Pairing,
    stream: tokio::net::TcpStream,
    authority: &str,
) -> Result<tokio_rustls::client::TlsStream<tokio::net::TcpStream>, Error> {
    let name =
        rustls::pki_types::ServerName::try_from(pairing.host.clone()).map_err(|_not_a_name| {
            rustls::Error::General(format!("{:?} isn't a host", pairing.host))
        })?;
    super::tls::connector(pairing.fingerprint)?
        .connect(name, stream)
        .await
        .map_err(|error| refused_certificate(error, authority))
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

/// The other end of a line, its handshake done: the tests' edualcd.
pub async fn accept<S>(stream: S) -> Result<WebSocketStream<S>, Error>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    Ok(tokio_tungstenite::accept_async_with_config(stream, Some(config())).await?)
}

/// Sends a message.
pub async fn send<S, T>(socket: &mut S, message: &T) -> Result<(), Error>
where
    S: SinkExt<Message, Error = tokio_tungstenite::tungstenite::Error> + Unpin,
    T: Serialize,
{
    let bytes = super::encode(message)?;
    socket.send(Message::Binary(bytes.into())).await?;
    Ok(())
}

/// The next message; pings, pongs and text frames between are passed over (tungstenite answers
/// pings itself).
pub async fn recv<S, T>(socket: &mut S) -> Result<T, Error>
where
    S: StreamExt<Item = Result<Message, tokio_tungstenite::tungstenite::Error>> + Unpin,
    T: DeserializeOwned,
{
    loop {
        if let Some(message) = next(socket).await? {
            return Ok(message);
        }
    }
}

/// What the next frame brings: a message, or none for a ping, a pong or a text frame, which
/// say only that the other side is there (tungstenite answers pings itself). A line watched for
/// silence counts each frame, not each message.
pub async fn next<S, T>(socket: &mut S) -> Result<Option<T>, Error>
where
    S: StreamExt<Item = Result<Message, tokio_tungstenite::tungstenite::Error>> + Unpin,
    T: DeserializeOwned,
{
    match socket.next().await {
        Some(Ok(Message::Binary(bytes))) => Ok(Some(super::decode(&bytes)?)),
        Some(Ok(Message::Close(_))) | None => Err(Error::Closed),
        Some(Ok(Message::Text(_) | Message::Ping(_) | Message::Pong(_) | Message::Frame(_))) => {
            Ok(None)
        }
        Some(Err(error)) => Err(error.into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ipc::{FromDaemon, Generation, Stream, ToDaemon};

    /// A demo is reached in plain, at its line, with no token.
    // tungstenite's handshake callback returns its whole response as the error.
    #[expect(clippy::result_large_err)]
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
            let socket = tokio_tungstenite::accept_hdr_async(
                stream,
                |request: &tokio_tungstenite::tungstenite::handshake::server::Request, response| {
                    asked = request.uri().query().map(str::to_owned);
                    Ok(response)
                },
            )
            .await
            .expect("handshake");
            drop(socket);
            asked
        });
        let server = Server::Demo(Demo {
            host: "127.0.0.1".to_owned(),
            port: address.port(),
        });
        connect(&server, &[address]).await.expect("reached");
        assert_eq!(
            demo.await.expect("the demo ran"),
            Some(format!("token=&protocol={}", crate::ipc::PROTOCOL))
        );
    }

    /// A ping comes as no message, but comes: a line watched for silence hears an idle edualcd's
    /// pings; what is said after it comes as said.
    #[tokio::test]
    async fn a_ping_is_heard_as_no_message() {
        let (window_end, daemon_end) = tokio::net::UnixStream::pair().expect("a socket pair");
        let daemon = tokio::spawn(async move {
            let mut socket = accept(daemon_end).await.expect("handshake");
            socket
                .send(Message::Ping(Vec::new().into()))
                .await
                .expect("pinged");
            send(&mut socket, &FromDaemon::Hosted(Vec::new()))
                .await
                .expect("sent");
            socket
        });
        let (mut socket, _response) = tokio_tungstenite::client_async_with_config(
            "ws://edualcd/",
            window_end,
            Some(config()),
        )
        .await
        .expect("handshake");
        let pinged = next::<_, FromDaemon>(&mut socket).await.expect("a frame");
        assert!(pinged.is_none(), "{pinged:?}");
        let said = next::<_, FromDaemon>(&mut socket).await.expect("a frame");
        assert!(matches!(said, Some(FromDaemon::Hosted(_))), "{said:?}");
        drop(daemon.await.expect("the daemon's end ran"));
    }

    /// Messages go both ways whole over a socket pair, a long line among them; one bigger than
    /// a message may be ends the line.
    #[tokio::test]
    async fn whole_messages_go_through_and_oversized_ones_end_it() {
        let (window_end, daemon_end) = tokio::net::UnixStream::pair().expect("a socket pair");
        let daemon = tokio::spawn(async move {
            let mut socket = accept(daemon_end).await.expect("handshake");
            let hello: ToDaemon = recv(&mut socket).await.expect("a hello");
            assert!(matches!(
                hello,
                ToDaemon::Hello {
                    protocol: crate::ipc::PROTOCOL,
                    ..
                }
            ));
            let line = "x".repeat(1024 * 1024);
            let output = FromDaemon::Output {
                key: Generation::default().next(),
                stream: Stream::Out,
                line,
            };
            send(&mut socket, &output).await.expect("sent");
            recv::<_, ToDaemon>(&mut socket).await
        });
        let (mut socket, _response) = tokio_tungstenite::client_async_with_config(
            "ws://edualcd/",
            window_end,
            Some(config()),
        )
        .await
        .expect("handshake");
        send(
            &mut socket,
            &ToDaemon::Hello {
                protocol: crate::ipc::PROTOCOL,
                window: crate::ipc::WindowKey::new(),
            },
        )
        .await
        .expect("sent");
        let output: FromDaemon = recv(&mut socket).await.expect("the line");
        assert!(matches!(output, FromDaemon::Output { line, .. } if line.len() == 1024 * 1024));
        let huge = ToDaemon::Input {
            key: Generation::default(),
            line: "y".repeat(MAX_MESSAGE),
        };
        // The window's own config refuses to send it, or the daemon refuses to take it.
        let sent = send(&mut socket, &huge).await;
        let taken = daemon.await.expect("the daemon's end ran");
        assert!(sent.is_err() || taken.is_err());
    }
}
