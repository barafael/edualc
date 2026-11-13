//! Starting `claude` for the daemon: its command line and its environment. What the window
//! asked for is checked here, as a request from another process: the command line is made from
//! typed values alone.

use std::path::{Path, PathBuf};
use std::process::Stdio;

use tokio::process::{Child, Command};

use crate::claude::{Backend, Conversation, FolderConfiguration, Launch};
use crate::sessions::MessagingName;

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
    #[error(
        "couldn't start claude: {0}. Is Claude Code installed and `claude` on PATH? CLAUDE_BIN can name it."
    )]
    Start(#[source] std::io::Error),
    #[error("couldn't start edualc's adapter for agy: {0}")]
    Adapter(#[source] std::io::Error),
    #[error("agy sessions can't be copied yet, only continued")]
    AgyFork,
}

/// A started process, its pipes still in it. Dropped, it is killed.
#[derive(Debug)]
pub(crate) struct Spawned {
    pub(crate) child: Child,
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
    if launch.backend == Backend::Agy {
        return spawn_agy(launch, &cwd, runtime);
    }
    let mut command = Command::new(crate::claude::program());
    command.args([
        "-p",
        "--input-format",
        "stream-json",
        "--output-format",
        "stream-json",
        "--verbose",
        "--include-partial-messages",
        // Echoes what reaches the session's inbox from other sessions, which is shown; the
        // window's own messages come back too, and are left out.
        "--replay-user-messages",
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
    let name = MessagingName::of(launch.conversation.id(), &cwd);
    command.arg(format!("--name={name}"));
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
    command
        .current_dir(&cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    // The child's pipes are made with the daemon's runtime.
    let _in_runtime = runtime.enter();
    Ok(Spawned {
        child: command.spawn().map_err(Error::Start)?,
    })
}

/// An agy session: edualc's own adapter (see `agy`), which runs `agy acp` confined and speaks
/// stream-json to the daemon as `claude` does. The adapter itself isn't confined: it serves
/// the session's inbox, publishes its record and carries its messages, which the sandbox
/// keeps from agy.
fn spawn_agy(
    launch: &Launch,
    cwd: &Path,
    runtime: &tokio::runtime::Handle,
) -> Result<Spawned, Error> {
    let program = std::env::current_exe().map_err(Error::Adapter)?;
    let mut command = Command::new(program);
    command.arg(crate::agy::ADAPTER_FLAG);
    command.arg(format!("--cwd={}", cwd.display()));
    match &launch.conversation {
        Conversation::New(id) => command.arg(format!("--session-id={id}")),
        Conversation::Continue(id) => command.arg(format!("--resume={id}")),
        Conversation::Fork { .. } => return Err(Error::AgyFork),
    };
    if let Some(model) = &launch.model {
        command.arg(format!("--model={}", model_name(model)?));
    }
    for variable in INHERITED_SESSION {
        command.env_remove(variable);
    }
    command
        .current_dir(cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let _in_runtime = runtime.enter();
    Ok(Spawned {
        child: command.spawn().map_err(Error::Adapter)?,
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
