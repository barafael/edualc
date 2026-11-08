//! The server's side: its command line, the daemon it is or reaches, the token pages show, and
//! carrying each page's line on to the daemon.

use dioxus::server::axum::extract::ws::{Message as PageMessage, WebSocket};
use edualc_host::ipc::{self, ToDaemon, ws};
use futures_util::{SinkExt, StreamExt};
use tokio_tungstenite::tungstenite::Message as DaemonMessage;

/// What a page must show to be let through: the token printed as the server starts; nothing,
/// for the demo, which runs nothing.
#[derive(Debug, Clone)]
pub enum Access {
    /// Pages that show this token, to the daemon.
    Token(String),
    /// Anyone, to the demo's script.
    Demo,
}

impl Access {
    /// A token no one can guess: two random UUIDs' bits, as hex.
    fn new() -> Self {
        Self::Token(format!(
            "{}{}",
            uuid::Uuid::new_v4().simple(),
            uuid::Uuid::new_v4().simple()
        ))
    }

    /// Whether `token` is the server's, compared in constant time.
    pub fn admits(&self, token: &str) -> bool {
        let Self::Token(ours) = self else {
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

/// The command line: the daemon, the decoder, or the server.
#[derive(Debug, clap::Parser)]
#[command(
    name = "edualcd",
    version,
    about = "edualc's daemon, and the server of its page",
    after_help = TOKEN_HELP
)]
struct Args {
    /// Run as the daemon (the windows start it so).
    #[arg(long)]
    daemon: bool,
    /// Where pages reach the server; Dioxus's address (127.0.0.1:8080 unless `dx` says
    /// otherwise) if not given. Keep it on the loopback: the line is not encrypted.
    #[arg(long, value_name = "ADDRESS")]
    listen: Option<std::net::SocketAddr>,
    /// Serve the demo instead: Claude Code played by a script, on a demo project, for anyone
    /// (no token); nothing runs and nothing on the disk is read.
    #[arg(long, conflicts_with = "daemon")]
    demo: bool,
}

/// The `about` after the options: where the token comes from.
const TOKEN_HELP: &str = "The token pages must show is EDUALC_TOKEN's, or a new random one if \
it isn't set. It isn't taken on the command line, which every user of the machine can read.";

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
        daemon,
        listen,
        demo,
    } = clap::Parser::parse();
    if daemon {
        return edualc_host::daemon::run();
    }
    let variable = edualc_host::ipc::TOKEN_VARIABLE;
    let token = match std::env::var(variable) {
        Ok(token) => Some(token),
        Err(std::env::VarError::NotPresent) => None,
        Err(std::env::VarError::NotUnicode(_)) => {
            eprintln!("edualcd: {variable} isn't text");
            return std::process::ExitCode::from(2);
        }
    };
    let access = match token {
        Some(_) if demo => {
            eprintln!("edualcd: the demo lets anyone in; {variable} is ignored");
            Access::Demo
        }
        _ if demo => Access::Demo,
        // An empty token would let every page through, a site's in the browser too.
        Some(token) if token.is_empty() => {
            eprintln!(
                "edualcd: the token is empty; give one no one can guess, or none for a random one"
            );
            return std::process::ExitCode::from(2);
        }
        Some(token) => Access::Token(token),
        None => Access::new(),
    };
    serve(listen, access)
}

/// Serves the page, and the lines of pages that show `access`'s token, until stopped.
fn serve(listen: Option<std::net::SocketAddr>, access: Access) -> std::process::ExitCode {
    let shown = |address: std::net::SocketAddr| match &access {
        Access::Demo => eprintln!("edualcd: edualc's demo is at http://{address}/"),
        Access::Token(token) => {
            eprintln!("edualcd: edualc's page is at http://{address}/#token={token}");
        }
    };
    match listen {
        Some(address) => {
            let runtime = match tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
            {
                Ok(runtime) => runtime,
                Err(error) => {
                    tracing::error!(%error, "no runtime");
                    return std::process::ExitCode::FAILURE;
                }
            };
            shown(address);
            let router = router(access);
            let served = runtime.block_on(async move {
                let listener = tokio::net::TcpListener::bind(address).await?;
                dioxus::server::axum::serve(listener, router).await
            });
            match served {
                Ok(()) => std::process::ExitCode::SUCCESS,
                Err(error) => {
                    tracing::error!(%error, "couldn't serve the page");
                    std::process::ExitCode::FAILURE
                }
            }
        }
        // At Dioxus's address, as `dx serve` sets it.
        None => {
            shown(dioxus::cli_config::fullstack_address_or_localhost());
            dioxus::server::serve(move || {
                let access = access.clone();
                async move { Ok(router(access)) }
            })
        }
    }
}

