//! Starting `claude` for the daemon: its command line, its environment, its sandbox. What the
//! window asked for is checked here, as a request from another process: the command line is
//! made from typed values alone.

use std::path::{Path, PathBuf};
use std::process::Stdio;

use tokio::process::{Child, Command};

use crate::claude::{Launch, Resume};
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
    match &launch.resume {
        // One argument: the CLI's `--resume` takes its value only if it doesn't look like an
        // option. The id is a UUID, checked as it was read.
        Some(Resume::Continue(id)) => {
            command.arg(format!("--resume={id}"));
        }
        Some(Resume::Fork(id)) => {
            command.args([format!("--resume={id}"), "--fork-session".to_owned()]);
        }
        None => {}
    }
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
}
