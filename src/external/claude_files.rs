//! The files Claude Code keeps that edualc reads, in the fields read: `~/.claude.json` (the
//! folders whose trust dialog was accepted), its user settings (whether its own sandbox is on),
//! the records of running sessions (`~/.claude/sessions/<pid>.json`), and the sign-in
//! (`~/.claude/.credentials.json`).

use std::collections::HashMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::external::wire::SessionId;
use crate::sessions::{LiveKind, LiveStatus};

/// A running session's record, `sessions/<pid>.json` in Claude Code's home, in the fields
/// other sessions read to find it and message it: mirrored from a sandboxed session's own
/// home into the user's (see `mirror`). Others are dropped.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SessionRecord {
    pub(crate) pid: u32,
    pub(crate) session_id: SessionId,
    pub(crate) cwd: PathBuf,
    pub(crate) proc_start: String,
    pub(crate) messaging_socket_path: PathBuf,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) started_at: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) peer_protocol: Option<u32>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) peer_features: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) kind: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) entrypoint: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) pid_domain: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) name_source: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) name_since: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) status: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) updated_at: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) status_updated_at: Option<u64>,
}

/// The key beside a session's record, `sessions/<pid>.<hash>.key`, that other sessions
/// authenticate to its socket with. Not `Debug`: it holds the token.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SessionKey {
    pub(crate) peer_token: String,
    pub(crate) proc_start: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) pid_domain: Option<String>,
}

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
