//! The connections: TCP taken, TLS handshaken within [`HANDSHAKE`], and handed to whatever
//! serves HTTP on them. Only [`MOST_HANDSHAKES`] are under way at once; one over that is
//! dropped, so a flood of connections that never finish a handshake holds nothing up.

use std::net::SocketAddr;
use std::time::Duration;

use axum::serve::Listener;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use tokio::task::JoinSet;
use tokio_rustls::TlsAcceptor;

/// How long a connection has for its TLS handshake.
const HANDSHAKE: Duration = Duration::from_secs(10);
/// How many handshakes are under way at once.
const MOST_HANDSHAKES: usize = 64;
/// Connections whose handshake is done, waiting to be served.
const WAITING: usize = 64;

/// A connection whose TLS handshake is done.
type Secured = tokio_rustls::server::TlsStream<TcpStream>;

/// Serves `app` over TLS at `listen`, until it ends.
///
/// # Errors
/// If `listen` can't be bound, or serving ends in error.
pub async fn serve(
    listen: SocketAddr,
    acceptor: TlsAcceptor,
    app: axum::Router,
) -> std::io::Result<()> {
    let tcp = TcpListener::bind(listen).await?;
    let local = tcp.local_addr()?;
    let (accepted, taken) = mpsc::channel(WAITING);
    tokio::spawn(handshakes(tcp, acceptor, accepted));
    axum::serve(TlsListener { taken, local }, app).await
}

/// Serves `app` over plain HTTP at `listen`: the demo, which runs nothing and holds no secret.
///
/// # Errors
/// If `listen` can't be bound, or serving ends in error.
pub async fn serve_plain(listen: SocketAddr, app: axum::Router) -> std::io::Result<()> {
    axum::serve(TcpListener::bind(listen).await?, app).await
}

/// Takes connections and shakes hands with each on a task of its own.
async fn handshakes(
    tcp: TcpListener,
    acceptor: TlsAcceptor,
    accepted: mpsc::Sender<(Secured, SocketAddr)>,
) {
    let mut shaking: JoinSet<Option<(Secured, SocketAddr)>> = JoinSet::new();
    loop {
        tokio::select! {
            connection = tcp.accept() => match connection {
                Ok((stream, peer)) if shaking.len() < MOST_HANDSHAKES => {
                    let acceptor = acceptor.clone();
                    shaking.spawn(async move {
                        match tokio::time::timeout(HANDSHAKE, acceptor.accept(stream)).await {
                            Ok(Ok(secured)) => Some((secured, peer)),
                            Ok(Err(error)) => {
                                tracing::debug!(%error, %peer, "a TLS handshake failed");
                                None
                            }
                            Err(_late) => {
                                tracing::debug!(%peer, "a TLS handshake took too long");
                                None
                            }
                        }
                    });
                }
                Ok((_stream, peer)) => tracing::warn!(%peer, "too many handshakes: dropped"),
                Err(error) => {
                    tracing::warn!(%error, "couldn't take a connection");
                    // Out of descriptors, say: not to be tried again at once.
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
            },
            Some(done) = shaking.join_next() => match done {
                Ok(Some(secured)) => {
                    if accepted.send(secured).await.is_err() {
                        return;
                    }
                }
                Ok(None) => {}
                Err(error) => tracing::error!(%error, "a handshake's task failed"),
            },
        }
    }
}

/// What `axum::serve` takes connections from: those whose handshakes are done.
struct TlsListener {
    taken: mpsc::Receiver<(Secured, SocketAddr)>,
    local: SocketAddr,
}

impl Listener for TlsListener {
    type Io = Secured;
    type Addr = SocketAddr;

    async fn accept(&mut self) -> (Self::Io, Self::Addr) {
        match self.taken.recv().await {
            Some(connection) => connection,
            // The handshakes ended: nothing comes anymore, and nothing is to be served.
            None => std::future::pending().await,
        }
    }

    fn local_addr(&self) -> std::io::Result<Self::Addr> {
        Ok(self.local)
    }
}
