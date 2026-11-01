//! What `/proc` says about a process: who started it, when, and what it runs.

use std::path::Path;

/// From `/proc/<pid>/stat`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Stat {
    pub(crate) parent: u32,
    /// Ticks after boot: with the pid, it names one process, as pids are reused.
    pub(crate) started: String,
}

pub(crate) fn stat(pid: u32) -> Option<Stat> {
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
pub(crate) fn is_running(pid: u32, started: &str) -> bool {
    stat(pid).is_some_and(|stat| stat.started == started)
}

/// Whether process `pid` is `ancestor` or was started, at any remove, by it.
pub(crate) fn descends_from(pid: u32, ancestor: u32) -> bool {
    let mut current = pid;
    // A chain of parents ends at 1; one far longer than any real one means a loop.
    for _ in 0..256 {
        if current == ancestor {
            return true;
        }
        match stat(current) {
            Some(stat) if stat.parent != 0 && stat.parent != current => current = stat.parent,
            Some(_) | None => return false,
        }
    }
    false
}

/// The processes of this user's that have the pipe `inode` open, at either end.
pub(crate) fn holding_pipe(inode: u64) -> Vec<u32> {
    let wanted = format!("pipe:[{inode}]");
    std::fs::read_dir("/proc")
        .into_iter()
        .flatten()
        .filter_map(Result::ok)
        .filter_map(|entry| entry.file_name().to_str()?.parse::<u32>().ok())
        .filter(|pid| {
            // Another user's processes, and those that keep theirs hidden, can't be read.
            std::fs::read_dir(format!("/proc/{pid}/fd"))
                .into_iter()
                .flatten()
                .filter_map(Result::ok)
                .any(|fd| {
                    std::fs::read_link(fd.path())
                        .is_ok_and(|target| target.as_os_str() == wanted.as_str())
                })
        })
        .collect()
}

/// Whether process `pid` runs Claude Code: the native `claude`, or Node running the npm
/// package.
pub(crate) fn runs_claude(pid: u32) -> bool {
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
