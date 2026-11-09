//! The server's side: its command line, the daemon it runs, its TLS, the token windows and
//! pages show, and carrying each one's line to the daemon.

use std::io::IsTerminal as _;
use std::net::{IpAddr, SocketAddr};
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;

use dioxus::server::axum::extract::ws::{Message as PageMessage, WebSocket};
use edualc_host::daemon::{self, Daemon};
use edualc_host::ipc::pairing::Pairing;
use edualc_host::ipc::{self, ToDaemon};
use futures_util::{SinkExt, StreamExt};
use tokio::sync::{mpsc, watch};
use tokio::time::Instant;

use crate::state::State;

/// Where edualcd listens unless told otherwise: this machine alone.
const DEFAULT_LISTEN: &str = "127.0.0.1:7878";
/// How long a connection has for its TLS handshake.
const HANDSHAKE: Duration = Duration::from_secs(10);
/// How often a line is pinged, and how long one that says nothing, not even a pong, is kept: a
/// window whose network went away is let go of, so that it can take its sessions back as it
/// comes again.
const PING: Duration = Duration::from_secs(15);
const SILENT: Duration = Duration::from_secs(45);

/// What a line must show to be let through.
#[derive(Debug, Clone)]
pub enum Access {
    /// Lines that show this token, to the daemon.
    Token(String, Daemon),
    /// Anyone, to the demo's script.
    Demo,
}

impl Access {
    /// Whether `token` is the server's, compared in constant time.
    pub fn admits(&self, token: &str) -> bool {
        let Self::Token(ours, _) = self else {
            return true;
        };
        let (ours, theirs) = (ours.as_bytes(), token.as_bytes());
        ours.len() == theirs.len()
            && ours
                .iter()
                .zip(theirs)
                .fold(0u8, |differs, (a, b)| differs | (a ^ b))
                == 0
    }
}

/// The command line: the server, its address, or the decoder.
#[derive(Debug, clap::Parser)]
#[command(
    name = "edualcd",
    version,
    about = "edualc's daemon: runs Claude Code for edualc's windows, and serves its page",
    after_help = AFTER_HELP
)]
struct Args {
    /// Where windows and pages reach edualcd. Listen on another machine's network (or 0.0.0.0,
    /// all of them) to reach it from there: the line is encrypted, and only those holding the
    /// token get through.
    #[arg(long, value_name = "ADDRESS", default_value = DEFAULT_LISTEN)]
    listen: SocketAddr,
    /// The name windows reach edualcd by, in the address it prints: the listening address, or
    /// this machine's name if it listens on all.
    #[arg(long, value_name = "HOST")]
    name: Option<String>,
    /// Print the address a window pairs with (`edualc --pair ADDRESS`) and a page opens, which
    /// holds the token, and end.
    #[arg(long, conflicts_with = "demo")]
    pair: bool,
    /// Serve the demo instead, over plain HTTP: Claude Code played by a script, on a demo
    /// project, for anyone (no token); nothing runs and nothing on the disk is read.
    #[arg(long)]
    demo: bool,
}

/// The help's end: where the token and the certificate come from.
const AFTER_HELP: &str = "edualcd makes its certificate and its token on its first start and \
keeps them in $XDG_STATE_HOME/edualcd (~/.local/state/edualcd); remove that folder to make them \
anew. EDUALC_TOKEN, if set, is the token instead. Neither is taken on the command line, which \
every user of the machine can read.";

pub fn main() -> std::process::ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| {
                "edualcd=info,edualc_host=info,edualc_core=info,edualc_server=info".into()
            }),
        )
        .with_writer(std::io::stderr)
        .init();
    // The image decoder the daemon and the windows start, confined before it reads.
    if let Some(job) = std::env::args()
        .nth(1)
        .and_then(|argument| argument.parse::<edualc_host::decode::Job>().ok())
    {
        return edualc_host::decode::run(job);
    }
    let Args {
        listen,
        name,
        pair,
        demo,
    } = clap::Parser::parse();
    daemon::harden();
    if demo {
        return serve_demo(listen);
    }
    let host = name.clone().unwrap_or_else(|| host_of(listen.ip()));
    let state = match State::load(&certificate_names(listen.ip(), name)) {
        Ok(state) => state,
        Err(error) => {
            eprintln!("edualcd: {error}");
            return std::process::ExitCode::FAILURE;
        }
    };
    let token = match token(&state) {
        Ok(token) => token,
        Err(code) => return code,
    };
    let pairing = Pairing {
        host,
        port: listen.port(),
        fingerprint: state.fingerprint(),
        token,
    };
    if pair {
        println!("{}", pairing.url());
        return std::process::ExitCode::SUCCESS;
    }
    // Under `dx serve`, while the page is worked on: Dioxus's address, plain HTTP.
    if dioxus::cli_config::is_cli_enabled() {
        return serve_under_dx(pairing.token);
    }
    serve(listen, &pairing, state)
}

