//! edualc, a desktop window for Claude Code. The `claude` CLI does the work (the model, tools,
//! permissions, sessions, MCP servers) and is spoken to in stream-json, as the Agent SDKs
//! speak to it; this window shows what it does and asks what it asks.

use std::path::{Path, PathBuf};

use slint::ComponentHandle;

mod app;
mod appearance;
mod buses;
mod chat;
mod claude;
mod confine;
mod daemon;
mod decode;
mod desktop;
mod external;
mod grouping;
mod home;
mod ipc;
mod link;
mod markdown;
mod mirror;
mod plan;
mod process;
mod sandbox;
mod sessions;
mod settings;
mod spawn;
mod tools;
mod trust;
mod watcher;

/// The UI Slint compiles from `ui/app.slint` (see `build.rs`).
// Slint's code, not ours: it shares state through `Rc` and cells, and discards what it likes.
#[allow(
    clippy::disallowed_types,
    clippy::let_underscore_must_use,
    clippy::let_underscore_untyped,
    clippy::wildcard_enum_match_arm,
    clippy::indexing_slicing,
    clippy::match_same_arms,
    clippy::redundant_clone,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_lossless,
    clippy::needless_pass_by_value,
    clippy::default_trait_access,
    clippy::redundant_closure_for_method_calls,
    clippy::items_after_statements,
    clippy::unneeded_field_pattern,
    clippy::ignored_unit_patterns,
    clippy::map_unwrap_or,
    clippy::derive_partial_eq_without_eq,
    clippy::bool_to_int_with_if
)]
mod generated {
    include!(concat!(env!("OUT_DIR"), "/window.rs"));
}
mod ui;

use external::wire::SessionId;

const USAGE: &str =
    "usage: edualc [--model MODEL] [--permission-mode MODE] [--resume SESSION] [DIR] [PROMPT]";

/// What the command line asks for, as `claude`'s own would: where, which model, a session to
/// continue, and a first prompt.
#[derive(Debug, Default)]
struct Args {
    dir: Option<PathBuf>,
    model: Option<String>,
    mode: Option<app::PermissionMode>,
    resume: Option<SessionId>,
    prompt: Option<String>,
}

impl Args {
    fn parse(mut args: impl Iterator<Item = String>) -> Result<Self, String> {
        let mut parsed = Args::default();
        let mut positional = Vec::new();
        while let Some(arg) = args.next() {
            match arg.as_str() {
                "-m" | "--model" => {
                    parsed.model = Some(args.next().ok_or("--model needs a model")?);
                }
                "--permission-mode" => {
                    let name = args.next().ok_or("--permission-mode needs a mode")?;
                    let mode = name.parse().map_err(|error| {
                        format!("unknown permission mode {name}: {error}\n{USAGE}")
                    })?;
                    parsed.mode = Some(mode);
                }
                "-r" | "--resume" => {
                    let id = args.next().ok_or("--resume needs a session id")?;
                    let id =
                        SessionId::try_from(id).map_err(|error| format!("{error}\n{USAGE}"))?;
                    parsed.resume = Some(id);
                }
                "-h" | "--help" => return Err(USAGE.to_owned()),
                _ if arg.starts_with('-') => return Err(format!("unknown option {arg}\n{USAGE}")),
                _ => positional.push(arg),
            }
        }
        let mut positional = positional.into_iter();
        parsed.dir = positional.next().map(PathBuf::from);
        parsed.prompt = positional.next();
        if positional.next().is_some() {
            return Err(USAGE.to_owned());
        }
        Ok(parsed)
    }

    /// The working directory: as given, or where this was started.
    fn working_directory(&self) -> PathBuf {
        let path = self
            .dir
            .clone()
            .or_else(|| std::env::current_dir().ok())
            .unwrap_or_else(|| dirs::home_dir().unwrap_or_default());
        path.canonicalize().unwrap_or(path)
    }
}

/// Set once the window was started again with its D-Bus proxies.
const PROXIED: &str = "EDUALC_PROXIED";

