//! The messages on a line between a window and the daemon: WebSocket, each message one binary
//! frame of CBOR (see the parent module). Over the daemon's Unix socket, as here, or over TCP
//! for a page, as its server takes them.

use futures_util::{SinkExt, StreamExt};
use serde::Serialize;
use serde::de::DeserializeOwned;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;

use super::{Error, MAX_MESSAGE};

/// The address a window asks for on the Unix socket; no name resolves, the socket is the place.
const URL: &str = "ws://edualcd/ws";

/// How big a message and a frame may be: a peer sending more is broken or hostile.
fn config() -> WebSocketConfig {
    WebSocketConfig::default()
        .max_message_size(Some(MAX_MESSAGE))
        .max_frame_size(Some(MAX_MESSAGE))
}

/// The window's end of the line, its handshake done.
pub async fn connect<S>(stream: S) -> Result<WebSocketStream<S>, Error>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let (socket, _response) =
        tokio_tungstenite::client_async_with_config(URL, stream, Some(config())).await?;
    Ok(socket)
}

/// The daemon's end of the line, its handshake done.
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
                    protocol: crate::ipc::PROTOCOL
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
        let mut socket = connect(window_end).await.expect("handshake");
        send(
            &mut socket,
            &ToDaemon::Hello {
                protocol: crate::ipc::PROTOCOL,
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
