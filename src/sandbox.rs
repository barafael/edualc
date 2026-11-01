//! Claude Code in a Landlock sandbox. The `claude` process and everything it starts (its tools'
//! commands, hooks, MCP servers) see the system and the toolchains read-only, may write only the
//! project, its git directory, caches and temporary files, may reach no socket file but the
//! ssh agent's, may signal nothing outside, and gain no privileges (`sudo` and `pkexec` don't
//! work). Other Claude Code sessions may message it, but it messages none of them: one that
//! isn't sandboxed could do for it what the sandbox forbids. What Claude Code runs on its own
//! (settings and their hooks, skills, plugins) stays read-only, so nothing can be planted there
//! to run outside the sandbox later.
//!
//! Only the child is restricted: a thread restricts itself and starts it, and this window,
//! which opens editors, notifies and stops other sessions, stays free. Landlock can't be
//! widened once applied; every start gets a fresh ruleset, so a change takes a restart.
//!
//! Not covered: the network (Landlock tells ports apart, not hosts, so 443 would reach any host
//! anyway), and Claude Code's own credentials, which it must read to work.
//!
//! Claude Code has a sandbox of its own, for its shell commands: bubblewrap, which filters the
//! network by host but leaves its file tools, hooks and MCP servers out. The two don't stack:
//! bubblewrap mounts, and Landlock forbids mounting to what it restricts. Whoever turned Claude
//! Code's on in their own settings keeps it, and this one stays off.

use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};

use crate::external::claude_files::UserSettings;
use landlock::{
    ABI, Access, AccessFs, BitFlags, CompatLevel, Compatible, PathBeneath, PathFd, Ruleset,
    RulesetAttr, RulesetCreatedAttr, RulesetStatus, Scope,
};

/// The newest Landlock this window knows: 9 controls connecting to socket files.
const ABI_KNOWN: ABI = ABI::V9;

/// What `~/.claude` holds that Claude Code runs or obeys: kept read-only.
const CLAUDE_CODE_RUNS: &[&str] = &[
    "settings.json",
    "settings.local.json",
    "CLAUDE.md",
    "skills",
    "plugins",
    "agents",
    "commands",
    "hooks",
    "output-styles",
    "keybindings.json",
];

/// What a sandboxed Claude may reach.
#[derive(Debug, Clone, Default)]
pub(crate) struct Policy {
    /// Read and run, not change.
    read: Vec<PathBuf>,
    /// Anything a file can have done to it, but connect to a socket in it.
    write: Vec<PathBuf>,
    /// Devices it reads, writes and controls (terminals).
    devices: Vec<PathBuf>,
    /// Socket files it may connect to.
    sockets: Vec<PathBuf>,
    /// Folders it may make a socket file in, for others to connect to; it may neither connect
    /// to one there nor remove one, as it would to sweep away sessions it takes for ended.
    inboxes: Vec<PathBuf>,
}

/// How far the kernel enforced the sandbox.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) enum Enforcement {
    Full,
    /// An older kernel: some of it (socket files, say) isn't enforced.
    Partial,
    /// No Landlock in this kernel.
    None,
}

