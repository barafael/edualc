//! The connections: TCP taken, TLS handshaken within [`HANDSHAKE`], and handed to whatever
//! serves HTTP on them. Only [`MOST_HANDSHAKES`] are under way at once; one over that is
//! dropped, so a flood of connections that never finish a handshake holds nothing up.

use std::net::SocketAddr;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;

use axum::serve::Listener;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{mpsc, watch};
use tokio::task::JoinSet;
use tokio::time::Instant;
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
    let app = app.into_make_service_with_connect_info::<Hearing>();
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

/// A connection that says when it last read anything: a message that takes long to arrive (a
/// phone's prompt with images) shows the line alive as it arrives, not only once it is whole (a
/// pong sent after it waits behind it, and a page's browser sends none of its own).
pub struct Heard<Io = Secured> {
    secured: Io,
    read: watch::Sender<Instant>,
}

impl<Io> Heard<Io> {
    fn new(secured: Io) -> Self {
        Self {
            secured,
            read: watch::Sender::new(Instant::now()),
        }
    }
}

impl<Io: tokio::io::AsyncRead + Unpin> tokio::io::AsyncRead for Heard<Io> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        context: &mut Context<'_>,
        buffer: &mut tokio::io::ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        let before = buffer.filled().len();
        let polled = Pin::new(&mut self.secured).poll_read(context, buffer);
        if matches!(polled, Poll::Ready(Ok(()))) && buffer.filled().len() > before {
            self.read.send_replace(Instant::now());
        }
        polled
    }
}

impl<Io: tokio::io::AsyncWrite + Unpin> tokio::io::AsyncWrite for Heard<Io> {
    fn poll_write(
        mut self: Pin<&mut Self>,
        context: &mut Context<'_>,
        bytes: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        Pin::new(&mut self.secured).poll_write(context, bytes)
    }

    fn poll_flush(
        mut self: Pin<&mut Self>,
        context: &mut Context<'_>,
    ) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.secured).poll_flush(context)
    }

    fn poll_shutdown(
        mut self: Pin<&mut Self>,
        context: &mut Context<'_>,
    ) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.secured).poll_shutdown(context)
    }

    fn poll_write_vectored(
        mut self: Pin<&mut Self>,
        context: &mut Context<'_>,
        buffers: &[std::io::IoSlice<'_>],
    ) -> Poll<std::io::Result<usize>> {
        Pin::new(&mut self.secured).poll_write_vectored(context, buffers)
    }

    fn is_write_vectored(&self) -> bool {
        self.secured.is_write_vectored()
    }
}

/// When a line's connection last read anything, for its route (axum's connect info). Served in
/// plain (the demo), there is none.
#[derive(Debug, Clone)]
pub struct Hearing(pub watch::Receiver<Instant>);

impl axum::extract::connect_info::Connected<axum::serve::IncomingStream<'_, TlsListener>>
    for Hearing
{
    fn connect_info(stream: axum::serve::IncomingStream<'_, TlsListener>) -> Self {
        Self(stream.io().read.subscribe())
    }
}

/// What `axum::serve` takes connections from: those whose handshakes are done.
pub struct TlsListener {
    taken: mpsc::Receiver<(Secured, SocketAddr)>,
    local: SocketAddr,
}

impl Listener for TlsListener {
    type Io = Heard;
    type Addr = SocketAddr;

    async fn accept(&mut self) -> (Self::Io, Self::Addr) {
        match self.taken.recv().await {
            Some((secured, peer)) => (Heard::new(secured), peer),
            // The handshakes ended: nothing comes anymore, and nothing is to be served.
            None => std::future::pending().await,
        }
    }

    fn local_addr(&self) -> std::io::Result<Self::Addr> {
        Ok(self.local)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A connection says when it last read something, and only then.
    #[tokio::test]
    async fn a_connection_says_when_it_last_read() {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
        let (mut window, server) = tokio::io::duplex(64);
        let mut server = Heard::new(server);
        let opened = *server.read.borrow();
        let hearing = server.read.subscribe();
        tokio::time::sleep(Duration::from_millis(5)).await;
        window.write_all(b"part").await.expect("written");
        let mut bytes = [0; 4];
        server.read_exact(&mut bytes).await.expect("read");
        let read = *hearing.borrow();
        assert!(read > opened);
        drop(window);
        assert_eq!(server.read(&mut bytes).await.expect("its end"), 0);
        assert_eq!(*hearing.borrow(), read, "an end read is nothing heard");
    }
}
