//! Formats other programs write, as types: Claude Code's stream-json and transcripts (`wire`),
//! the inputs of its tools the window knows (`tool_input`), the files it keeps
//! (`claude_files`), niri's IPC (`niri`), the Agent Client Protocol agy and opencode speak
//! (`acp`), what agy keeps of its conversations (`agy`), and what agents say to edualc's
//! messaging tool over MCP (`mcp`). They grow with each
//! release, so their types take keys they don't know, and the `serde-deny-unknown-fields` rule
//! leaves this module out; the formats edualc owns refuse unknown keys.

#[cfg(not(target_arch = "wasm32"))]
pub mod acp;
#[cfg(not(target_arch = "wasm32"))]
pub mod agy;
pub mod claude_files;
#[cfg(not(target_arch = "wasm32"))]
pub mod mcp;
pub mod niri;
pub mod tool_input;
pub mod wire;