/// Whether the user turned on Claude Code's own sandbox, in their own settings. A project's
/// settings don't count: a repository could otherwise turn this sandbox off by asking for the
/// other, which may not even run here.
pub(crate) fn claude_codes_own_is_on() -> bool {
    let dir = crate::sessions::config_dir();
    ["settings.json", "settings.local.json"].iter().any(|name| {
        std::fs::read_to_string(dir.join(name))
            .ok()
            .and_then(|text| serde_json::from_str::<UserSettings>(&text).ok())
            .and_then(|settings| settings.sandbox)
            .is_some_and(|own| own.enabled)
    })
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum Error {
    #[error("couldn't set up the sandbox: {0}")]
    Ruleset(#[from] landlock::RulesetError),
}

/// What the sandbox lets through besides the project and the usual toolchains.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Allowances<'a> {
    /// The Wayland display and the GPU, for testing user interfaces. Clients of the display
    /// can read the clipboard and the screen; niri's own socket, which starts programs outside
    /// the sandbox, stays closed even so.
    pub(crate) desktop: bool,
    /// More to read, as the user's settings name it.
    pub(crate) read: &'a [PathBuf],
    /// More to write.
    pub(crate) write: &'a [PathBuf],
}

impl Policy {
    /// The policy for working in `project`.
    pub(crate) fn for_project(project: &Path, allowances: Allowances<'_>) -> Self {
        let home = dirs::home_dir().unwrap_or_default();
        let at_home = |relative: &str| home.join(relative);
        let mut policy = Self::default();
        policy.read.extend(
            [
                "/usr",
                "/opt",
                "/etc",
                "/bin",
                "/sbin",
                "/lib",
                "/lib64",
                "/proc",
                "/sys",
                // Where /etc/resolv.conf points with systemd-resolved.
                "/run/systemd/resolve",
            ]
            .map(PathBuf::from),
        );
        policy.read.extend(
            [
                // Toolchains, and how they are set up (not cargo's credentials beside them).
                ".cargo/bin",
                ".cargo/config.toml",
                ".cargo/config",
                ".cargo/env",
                ".rustup",
                ".local/bin",
                ".nvm",
                ".bun",
                ".pyenv",
                ".sdkman",
                ".local/share/pnpm",
                ".local/share/uv",
                ".local/share/mise",
                ".config/uv",
                ".config/mise",
                // `gh`'s login: Claude opening pull requests is common work.
                ".config/gh",
                ".local/share/claude",
                ".local/share/fonts",
                ".gitconfig",
                ".config/git",
                ".config/fontconfig",
                ".ssh/known_hosts",
                ".ssh/config",
                ".bashrc",
                ".bash_profile",
                ".profile",
                ".zshrc",
                ".zshenv",
                ".config/fish",
                ".claude",
            ]
            .map(at_home),
        );
        policy.read.extend(public_keys(&home.join(".ssh")));
        policy.read.extend(git_includes(&home));
        policy.write.extend([project.to_owned()]);
        policy.write.extend(git_dirs(project));
        policy
            .write
            .extend(["/tmp", "/var/tmp", "/dev/shm"].map(PathBuf::from));
        policy.write.extend(
            [
                ".cache",
                ".cargo/registry",
                ".cargo/git",
                ".npm",
                "go",
                ".m2",
                ".gradle",
                ".claude.json",
            ]
            .map(at_home),
        );
        policy.read.extend(allowances.read.iter().cloned());
        policy.write.extend(allowances.write.iter().cloned());
        policy.write.extend(claude_data(&home.join(".claude")));
        policy.devices.extend(
            [
                "/dev/null",
                "/dev/zero",
                "/dev/full",
                "/dev/random",
                "/dev/urandom",
                "/dev/tty",
                "/dev/ptmx",
                "/dev/pts",
            ]
            .map(PathBuf::from),
        );
        policy
            .sockets
            .extend(std::env::var_os("SSH_AUTH_SOCK").map(PathBuf::from));
        policy.inboxes.extend(
            std::env::var_os("XDG_RUNTIME_DIR")
                .and_then(|runtime| messaging_folder(&PathBuf::from(runtime))),
        );
        if allowances.desktop {
            if let (Some(runtime), Some(display)) = (
                std::env::var_os("XDG_RUNTIME_DIR"),
                std::env::var_os("WAYLAND_DISPLAY"),
            ) {
                policy.sockets.push(PathBuf::from(runtime).join(display));
            }
            policy.devices.push(PathBuf::from("/dev/dri"));
        }
        policy
    }

    /// Variables of this window's environment the child isn't given: ways out of the sandbox
    /// (the session bus starts programs; niri's socket does too) and displays it may not use.
    pub(crate) fn hidden_variables(&self) -> &'static [&'static str] {
        if self
            .devices
            .iter()
            .any(|device| device == Path::new("/dev/dri"))
        {
            &["DBUS_SESSION_BUS_ADDRESS", "NIRI_SOCKET", "DISPLAY"]
        } else {
            &[
                "DBUS_SESSION_BUS_ADDRESS",
                "NIRI_SOCKET",
                "DISPLAY",
                "WAYLAND_DISPLAY",
            ]
        }
    }

    /// Restricts the calling thread, and what it starts from now on, to this policy. Call it on
    /// a thread of its own that ends after starting the child.
    pub(crate) fn restrict_this_thread(&self) -> Result<Enforcement, Error> {
        let all = AccessFs::from_all(ABI_KNOWN);
        let read = AccessFs::from_read(ABI_KNOWN);
        let write = all & !AccessFs::ResolveUnix;
        let device = read | AccessFs::WriteFile | AccessFs::IoctlDev;
        let ruleset = Ruleset::default()
            .set_compatibility(CompatLevel::BestEffort)
            .handle_access(all)?
            .scope(Scope::AbstractUnixSocket | Scope::Signal)?
            .create()?
            .add_rules(rules(&self.read, read))?
            .add_rules(rules(&self.write, write))?
            .add_rules(rules(&self.devices, device))?
            .add_rules(rules(&self.sockets, AccessFs::ResolveUnix.into()))?
            .add_rules(rules(&self.inboxes, AccessFs::MakeSock.into()))?;
        let status = ruleset.restrict_self()?;
        Ok(match status.ruleset {
            RulesetStatus::FullyEnforced => Enforcement::Full,
            RulesetStatus::PartiallyEnforced => Enforcement::Partial,
            RulesetStatus::NotEnforced => Enforcement::None,
        })
    }
}