/// EDUALC_TOKEN's, or the one kept.
fn token(state: &State) -> Result<String, std::process::ExitCode> {
    let variable = ipc::TOKEN_VARIABLE;
    match std::env::var(variable) {
        // An empty token would let every line through, a site's in the browser too.
        Ok(token) if token.is_empty() => {
            eprintln!("edualcd: {variable} is empty; give one no one can guess, or unset it");
            Err(std::process::ExitCode::from(2))
        }
        Ok(token) => Ok(token),
        Err(std::env::VarError::NotPresent) => Ok(state.token.clone()),
        Err(std::env::VarError::NotUnicode(_)) => {
            eprintln!("edualcd: {variable} isn't text");
            Err(std::process::ExitCode::from(2))
        }
    }
}

/// The host windows are told to reach edualcd at: the address it listens at, or this machine's
/// name where it listens on all.
fn host_of(ip: IpAddr) -> String {
    if ip.is_unspecified() {
        machine_name().unwrap_or_else(|| "localhost".to_owned())
    } else {
        ip.to_string()
    }
}

fn machine_name() -> Option<String> {
    std::fs::read_to_string("/proc/sys/kernel/hostname")
        .ok()
        .map(|name| name.trim().to_owned())
        .filter(|name| !name.is_empty())
}

/// What a certificate made now names: this machine by its names and loopback addresses, and the
/// address and name it is reached by.
fn certificate_names(ip: IpAddr, name: Option<String>) -> Vec<String> {
    let mut names: Vec<String> = ["localhost", "127.0.0.1", "::1"]
        .into_iter()
        .map(str::to_owned)
        .chain(machine_name())
        .chain((!ip.is_unspecified()).then(|| ip.to_string()))
        .chain(name)
        .collect();
    names.sort();
    names.dedup();
    names
}

/// A runtime for the server's tasks.
fn runtime() -> Result<tokio::runtime::Runtime, std::process::ExitCode> {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|error| {
            tracing::error!(%error, "no runtime");
            std::process::ExitCode::FAILURE
        })
}

/// Serves the page, and the lines of windows and pages that show the token, over TLS, until
/// stopped.
fn serve(listen: SocketAddr, pairing: &Pairing, state: State) -> std::process::ExitCode {
    let acceptor = match ipc::tls::acceptor(state.certificate, state.key) {
        Ok(acceptor) => acceptor,
        Err(error) => {
            eprintln!("edualcd: its certificate doesn't serve: {error}");
            return std::process::ExitCode::FAILURE;
        }
    };
    let runtime = match runtime() {
        Ok(runtime) => runtime,
        Err(code) => return code,
    };
    tell_address(pairing);
    let token = pairing.token.clone();
    let served = runtime.block_on(async move {
        let tcp = tokio::net::TcpListener::bind(listen).await?;
        let local = tcp.local_addr()?;
        let (accepted, taken) = mpsc::channel(daemon::QUEUE);
        tokio::spawn(handshakes(tcp, acceptor, accepted));
        let access = Access::Token(token, Daemon::start());
        let app = router(access).into_make_service_with_connect_info::<Hearing>();
        dioxus::server::axum::serve(TlsListener { taken, local }, app).await
    });
    match served {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(error) => {
            tracing::error!(%error, "couldn't serve");
            std::process::ExitCode::FAILURE
        }
    }
}

/// Says where edualcd is. The address with its token only to a terminal: a service's log is
/// read by more than its user.
fn tell_address(pairing: &Pairing) {
    if std::io::stderr().is_terminal() {
        eprintln!(
            "edualcd: edualc's page is at {url}\nedualcd: pair a window with: edualc --pair '{url}'",
            url = pairing.url(),
        );
    } else {
        eprintln!(
            "edualcd: listening at https://{}/, certificate {}; `edualcd --pair` prints the \
             address windows pair with",
            pairing.authority(),
            pairing.fingerprint
        );
    }
}

/// The demo, over plain HTTP: it runs nothing and holds no secret.
fn serve_demo(listen: SocketAddr) -> std::process::ExitCode {
    let runtime = match runtime() {
        Ok(runtime) => runtime,
        Err(code) => return code,
    };
    eprintln!("edualcd: edualc's demo is at http://{listen}/");
    let served = runtime.block_on(async move {
        let listener = tokio::net::TcpListener::bind(listen).await?;
        dioxus::server::axum::serve(listener, router(Access::Demo)).await
    });
    match served {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(error) => {
            tracing::error!(%error, "couldn't serve the demo");
            std::process::ExitCode::FAILURE
        }
    }
}

