//! edualc's window, apart from how it is drawn: the conversation with Claude Code (spoken to in
//! stream-json, as the Agent SDKs speak to it), the one owner of its state (`app`), what of it a
//! window shows (`present`), and the line to the daemon that runs Claude Code and reads its
//! files (`ipc`, `link`). The Slint window and the Dioxus page are frontends of it; the daemon
//! (`edualc-host`) shares its types and what only Linux has (`#[cfg(not(target_arch =
//! "wasm32"))]`), which a page leaves out.

pub mod app;
pub mod chat;
pub mod cjk;
pub mod claude;
pub mod decode;
pub mod desktop;
pub mod external;
pub mod folder;
pub mod grouping;
#[cfg(not(target_arch = "wasm32"))]
pub mod home;
pub mod ipc;
#[cfg(not(target_arch = "wasm32"))]
pub mod line;
pub mod link;
pub mod markdown;
pub mod plan;
pub mod present;
pub mod process;
pub mod repeat;
pub mod sandbox;
pub mod sessions;
pub mod settings;
pub mod sidebar;
pub mod tools;
pub mod trust;
