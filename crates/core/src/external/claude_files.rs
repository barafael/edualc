//! The files Claude Code keeps that edualc reads, in the fields read: `~/.claude.json` (the
//! folders whose trust dialog was accepted), its user settings (whether its own sandbox is on), a
//! folder's own settings and `.mcp.json` (what they would run, asked about before trusting it),
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
pub struct SessionRecord {
    pub pid: u32,
    pub session_id: SessionId,
    pub cwd: PathBuf,
    pub proc_start: String,
    pub messaging_socket_path: PathBuf,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_at: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub peer_protocol: Option<u32>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub peer_features: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub entrypoint: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pid_domain: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name_source: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name_since: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub updated_at: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status_updated_at: Option<u64>,
}

/// The key beside a session's record, `sessions/<pid>.<hash>.key`, that other sessions
/// authenticate to its socket with. Not `Debug`: it holds the token.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionKey {
    pub peer_token: String,
    pub proc_start: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pid_domain: Option<String>,
}

/// `~/.claude/.credentials.json`.
#[derive(Deserialize)]
pub struct Credentials {
    #[serde(rename = "claudeAiOauth")]
    pub sign_in: Option<SignIn>,
}

/// The claude.ai sign-in in it. Not `Debug`: it holds the token.
#[derive(Deserialize)]
pub struct SignIn {
    #[serde(rename = "accessToken")]
    pub access_token: String,
    /// Milliseconds since the epoch.
    #[serde(rename = "expiresAt")]
    pub expires_at_ms: Option<i64>,
}

/// `~/.claude.json`.
#[derive(Debug, Default, Deserialize)]
pub struct ClaudeJson {
    #[serde(default)]
    pub projects: HashMap<PathBuf, Project>,
}

/// A folder in `~/.claude.json`.
#[derive(Debug, Deserialize)]
pub struct Project {
    #[serde(default, rename = "hasTrustDialogAccepted")]
    pub accepted: bool,
}

/// `settings.json` or `settings.local.json` in Claude Code's configuration folder.
#[derive(Debug, Default, Deserialize)]
pub struct UserSettings {
    #[serde(default)]
    pub sandbox: Option<OwnSandbox>,
}

/// Claude Code's own sandbox, as its settings set it.
#[derive(Debug, Default, Deserialize)]
pub struct OwnSandbox {
    #[serde(default)]
    pub enabled: bool,
}

/// A folder's `.claude/settings.json` or `settings.local.json`, in what it has Claude Code run
/// or set as it starts and works there.
#[derive(Debug, Default, Deserialize)]
pub struct ProjectSettings {
    /// Commands run on Claude Code's events, by event ("PreToolUse").
    #[serde(default)]
    pub hooks: HashMap<String, Vec<HookMatcher>>,
    /// Environment variables set for the session and what it runs; only the names are read.
    #[serde(default)]
    pub env: HashMap<String, serde::de::IgnoredAny>,
    /// A command whose output is used as the API key.
    #[serde(default, rename = "apiKeyHelper")]
    pub api_key_helper: Option<String>,
    #[serde(default, rename = "statusLine")]
    pub status_line: Option<StatusLine>,
    /// Plugins, which bring hooks and MCP servers of their own, by name and whether on.
    #[serde(default, rename = "enabledPlugins")]
    pub enabled_plugins: HashMap<String, bool>,
}

/// Hooks for an event, for the tools `matcher` names (all, if empty).
#[derive(Debug, Deserialize)]
pub struct HookMatcher {
    #[serde(default)]
    pub matcher: String,
    #[serde(default)]
    pub hooks: Vec<Hook>,
}

/// A hook: a command, or a prompt or an agent (which run no command of their own).
#[derive(Debug, Deserialize)]
pub struct Hook {
    #[serde(default)]
    pub command: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct StatusLine {
    #[serde(default)]
    pub command: Option<String>,
}

/// A folder's `.mcp.json`: the MCP servers it has Claude Code start.
#[derive(Debug, Default, Deserialize)]
pub struct McpJson {
    #[serde(default, rename = "mcpServers")]
    pub servers: HashMap<String, McpServer>,
}

/// An MCP server: a command started (with its arguments), or one reached at a URL.
#[derive(Debug, Deserialize)]
pub struct McpServer {
    #[serde(default)]
    pub command: Option<String>,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub url: Option<String>,
}

/// What Claude Code keeps beside a subagent's transcript (`subagents/agent-<id>.meta.json`):
/// `finished` is set once it ends.
#[derive(Debug, Deserialize)]
pub struct AgentRecord {
    #[serde(default)]
    pub finished: bool,
}

/// A running session's record.
#[derive(Debug, Deserialize)]
pub struct LiveRecord {
    pub pid: u32,
    #[serde(rename = "sessionId")]
    pub session_id: SessionId,
    pub cwd: PathBuf,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub kind: LiveKind,
    #[serde(default)]
    pub status: LiveStatus,
    /// The process's start time, as `/proc/<pid>/stat` has it: tells a reused pid apart.
    #[serde(rename = "procStart")]
    pub proc_start: Option<String>,
}
