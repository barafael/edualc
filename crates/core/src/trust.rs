//! Whether a folder is trusted to run Claude Code in. Its hooks, `env` and other settings take
//! effect the moment `claude` starts there, and `claude -p` skips the trust dialog the terminal
//! shows, so this window asks instead. Trusted are the folders, with what is under them, trusted
//! here (kept in this window's settings) and those that the terminal's dialog accepted (as
//! `~/.claude.json` keeps them). The daemon reads the latter for the window, which can't: that
//! file holds MCP servers' environments, tokens among them, and Claude Code replaces it as it
//! writes, so a Landlock rule for it would stop at the first rewrite anyway.

#[cfg(not(target_arch = "wasm32"))]
use std::io::Read;
#[cfg(not(target_arch = "wasm32"))]
use std::os::fd::{AsFd, OwnedFd};
use std::path::{Path, PathBuf};

#[cfg(not(target_arch = "wasm32"))]
use rustix::fs::{Mode, OFlags};

use crate::external::claude_files::{ClaudeJson, McpJson, ProjectSettings};
use crate::ipc::Runs;
use crate::sessions;
use crate::settings::Settings;

/// The most of a folder's settings file read: one larger isn't one.
const SETTINGS_BYTES: u64 = 1024 * 1024;
/// The most of what a folder runs that is listed, and of each text in it, in characters.
const MOST_RUNS: usize = 40;
const MOST_CHARS: usize = 300;

/// Whether `folder`, or one it is in, was trusted in this window.
pub fn trusted_in_edualc(folder: &Path, settings: &Settings) -> bool {
    within(folder, &settings.trusted_folders)
}

/// Whether the terminal's trust dialog was accepted for `folder` or one it is in, as
/// `~/.claude.json` says now.
pub fn accepted_in_terminal(folder: &Path) -> bool {
    within(folder, &accepted_folders())
}

fn within(folder: &Path, trusted: &[PathBuf]) -> bool {
    folder
        .ancestors()
        .any(|place| trusted.iter().any(|trusted| trusted == place))
}

/// The folders whose trust dialog was accepted in the terminal; none where Claude Code never
/// ran there.
fn accepted_folders() -> Vec<PathBuf> {
    let path = sessions::global_config_path();
    let small = std::fs::metadata(&path).is_ok_and(|metadata| {
        metadata.is_file() && metadata.len() <= sessions::GLOBAL_CONFIG_BYTES
    });
    if !small {
        return Vec::new();
    }
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(error) => {
            tracing::warn!(%error, "couldn't read ~/.claude.json; no folder counts as trusted");
            return Vec::new();
        }
    };
    match serde_json::from_str::<ClaudeJson>(&text) {
        Ok(claude) => claude
            .projects
            .into_iter()
            .filter(|(_, project)| project.accepted)
            .map(|(folder, _)| folder)
            .collect(),
        Err(error) => {
            tracing::warn!(%error, "~/.claude.json doesn't parse; no folder counts as trusted");
            Vec::new()
        }
    }
}

/// What `folder`'s own configuration would have Claude Code run or set there: hooks, helpers,
/// plugins, environment variables (by name) from `.claude/settings.json` and
/// `settings.local.json`, and MCP servers from `.mcp.json`. The folder isn't trusted yet: no link
/// in it is followed (a repository could link to the user's own configuration), a file too
/// large or not plain is said to be unreadable rather than left out, and every text is cut
/// short and kept free of control characters.
#[cfg(not(target_arch = "wasm32"))]
pub fn what_runs(folder: &Path) -> Vec<Runs> {
    let mut runs = Vec::new();
    let opened = rustix::fs::open(
        folder,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
        Mode::empty(),
    );
    let folder = match opened {
        Ok(folder) => folder,
        Err(error) => {
            runs.push(unreadable("the folder", &error.to_string()));
            return runs;
        }
    };
    match open_in(&folder, ".claude", OFlags::DIRECTORY) {
        Ok(Some(settings_folder)) => {
            for name in ["settings.json", "settings.local.json"] {
                let file = format!(".claude/{name}");
                match read_in(&settings_folder, name) {
                    Ok(Some(text)) => match serde_json::from_str::<ProjectSettings>(&text) {
                        Ok(settings) => runs.extend(settings_runs(settings)),
                        Err(error) => {
                            runs.push(unreadable(&file, &format!("doesn't parse: {error}")));
                        }
                    },
                    Ok(None) => {}
                    Err(reason) => runs.push(unreadable(&file, &reason)),
                }
            }
        }
        Ok(None) => {}
        Err(reason) => runs.push(unreadable(".claude", &reason)),
    }
    match read_in(&folder, ".mcp.json") {
        Ok(Some(text)) => match serde_json::from_str::<McpJson>(&text) {
            Ok(mcp) => runs.extend(mcp_runs(mcp)),
            Err(error) => runs.push(unreadable(".mcp.json", &format!("doesn't parse: {error}"))),
        },
        Ok(None) => {}
        Err(reason) => runs.push(unreadable(".mcp.json", &reason)),
    }
    runs
}