/// At Dioxus's address, as `dx serve` sets it, over plain HTTP: for working on the page. Each
/// time `dx` builds the router anew a daemon of its own starts, and the last one's sessions
/// end.
fn serve_under_dx(token: String) -> std::process::ExitCode {
    let address = dioxus::cli_config::fullstack_address_or_localhost();
    eprintln!("edualcd: under dx, edualc's page is at http://{address}/#token={token}");
    dioxus::server::serve(move || {
        let access = Access::Token(token.clone(), Daemon::start());
        async move { Ok(router(access)) }
    })
}

/// The page and its assets, and its line, with what every line is checked against.
fn router(access: Access) -> dioxus::server::axum::Router {
    dioxus::server::router(crate::web::App)
        .route(
            "/api/line",
            dioxus::server::axum::routing::get(crate::line::line),
        )
        .layer(dioxus::server::axum::Extension(access))
}

/// A connection whose TLS handshake is done.
type Secured = tokio_rustls::server::TlsStream<tokio::net::TcpStream>;

/// A connection that says when it last read anything: a message that takes long to arrive
/// shows the line alive as it arrives, not only once it is whole (a pong sent after it waits
/// behind it, and a page's browser sends none of its own).
struct Heard<Io = Secured> {
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

/// When a line's connection last read anything, for its handler (axum's connect info).
#[derive(Debug, Clone)]
pub struct Hearing(pub watch::Receiver<Instant>);

impl
    dioxus::server::axum::extract::connect_info::Connected<
        dioxus::server::axum::serve::IncomingStream<'_, TlsListener>,
    > for Hearing
{
    fn connect_info(stream: dioxus::server::axum::serve::IncomingStream<'_, TlsListener>) -> Self {
        Self(stream.io().read.subscribe())
    }
}

/// Connections, their TLS handshakes done, as axum takes them.
struct TlsListener {
    taken: mpsc::Receiver<(Secured, SocketAddr)>,
    local: SocketAddr,
}

impl dioxus::server::axum::serve::Listener for TlsListener {
    type Io = Heard;
    type Addr = SocketAddr;

    async fn accept(&mut self) -> (Self::Io, Self::Addr) {
        match self.taken.recv().await {
            Some((secured, peer)) => (Heard::new(secured), peer),
            // The handshakes' task ended with the runtime: nothing more comes.
            None => std::future::pending().await,
        }
    }

    fn local_addr(&self) -> std::io::Result<Self::Addr> {
        Ok(self.local)
    }
}

/// Takes connections, and does each one's TLS handshake on a task of its own: one that is slow
/// doesn't hold up the others.
async fn handshakes(
    tcp: tokio::net::TcpListener,
    acceptor: tokio_rustls::TlsAcceptor,
    accepted: mpsc::Sender<(Secured, SocketAddr)>,
) {
    loop {
        let (stream, peer) = match tcp.accept().await {
            Ok(connection) => connection,
            Err(error) => {
                // Out of descriptors, say: tried again in a moment.
                tracing::warn!(%error, "couldn't take a connection");
                tokio::time::sleep(Duration::from_millis(100)).await;
                continue;
            }
        };
        if let Err(error) = stream.set_nodelay(true) {
            tracing::debug!(%error, "a connection without TCP_NODELAY");
        }
        let acceptor = acceptor.clone();
        let accepted = accepted.clone();
        tokio::spawn(async move {
            match tokio::time::timeout(HANDSHAKE, acceptor.accept(stream)).await {
                Ok(Ok(stream)) => {
                    if accepted.send((stream, peer)).await.is_err() {
                        tracing::debug!("a connection came as the server ended");
                    }
                }
                Ok(Err(error)) => tracing::info!(%error, %peer, "a TLS handshake failed"),
                Err(_elapsed) => tracing::info!(%peer, "a TLS handshake took too long"),
            }
        });
    }
}

/// Carries a page's line to the demo's script, which answers as the daemon would. It pings
/// the window every [`PING`], as `carry` does: a window that hears nothing for [`SILENT`], not
/// even the answers to its own pings (which go out only with what is written next), takes the
/// line as lost.
pub async fn play(page: WebSocket) {
    let (requests, received) = async_channel::unbounded();
    let (outbox, answers) = async_channel::unbounded();
    tokio::spawn(edualc_host::demo::run(received, outbox));
    let (mut to_page, mut from_page) = page.split();
    let mut ping = tokio::time::interval(PING);
    loop {
        tokio::select! {
            _ = ping.tick() => {
                if to_page.send(PageMessage::Ping(Vec::new().into())).await.is_err() {
                    break;
                }
            },
            asked = from_page.next() => match asked {
                Some(Ok(PageMessage::Binary(bytes))) => match ipc::decode::<ToDaemon>(&bytes) {
                    Ok(message) => {
                        if requests.send(message).await.is_err() {
                            break;
                        }
                    }
                    Err(error) => {
                        tracing::info!(%error, "a page said what isn't a message");
                        break;
                    }
                },
                Some(Ok(PageMessage::Close(_)) | Err(_)) | None => break,
                Some(Ok(_)) => {}
            },
            said = answers.recv() => match said {
                Ok(message) => match ipc::encode(&message) {
                    Ok(bytes) => {
                        if to_page.send(PageMessage::Binary(bytes.into())).await.is_err() {
                            break;
                        }
                    }
                    Err(error) => {
                        tracing::error!(%error, "a demo message didn't encode");
                        break;
                    }
                },
                Err(_) => break,
            },
        }
    }
}

/// How long a line has been silent: since its last message, or, where its connection says so,
/// since anything at all arrived, a message only part of the way there too.
struct Silence {
    heard: Option<Hearing>,
    message: Instant,
}

impl Silence {
    fn new(heard: Option<Hearing>) -> Self {
        Self {
            heard,
            message: Instant::now(),
        }
    }

