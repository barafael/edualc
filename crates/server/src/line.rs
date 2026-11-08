//! A page's line to the daemon, at `/api/line?token=…`: a WebSocket, each message one binary
//! frame of CBOR, the same messages the native window sends on the daemon's socket. The server
//! carries the frames on to the daemon once the page shows the server's token.
//!
//! A plain route rather than a Dioxus server function: Dioxus's sockets keep axum's limits (16
//! MiB a frame), and a message with a few pasted images is larger. Here a frame may be as large
//! as a message the protocol allows (`ipc::MAX_MESSAGE`).

#[cfg(feature = "server")]
pub use server::line;
#[cfg(feature = "web")]
pub use web::open;

#[cfg(feature = "server")]
mod server {
    use dioxus::server::axum::Extension;
    use dioxus::server::axum::extract::Query;
    use dioxus::server::axum::extract::ws::WebSocketUpgrade;
    use dioxus::server::axum::http::StatusCode;
    use dioxus::server::axum::response::{IntoResponse, Response};
    use edualc_core::ipc::MAX_MESSAGE;

    use crate::serve::Access;

    /// What a page asks the line with.
    #[derive(serde::Deserialize)]
    #[serde(deny_unknown_fields)]
    pub struct Asked {
        token: String,
    }

    /// Opens a line to the daemon (to the demo's script, for the demo), for a page holding the
    /// server's token.
    pub async fn line(
        Query(Asked { token }): Query<Asked>,
        Extension(access): Extension<Access>,
        upgrade: WebSocketUpgrade,
    ) -> Response {
        if !access.admits(&token) {
            return (
                StatusCode::FORBIDDEN,
                "this page's token isn't the server's",
            )
                .into_response();
        }
        let upgrade = upgrade
            .max_message_size(MAX_MESSAGE)
            .max_frame_size(MAX_MESSAGE);
        match access {
            Access::Demo => upgrade.on_upgrade(crate::serve::play),
            Access::Token(_) => upgrade.on_upgrade(crate::serve::carry),
        }
    }
}

#[cfg(feature = "web")]
mod web {
    use edualc_core::ipc::{self, FromDaemon, ToDaemon};
    use futures_util::stream::{SplitSink, SplitStream};
    use futures_util::{SinkExt, StreamExt};
    use gloo_net::websocket::Message;
    use gloo_net::websocket::futures::WebSocket;

    /// The page's end of the line, what it sends.
    pub struct Sender(SplitSink<WebSocket, Message>);

    /// The page's end of the line, what it hears.
    pub struct Receiver(SplitStream<WebSocket>);

    /// Opens the line on the page's own server, with the page's token.
    pub fn open(token: &str) -> Result<(Sender, Receiver), String> {
        let location = web_sys::window().ok_or("no window")?.location();
        let host = location
            .host()
            .map_err(|error| format!("no address: {error:?}"))?;
        let secure = location
            .protocol()
            .is_ok_and(|protocol| protocol == "https:");
        let scheme = if secure { "wss" } else { "ws" };
        let url = format!("{scheme}://{host}/api/line?token={}", encoded(token));
        let socket = WebSocket::open(&url).map_err(|error| error.to_string())?;
        let (sink, stream) = socket.split();
        Ok((Sender(sink), Receiver(stream)))
    }

    impl Sender {
        pub async fn send(&mut self, message: &ToDaemon) -> Result<(), String> {
            let bytes = ipc::encode(message).map_err(|error| error.to_string())?;
            self.0
                .send(Message::Bytes(bytes))
                .await
                .map_err(|error| error.to_string())
        }
    }

    impl Receiver {
        /// The next message; text frames are passed over.
        pub async fn recv(&mut self) -> Result<FromDaemon, String> {
            loop {
                match self.0.next().await {
                    Some(Ok(Message::Bytes(bytes))) => {
                        return ipc::decode(&bytes).map_err(|error| error.to_string());
                    }
                    Some(Ok(Message::Text(_))) => {}
                    Some(Err(error)) => return Err(error.to_string()),
                    None => return Err("the line closed".to_owned()),
                }
            }
        }
    }

    /// `text` as a URL's query may hold it: all but letters, digits and `-._~` percent-encoded.
    fn encoded(text: &str) -> String {
        use std::fmt::Write as _;
        let mut out = String::with_capacity(text.len());
        for byte in text.bytes() {
            if byte.is_ascii_alphanumeric() || b"-._~".contains(&byte) {
                out.push(char::from(byte));
            } else if write!(out, "%{byte:02X}").is_err() {
                tracing::debug!("a String refused a write");
            }
        }
        out
    }
}