/// What a settings file runs or sets, in a steady order.
fn settings_runs(settings: ProjectSettings) -> Vec<Runs> {
    let mut runs = Vec::new();
    let mut events: Vec<_> = settings.hooks.into_iter().collect();
    events.sort_by(|left, right| left.0.cmp(&right.0));
    for (event, matchers) in events {
        for matcher in matchers {
            for command in matcher.hooks.into_iter().filter_map(|hook| hook.command) {
                runs.push(Runs::Hook {
                    event: shown(&event),
                    matcher: shown(&matcher.matcher),
                    command: shown(&command),
                });
            }
        }
    }
    if let Some(command) = settings.api_key_helper {
        runs.push(Runs::ApiKeyHelper(shown(&command)));
    }
    if let Some(command) = settings.status_line.and_then(|line| line.command) {
        runs.push(Runs::StatusLine(shown(&command)));
    }
    let mut plugins: Vec<String> = settings
        .enabled_plugins
        .into_iter()
        .filter(|(_, on)| *on)
        .map(|(name, _)| shown(&name))
        .collect();
    plugins.sort();
    runs.extend(plugins.into_iter().map(Runs::Plugin));
    let mut names: Vec<String> = settings.env.into_keys().map(|name| shown(&name)).collect();
    if !names.is_empty() {
        names.sort();
        names.truncate(MOST_RUNS);
        runs.push(Runs::Environment(names));
    }
    runs.truncate(MOST_RUNS);
    runs
}

/// The MCP servers `.mcp.json` starts or reaches, by name.
fn mcp_runs(mcp: McpJson) -> Vec<Runs> {
    let mut servers: Vec<_> = mcp.servers.into_iter().collect();
    servers.sort_by(|left, right| left.0.cmp(&right.0));
    servers
        .into_iter()
        .take(MOST_RUNS)
        .map(|(name, server)| {
            let runs = match (server.command, server.url) {
                (Some(command), _) => std::iter::once(command)
                    .chain(server.args)
                    .collect::<Vec<_>>()
                    .join(" "),
                (None, Some(url)) => url,
                (None, None) => String::new(),
            };
            Runs::McpServer {
                name: shown(&name),
                runs: shown(&runs),
            }
        })
        .collect()
}

fn unreadable(file: &str, reason: &str) -> Runs {
    Runs::Unreadable {
        file: file.to_owned(),
        reason: shown(reason),
    }
}

/// A text from the folder as it is shown: control characters as spaces, cut at [`MOST_CHARS`].
fn shown(text: &str) -> String {
    let mut shown: String = text
        .chars()
        .map(|character| {
            if character.is_control() {
                ' '
            } else {
                character
            }
        })
        .take(MOST_CHARS)
        .collect();
    if text.chars().nth(MOST_CHARS).is_some() {
        shown.push('…');
    }
    shown
}

/// `name` in the folder `at`, not followed if it is a link, nor waited on if it is a pipe;
/// `None` if there is none, or why it couldn't be opened.
#[cfg(not(target_arch = "wasm32"))]
fn open_in(at: impl AsFd, name: &str, kind: OFlags) -> Result<Option<OwnedFd>, String> {
    let flags = OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC | kind;
    match rustix::fs::openat(at, name, flags, Mode::empty()) {
        Ok(fd) => Ok(Some(fd)),
        Err(rustix::io::Errno::NOENT) => Ok(None),
        Err(rustix::io::Errno::LOOP) => Err("it is a link, which isn't followed".to_owned()),
        Err(error) => Err(error.to_string()),
    }
}

