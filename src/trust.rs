//! Whether a folder is trusted to run Claude Code in. Its hooks, `env` and other settings take
//! effect the moment `claude` starts there, and `claude -p` skips the trust dialog the terminal
//! shows, so this window asks instead. Trusted are the folders, with what is under them, that
//! the terminal's dialog accepted (as `~/.claude.json` keeps them) and those trusted here (kept
//! in this window's settings).

use std::path::{Path, PathBuf};

use crate::external::claude_files::ClaudeJson;
use crate::sessions;
use crate::settings::Settings;

pub(crate) fn trusted(folder: &Path, settings: &Settings) -> bool {
    let accepted = accepted_in_terminal();
    folder.ancestors().any(|place| {
        accepted.iter().any(|accepted| accepted == place)
            || settings
                .trusted_folders
                .iter()
                .any(|trusted| trusted == place)
    })
}

/// The folders whose trust dialog was accepted in the terminal.
fn accepted_in_terminal() -> Vec<PathBuf> {
    let path = sessions::global_config_path();
    let small = std::fs::metadata(&path).is_ok_and(|metadata| {
        metadata.is_file() && metadata.len() <= sessions::GLOBAL_CONFIG_BYTES
    });
    let read = small
        .then(|| std::fs::read_to_string(&path).ok())
        .flatten()
        .map(|text| serde_json::from_str::<ClaudeJson>(&text));
    match read {
        Some(Ok(claude)) => claude
            .projects
            .into_iter()
            .filter(|(_, project)| project.accepted)
            .map(|(folder, _)| folder)
            .collect(),
        Some(Err(error)) => {
            tracing::warn!(%error, "~/.claude.json doesn't parse; no folder counts as trusted");
            Vec::new()
        }
        None => Vec::new(),
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
        assert!(trusted(
            Path::new("/nowhere/edualc-trust-test/sub"),
            &settings
        ));
        assert!(!trusted(Path::new("/nowhere/edualc-other"), &settings));
    }
}
