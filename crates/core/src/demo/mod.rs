//! The demo: Claude Code played by a script (`scenarios`), and a demo project's sessions in
//! place of `~/.claude` (`world`), for a page anyone may open (`edualcd --demo`) and for the
//! browser demo of the Slint window: nothing runs and nothing on the disk is read. It is built
//! only where there is a demo (the `demo` feature), as it carries the project's transcripts.

pub mod scenarios;
pub mod script;
mod words;
pub mod world;
