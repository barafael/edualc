//! Starting `claude` for the daemon: its command line, its environment, its sandbox. What the
//! window asked for is checked here, as a request from another process: the command line is
//! made from typed values alone.

use std::io::{PipeReader, PipeWriter, Write as _};
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::process::Stdio;

use rustix::io::FdFlags;
use tokio::process::{Child, Command};

use crate::claude::{Conversation, FolderConfiguration, Launch};
use crate::external::wire::SessionId;
use crate::home::{self, AccessToken, CopyBack, Home, Scratch};
use crate::sandbox::{self, Allowances, Enforcement, Own, Policy};
use crate::{mirror, process, sessions};

/// Set by Claude Code for its own children. Launched from inside a Claude Code session, the
/// child would take them as its own and think itself nested.
const INHERITED_SESSION: &[&str] = &[
    "CLAUDECODE",
    "CLAUDE_CODE_CHILD_SESSION",
    "CLAUDE_CODE_SESSION_ID",
    "CLAUDE_CODE_SESSION_ATTENDED",
    "CLAUDE_CODE_ENTRYPOINT",
    "CLAUDE_CODE_EXECPATH",
    "CLAUDE_CODE_MESSAGING_SOCKET",
    "CLAUDE_CODE_MESSAGING_TOKEN",
    "CLAUDE_CODE_SSE_PORT",
    "CLAUDE_PID",
    "CLAUDE_EFFORT",
];

