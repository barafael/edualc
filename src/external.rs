//! Formats other programs write, as types: Claude Code's stream-json and transcripts (`wire`),
//! the files it keeps (`claude_files`), and niri's IPC (`niri`). They grow with each release, so
//! their types take keys they don't know, and the `serde-deny-unknown-fields` rule leaves this
//! module out; the formats edualc owns refuse unknown keys.

pub(crate) mod claude_files;
pub(crate) mod niri;
pub(crate) mod wire;
