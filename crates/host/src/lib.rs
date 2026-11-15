//! edualc's daemon (`edualcd`): runs Claude Code's processes (and ACP agents') for the windows,
//! reads Claude Code's files for them, and does what the desktop is asked. Its parts are actors
//! with sans-io cores (see [`daemon`]).

// The window's modules, under the names this crate's modules know them by.
pub use edualc_core::*;

pub mod acp;
pub mod daemon;
pub mod files;
pub mod keyring;
pub mod spawn;
