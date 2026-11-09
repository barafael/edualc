//! How the window starts: its command line read, its D-Bus proxies started (see `buses`), its
//! edualcd reached, where it works found on edualcd's machine, and the window confined before it
//! opens.

use std::path::PathBuf;

use slint::ComponentHandle;

use crate::external::wire::SessionId;
use crate::ipc::pairing::Server;
use crate::ipc::servers::{self, Servers};
use crate::ipc::{Answer, Read};
use crate::line;
use crate::sessions::Saved;
use crate::{app, buses, confine, decode, desktop, generated, ui};

/// A native desktop window for Claude Code.
///
/// Its arguments read as `claude`'s own: where to work, which model, a session to continue, and
/// a first prompt. Claude Code runs where its edualcd does (this machine's, or another's it was
/// paired with): the folder is one there.
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
    /// The edualcd to reach, by the name it was paired as; the last one paired if not given.
    #[arg(long, value_name = "NAME")]
    server: Option<String>,
    /// Pair with an edualcd, by the address it prints (`edualcd --pair`), and end. A demo's
    /// address (`edualcd --demo`) pairs too.
    #[arg(long, value_name = "ADDRESS", conflicts_with_all = ["resume", "dir", "prompt"])]
    pair: Option<String>,
    /// The name to pair it as: its host's, if not given.
    #[arg(long = "as", value_name = "NAME", requires = "pair")]
    pair_as: Option<String>,
    /// The folder to work in, on edualcd's machine; where edualc was started, if that is a
    /// folder there too, else the home folder there.
    dir: Option<PathBuf>,
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

/// A session's id, as Claude Code writes one; anything else is refused before it reaches
/// claude's command line.
fn session_id(id: &str) -> Result<SessionId, String> {
    SessionId::try_from(id.to_owned()).map_err(|error| error.to_string())
}