    /// When the line is to be taken as gone, unless it says something before.
    fn ends_at(&self) -> Instant {
        let read = self.heard.as_ref().map(|Hearing(read)| *read.borrow());
        read.map_or(self.message, |read| read.max(self.message)) + SILENT
    }
}

/// Carries a window's or a page's line to the daemon, message by message, until either hangs
/// up, or the line goes silent for [`SILENT`] (it is pinged every [`PING`]). A line whose
/// connection says when it last read (`heard`) isn't silent while a message is arriving.
pub async fn carry(line: WebSocket, heard: Option<Hearing>, daemon: Daemon) {
    let (requests, received) = mpsc::channel(daemon::QUEUE);
    let (outbox, mut answers) = mpsc::unbounded_channel();
    let (mut to_window, mut from_window) = line.split();
    // Each way its own loop, the two polled together: an answer waiting for the window to read
    // doesn't stop its requests being read, which would leave both writing and neither reading.
    let asked = async move {
        let mut silence = Silence::new(heard);
        loop {
            // A message part of the way there stays where the socket reads it: a next taken
            // up again goes on with it.
            let next = tokio::select! {
                next = from_window.next() => Ok(next),
                () = tokio::time::sleep_until(silence.ends_at()) => Err(()),
            };
            if next.is_err() && Instant::now() < silence.ends_at() {
                // Something arrived meanwhile.
                continue;
            }
            silence.message = Instant::now();
            match next {
                Ok(Some(Ok(PageMessage::Binary(bytes)))) => match ipc::decode::<ToDaemon>(&bytes) {
                    Ok(request) => {
                        if requests.send(request).await.is_err() {
                            return;
                        }
                    }
                    Err(error) => {
                        tracing::info!(%error, "a window said what isn't a message");
                        return;
                    }
                },
                Ok(Some(Ok(PageMessage::Close(_))) | None) => return,
                // Pongs say the window is there; pings are answered by the socket itself.
                Ok(Some(Ok(_))) => {}
                Ok(Some(Err(error))) => {
                    tracing::info!(%error, "a window's line broke");
                    return;
                }
                Err(()) => {
                    tracing::info!("a window's line went silent");
                    return;
                }
            }
        }
    };
    let said = async move {
        let mut ping = tokio::time::interval(PING);
        loop {
            let message = tokio::select! {
                answer = answers.recv() => match answer {
                    Some(answer) => match ipc::encode(&answer) {
                        Ok(bytes) => PageMessage::Binary(bytes.into()),
                        Err(error) => {
                            tracing::error!(%error, "a message for a window didn't encode");
                            return;
                        }
                    },
                    // Everything that could say more is gone.
                    None => return,
                },
                _ = ping.tick() => PageMessage::Ping(Vec::new().into()),
            };
            if let Err(error) = to_window.send(message).await {
                tracing::info!(%error, "a window stopped listening");
                return;
            }
        }
    };
    // The window's connection runs to its end, whichever way the line ends: its requests close
    // as `asked` ends, its outbox as `said` does, and it leaves the hub in order.
    let pump = async {
        tokio::select! {
            () = asked => {}
            () = said => {}
        }
    };
    tokio::join!(daemon.window(received, outbox), pump);
}

#[cfg(test)]
mod tests {
    use super::*;
    use edualc_host::ipc::pairing::{Fingerprint, Server};

