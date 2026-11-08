//! The server's side: its command line, the daemon it is or reaches, the token pages show, and
//! carrying each page's line on to the daemon.

use dioxus::fullstack::{CborEncoding, TypedWebsocket};
use edualc_host::ipc::{self, FromDaemon, ToDaemon, ws};
use futures_util::{SinkExt, StreamExt};

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
    about = "edualc's daemon, and the server of its page"
)]
struct Args {
    /// Run as the daemon (the windows start it so).
    #[arg(long)]
    daemon: bool,
    /// Where pages reach the server; Dioxus's address (127.0.0.1:8080 unless `dx` says
    /// otherwise) if not given. Keep it on the loopback: the line is not encrypted.
    #[arg(long, value_name = "ADDRESS")]
    listen: Option<std::net::SocketAddr>,
    /// The token pages show; a new random one if not given (or `EDUALC_TOKEN`).
    #[arg(long, env = "EDUALC_TOKEN", hide_env_values = true)]
    token: Option<String>,
    /// Serve the demo instead: Claude Code played by a script, on a demo project, for anyone
    /// (no token); nothing runs and nothing on the disk is read.
    #[arg(long, conflicts_with_all = ["daemon", "token"])]
    demo: bool,
}

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
        token,
        demo,
    } = clap::Parser::parse();
    if daemon {
        return edualc_host::daemon::run();
    }
    let access = if demo {
        Access::Demo
    } else {
        token.map_or_else(Access::new, Access::Token)
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

/// The page, its assets and server functions, with the token every line is checked against.
fn router(access: Access) -> dioxus::server::axum::Router {
    dioxus::server::router(crate::web::App).layer(dioxus::server::axum::Extension(access))
}

/// Carries a page's line to the demo's script, which answers as the daemon would.
pub async fn play(page: TypedWebsocket<ToDaemon, FromDaemon, CborEncoding>) {
    let (requests, received) = async_channel::unbounded();
    let (outbox, answers) = async_channel::unbounded();
    tokio::spawn(edualc_host::demo::run(received, outbox));
    let (mut to_page, mut from_page) = page.split();
    loop {
        tokio::select! {
            asked = from_page.next() => match asked {
                Some(Ok(message)) => {
                    if requests.send(message).await.is_err() {
                        break;
                    }
                }
                Some(Err(_)) | None => break,
            },
            said = answers.recv() => match said {
                Ok(message) => {
                    if to_page.send(message).await.is_err() {
                        break;
                    }
                }
                Err(_) => break,
            },
        }
    }
}

/// Carries a page's line on to the daemon (started if none runs), message by message, until
/// either hangs up. What the page says is checked by the daemon as a native window's is.
pub async fn carry(page: TypedWebsocket<ToDaemon, FromDaemon, CborEncoding>) {
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
    loop {
        tokio::select! {
            asked = from_page.next() => match asked {
                Some(Ok(message)) => {
                    if let Err(error) = ws::send(&mut to_daemon, &message).await {
                        tracing::info!(%error, "the daemon hung up on a page");
                        break;
                    }
                }
                Some(Err(error)) => {
                    tracing::info!(%error, "a page's line broke");
                    break;
                }
                None => break,
            },
            said = ws::recv::<_, FromDaemon>(&mut from_daemon) => match said {
                Ok(message) => {
                    if let Err(error) = to_page.send(message).await {
                        tracing::info!(%error, "a page stopped listening");
                        break;
                    }
                }
                Err(error) => {
                    tracing::info!(%error, "the daemon hung up on a page");
                    break;
                }
            },
        }
    }
    if let Err(error) = to_daemon.close().await {
        tracing::debug!(%error, "the daemon's line was gone already");
    }
}