/// A rule for each of `paths` that exists; one that doesn't is no rule (and needs none). A file
/// gets only the rights a file can have: a folder's (making entries in it, say) on a file would
/// count as not enforced, and the whole sandbox as only partly.
fn rules(
    paths: &[PathBuf],
    access: BitFlags<AccessFs>,
) -> impl Iterator<Item = Result<PathBeneath<PathFd>, landlock::RulesetError>> + '_ {
    paths
        .iter()
        .filter_map(move |path| match PathFd::new(path) {
            Ok(fd) => {
                let access = if path.is_dir() {
                    access
                } else {
                    access & AccessFs::from_file(ABI_KNOWN)
                };
                Some(Ok(PathBeneath::new(fd, access)))
            }
            Err(error) => {
                tracing::debug!(%error, "nothing there to allow");
                None
            }
        })
}

/// The files the user's git configuration includes (`include.path`, `includeIf.….path`): git
/// stops at one it can't read. Each file alone, not the folder it is in.
fn git_includes(home: &Path) -> Vec<PathBuf> {
    let listed = match crate::desktop::run(
        crate::desktop::Program::Git,
        &["config", "--global", "--get-regexp", r"^include.*\.path$"],
    ) {
        Ok(listed) => String::from_utf8_lossy(&listed).into_owned(),
        Err(error) => {
            tracing::debug!(%error, "no git configuration included");
            return Vec::new();
        }
    };
    listed
        .lines()
        .filter_map(|line| line.split_once(' ').map(|(_, path)| path.trim()))
        .map(|path| match path.strip_prefix("~/") {
            Some(rest) => home.join(rest),
            None => PathBuf::from(path),
        })
        .collect()
}

/// The folder Claude Code's sessions put their messaging sockets in, made as Claude Code makes
/// it if no session has yet: the sandboxed one couldn't.
fn messaging_folder(runtime: &Path) -> Option<PathBuf> {
    let folder = runtime.join("cc-socks");
    match std::fs::DirBuilder::new().mode(0o700).create(&folder) {
        Ok(()) => Some(folder),
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => Some(folder),
        Err(error) => {
            tracing::debug!(%error, "no messaging folder");
            None
        }
    }
}

/// `~/.ssh`'s public keys: readable, unlike the private ones beside them.
fn public_keys(ssh: &Path) -> Vec<PathBuf> {
    std::fs::read_dir(ssh)
        .into_iter()
        .flatten()
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|extension| extension == "pub"))
        .collect()
}

/// What in `~/.claude` Claude Code writes as it works (transcripts, session records, todos,
/// caches): all of it but what it runs or obeys, and scripts.
fn claude_data(claude: &Path) -> Vec<PathBuf> {
    std::fs::read_dir(claude)
        .into_iter()
        .flatten()
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| {
            let name = path
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_default();
            !CLAUDE_CODE_RUNS.contains(&name.as_str()) && !name.ends_with(".sh")
        })
        .collect()
}

/// The git directories a project's commits go to: the repository's `.git` when the project is
/// in one (perhaps below its top), and for a linked worktree also the main repository's, which
/// holds the objects and refs.
fn git_dirs(project: &Path) -> Vec<PathBuf> {
    let Some(dot_git) = project
        .ancestors()
        .map(|place| place.join(".git"))
        .find(|dot_git| dot_git.exists())
    else {
        return Vec::new();
    };
    if dot_git.is_dir() {
        return vec![dot_git];
    }
    // A worktree's `.git` file points at `<main>/.git/worktrees/<name>`, whose `commondir`
    // names the main `.git`.
    let Some(gitdir) = std::fs::read_to_string(&dot_git).ok().and_then(|pointer| {
        pointer
            .strip_prefix("gitdir:")
            .map(|path| PathBuf::from(path.trim()))
    }) else {
        return Vec::new();
    };
    let gitdir = dot_git
        .parent()
        .map_or_else(|| gitdir.clone(), |base| base.join(&gitdir));
    let common = std::fs::read_to_string(gitdir.join("commondir"))
        .ok()
        .map(|relative| gitdir.join(relative.trim()));
    std::iter::once(gitdir).chain(common).collect()
}

#[cfg(test)]
mod tests {
    use std::os::unix::net::{UnixListener, UnixStream};
    use std::process::Command;

    use super::*;

