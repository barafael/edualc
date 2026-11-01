//! Starting `claude` for the daemon: its command line, its environment, its sandbox. What the
//! window asked for is checked here, as a request from another process: the command line is
//! made from typed values alone.

use std::path::{Path, PathBuf};
use std::process::Stdio;

use tokio::process::{Child, Command};

use crate::claude::{Conversation, Launch};
use crate::external::wire::SessionId;
use crate::sandbox::{self, Allowances, Enforcement, Policy};

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
}

/// The name other Claude Code sessions message a session's process by (`--name`): "edualc-",
/// then an adjective and a noun picked by the session's id, so the same each time it runs. The
/// CLI's own name comes from the process id, and changes as the session is started again.
#[derive(Debug, Clone, PartialEq, Eq, Hash, derive_more::Display)]
struct MessagingName(String);

impl MessagingName {
    fn of(id: &SessionId) -> Self {
        let digits: String = id
            .as_str()
            .chars()
            .filter(char::is_ascii_hexdigit)
            .collect();
        let number = |at: usize| {
            digits
                .get(at..at + 8)
                .and_then(|hex| u32::from_str_radix(hex, 16).ok())
                .and_then(|number| usize::try_from(number).ok())
                .unwrap_or(0)
        };
        let words = petname::Petnames::small();
        let word = |list: &[&'static str], number: usize| {
            list.get(number % list.len().max(1))
                .copied()
                .unwrap_or("session")
        };
        Self(format!(
            "edualc-{}-{}",
            word(&words.adjectives, number(0)),
            word(&words.nouns, number(8))
        ))
    }
}

/// A started process, its pipes still in it, and how far it is sandboxed. Dropped, it is
/// killed.
#[derive(Debug)]
pub(crate) struct Spawned {
    pub(crate) child: Child,
    pub(crate) enforcement: Option<Enforcement>,
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
        MessagingName::of(launch.conversation.id())
    ));
    if let Some(model) = &launch.model {
        command.arg(format!("--model={}", model_name(model)?));
    }
    for name in INHERITED_SESSION {
        command.env_remove(name);
    }
    let policy = match &launch.sandbox {
        Some(wish) => {
            let read = absolute(&wish.read)?;
            let write = absolute(&wish.write)?;
            let allowances = Allowances {
                desktop: wish.desktop,
                read: &read,
                write: &write,
            };
            Some(Policy::for_project(&cwd, allowances))
        }
        None => None,
    };
    if let Some(policy) = &policy {
        for name in policy.hidden_variables() {
            command.env_remove(name);
        }
    }
    command
        .current_dir(&cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    match &policy {
        // Restricted, the thread starts the child, which keeps the restriction, and ends.
        Some(policy) => std::thread::scope(|scope| {
            scope
                .spawn(|| {
                    let enforcement = policy.restrict_this_thread()?;
                    // The child's pipes are made with the daemon's runtime, from this thread.
                    let _in_runtime = runtime.enter();
                    let child = command.spawn().map_err(Error::Start)?;
                    Ok(Spawned {
                        child,
                        enforcement: Some(enforcement),
                    })
                })
                .join()
                .unwrap_or_else(|panic| std::panic::resume_unwind(panic))
        }),
        None => {
            let _in_runtime = runtime.enter();
            Ok(Spawned {
                child: command.spawn().map_err(Error::Start)?,
                enforcement: None,
            })
        }
    }
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

    /// A session keeps its name however often it is started: the name comes from its id alone,
    /// "edualc-" and two lowercase words; other ids get other names, but for rare collisions.
    #[test]
    fn a_session_is_named_by_its_id() {
        let id = |text: &str| SessionId::try_from(text.to_owned()).expect("a session id");
        let session = "d1ba2743-c47a-4446-aefc-d4594dbc045d";
        let name = MessagingName::of(&id(session));
        assert_eq!(name, MessagingName::of(&id(session)), "started again");
        let words: Vec<&str> = name.0.split('-').collect();
        assert_eq!(words.len(), 3, "{name}");
        assert_eq!(words.first(), Some(&"edualc"));
        assert!(
            words
                .iter()
                .all(|word| !word.is_empty()
                    && word.chars().all(|letter| letter.is_ascii_lowercase())),
            "{name}"
        );
        let names: std::collections::HashSet<MessagingName> = (0..200)
            .map(|_| MessagingName::of(&SessionId::new_random()))
            .collect();
        assert!(names.len() > 190, "only {} names of 200", names.len());
    }
}