/// Why edualc doesn't start where its command line points.
#[derive(Debug, thiserror::Error)]
enum WhereError {
    #[error("{0}")]
    Folder(String),
    /// No folder by that name, nor a path: `claude` takes its prompt first, and this may be one.
    #[error("{problem}. A first prompt comes after the folder, as in: edualc . \"{prompt}\"")]
    Prompt { prompt: String, problem: String },
    #[error("There is no session {0}")]
    NoSession(SessionId),
    #[error(transparent)]
    Line(#[from] line::Error),
}

/// `path` made whole against where edualc was started, without looking at this machine's files:
/// the folder is one on edualcd's machine.
fn whole(path: &std::path::Path) -> PathBuf {
    std::path::absolute(path).unwrap_or_else(|_no_current_folder| path.to_owned())
}

/// The folder to work in, as edualcd finds it on its machine: as named; else, continuing a
/// session, the one it was started in (`claude --resume` goes on in whatever folder it is
/// started from, its tools working there); else where edualc was started, if that is a folder
/// there, or the home folder there. A session to continue that no transcript has is refused.
fn working_folder(
    connected: &mut line::Connected,
    dir: Option<PathBuf>,
    resume: Option<&SessionId>,
) -> Result<PathBuf, WhereError> {
    let saved = match resume {
        Some(id) => match connected.saved(id)? {
            Saved::Nowhere => return Err(WhereError::NoSession(id.clone())),
            Saved::StartedIn(folder) => Some(folder),
            Saved::Unplaced => None,
        },
        None => None,
    };
    let (asked, named) = match (dir, saved) {
        (Some(dir), _) => (whole(&dir), Some(dir)),
        (None, Some(started)) => (started, None),
        (None, None) => {
            let here = std::env::current_dir().unwrap_or_default();
            return Ok(match folder(connected, here)? {
                Ok(folder) => folder,
                // This machine's folder isn't edualcd's: its home folder, as a page opens.
                Err(_not_there) => connected
                    .greeting
                    .home
                    .clone()
                    .unwrap_or_else(|| PathBuf::from("/")),
            });
        }
    };
    match folder(connected, asked)? {
        Ok(folder) => Ok(folder),
        Err(problem) => Err(match named {
            Some(dir) if !dir.as_os_str().as_encoded_bytes().contains(&b'/') => {
                WhereError::Prompt {
                    prompt: dir.display().to_string(),
                    problem,
                }
            }
            Some(_) | None => WhereError::Folder(problem),
        }),
    }
}

/// The folder at `path` on edualcd's machine, made whole, or why it isn't one.
fn folder(
    connected: &mut line::Connected,
    path: PathBuf,
) -> Result<Result<PathBuf, String>, WhereError> {
    let Answer::Folder(found) = connected.ask(Read::Folder(path))? else {
        return Err(line::Error::Unanswered.into());
    };
    Ok(found)
}

/// Where the paired edualcds are kept: `~/.config/edualc/servers.json`.
fn servers_file() -> Result<PathBuf, servers::Error> {
    Ok(dirs::config_dir()
        .ok_or(servers::Error::NoConfig)?
        .join("edualc")
        .join("servers.json"))
}

/// The paired edualcds, and where they are kept.
fn paired() -> Result<(Servers, PathBuf), servers::Error> {
    let path = servers_file()?;
    Ok((Servers::load(&path)?, path))
}

/// Pairs with the edualcd at `address` (or the demo there), as `name` or its host's.
fn pair(address: &str, name: Option<String>) -> std::process::ExitCode {
    let pairing = match Server::parse(address) {
        Ok(pairing) => pairing,
        Err(error) => {
            eprintln!("edualc: {error}");
            return std::process::ExitCode::from(2);
        }
    };
    // Reached once, so that a wrong address or token is said now rather than as a window opens.
    if let Err(error) = line::connect(&pairing, line::Lookup::Once) {
        eprintln!("edualc: {error}");
        return std::process::ExitCode::FAILURE;
    }
    let saved = paired().and_then(|(mut servers, path)| {
        let name = name.unwrap_or_else(|| servers.name_for(&pairing));
        servers.pair(name.clone(), pairing);
        servers.save(&path).map(|()| (name, path))
    });
    match saved {
        Ok((name, path)) => {
            eprintln!("edualc: paired with {name} (kept in {})", path.display());
            std::process::ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("edualc: {error}");
            std::process::ExitCode::FAILURE
        }
    }
}

pub(crate) fn main() -> Result<std::process::ExitCode, slint::PlatformError> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "edualc=info".into()),
        )
        .with_writer(std::io::stderr)
        .init();

    // The image decoder the window starts, confined before it reads (see `decode`).
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
        server,
        pair: pairing,
        pair_as,
        dir,
        prompt,
    } = clap::Parser::parse();
    if let Some(address) = pairing {
        return Ok(pair(&address, pair_as));
    }
    // The first process starts the D-Bus proxies and the window as its child, and waits for it.
    if std::env::var_os(buses::PROXIED).is_none() {
        return Ok(buses::run_window().unwrap_or_else(|error| {
            eprintln!("edualc: couldn't start the window: {error}");
            std::process::ExitCode::FAILURE
        }));
    }
    if !buses::end_with_first_process() {
        return Ok(std::process::ExitCode::FAILURE);
    }
    let chosen = paired().and_then(|(servers, _path)| servers.chosen(server.as_deref()));
    let (name, pairing) = match chosen {
        Ok(chosen) => chosen,
        Err(error) => {
            eprintln!("edualc: {error}");
            return Ok(std::process::ExitCode::FAILURE);
        }
    };
    // In this order: edualcd is reached and the desktop asked while the window is free; then it
    // is confined, and all it makes after keeps that.
    let mut connected = match line::connect(&pairing, line::Lookup::Once) {
        Ok(connected) => connected,
        Err(error) => {
            eprintln!("edualc: {name}: {error}");
            return Ok(std::process::ExitCode::FAILURE);
        }
    };
    let cwd = match working_folder(&mut connected, dir, resume.as_ref()) {
        Ok(folder) => folder,
        Err(error) => {
            eprintln!("edualc: {error}");
            return Ok(std::process::ExitCode::from(2));
        }
    };
    // Asked before the window is confined, of programs it may not run then.
    let (fonts, text_size) = std::thread::scope(|scope| {
        let fonts = scope.spawn(crate::appearance::fonts);
        let text_size = crate::appearance::desktop_text_size();
        let fonts = fonts
            .join()
            .unwrap_or_else(|panic| std::panic::resume_unwind(panic));
        (fonts, text_size)
    });
    let desktop = app::Desktop::of(&connected.greeting, text_size);
    match confine::confine(&connected.addresses) {
        Ok(confinement) => tracing::info!(?confinement, "the window is confined"),
        Err(error) => {
            eprintln!("edualc: {error}");
            return Ok(std::process::ExitCode::FAILURE);
        }
    }
    let (events, received) = async_channel::unbounded();
    let (link, greeting) = line::run(connected, events.clone());
    let window = generated::AppWindow::new()?;
    // Before `show`: the compositor is told the id once, as the window is made.
    slint::set_xdg_app_id(desktop::APP_ID)?;
    ui::wire(&window, &events);
    let frontend = ui::SlintFrontend::new(window.clone_strong(), fonts, events.clone());
    let mut app = app::App::new(
        frontend,
        events,
        link,
        desktop,
        greeting.settings,
        cwd,
        model,
    );
    app.begin(
        greeting.hosted,
        greeting.live,
        resume,
        mode,
        prompt.as_deref(),
    );
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
        // A folder on edualcd's machine: taken as written, not looked for here.
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

    /// Pairing takes an address and, maybe, a name; it starts no window, so a folder or a
    /// session beside it is refused.
    #[test]
    fn pairing_stands_alone() {
        let paired = args("--pair https://h:1/#token=t --as box").expect("valid arguments");
        assert_eq!(paired.pair.as_deref(), Some("https://h:1/#token=t"));
        assert_eq!(paired.pair_as.as_deref(), Some("box"));
        assert!(args("--pair https://h:1/ /tmp").is_err());
        assert!(args("--as box").is_err());
        assert_eq!(
            args("--server box")
                .map(|parsed| parsed.server)
                .ok()
                .flatten(),
            Some("box".to_owned())
        );
    }

    /// A relative folder is made whole against where edualc started, without looking for it
    /// here.
    #[test]
    fn a_relative_folder_is_made_whole_without_looking() {
        let here = std::env::current_dir().expect("a current folder");
        assert_eq!(
            whole(std::path::Path::new("no/such/folder")),
            here.join("no/such/folder")
        );
        assert_eq!(whole(std::path::Path::new("/abs")), PathBuf::from("/abs"));
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
