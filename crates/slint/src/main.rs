//! edualc's native window for Claude Code, drawn with Slint: a frontend of `edualc-core`'s app
//! and presenter, its line to the daemon (`edualcd`) over the daemon's Unix socket.
//!
//! The same window also runs in a browser, as a demo (`web`): there the daemon is a script in
//! the page, and what only Linux has (processes, sockets) is left out.

// The app's modules, under the names this crate's modules know them by.
use edualc_core::*;

mod appearance;
#[cfg(not(target_arch = "wasm32"))]
mod connect;
#[cfg(not(target_arch = "wasm32"))]
mod start;
#[cfg(target_arch = "wasm32")]
mod web;
#[cfg(target_arch = "wasm32")]
mod words;

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
