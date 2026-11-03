//! edualc, a desktop window for Claude Code. The `claude` CLI does the work (the model, tools,
//! permissions, sessions, MCP servers) and is spoken to in stream-json, as the Agent SDKs
//! speak to it; this window shows what it does and asks what it asks.

use std::path::PathBuf;

use slint::ComponentHandle;

use folder::{Folder, FolderError};

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
    #[arg(long = "permission-mode", value_name = "MODE", value_parser = permission_modes())]
    mode: Option<app::PermissionMode>,
    /// A session to continue, by its id.
    #[arg(short, long, value_name = "SESSION", value_parser = session_id)]
    resume: Option<SessionId>,
    /// The folder to work in; where edualc was started, if none.
    #[arg(value_parser = folder)]
    dir: Option<Folder>,
    /// A first prompt.
    prompt: Option<String>,
}

/// The permission modes by the names Claude Code gives them, each with what it lets through, so
/// help lists them and a wrong one is told the right ones.
fn permission_modes() -> impl clap::builder::TypedValueParser<Value = app::PermissionMode> {
    use clap::builder::{PossibleValue, PossibleValuesParser, TypedValueParser as _};
    PossibleValuesParser::new(
        <app::PermissionMode as strum::IntoEnumIterator>::iter()
            .map(|mode| PossibleValue::new(<&'static str>::from(mode)).help(mode.description())),
    )
    .try_map(|name| name.parse::<app::PermissionMode>())
}

/// Why the folder named on the command line isn't worked in.
#[derive(Debug, thiserror::Error)]
enum DirError {
    #[error(transparent)]
    Folder(#[from] FolderError),
    /// No folder by that name, nor a path: `claude` takes its prompt first, and this may be one.
    #[error("{source}. A first prompt comes after the folder, as in: edualc . \"{prompt}\"")]
    Prompt {
        prompt: String,
        #[source]
        source: FolderError,
    },
}

/// The folder a command line names, relative to where edualc was started.
fn folder(text: &str) -> Result<Folder, DirError> {
    Folder::at(PathBuf::from(text)).map_err(|error| match error {
        FolderError::Missing(_) if !text.contains('/') => DirError::Prompt {
            prompt: text.to_owned(),
            source: error,
        },
        FolderError::Missing(_) | FolderError::NotAFolder(_) | FolderError::Unreadable { .. } => {
            DirError::Folder(error)
        }
    })
}

/// Where edualc was started, or the home folder where that is gone.
fn here() -> Result<Folder, FolderError> {
    Folder::at(
        std::env::current_dir()
            .ok()
            .or_else(dirs::home_dir)
            .unwrap_or_default(),
    )
}

/// Why edualc doesn't start where its command line points.
#[derive(Debug, thiserror::Error)]
enum WhereError {
    #[error(transparent)]
    Folder(#[from] FolderError),
    #[error("There is no session {0}")]
    NoSession(SessionId),
}

/// The folder to work in: as named; else, continuing a session, the one it was started in
/// (`claude --resume` goes on in whatever folder it is started from, its tools working there);
/// else where edualc was started. A session to continue that no transcript has is refused.
fn working_folder(dir: Option<Folder>, resume: Option<&SessionId>) -> Result<Folder, WhereError> {
    let saved = resume.map(|id| (id, sessions::saved(id)));
    Ok(match (dir, saved) {
        (_, Some((id, sessions::Saved::Nowhere))) => return Err(WhereError::NoSession(id.clone())),
        (Some(folder), _) => folder,
        (None, Some((_, sessions::Saved::StartedIn(started)))) => Folder::at(started)?,
        (None, Some((_, sessions::Saved::Unplaced)) | None) => here()?,
    })
}

/// A session's id, as Claude Code writes one; anything else is refused before it reaches
/// claude's command line.
fn session_id(id: &str) -> Result<SessionId, String> {
    SessionId::try_from(id.to_owned()).map_err(|error| error.to_string())
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
    let Args {
        model,
        mode,
        resume,
        dir,
        prompt,
    } = clap::Parser::parse();
    let cwd = match working_folder(dir, resume.as_ref()) {
        Ok(folder) => folder.into_path(),
        Err(error) => {
            eprintln!("edualc: {error}");
            return Ok(std::process::ExitCode::from(2));
        }
    };

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
    let mut app = app::App::new(window.clone_strong(), events, link, desktop, cwd, model);
    app.begin(hosted, resume, mode, prompt.as_deref());
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
        assert_eq!(
            parsed.dir.map(Folder::into_path),
            PathBuf::from("/tmp").canonicalize().ok()
        );
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

    /// A folder that can't be worked in is said as the folder list says it; a lone word that
    /// names none may be a prompt, given first as `claude` takes it, and is told where it goes.
    #[test]
    fn a_wrong_folder_is_said_before_anything_starts() {
        let said = |argument: &str| {
            Args::try_parse_from(["edualc", argument])
                .map(|_| ())
                .map_err(|error| error.to_string())
                .expect_err(argument)
        };
        let prompt = said("explain this repository");
        assert!(
            prompt.contains("There is no folder explain this repository")
                && prompt.contains("edualc . \"explain this repository\""),
            "{prompt}"
        );
        let missing = said("/no/such/edualc/folder");
        assert!(
            missing.contains("There is no folder /no/such/edualc/folder")
                && !missing.contains("prompt"),
            "{missing}"
        );
        let manifest = concat!(env!("CARGO_MANIFEST_DIR"), "/Cargo.toml");
        assert!(
            said(manifest).contains(&format!("{manifest} is a file, not a folder")),
            "{manifest}"
        );
    }

    /// Help lists every permission mode by the name Claude Code takes, with what it lets through.
    #[test]
    fn help_lists_the_permission_modes() {
        let help = Args::command().render_long_help().to_string();
        for mode in <app::PermissionMode as strum::IntoEnumIterator>::iter() {
            let name: &str = mode.into();
            let listed = help.lines().any(|line| {
                line.trim_start()
                    .strip_prefix(&format!("- {name}:"))
                    .is_some_and(|rest| rest.trim() == mode.description())
            });
            assert!(listed, "{name} in\n{help}");
        }
    }
}
