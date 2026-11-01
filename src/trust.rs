//! Whether a folder is trusted to run Claude Code in. Its hooks, `env` and other settings take
//! effect the moment `claude` starts there, and `claude -p` skips the trust dialog the terminal
//! shows, so this window asks instead. Trusted are the folders, with what is under them, trusted
//! here (kept in this window's settings) and those that the terminal's dialog accepted (as
//! `~/.claude.json` keeps them). The daemon reads the latter for the window, which can't: that
//! file holds MCP servers' environments, tokens among them, and Claude Code replaces it as it
//! writes, so a Landlock rule for it would stop at the first rewrite anyway.

use std::path::{Path, PathBuf};

use crate::external::claude_files::ClaudeJson;
use crate::sessions;
use crate::settings::Settings;

/// Whether `folder`, or one it is in, was trusted in this window.
pub(crate) fn trusted_in_edualc(folder: &Path, settings: &Settings) -> bool {
    within(folder, &settings.trusted_folders)
}

/// Whether the terminal's trust dialog was accepted for `folder` or one it is in, as
/// `~/.claude.json` says now.
pub(crate) fn accepted_in_terminal(folder: &Path) -> bool {
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
}
