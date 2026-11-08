//! Formats other programs write, as types: Claude Code's stream-json and transcripts (`wire`),
//! the files it keeps (`claude_files`), the control messages edualc's messaging tool reads
//! (`mcp`), niri's IPC (`niri`), and the Agent Client Protocol agy speaks (`acp`). They grow
//! with each release, so their types take keys they don't know, and the
//! `serde-deny-unknown-fields` rule leaves this module out; the formats edualc owns refuse
//! unknown keys.

#[cfg(not(target_arch = "wasm32"))]
pub mod acp;
pub mod claude_files;
pub mod niri;
pub mod wire;