/// The page and its assets, and its line, with the token every line is checked against.
fn router(access: Access) -> dioxus::server::axum::Router {
    dioxus::server::router(crate::web::App)
        .route(
            "/api/line",
            dioxus::server::axum::routing::get(crate::line::line),
        )
        .layer(dioxus::server::axum::Extension(access))
}

/// Carries a page's line to the demo's script, which answers as the daemon would.
pub async fn play(page: WebSocket) {
    let (requests, received) = async_channel::unbounded();
    let (outbox, answers) = async_channel::unbounded();
    tokio::spawn(edualc_host::demo::run(received, outbox));
    let (mut to_page, mut from_page) = page.split();
    loop {
        tokio::select! {
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

/// Carries a page's line on to the daemon (started if none runs), frame by frame as it is,
/// until either hangs up. What the page says is checked by the daemon as a native window's is,
/// so the server doesn't read it.
pub async fn carry(page: WebSocket) {
    // This program is the daemon too.
    let reached = tokio::task::spawn_blocking(|| {
        let program = std::env::current_exe().map_err(ipc::ReachError::Start)?;
        ipc::reach(&program)
    })
    .await;
    let stream = match reached {
        Ok(Ok(stream)) => stream,
        Ok(Err(error)) => {
            tracing::warn!(%error, "a page's line found no daemon");
            return;
        }
        Err(error) => {
            tracing::error!(%error, "reaching the daemon failed");
            return;
        }
    };
    let daemon = match async {
        stream.set_nonblocking(true).map_err(ipc::Error::Io)?;
        let stream = tokio::net::UnixStream::from_std(stream).map_err(ipc::Error::Io)?;
        ws::connect(stream).await
    }
    .await
    {
        Ok(daemon) => daemon,
        Err(error) => {
            tracing::warn!(%error, "a page's line to the daemon didn't open");
            return;
        }
    };
    let (mut to_page, mut from_page) = page.split();
    let (mut to_daemon, mut from_daemon) = daemon.split();
    // Each way its own loop, the two polled together: a message waiting for the daemon to read
    // doesn't stop what the daemon says being read, which would leave both writing and neither
    // reading.
    let asked = async {
        loop {
            match from_page.next().await {
                Some(Ok(PageMessage::Binary(bytes))) => {
                    if let Err(error) = to_daemon.send(DaemonMessage::Binary(bytes)).await {
                        tracing::info!(%error, "the daemon hung up on a page");
                        return;
                    }
                }
                Some(Ok(PageMessage::Close(_))) | None => return,
                // Pings are answered by the socket itself; a page sends no text.
                Some(Ok(_)) => {}
                Some(Err(error)) => {
                    tracing::info!(%error, "a page's line broke");
                    return;
                }
            }
        }
    };
    let said = async {
        loop {
            match from_daemon.next().await {
                Some(Ok(DaemonMessage::Binary(bytes))) => {
                    if let Err(error) = to_page.send(PageMessage::Binary(bytes)).await {
                        tracing::info!(%error, "a page stopped listening");
                        return;
                    }
                }
                Some(Ok(DaemonMessage::Close(_))) | None => {
                    tracing::info!("the daemon hung up on a page");
                    return;
                }
                Some(Ok(_)) => {}
                Some(Err(error)) => {
                    tracing::info!(%error, "the daemon's line broke");
                    return;
                }
            }
        }
    };
    tokio::select! {
        () = asked => {}
        () = said => {}
    }
    if let Err(error) = to_daemon.close().await {
        tracing::debug!(%error, "the daemon's line was gone already");
    }
}
