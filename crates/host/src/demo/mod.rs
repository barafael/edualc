//! The demo (`edualcd --demo`): Claude Code played by a script (`daemon`, `scenarios`), and a
//! demo project's sessions in place of `~/.claude` (`demo`), for a page anyone may open: nothing
//! runs and nothing on the disk is read. It opens on a session that fixed the project's
//! elevation gain.

mod daemon;
mod scenarios;
pub mod world;

pub use daemon::run;
