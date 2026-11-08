//! A page's line to the daemon: a typed WebSocket (CBOR, one binary frame a message, the same
//! messages the native window sends on the daemon's socket), which the server carries on to the
//! daemon, once the page shows the server's token.

use dioxus::fullstack::{CborEncoding, WebSocketOptions, Websocket};
use dioxus::prelude::*;
use edualc_core::ipc::{FromDaemon, ToDaemon};

#[cfg(feature = "server")]
use crate::serve::Access;

/// Opens a line to the daemon, for the page holding `token`.
#[get("/api/line?token", access: dioxus::server::axum::Extension<Access>)]
pub async fn line(
    token: String,
    options: WebSocketOptions,
) -> Result<Websocket<ToDaemon, FromDaemon, CborEncoding>> {
    if !access.admits(&token) {
        return Err(ServerFnError::new("this page's token isn't the server's").into());
    }
    if access.is_demo() {
        return Ok(options.on_upgrade(crate::serve::play));
    }
    Ok(options.on_upgrade(crate::serve::carry))
}
