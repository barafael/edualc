//! The files Claude Code keeps that edualc reads, in the fields read: `~/.claude.json` (the
//! folders whose trust dialog was accepted), its user settings (whether its own sandbox is on),
//! the records of running sessions (`~/.claude/sessions/<pid>.json`), and the sign-in
//! (`~/.claude/.credentials.json`).

use std::collections::HashMap;
use std::path::PathBuf;

use serde::Deserialize;

use crate::external::wire::SessionId;
use crate::sessions::{LiveKind, LiveStatus};

/// `~/.claude/.credentials.json`.
#[derive(Deserialize)]
pub(crate) struct Credentials {
    #[serde(rename = "claudeAiOauth")]
    pub(crate) sign_in: Option<SignIn>,
}

/// The claude.ai sign-in in it. Not `Debug`: it holds the token.
#[derive(Deserialize)]
pub(crate) struct SignIn {
    #[serde(rename = "accessToken")]
    pub(crate) access_token: String,
    /// Milliseconds since the epoch.
    #[serde(rename = "expiresAt")]
    pub(crate) expires_at_ms: Option<i64>,
}

/// `~/.claude.json`.
#[derive(Debug, Default, Deserialize)]
pub(crate) struct ClaudeJson {
    #[serde(default)]
    pub(crate) projects: HashMap<PathBuf, Project>,
}

/// A folder in `~/.claude.json`.
#[derive(Debug, Deserialize)]
pub(crate) struct Project {
    #[serde(default, rename = "hasTrustDialogAccepted")]
    pub(crate) accepted: bool,
}

/// `settings.json` or `settings.local.json` in Claude Code's configuration folder.
#[derive(Debug, Default, Deserialize)]
pub(crate) struct UserSettings {
    #[serde(default)]
    pub(crate) sandbox: Option<OwnSandbox>,
}

/// Claude Code's own sandbox, as its settings set it.
#[derive(Debug, Default, Deserialize)]
pub(crate) struct OwnSandbox {
    #[serde(default)]
    pub(crate) enabled: bool,
}

/// A running session's record.
#[derive(Debug, Deserialize)]
pub(crate) struct LiveRecord {
    pub(crate) pid: u32,
    #[serde(rename = "sessionId")]
    pub(crate) session_id: SessionId,
    pub(crate) cwd: PathBuf,
    #[serde(default)]
    pub(crate) name: String,
    #[serde(default)]
    pub(crate) kind: LiveKind,
    #[serde(default)]
    pub(crate) status: LiveStatus,
    /// The process's start time, as `/proc/<pid>/stat` has it: tells a reused pid apart.
    #[serde(rename = "procStart")]
    pub(crate) proc_start: Option<String>,
}
