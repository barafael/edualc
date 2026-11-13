//! edualcd's network side: windows that aren't on this machine (a page, an app) reach the daemon
//! over TLS, in a WebSocket at `/api/line`, beside the Unix socket the windows that are on it
//! use. A line shows the token in its first frame (`line`) and is then one more window of the
//! daemon's (`daemon::Line`), whose asks of this machine's desktop are refused.
//!
//! The certificate is edualcd's own, made on its first start and kept with the token
//! (`state`); a window trusts exactly the one it was paired with, by its SHA-256 (see
//! `ipc::pairing`), whatever it names. Connections are taken, and their TLS handshakes done,
//! here (`listener`); what is served over them is a router (`line::router`), which a server
//! that serves a page besides adds its routes to.

mod line;
mod listener;
pub mod state;

pub use line::{Access, router};
pub use listener::serve;

/// What the daemon's network side is told: where to listen, with what TLS, and the token a line
/// must show.
pub struct Config {
    pub listen: std::net::SocketAddr,
    pub acceptor: tokio_rustls::TlsAcceptor,
    pub token: crate::ipc::Token,
}

/// Where it listens; neither the TLS nor the token.
impl std::fmt::Debug for Config {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Config")
            .field("listen", &self.listen)
            .finish_non_exhaustive()
    }
}
