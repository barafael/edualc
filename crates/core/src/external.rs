//! Formats other programs write, as types: Claude Code's stream-json and transcripts (`wire`),
//! the files it keeps (`claude_files`), and the control messages edualc's messaging tool reads
//! (`mcp`). They grow with each release, so their types take keys they don't know, and the
//! `serde-deny-unknown-fields` rule leaves this module out; the formats edualc owns refuse
//! unknown keys.

pub mod claude_files;
pub mod mcp;
pub mod wire;