#[derive(Debug, thiserror::Error)]
pub(crate) enum Error {
    #[error("{0} isn't a folder to work in")]
    NoFolder(PathBuf),
    #[error("{0:?} isn't a model's name")]
    NoModel(String),
    #[error("{0} isn't a full path")]
    NotAbsolute(PathBuf),
    #[error(
        "couldn't start claude: {0}. Is Claude Code installed and `claude` on PATH? CLAUDE_BIN can name it."
    )]
    Start(#[source] std::io::Error),
    #[error(transparent)]
    Sandbox(#[from] sandbox::Error),
    #[error("couldn't make the session its own Claude Code home: {0}")]
    Home(#[from] home::Error),
    #[error("couldn't hand the session the sign-in: {0}")]
    SignIn(#[source] std::io::Error),
    #[error(
        "processes {0:?} started with the pipe meant for the session's sign-in, so it wasn't handed over; start the session again"
    )]
    SignInPipeShared(Vec<u32>),
}

/// The name other Claude Code sessions message a session's process by (`--name`): an adjective
/// and a noun picked by the session's id, so the same each time it runs, then its project. The
/// CLI's own name comes from the process id, and changes as the session is started again.
#[derive(Debug, Clone, PartialEq, Eq, Hash, derive_more::Display)]
struct MessagingName(String);

impl MessagingName {
    /// The name of session `id` in the folder `cwd`: an adjective, a noun, the folder's name
    /// and two digits, all but the folder picked by the id.
    fn of(id: &SessionId, cwd: &Path) -> Self {
        Self(format!(
            "{}-{}-{:02}",
            sessions::PetName::of(id),
            project_part(cwd),
            sessions::id_number(id, 16) % 100
        ))
    }
}

/// The most of the folder's name a session's name takes.
const PROJECT_PART_CHARS: usize = 20;

/// The folder's name as a session's name takes it, as Claude Code writes its own names
/// (`claude_but_sane` as `claude-but-sane`): lowercase letters and digits, the rest a dash, at
/// most [`PROJECT_PART_CHARS`]. "project" for a folder with no such letters.
fn project_part(cwd: &Path) -> String {
    let folder = cwd
        .file_name()
        .map(|name| name.to_string_lossy().to_lowercase())
        .unwrap_or_default();
    let mut part = String::new();
    for character in folder.chars() {
        if character.is_ascii_alphanumeric() {
            part.push(character);
        } else if !part.is_empty() && !part.ends_with('-') {
            part.push('-');
        }
    }
    let part: String = part.chars().take(PROJECT_PART_CHARS).collect();
    match part.trim_end_matches('-') {
        "" => "project".to_owned(),
        trimmed => trimmed.to_owned(),
    }
}

/// A started process, its pipes still in it, and how far it is sandboxed. Dropped, it is
/// killed, and a sandboxed one's temporary folder removed.
#[derive(Debug)]
pub(crate) struct Spawned {
    pub(crate) child: Child,
    pub(crate) enforcement: Option<Enforcement>,
    /// A sandboxed session's own temporary folder, kept while it runs.
    pub(crate) scratch: Option<Scratch>,
    /// A sandboxed session's record to mirror for other sessions to message it by.
    pub(crate) record: Option<mirror::Source>,
    /// A resumed session whose transcript went on separately in and out of the sandbox: the
    /// copy it doesn't go on from, for the user to be told of.
    pub(crate) diverged: Option<home::Diverged>,
}

/// What a sandboxed start takes: its sandbox, its own home and temporary folder, and the
/// sign-in on its way to it, if the user is signed in.
struct Sandboxed {
    policy: Policy,
    home: Home,
    scratch: Scratch,
    sign_in: Option<SignIn>,
    diverged: Option<home::Diverged>,
}

/// The user's sign-in on its way to a sandboxed session, through a pipe the session inherits:
/// read outside the sandbox, and never in a file or a variable the session's commands could
/// read.
struct SignIn {
    token: AccessToken,
    reader: PipeReader,
    writer: PipeWriter,
}

impl SignIn {
    /// The pipe, its reading end made inheritable and its number named to the child; none if
    /// the user isn't signed in.
    fn prepare(command: &mut Command) -> Result<Option<Self>, Error> {
        let Some(token) = AccessToken::current() else {
            return Ok(None);
        };
        let (reader, writer) = std::io::pipe().map_err(Error::SignIn)?;
        // Inheritable from here on, the pipe goes to any process the daemon starts until it is
        // closed again after this child's start; `hand_over` looks for such. If that is ever
        // found, the `command-fds` crate maps a descriptor into the one child alone instead.
        rustix::io::fcntl_setfd(&reader, FdFlags::empty())
            .map_err(|errno| Error::SignIn(errno.into()))?;
        command.env(
            "CLAUDE_CODE_OAUTH_TOKEN_FILE_DESCRIPTOR",
            reader.as_raw_fd().to_string(),
        );
        Ok(Some(Self {
            token,
            reader,
            writer,
        }))
    }

    /// Hands the sign-in to `child` once no other process has the pipe. The reading end is
    /// closed here first, so no later process inherits it. A child that ended without reading
    /// it (a stand-in for `claude` in a test, say) is no failure.
    fn hand_over(self, child: u32) -> Result<(), Error> {
        let Self {
            token,
            reader,
            mut writer,
        } = self;
        rustix::io::fcntl_setfd(&reader, FdFlags::CLOEXEC)
            .map_err(|errno| Error::SignIn(errno.into()))?;
        let inode = rustix::fs::fstat(&reader)
            .map_err(|errno| Error::SignIn(errno.into()))?
            .st_ino;
        drop(reader);
        let daemon = std::process::id();
        let others: Vec<u32> = process::holding_pipe(inode)
            .into_iter()
            .filter(|pid| *pid != daemon && !process::descends_from(*pid, child))
            .collect();
        if !others.is_empty() {
            return Err(Error::SignInPipeShared(others));
        }
        match writer.write_all(token.as_bytes()) {
            Err(error) if error.kind() != std::io::ErrorKind::BrokenPipe => {
                Err(Error::SignIn(error))
            }
            Ok(()) | Err(_) => Ok(()),
        }
    }
}

/// The `claude` to run: `$CLAUDE_BIN` as the daemon was started with, or the one on `PATH`.
pub(crate) fn program() -> std::ffi::OsString {
    std::env::var_os("CLAUDE_BIN").unwrap_or_else(|| "claude".into())
}

/// Starts the process; `runtime` is the daemon's, which the process's pipes and exit are waited
/// on with.
pub(crate) fn spawn(launch: &Launch, runtime: &tokio::runtime::Handle) -> Result<Spawned, Error> {
    let cwd = launch
        .cwd
        .canonicalize()
        .ok()
        .filter(|cwd| cwd.is_dir())
        .ok_or_else(|| Error::NoFolder(launch.cwd.clone()))?;
    let mut command = Command::new(program());
    command.args([
        "-p",
        "--input-format",
        "stream-json",
        "--output-format",
        "stream-json",
        "--verbose",
        "--include-partial-messages",
        // Permission requests come to the window as `can_use_tool` control requests.
        "--permission-prompt-tool",
        "stdio",
    ]);
    // One argument each: the CLI takes an option's value only if it doesn't look like an
    // option. The ids are UUIDs, checked as they were read or made.
    match &launch.conversation {
        Conversation::New(id) => {
            command.arg(format!("--session-id={id}"));
        }
        Conversation::Continue(id) => {
            command.arg(format!("--resume={id}"));
        }
        Conversation::Fork { of, id } => {
            command.args([
                format!("--resume={of}"),
                "--fork-session".to_owned(),
                format!("--session-id={id}"),
            ]);
        }
    }
    command.arg(format!(
        "--name={}",
        MessagingName::of(launch.conversation.id(), &cwd)
    ));
    if let Some(model) = &launch.model {
        command.arg(format!("--model={}", model_name(model)?));
    }
    match launch.folder_configuration {
        FolderConfiguration::Read => {}
        FolderConfiguration::Skipped => {
            command.args(["--setting-sources=user", "--strict-mcp-config"]);
        }
    }
    for name in INHERITED_SESSION {
        command.env_remove(name);
    }
    let sandboxed = match &launch.sandbox {
        Some(wish) => Some(sandboxed(launch, wish, &cwd, &mut command)?),
        None => None,
    };
    command
        .current_dir(&cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    match sandboxed {
        Some(Sandboxed {
            policy,
            home,
            scratch,
            sign_in,
            diverged,
        }) => {
            // Restricted, the thread starts the child, which keeps the restriction, and ends.
            let (child, enforcement) = std::thread::scope(|scope| {
                scope
                    .spawn(|| {
                        let enforcement = policy.restrict_this_thread()?;
                        if let Some(note) = policy.note_for_claude(enforcement) {
                            command.arg(format!("--append-system-prompt={note}"));
                        }
                        // The child's pipes are made with the daemon's runtime, from this thread.
                        let _in_runtime = runtime.enter();
                        let child = command.spawn().map_err(Error::Start)?;
                        Ok::<_, Error>((child, enforcement))
                    })
                    .join()
                    .unwrap_or_else(|panic| std::panic::resume_unwind(panic))
            })?;
            if let (Some(sign_in), Some(pid)) = (sign_in, child.id()) {
                // On failure the child, dropped, is killed: it never had the sign-in.
                sign_in.hand_over(pid)?;
            }
            Ok(Spawned {
                child,
                enforcement: Some(enforcement),
                scratch: Some(scratch),
                record: Some(mirror::Source {
                    home,
                    id: launch.conversation.id().clone(),
                    cwd,
                }),
                diverged,
            })
        }
        None => {
            let diverged = match &launch.conversation {
                Conversation::Continue(id) | Conversation::Fork { of: id, .. } => {
                    brought_back(&cwd, id)
                }
                Conversation::New(_) => None,
            };
            let _in_runtime = runtime.enter();
            Ok(Spawned {
                child: command.spawn().map_err(Error::Start)?,
                enforcement: None,
                scratch: None,
                record: None,
                diverged,
            })
        }
    }
}

/// Before session `id` is resumed outside the sandbox: what it last wrote in its sandboxed home,
/// brought back so that it goes on from there; the copy left behind where the two went their
/// own ways. A copy that can't be brought back doesn't keep the session from starting.
fn brought_back(cwd: &Path, id: &SessionId) -> Option<home::Diverged> {
    match home::bring_back(cwd, id) {
        Ok(CopyBack::Diverged(diverged)) => Some(diverged),
        Ok(
            CopyBack::Copied | CopyBack::Unchanged | CopyBack::NoneWritten | CopyBack::UsersGoesOn,
        ) => None,
        Err(error) => {
            tracing::warn!(%error, "the sandboxed home's transcript isn't brought back");
            None
        }
    }
}

/// A sandboxed start: the session's own home seeded and its temporary folder made (see
/// `home`), its sandbox, and its environment pointing it at them.
fn sandboxed(
    launch: &Launch,
    wish: &crate::claude::SandboxWish,
    cwd: &Path,
    command: &mut Command,
) -> Result<Sandboxed, Error> {
    let home = Home::of(cwd).ok_or(home::Error::NoStateFolder)?;
    let scratch = Scratch::make(launch.conversation.id())?;
    let diverged = home::prepare(&home, cwd, &scratch, &launch.conversation)?;
    let read = absolute(&wish.read)?;
    let write = absolute(&wish.write)?;
    let policy = Policy::for_project(
        cwd,
        Own {
            home: home.path(),
            scratch: scratch.path(),
        },
        Allowances {
            desktop: wish.desktop,
            hardware: wish.hardware,
            read: &read,
            write: &write,
        },
    );
    for name in policy.hidden_variables() {
        command.env_remove(name);
    }
    command
        .env("CLAUDE_CONFIG_DIR", home.path())
        .env("CLAUDE_CODE_TMPDIR", scratch.path())
        .env("TMPDIR", scratch.path());
    let sign_in = SignIn::prepare(command)?;
    Ok(Sandboxed {
        policy,
        home,
        scratch,
        sign_in,
        diverged,
    })
}

/// A model's name or alias as the CLI takes them ("opus", "claude-sonnet-5-5[1m]"): nothing
/// that could be read as more than that.
fn model_name(model: &str) -> Result<&str, Error> {
    let fits = !model.is_empty()
        && model.len() <= 100
        && !model.starts_with('-')
        && model
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || "._:-[]".contains(character));
    if fits {
        Ok(model)
    } else {
        Err(Error::NoModel(model.to_owned()))
    }
}

fn absolute(paths: &[PathBuf]) -> Result<Vec<PathBuf>, Error> {
    paths
        .iter()
        .map(|path| {
            if path.is_absolute() {
                Ok(path.clone())
            } else {
                Err(Error::NotAbsolute(path.clone()))
            }
        })
        .collect()
}

/// Whether `path` names an existing folder, for checks before a request is taken.
pub(crate) fn is_folder(path: &Path) -> bool {
    path.is_absolute() && path.is_dir()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_model_names_pass() {
        assert!(model_name("opus").is_ok());
        assert!(model_name("claude-sonnet-5-5[1m]").is_ok());
        assert!(model_name("--dangerously-skip-permissions").is_err());
        assert!(model_name("haiku; rm -rf ~").is_err());
        assert!(model_name("").is_err());
    }

    /// A session keeps its name however often it is started: an adjective and a noun from its
    /// id, its project, then two digits from its id; other ids get other names, but for rare
    /// collisions.
    #[test]
    fn a_session_is_named_by_its_project_and_id() {
        let id = |text: &str| SessionId::try_from(text.to_owned()).expect("a session id");
        let project = Path::new("/home/someone/code/claude_but_sane");
        let session = "d1ba2743-c47a-4446-aefc-d4594dbc045d";
        let name = MessagingName::of(&id(session), project);
        assert_eq!(
            name,
            MessagingName::of(&id(session), project),
            "started again"
        );
        let mut parts = name.0.splitn(3, '-');
        let (Some(adjective), Some(noun), Some(rest)) = (parts.next(), parts.next(), parts.next())
        else {
            panic!("two words first in {name}");
        };
        let words = petname::Petnames::small();
        assert!(words.adjectives.contains(&adjective), "{name}");
        assert!(words.nouns.contains(&noun), "{name}");
        let number = rest
            .strip_prefix("claude-but-sane-")
            .expect("named after its project");
        assert!(
            number.len() == 2 && number.chars().all(|digit| digit.is_ascii_digit()),
            "{name}"
        );
        let names: std::collections::HashSet<MessagingName> = (0..200)
            .map(|_| MessagingName::of(&SessionId::new_random(), project))
            .collect();
        assert!(names.len() > 195, "only {} names of 200", names.len());
    }

    /// A project's part of the name is its folder's, as Claude Code writes its own names, and
    /// short; a folder with no Latin letters or digits is "project".
    #[test]
    fn a_project_is_named_after_its_folder() {
        assert_eq!(
            project_part(Path::new("/code/claude_but_sane")),
            "claude-but-sane"
        );
        assert_eq!(
            project_part(Path::new("/code/My Project.v2")),
            "my-project-v2"
        );
        assert_eq!(
            project_part(Path::new("/code/a-very-long-project-name-indeed")),
            "a-very-long-project"
        );
        assert_eq!(project_part(Path::new("/home/someone/文档")), "project");
        assert_eq!(project_part(Path::new("/")), "project");
    }
}
