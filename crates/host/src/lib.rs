//! edualc's daemon (`edualcd`): runs Claude Code's processes (and ACP agents') for the windows,
//! reads Claude Code's files for them, and does what the desktop is asked. Its parts are actors
//! with sans-io cores (see [`daemon`]).

// The window's modules, under the names this crate's modules know them by.
use edualc_core::*;

mod acp;
mod daemon;
mod files;
mod keyring;
mod snapshots;
mod spawn;

// What `edualcd` starts as: the daemon, or an ACP session's adapter or its messaging tool. The
// image decoder's helper is the window's crate's (`edualc_core::decode`).
pub use acp::{ADAPTER_FLAG, TOOL_FLAG, run_adapter, run_tool};
pub use daemon::run;