    /// A command started from a restricted thread reads the project but nothing private, and
    /// the test's own thread stays free.
    #[test]
    fn a_sandboxed_child_sees_only_what_it_may() {
        let scratch = std::env::temp_dir().join(format!("edualc-sandbox-{}", std::process::id()));
        let project = scratch.join("project");
        let private = scratch.join("private");
        std::fs::create_dir_all(&project).expect("a project folder");
        std::fs::create_dir_all(&private).expect("a private folder");
        std::fs::write(project.join("open.txt"), "open").expect("a project file");
        std::fs::write(private.join("secret.txt"), "secret").expect("a private file");
        // The test runs below /tmp, which the policy opens; only the project is let in here.
        let policy = Policy {
            read: ["/usr", "/etc", "/proc"].map(PathBuf::from).to_vec(),
            write: vec![project.clone()],
            devices: vec![PathBuf::from("/dev/null")],
            sockets: Vec::new(),
            inboxes: Vec::new(),
        };
        let outcome = std::thread::scope(|scope| {
            scope
                .spawn(|| {
                    let enforcement = policy.restrict_this_thread().expect("a sandbox");
                    let cat = |path: &Path| {
                        Command::new("cat")
                            .arg(path)
                            .output()
                            .expect("cat runs")
                            .status
                            .success()
                    };
                    let sudo = Command::new("sudo")
                        .args(["-n", "true"])
                        .output()
                        .is_ok_and(|output| output.status.success());
                    (
                        enforcement,
                        cat(&project.join("open.txt")),
                        cat(&private.join("secret.txt")),
                        sudo,
                    )
                })
                .join()
                .expect("the restricted thread")
        });
        let (enforcement, open, secret, sudo) = outcome;
        if enforcement == Enforcement::None {
            return;
        }
        assert!(open, "the project is readable");
        assert!(!secret, "what is outside isn't");
        assert!(!sudo, "no privileges are gained");
        assert!(
            std::fs::read_to_string(private.join("secret.txt")).is_ok(),
            "this thread stays free"
        );
        std::fs::remove_dir_all(&scratch).expect("the scratch folder goes");
    }

    /// Claude Code's messaging: others reach a sandboxed session at its own socket, and it
    /// reaches none of theirs, nor takes one away.
    #[test]
    fn a_sandboxed_session_can_be_messaged_but_messages_no_one() {
        let scratch = std::env::temp_dir().join(format!("edualc-inbox-{}", std::process::id()));
        let inbox = scratch.join("cc-socks");
        std::fs::create_dir_all(&inbox).expect("an inbox folder");
        let peer = inbox.join("peer.sock");
        let _peer_listening = UnixListener::bind(&peer).expect("a peer's socket");
        let own = inbox.join("own.sock");
        let policy = Policy {
            read: ["/usr", "/etc", "/proc"].map(PathBuf::from).to_vec(),
            inboxes: vec![inbox],
            ..Policy::default()
        };
        let outcome = std::thread::scope(|scope| {
            scope
                .spawn(|| {
                    let enforcement = policy.restrict_this_thread().expect("a sandbox");
                    let own_listening = UnixListener::bind(&own);
                    // Landlock restricts this thread itself: no child is needed to try it.
                    let to_peer = UnixStream::connect(&peer);
                    let peer_removed = std::fs::remove_file(&peer);
                    (enforcement, own_listening, to_peer, peer_removed)
                })
                .join()
                .expect("the restricted thread")
        });
        let (enforcement, own_listening, to_peer, peer_removed) = outcome;
        let own_listening = own_listening.expect("its own socket is made");
        assert!(
            UnixStream::connect(&own).is_ok(),
            "others reach the sandboxed session"
        );
        if enforcement == Enforcement::Full {
            assert!(to_peer.is_err(), "it reaches no other session");
            assert!(peer_removed.is_err(), "nor takes another's socket away");
        }
        drop(own_listening);
        std::fs::remove_dir_all(&scratch).expect("the scratch folder goes");
    }

    #[test]
    fn what_claude_code_runs_stays_read_only() {
        let claude = std::env::temp_dir().join(format!("edualc-claude-{}", std::process::id()));
        for name in [
            "projects",
            "skills",
            "settings.json",
            "statusline.sh",
            "history.jsonl",
        ] {
            let path = claude.join(name);
            if name.contains('.') {
                std::fs::create_dir_all(&claude).expect("a folder");
                std::fs::write(&path, "").expect("a file");
            } else {
                std::fs::create_dir_all(&path).expect("a folder");
            }
        }
        let mut writable: Vec<String> = claude_data(&claude)
            .iter()
            .filter_map(|path| Some(path.file_name()?.to_string_lossy().into_owned()))
            .collect();
        writable.sort();
        assert_eq!(writable, ["history.jsonl", "projects"]);
        std::fs::remove_dir_all(&claude).expect("the scratch folder goes");
    }
}
