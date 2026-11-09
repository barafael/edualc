//! edualc's native window for Claude Code, drawn with GPUI: a frontend of `edualc-core`'s app
//! and presenter, as the Slint window is (`crates/slint`), whose start, desktop and confinement
//! it shares. Its line to its edualcd (on this machine or another) is a WebSocket over TLS. The
//! window confines itself before it shows (see `confine`).

// The app's modules, under the names this crate's modules know them by.
use edualc_core::*;

// The Slint window's, as they are: nothing in them is Slint's.
#[path = "../../slint/src/appearance.rs"]
mod appearance;
#[path = "../../slint/src/buses.rs"]
mod buses;
#[path = "../../slint/src/command_line.rs"]
mod command_line;
#[path = "../../slint/src/confine.rs"]
mod confine;
#[path = "../../slint/src/seat.rs"]
mod seat;

mod start;
mod ui;

fn main() -> std::process::ExitCode {
    start::main()
}