/// The plain file `name` in the folder `at`, read whole; `None` if there is none, or why it
/// couldn't be read (a link, not a plain file, too large, not text).
#[cfg(not(target_arch = "wasm32"))]
fn read_in(at: impl AsFd, name: &str) -> Result<Option<String>, String> {
    let Some(fd) = open_in(at, name, OFlags::empty())? else {
        return Ok(None);
    };
    let stat = rustix::fs::fstat(&fd).map_err(|error| error.to_string())?;
    if rustix::fs::FileType::from_raw_mode(stat.st_mode) != rustix::fs::FileType::RegularFile {
        return Err("it isn't a plain file".to_owned());
    }
    let mut bytes = Vec::new();
    std::fs::File::from(fd)
        .take(SETTINGS_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| error.to_string())?;
    if bytes.len() as u64 > SETTINGS_BYTES {
        return Err("it is too large to be settings".to_owned());
    }
    String::from_utf8(bytes)
        .map(Some)
        .map_err(|error| format!("it isn't UTF-8 text ({error})"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trust_covers_what_is_under_a_trusted_folder() {
        let settings = Settings {
            trusted_folders: vec![PathBuf::from("/nowhere/edualc-trust-test")],
            ..Settings::default()
        };
        assert!(trusted_in_edualc(
            Path::new("/nowhere/edualc-trust-test/sub"),
            &settings
        ));
        assert!(!trusted_in_edualc(
            Path::new("/nowhere/edualc-other"),
            &settings
        ));
    }

    /// A scratch folder of the test's own, made empty.
    fn scratch(name: &str) -> PathBuf {
        let folder = std::env::temp_dir().join(format!("edualc-{name}-{}", std::process::id()));
        if folder.exists() {
            std::fs::remove_dir_all(&folder).expect("an old scratch folder goes");
        }
        std::fs::create_dir_all(folder.join(".claude")).expect("a scratch folder");
        folder
    }

    /// Hooks, helpers, plugins turned on, environment variables by name and MCP servers with
    /// their command lines are listed, in a steady order; values of variables are not.
    #[test]
    fn a_folder_lists_what_it_would_run() {
        let folder = scratch("runs");
        let settings = serde_json::json!({
            "hooks": {
                "SessionStart": [{ "hooks": [{ "type": "command", "command": "./setup.sh" }] }],
                "PreToolUse": [{
                    "matcher": "Bash",
                    "hooks": [{ "type": "command", "command": "check\u{1b}[2J --strict" }],
                }],
            },
            "env": { "TOKEN": "secret-value", "API": "x" },
            "apiKeyHelper": "./key.sh",
            "enabledPlugins": { "on@market": true, "off@market": false },
        });
        std::fs::write(folder.join(".claude/settings.json"), settings.to_string())
            .expect("settings");
        let mcp = serde_json::json!({
            "mcpServers": {
                "docs": { "type": "http", "url": "https://example.org/mcp" },
                "files": { "command": "npx", "args": ["-y", "server-files", "/"] },
            },
        });
        std::fs::write(folder.join(".mcp.json"), mcp.to_string()).expect("an .mcp.json");
        assert_eq!(
            what_runs(&folder),
            vec![
                Runs::Hook {
                    event: "PreToolUse".to_owned(),
                    matcher: "Bash".to_owned(),
                    command: "check [2J --strict".to_owned(),
                },
                Runs::Hook {
                    event: "SessionStart".to_owned(),
                    matcher: String::new(),
                    command: "./setup.sh".to_owned(),
                },
                Runs::ApiKeyHelper("./key.sh".to_owned()),
                Runs::Plugin("on@market".to_owned()),
                Runs::Environment(vec!["API".to_owned(), "TOKEN".to_owned()]),
                Runs::McpServer {
                    name: "docs".to_owned(),
                    runs: "https://example.org/mcp".to_owned(),
                },
                Runs::McpServer {
                    name: "files".to_owned(),
                    runs: "npx -y server-files /".to_owned(),
                },
            ]
        );
        std::fs::remove_dir_all(&folder).expect("the scratch folder goes");
    }

    /// A folder whose files can't be read says so, instead of seeming to run nothing: a link
    /// isn't followed (it could lead to the user's own configuration), and a file that doesn't
    /// parse is named.
    #[test]
    fn what_cant_be_read_is_said() {
        let folder = scratch("unreadable");
        std::fs::write(folder.join("elsewhere.json"), r#"{ "mcpServers": {} }"#)
            .expect("a file to link to");
        std::os::unix::fs::symlink(folder.join("elsewhere.json"), folder.join(".mcp.json"))
            .expect("a link");
        std::fs::write(folder.join(".claude/settings.local.json"), "{ not json")
            .expect("settings that don't parse");
        let runs = what_runs(&folder);
        assert!(
            matches!(
                runs.as_slice(),
                [
                    Runs::Unreadable { file: local, .. },
                    Runs::Unreadable { file: mcp, reason },
                ] if local == ".claude/settings.local.json"
                    && mcp == ".mcp.json"
                    && reason.contains("link")
            ),
            "{runs:?}"
        );
        assert_eq!(what_runs(&folder.join("missing")).len(), 1);
        std::fs::remove_dir_all(&folder).expect("the scratch folder goes");
    }
}
