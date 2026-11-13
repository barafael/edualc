//! The demo (`edualcd --demo`): Claude Code played by a script (`edualc_core::demo`), and a demo
//! project's sessions in place of `~/.claude`, for a page anyone may open: nothing runs and
//! nothing on the disk is read or written. It opens on a session that fixed the project's
//! elevation gain.

mod daemon;

pub use daemon::run;
