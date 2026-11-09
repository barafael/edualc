//! The messages on a line between a window and edualcd: WebSocket over TLS (see `tls`), each
//! message one binary frame of CBOR (see the parent module), to edualcd's `/api/line`, as a
//! page's are.

use std::net::SocketAddr;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use serde::Serialize;
use serde::de::DeserializeOwned;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::http::StatusCode;
use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;

use super::MAX_MESSAGE;
use super::pairing::Pairing;

/// How long reaching edualcd may take: the connection, TLS and the WebSocket's handshake.
const REACH: Duration = Duration::from_secs(10);

/// A window's line to edualcd, its handshakes done.
pub type Line = WebSocketStream<tokio_rustls::client::TlsStream<tokio::net::TcpStream>>;

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

/// The addresses `pairing` names, looked up now: a window does it before it is confined, which
/// leaves it no way to ask DNS.
pub fn resolve(pairing: &Pairing) -> Result<Vec<SocketAddr>, Error> {
    use std::net::ToSocketAddrs as _;
    (pairing.host.as_str(), pairing.port)
        .to_socket_addrs()
        .map(Iterator::collect)
        .map_err(|source| Error::Unreachable {
            address: pairing.authority(),
            source,
        })
}

/// The window's end of the line to the edualcd `pairing` names, at one of `addresses`.
pub async fn connect(pairing: &Pairing, addresses: &[SocketAddr]) -> Result<Line, Error> {
    let authority = pairing.authority();
    tokio::time::timeout(REACH, reach(pairing, addresses))
        .await
        .map_err(|_elapsed| Error::Slow(authority))?
}

async fn reach(pairing: &Pairing, addresses: &[SocketAddr]) -> Result<Line, Error> {
    let authority = pairing.authority();
    let stream = tokio::net::TcpStream::connect(addresses)
        .await
        .map_err(|source| Error::Unreachable {
            address: authority.clone(),
            source,
        })?;
    stream.set_nodelay(true).map_err(Error::Io)?;
    let name =
        rustls::pki_types::ServerName::try_from(pairing.host.clone()).map_err(|_not_a_name| {
            rustls::Error::General(format!("{:?} isn't a host", pairing.host))
        })?;
    let stream = super::tls::connector(pairing.fingerprint)?
        .connect(name, stream)
        .await
        .map_err(|error| refused_certificate(error, &authority))?;
    match tokio_tungstenite::client_async_with_config(pairing.line_url(), stream, Some(config()))
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

/// A TLS handshake's failure, the pinned certificate's mismatch named.
fn refused_certificate(error: std::io::Error, authority: &str) -> Error {
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
        match socket.next().await {
            Some(Ok(Message::Binary(bytes))) => return Ok(super::decode(&bytes)?),
            Some(Ok(Message::Close(_))) | None => return Err(Error::Closed),
            Some(Ok(
                Message::Text(_) | Message::Ping(_) | Message::Pong(_) | Message::Frame(_),
            )) => {}
            Some(Err(error)) => return Err(error.into()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ipc::{FromDaemon, Generation, Stream, ToDaemon};

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
