//! edualc, a desktop window for Claude Code. The `claude` CLI does the work (the model, tools,
//! permissions, sessions, MCP servers) and is spoken to in stream-json, as the Agent SDKs
//! speak to it; this window shows what it does and asks what it asks.

use std::path::{Path, PathBuf};

use slint::ComponentHandle;

mod app;
mod appearance;
mod buses;
mod chat;
mod cjk;
mod claude;
mod confine;
mod daemon;
mod decode;
mod desktop;
mod external;
mod folder;
mod grouping;
mod home;
mod ipc;
mod link;
mod markdown;
mod mirror;
mod plan;
mod process;
mod relay;
mod sandbox;
mod sessions;
mod settings;
mod sidebar;
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

/// A native desktop window for Claude Code.
///
/// Its arguments read as `claude`'s own: where to work, which model, a session to continue, and
/// a first prompt. It needs `claude` (Claude Code) on the PATH.
#[derive(Debug, clap::Parser)]
#[command(name = "edualc", version, about)]
struct Args {
    /// The model, as `claude --model` takes it (an alias such as `sonnet`, or a full name).
    #[arg(short, long)]
    model: Option<String>,
    /// The permission mode to start in.
    #[arg(long = "permission-mode", value_name = "MODE", value_parser = permission_mode)]
    mode: Option<app::PermissionMode>,
    /// A session to continue, by its id.
    #[arg(short, long, value_name = "SESSION", value_parser = session_id)]
    resume: Option<SessionId>,
    /// The folder to work in; where edualc was started, if none.
    dir: Option<PathBuf>,
    /// A first prompt.
    prompt: Option<String>,
}

/// A permission mode by the name Claude Code gives it, the modes listed where it isn't one.
fn permission_mode(name: &str) -> Result<app::PermissionMode, String> {
    name.parse().map_err(|error: strum::ParseError| {
        let modes: Vec<&str> = <app::PermissionMode as strum::IntoEnumIterator>::iter()
            .map(<&str>::from)
            .collect();
        format!("{error}: one of {}", modes.join(", "))
    })
}

/// A session's id, as Claude Code writes one; anything else is refused before it reaches
/// claude's command line.
fn session_id(id: &str) -> Result<SessionId, String> {
    SessionId::try_from(id.to_owned()).map_err(|error| error.to_string())
}

impl Args {
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

    // Help and the version are printed here, and wrong arguments said, before anything starts.
    let args = <Args as clap::Parser>::parse();
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
    // Before `show`: the compositor is told the id once, as the window is made.
    slint::set_xdg_app_id(desktop::APP_ID)?;
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
    use clap::{CommandFactory as _, Parser as _};

    use super::*;

    fn args(line: &str) -> Result<Args, clap::Error> {
        Args::try_parse_from(std::iter::once("edualc").chain(line.split_whitespace()))
    }

    /// The command line as clap checks it in debug builds: no clashing names or broken values.
    #[test]
    fn the_command_line_holds_together() {
        Args::command().debug_assert();
    }

    /// Help and the version are asked for as anywhere, wherever they stand: shown, and nothing
    /// else done.
    #[test]
    fn help_and_version_are_asked_for() {
        for line in ["--help", "-h", "--model haiku --help"] {
            let shown = args(line).map(|_| ()).map_err(|error| error.kind());
            assert_eq!(shown, Err(clap::error::ErrorKind::DisplayHelp), "{line}");
        }
        for line in ["--version", "-V", "/tmp --version"] {
            let shown = args(line).map(|_| ()).map_err(|error| error.kind());
            assert_eq!(shown, Err(clap::error::ErrorKind::DisplayVersion), "{line}");
        }
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
        assert!(args("/tmp hello extra").is_err());
        let joined = args("--model=opus --permission-mode=acceptEdits").expect("valid arguments");
        assert_eq!(joined.model.as_deref(), Some("opus"));
        assert_eq!(joined.mode, Some(app::PermissionMode::AcceptEdits));
    }
}