/// Starts this program again in this process, its bus the daemon's filtering proxies (none
/// without them); returns only if that failed.
fn proxied(buses: &buses::Buses) -> std::io::Error {
    use std::os::unix::process::CommandExt;
    let program = match std::env::current_exe() {
        Ok(program) => program,
        Err(error) => return error,
    };
    let mut command = std::process::Command::new(program);
    command.args(std::env::args_os().skip(1)).env(PROXIED, "1");
    match &buses.session {
        Some(socket) => command.env(
            "DBUS_SESSION_BUS_ADDRESS",
            format!("unix:path={}", socket.display()),
        ),
        None => command.env_remove("DBUS_SESSION_BUS_ADDRESS"),
    };
    match &buses.accessibility {
        Some(socket) => command.env(
            "AT_SPI_BUS_ADDRESS",
            format!("unix:path={}", socket.display()),
        ),
        None => command.env_remove("AT_SPI_BUS_ADDRESS"),
    };
    command.exec()
}

fn main() -> Result<std::process::ExitCode, slint::PlatformError> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "edualc=info".into()),
        )
        .with_writer(std::io::stderr)
        .init();

    // The daemon the windows start: it runs Claude Code, and has no window.
    if std::env::args().nth(1).as_deref() == Some("--daemon") {
        return Ok(daemon::run());
    }
    // The image decoder the daemon and the window start, confined before it reads (see
    // `decode`).
    if let Some(job) = std::env::args()
        .nth(1)
        .and_then(|argument| argument.parse::<decode::Job>().ok())
    {
        return Ok(decode::run(job));
    }

    let args = match Args::parse(std::env::args().skip(1)) {
        Ok(args) => args,
        Err(message) => {
            eprintln!("{message}");
            std::process::exit(2);
        }
    };
    let cwd = args.working_directory();
    if !Path::new(&cwd).is_dir() {
        eprintln!("not a directory: {}", cwd.display());
        std::process::exit(2);
    }

    // In this order: the daemon is reached (and started) and the desktop asked while the window
    // is free; then it is confined, and all it makes after keeps that.
    let mut connected = match link::connect() {
        Ok(connected) => connected,
        Err(error) => {
            eprintln!("edualc: {error}");
            return Ok(std::process::ExitCode::FAILURE);
        }
    };
    // Once: started again with the D-Bus proxies for its bus, which can't be changed later in a
    // running process without `unsafe`.
    if std::env::var_os(PROXIED).is_none() {
        let error = proxied(&connected.buses);
        eprintln!("edualc: couldn't start again with the D-Bus proxies: {error}");
        return Ok(std::process::ExitCode::FAILURE);
    }
    let desktop = app::Desktop::find();
    match confine::confine() {
        Ok(confinement) => tracing::info!(?confinement, "the window is confined"),
        Err(error) => {
            eprintln!("edualc: {error}");
            return Ok(std::process::ExitCode::FAILURE);
        }
    }
    let (events, received) = async_channel::unbounded();
    let hosted = std::mem::take(&mut connected.hosted);
    let link = match link::run(connected, events.clone()) {
        Ok(link) => link,
        Err(error) => {
            eprintln!("edualc: {error}");
            return Ok(std::process::ExitCode::FAILURE);
        }
    };
    let window = generated::AppWindow::new()?;
    ui::wire(&window, &events);
    let mut app = app::App::new(
        window.clone_strong(),
        events,
        link,
        desktop,
        cwd,
        args.model,
    );
    app.begin(hosted, args.resume, args.mode, args.prompt.as_deref());
    slint::spawn_local(app.run(received)).expect("the event loop is set up");
    window.show()?;
    slint::run_event_loop_until_quit()?;
    Ok(std::process::ExitCode::SUCCESS)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(line: &str) -> Result<Args, String> {
        Args::parse(line.split_whitespace().map(str::to_owned))
    }

    #[test]
    fn arguments_read_like_claudes() {
        let id = "ee79c043-bfa7-44c0-82b6-509839a05c22";
        let parsed = args(&format!(
            "--model haiku --permission-mode plan -r {id} /tmp hello"
        ))
        .expect("valid arguments");
        assert_eq!(parsed.mode, Some(app::PermissionMode::Plan));
        assert_eq!(parsed.model.as_deref(), Some("haiku"));
        assert_eq!(parsed.resume, SessionId::try_from(id.to_owned()).ok());
        assert_eq!(parsed.dir, Some(PathBuf::from("/tmp")));
        assert_eq!(parsed.prompt.as_deref(), Some("hello"));
        assert!(args("--resume").is_err());
        // Ids end up on claude's command line: one that reads as an option is refused.
        assert!(args("--resume --dangerously-skip-permissions").is_err());
        assert!(args("--resume ../../etc/x").is_err());
        assert!(args("--bogus").is_err());
        assert!(args("--permission-mode sometimes").is_err());
    }
}
