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
/// other sessions read to find it and message it: written for an agy session (see
/// `agy::inbox`). Others are dropped.
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

/// A folder's `.claude/settings.json` or `settings.local.json`, in what it has Claude Code run
/// or set as it starts and works there.
#[derive(Debug, Default, Deserialize)]
pub(crate) struct ProjectSettings {
    /// Commands run on Claude Code's events, by event ("PreToolUse").
    #[serde(default)]
    pub(crate) hooks: HashMap<String, Vec<HookMatcher>>,
    /// Environment variables set for the session and what it runs; only the names are read.
    #[serde(default)]
    pub(crate) env: HashMap<String, serde::de::IgnoredAny>,
    /// A command whose output is used as the API key.
    #[serde(default, rename = "apiKeyHelper")]
    pub(crate) api_key_helper: Option<String>,
    #[serde(default, rename = "statusLine")]
    pub(crate) status_line: Option<StatusLine>,
    /// Plugins, which bring hooks and MCP servers of their own, by name and whether on.
    #[serde(default, rename = "enabledPlugins")]
    pub(crate) enabled_plugins: HashMap<String, bool>,
}

/// Hooks for an event, for the tools `matcher` names (all, if empty).
#[derive(Debug, Deserialize)]
pub(crate) struct HookMatcher {
    #[serde(default)]
    pub(crate) matcher: String,
    #[serde(default)]
    pub(crate) hooks: Vec<Hook>,
}

/// A hook: a command, or a prompt or an agent (which run no command of their own).
#[derive(Debug, Deserialize)]
pub(crate) struct Hook {
    #[serde(default)]
    pub(crate) command: Option<String>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct StatusLine {
    #[serde(default)]
    pub(crate) command: Option<String>,
}

/// A folder's `.mcp.json`: the MCP servers it has Claude Code start.
#[derive(Debug, Default, Deserialize)]
pub(crate) struct McpJson {
    #[serde(default, rename = "mcpServers")]
    pub(crate) servers: HashMap<String, McpServer>,
}

/// An MCP server: a command started (with its arguments), or one reached at a URL.
#[derive(Debug, Deserialize)]
pub(crate) struct McpServer {
    #[serde(default)]
    pub(crate) command: Option<String>,
    #[serde(default)]
    pub(crate) args: Vec<String>,
    #[serde(default)]
    pub(crate) url: Option<String>,
}

/// What Claude Code keeps beside a subagent's transcript (`subagents/agent-<id>.meta.json`):
/// `finished` is set once it ends.
#[derive(Debug, Deserialize)]
pub(crate) struct AgentRecord {
    #[serde(default)]
    pub(crate) finished: bool,
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