    /// An edualcd's TLS and WebSocket, on the loopback: a certificate made as edualcd makes
    /// its own, and a line taken the way a window's is, whose query it hands back.
    // tungstenite's handshake callback returns its own (large) error response.
    #[expect(clippy::result_large_err)]
    async fn listening() -> (
        SocketAddr,
        Fingerprint,
        tokio::task::JoinHandle<Option<String>>,
    ) {
        let made = rcgen::generate_simple_self_signed(vec!["localhost".to_owned()])
            .expect("a certificate");
        let certificate = made.cert.der().to_vec();
        let fingerprint = Fingerprint::of(&certificate);
        let acceptor =
            ipc::tls::acceptor(certificate, made.signing_key.serialize_der()).expect("an acceptor");
        let tcp = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("a port");
        let address = tcp.local_addr().expect("its address");
        let taken = tokio::spawn(async move {
            let (stream, _) = tcp.accept().await.ok()?;
            let stream = acceptor.accept(stream).await.ok()?;
            let mut query = None;
            let asked = |request: &tokio_tungstenite::tungstenite::handshake::server::Request,
                         response| {
                query = request.uri().query().map(str::to_owned);
                Ok(response)
            };
            let socket = tokio_tungstenite::accept_hdr_async(stream, asked)
                .await
                .ok()?;
            drop(socket);
            query
        });
        (address, fingerprint, taken)
    }

    /// A window reaches the edualcd it paired with, its protocol and token in the line's
    /// address; one whose certificate isn't the one paired with is refused before the token is
    /// sent.
    #[tokio::test]
    async fn a_window_trusts_exactly_the_certificate_it_paired_with() {
        let (address, fingerprint, taken) = listening().await;
        let pairing = Pairing {
            host: "127.0.0.1".to_owned(),
            port: address.port(),
            fingerprint,
            token: "t/k".to_owned(),
        };
        ipc::ws::connect(&Server::Paired(pairing.clone()), &[address])
            .await
            .expect("reached");
        let query = taken.await.expect("the server ran").expect("a line");
        assert_eq!(query, format!("token=t%2Fk&protocol={}", ipc::PROTOCOL));

        let (address, _theirs, taken) = listening().await;
        let impostor = Pairing {
            port: address.port(),
            fingerprint: Fingerprint::of(b"another certificate"),
            ..pairing
        };
        let refused = ipc::ws::connect(&Server::Paired(impostor), &[address]).await;
        assert!(
            matches!(refused, Err(ipc::Error::Certificate(_))),
            "{refused:?}"
        );
        assert_eq!(taken.await.expect("the server ran"), None, "no line opened");
    }

    #[test]
    fn only_the_token_gets_through_and_the_demo_lets_anyone() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("a runtime");
        let access =
            runtime.block_on(async { Access::Token("secret".to_owned(), Daemon::start()) });
        assert!(access.admits("secret"));
        assert!(!access.admits("secreT"));
        assert!(!access.admits(""));
        assert!(Access::Demo.admits(""));
    }

    #[test]
    fn the_address_printed_names_the_machine_where_it_listens_on_all() {
        assert_eq!(
            host_of("127.0.0.1".parse().expect("an address")),
            "127.0.0.1"
        );
        assert_ne!(host_of("0.0.0.0".parse().expect("an address")), "0.0.0.0");
        let names = certificate_names(
            "10.0.0.2".parse().expect("an address"),
            Some("box".to_owned()),
        );
        for name in ["localhost", "127.0.0.1", "::1", "10.0.0.2", "box"] {
            assert!(
                names.iter().any(|named| named == name),
                "{name} in {names:?}"
            );
        }
    }

    /// A line is silent from its last message, or from the last bytes its connection read, a
    /// message only part of the way there too, whichever came later.
    #[test]
    fn a_line_is_silent_from_whatever_it_last_said() {
        let message = Instant::now();
        let alone = Silence {
            heard: None,
            message,
        };
        assert_eq!(alone.ends_at(), message + SILENT);
        let read = watch::Sender::new(message - PING);
        let heard = Silence {
            heard: Some(Hearing(read.subscribe())),
            message,
        };
        assert_eq!(heard.ends_at(), message + SILENT);
        read.send_replace(message + PING);
        assert_eq!(heard.ends_at(), message + PING + SILENT);
    }

    /// A connection says when it last read something, and only then.
    #[tokio::test]
    async fn a_connection_says_when_it_last_read() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
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
