//! edualc's native window for Claude Code, drawn with Slint: a frontend of `edualc-core`'s app
//! and presenter, its line to the daemon (`edualcd`) over the daemon's Unix socket.
//!
//! The same window also runs in a browser, as a demo (`web`): there the daemon is a script in
//! the page, and what only Linux has (processes, sockets) is left out.

// The app's modules, under the names this crate's modules know them by.
use edualc_core::*;

mod appearance;
#[cfg_attr(target_arch = "wasm32", path = "web/colouring.rs")]
mod colouring;
#[cfg(not(target_arch = "wasm32"))]
mod connect;
mod highlight;
#[cfg(not(target_arch = "wasm32"))]
mod start;
#[cfg(target_arch = "wasm32")]
mod web;
#[cfg(target_arch = "wasm32")]
mod words;

/// The UI Slint compiles from `ui/app.slint` (see `build.rs`).
// Slint's code, not ours: it indexes and discards results as it likes. Only what it trips is let
// off, and only here.
#[allow(
    clippy::indexing_slicing,
    clippy::let_underscore_must_use,
    clippy::let_underscore_untyped,
    reason = "Slint's generated code"
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
