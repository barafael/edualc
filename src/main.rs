//! edualc, a desktop window for Claude Code. The `claude` CLI does the work (the model, tools,
//! permissions, sessions, MCP servers) and is spoken to in stream-json, as the Agent SDKs
//! speak to it; this window shows what it does and asks what it asks.
//!
//! The same window also runs in a browser, as a demo (`web`): there the daemon is a script in
//! the page, and what only Linux has (processes, sockets, the sandbox) is left out.

mod app;
mod appearance;
#[cfg(not(target_arch = "wasm32"))]
mod buses;
mod chat;
mod cjk;
mod claude;
#[cfg(not(target_arch = "wasm32"))]
mod confine;
#[cfg(not(target_arch = "wasm32"))]
mod daemon;
mod decode;
mod desktop;
mod external;
#[cfg(not(target_arch = "wasm32"))]
mod files;
mod folder;
mod grouping;
#[cfg(not(target_arch = "wasm32"))]
mod home;
mod ipc;
#[cfg_attr(target_arch = "wasm32", path = "web/link.rs")]
mod link;
mod markdown;
#[cfg(not(target_arch = "wasm32"))]
mod mirror;
mod plan;
mod present;
mod process;
#[cfg(not(target_arch = "wasm32"))]
mod relay;
mod repeat;
mod sandbox;
mod sessions;
mod settings;
mod sidebar;
#[cfg(not(target_arch = "wasm32"))]
mod spawn;
#[cfg(not(target_arch = "wasm32"))]
mod start;
mod tools;
mod trust;
#[cfg(target_arch = "wasm32")]
mod web;

/// The UI Slint compiles from `ui/app.slint` (see `build.rs`).
// Slint's code, not ours: it shares state through `Rc` and cells, and discards what it likes.
#[allow(
    clippy::disallowed_types,
    clippy::let_underscore_must_use,
    clippy::let_underscore_untyped,
    clippy::wildcard_enum_match_arm,
    clippy::indexing_slicing,
    clippy::match_same_arms,
    clippy::redundant_clone,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_lossless,
    clippy::needless_pass_by_value,
    clippy::default_trait_access,
    clippy::redundant_closure_for_method_calls,
    clippy::items_after_statements,
    clippy::unneeded_field_pattern,
    clippy::ignored_unit_patterns,
    clippy::map_unwrap_or,
    clippy::derive_partial_eq_without_eq,
    clippy::bool_to_int_with_if
)]
mod generated {
    include!(concat!(env!("OUT_DIR"), "/window.rs"));
}
mod ui;

#[cfg(not(target_arch = "wasm32"))]
fn main() -> Result<std::process::ExitCode, slint::PlatformError> {
    start::main()
}

/// The demo, as the page loads it.
#[cfg(target_arch = "wasm32")]
fn main() {
    web::main();
}
