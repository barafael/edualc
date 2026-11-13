//! edualcd's command line and what it runs: the daemon the first window starts beside itself
//! (or the image decoder, confined: see `decode`; or an agy session's adapter or its messaging
//! tool: see `agy`), and with `--listen`, as a user service runs it, its network side and the
//! page.

use std::net::{IpAddr, SocketAddr};

use clap::Parser as _;
use edualc_host::ipc::pairing::Pairing;
use edualc_host::ipc::{self, Token};
use edualc_host::net::{self, state::State};

/// Where windows are to reach edualcd unless told otherwise: this machine alone.
const DEFAULT_LISTEN: &str = "127.0.0.1:7878";

/// The command line: the daemon, its network side, or the address to pair a window with.
#[derive(Debug, clap::Parser)]
#[command(
    name = "edualcd",
    version,
    about = "edualc's daemon: runs Claude Code for edualc's windows, and serves its page",
    after_help = AFTER_HELP
)]
struct Args {
    /// Also serve windows that aren't on this machine (a page, an app) and the page itself at
    /// this address, over TLS, once they show the token. Without it, only windows on this machine, by the Unix
    /// socket, are served, and edualcd ends a few seconds after the last of them and its
    /// sessions.
    #[arg(long, value_name = "ADDRESS")]
    listen: Option<SocketAddr>,
    /// The name windows reach edualcd by, in the address it prints: the listening address, or
    /// this machine's name if it listens on all.
    #[arg(long, value_name = "HOST")]
    name: Option<String>,
    /// Print the address a window pairs with, which holds the token, and end.
    #[arg(long, conflicts_with = "demo")]
    pair: bool,
    /// Serve the demo instead, over plain HTTP (at --listen's address): Claude Code played by a
    /// script, on a demo project, for anyone (no token); nothing runs and nothing on the disk is
    /// read.
    #[arg(long)]
    demo: bool,
}

const AFTER_HELP: &str = "edualcd makes its certificate and its token on its first start and \
keeps them in $XDG_STATE_HOME/edualcd (~/.local/state/edualcd); remove that folder to make them \
anew. EDUALC_TOKEN, if set, is the token instead. Neither is taken on the command line, which \
every user of the machine can read.";

pub fn main() -> std::process::ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "edualc=info,edualc_host=info,edualc_core=info".into()),
        )
        .with_writer(std::io::stderr)
        .init();
    // An agy session's adapter, which the daemon starts in place of claude, and the messaging
    // tool it gives agy (see `agy`).
    match std::env::args().nth(1).as_deref() {
        Some(edualc_host::agy::ADAPTER_FLAG) => return edualc_host::agy::run_adapter(),
        Some(edualc_host::agy::TOOL_FLAG) => return edualc_host::agy::run_tool(),
        Some(_) | None => {}
    }
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
    } = Args::parse();
    if demo {
        return serve_demo(listen.unwrap_or_else(default_listen));
    }
    if listen.is_none() && !pair {
        return edualc_host::daemon::run(edualc_host::daemon::Options::default());
    }
    match network(listen.unwrap_or_else(default_listen), name, pair) {
        Ok(Some(options)) => edualc_host::daemon::run(options),
        Ok(None) => std::process::ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("edualcd: {message}");
            std::process::ExitCode::FAILURE
        }
    }
}

fn default_listen() -> SocketAddr {
    DEFAULT_LISTEN
        .parse()
        .unwrap_or_else(|_| SocketAddr::from(([127, 0, 0, 1], 7878)))
}

/// The daemon's network side, made ready; or, for `--pair`, the address printed and nothing
/// to run.
fn network(
    listen: SocketAddr,
    name: Option<String>,
    pair: bool,
) -> Result<Option<edualc_host::daemon::Options>, String> {
    let state = State::load(&certificate_names(listen.ip(), name.clone()))
        .map_err(|error| error.to_string())?;
    let token = token(&state)?;
    let pairing = Pairing {
        host: name.unwrap_or_else(|| host_of(listen.ip())),
        port: listen.port(),
        fingerprint: state.fingerprint(),
        token: token.clone(),
    };
    if pair {
        println!("{}", pairing.url());
        return Ok(None);
    }
    page_files()?;
    let acceptor = ipc::tls::acceptor(state.certificate, state.key)
        .map_err(|error| format!("its certificate doesn't serve: {error}"))?;
    tell_address(&pairing);
    Ok(Some(edualc_host::daemon::Options {
        network: Some(net::Config {
            listen,
            acceptor,
            token,
            page: Some(dioxus::server::router(crate::web::App)),
        }),
    }))
}

/// EDUALC_TOKEN's, or the one kept.
fn token(state: &State) -> Result<Token, String> {
    let variable = ipc::TOKEN_VARIABLE;
    match std::env::var(variable) {
        // An empty token would let every line through.
        Ok(token) if token.is_empty() => Err(format!(
            "{variable} is empty; give one no one can guess, or unset it"
        )),
        Ok(token) => Ok(Token::new(token)),
        Err(std::env::VarError::NotPresent) => Ok(state.token.clone()),
        Err(std::env::VarError::NotUnicode(_)) => Err(format!("{variable} isn't text")),
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

/// Says where edualcd is. The address with its token only to a terminal: a service's log is
/// read by more than its user.
fn tell_address(pairing: &Pairing) {
    use std::io::IsTerminal as _;
    if std::io::stderr().is_terminal() {
        eprintln!(
            "edualcd: pair a window or open a page with: {}",
            pairing.url()
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

/// The folder the page's files are served from: `DIOXUS_PUBLIC_PATH`'s, else `public` beside this
/// program, where `dx bundle` puts it. Dioxus panics if it isn't there, so it is checked first.
fn page_files() -> Result<(), String> {
    let folder = std::env::var_os("DIOXUS_PUBLIC_PATH")
        .map(std::path::PathBuf::from)
        .or_else(|| {
            std::env::current_exe()
                .ok()?
                .parent()
                .map(|beside| beside.join("public"))
        })
        .unwrap_or_default();
    if folder.is_dir() {
        Ok(())
    } else {
        Err(format!(
            "the page's files aren't in {}: set DIOXUS_PUBLIC_PATH to their folder (the user \
             service does)",
            folder.display()
        ))
    }
}

/// The demo, over plain HTTP: it runs nothing and holds no secret.
fn serve_demo(listen: SocketAddr) -> std::process::ExitCode {
    if let Err(message) = page_files() {
        eprintln!("edualcd: {message}");
        return std::process::ExitCode::FAILURE;
    }
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("edualcd: no runtime: {error}");
            return std::process::ExitCode::FAILURE;
        }
    };
    eprintln!("edualcd: edualc's demo is at http://{listen}/");
    let served = runtime.block_on(async move {
        let (lines, mut taken) = tokio::sync::mpsc::channel(8);
        tokio::spawn(async move {
            while let Some(line) = taken.recv().await {
                tokio::spawn(edualc_host::demo::run(line));
            }
        });
        let app =
            dioxus::server::router(crate::web::App).merge(net::router(net::Access::demo(lines)));
        net::serve_plain(listen, app).await
    });
    match served {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("edualcd: couldn't serve the demo: {error}");
            std::process::ExitCode::FAILURE
        }
    }
}
