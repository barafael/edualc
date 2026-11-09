//! edualc's native window for Claude Code, drawn with Slint: a frontend of `edualc-core`'s app
//! and presenter, its line to its edualcd (on this machine or another) a WebSocket over TLS.
//! The window confines itself before it shows (see `confine`).

// The app's modules, under the names this crate's modules know them by.
use edualc_core::*;

mod appearance;
mod buses;
mod command_line;
mod confine;
mod seat;
mod start;

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

fn main() -> Result<std::process::ExitCode, slint::PlatformError> {
    start::main()
}
