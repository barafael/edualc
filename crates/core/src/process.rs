//! What `/proc` says about a process: who started it, when, and what it runs.

use std::path::Path;

/// From `/proc/<pid>/stat`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Stat {
    pub parent: u32,
    /// Ticks after boot: with the pid, it names one process, as pids are reused.
    pub started: String,
}

pub fn stat(pid: u32) -> Option<Stat> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    // The command (field 2) may hold spaces and ends in `)`; field 4 is the parent, field 22
    // the start time.
    let (_, fields) = stat.rsplit_once(')')?;
    let mut fields = fields.split_whitespace();
    let parent = fields.nth(1)?.parse().ok()?;
    let started = fields.nth(17)?.to_owned();
    Some(Stat { parent, started })
}

/// Whether process `pid` runs, and is the one that started at `started`.
pub fn is_running(pid: u32, started: &str) -> bool {
    stat(pid).is_some_and(|stat| stat.started == started)
}

/// Whether process `pid` runs Claude Code: the native `claude`, or Node running the npm
/// package.
pub fn runs_claude(pid: u32) -> bool {
    let named = std::fs::read_to_string(format!("/proc/{pid}/comm"))
        .is_ok_and(|command| command.trim_end() == "claude");
    named
        || std::fs::read(format!("/proc/{pid}/cmdline")).is_ok_and(|cmdline| {
            cmdline
                .split(|&byte| byte == 0)
                .take(2)
                .map(|argument| Path::new(std::str::from_utf8(argument).unwrap_or("")))
                .any(|argument| {
                    argument
                        .file_name()
                        .is_some_and(|name| name.to_string_lossy().starts_with("claude"))
                        || argument.components().any(|part| {
                            part.as_os_str() == "claude-code" || part.as_os_str() == "claude"
                        })
                })
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn this_process_reads_back() {
        let own = stat(std::process::id()).expect("our own stat");
        assert!(is_running(std::process::id(), &own.started));
        assert!(!is_running(std::process::id(), "0"));
        assert!(!runs_claude(std::process::id()));
    }
}
