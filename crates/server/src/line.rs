//! The page's line to edualcd, at `/api/line?protocol=…`: a WebSocket (over TLS when the page was
//! served so), each message one binary frame of what edualcd's Unix socket carries (postcard's
//! COBS, `ipc::frame`). The first frame is the token (`ipc::Auth`), which is in no address.

#[cfg(feature = "web")]
pub use web::open;

#[cfg(feature = "web")]
mod web {
    use edualc_core::ipc::{self, Auth, Deframer, FromDaemon, ToDaemon, Token};
    use futures_util::stream::{SplitSink, SplitStream};
    use futures_util::{SinkExt, StreamExt};
    use gloo_net::websocket::Message;
    use gloo_net::websocket::futures::WebSocket;

    /// The page's end of the line, what it sends.
    pub struct Sender(SplitSink<WebSocket, Message>);

    /// The page's end of the line, what it hears.
    pub struct Receiver {
        stream: SplitStream<WebSocket>,
        deframer: Deframer<FromDaemon>,
    }

    /// Opens the line on the page's own server, and shows it the page's token.
    pub async fn open(token: &str) -> Result<(Sender, Receiver), String> {
        let location = web_sys::window().ok_or("no window")?.location();
        let host = location
            .host()
            .map_err(|error| format!("no address: {error:?}"))?;
        let secure = location
            .protocol()
            .is_ok_and(|protocol| protocol == "https:");
        let scheme = if secure { "wss" } else { "ws" };
        let url = format!("{scheme}://{host}/api/line?protocol={}", ipc::PROTOCOL);
        let socket = WebSocket::open(&url).map_err(|error| error.to_string())?;
        let (sink, stream) = socket.split();
        let mut sender = Sender(sink);
        let auth = ipc::frame(&Auth {
            token: Token::new(token.to_owned()),
        })
        .map_err(|error| error.to_string())?;
        sender
            .0
            .send(Message::Bytes(auth))
            .await
            .map_err(|error| error.to_string())?;
        Ok((
            sender,
            Receiver {
                stream,
                deframer: Deframer::default(),
            },
        ))
    }

    impl Sender {
        pub async fn send(&mut self, message: &ToDaemon) -> Result<(), String> {
            let frame = ipc::frame(message).map_err(|error| error.to_string())?;
            self.0
                .send(Message::Bytes(frame))
                .await
                .map_err(|error| error.to_string())
        }
    }

    impl Receiver {
        /// The next message; text frames are passed over.
        pub async fn recv(&mut self) -> Result<FromDaemon, String> {
            loop {
                if let Some(message) = self.deframer.take() {
                    return Ok(message);
                }
                match self.stream.next().await {
                    Some(Ok(Message::Bytes(bytes))) => self
                        .deframer
                        .push(&bytes)
                        .map_err(|error| error.to_string())?,
                    Some(Ok(Message::Text(_))) => {}
                    Some(Err(error)) => return Err(error.to_string()),
                    None => return Err("the line closed".to_owned()),
                }
            }
        }
    }
}
