//! Claude Code in a Landlock sandbox. The `claude` process and everything it starts (its tools'
//! commands, hooks, MCP servers) see the system and the toolchains read-only, may write only the
//! project, its git directory, caches, and a Claude Code home and temporary folder of the
//! session's own (see `home`), may reach no socket file but the ssh agent's, may signal nothing
//! outside, and gain no privileges (`sudo` and `pkexec` don't work). Other Claude Code sessions
//! may message it, but it messages none of them: one that isn't sandboxed could do for it what
//! the sandbox forbids.
//!
//! Nothing it writes is read outside the sandbox as configuration: the user's own Claude Code
//! home (`~/.claude`, `~/.claude.json`) and the other sessions' temporary files are out of its
//! reach, but for what in `~/.claude` is configuration, which it may read. So nothing can be
//! planted there for a session outside the sandbox to run, obey or source later, and the
//! user's sign-in stays out of it: the session gets a token through a pipe.
//!
//! Only the child is restricted: a thread restricts itself and starts it, and this window,
//! which opens editors, notifies and stops other sessions, stays free. Landlock can't be
//! widened once applied; every start gets a fresh ruleset, so a change takes a restart.
//!
//! Not covered: the network (Landlock tells ports apart, not hosts, so 443 would reach any host
//! anyway).
//!
//! Claude Code has a sandbox of its own, for its shell commands: bubblewrap, which filters the
//! network by host but leaves its file tools, hooks and MCP servers out. The two don't stack:
//! bubblewrap mounts, and Landlock forbids mounting to what it restricts. Whoever turned Claude
//! Code's on in their own settings keeps it, and this one stays off.

#[cfg(not(target_arch = "wasm32"))]
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};

use crate::external::claude_files::UserSettings;
#[cfg(not(target_arch = "wasm32"))]
use landlock::{
    ABI, Access, AccessFs, BitFlags, CompatLevel, Compatible, PathBeneath, PathFd, Ruleset,
    RulesetAttr, RulesetCreatedAttr, RulesetStatus, Scope,
};

/// The newest Landlock this window knows: 9 controls connecting to socket files.
#[cfg(not(target_arch = "wasm32"))]
const ABI_KNOWN: ABI = ABI::V9;

/// What `~/.claude` holds that Claude Code runs or obeys: readable to the sandbox, and copied
/// into a session's own home at each launch (see `home`).
pub(crate) const CLAUDE_CODE_RUNS: &[&str] = &[
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

// What a sandboxed Claude is told about its sandbox, in parts (see `Policy::note_for_claude`).
// Not told, it took a denied podman for a broken install, asked for `dangerouslyDisableSandbox`,
// which lifts only Claude Code's own sandbox, and, finding no other session to tell, left what
// it had for them in the project as a file.
const NOTE_FILES: &str = "\
This session runs in edualc, which keeps Claude Code and everything it starts in a Landlock \
sandbox. It may change only the project, its git folder, caches, /dev/shm and two folders of \
its own: its Claude Code home, which $CLAUDE_CONFIG_DIR names, and a temporary folder, which \
$TMPDIR names. /tmp and /var/tmp can't be written. ~/.claude isn't its home, and it can read \
only part of it. The system and the usual toolchains are read-only, and the user's other \
files are out of reach.";
const NOTE_SOCKETS: &str = "\
So is every socket file but the ssh agent's: no D-Bus, no container engine (podman, docker), \
nothing in /run/user.";
const NOTE_SOCKETS_BUT_DISPLAY: &str = "\
So is every socket file but the ssh agent's and the Wayland display's: no D-Bus, no container \
engine (podman, docker), nothing else in /run/user.";
const NOTE_HARDWARE: &str = "\
USB devices and the serial ports there were as it started are open to it (debug probes, \
boards); a port plugged in, or plugged in again, since needs the session started again.";
const NOTE_NO_HARDWARE: &str = "\
USB devices and serial ports are closed to it; the user can open them for this folder in \
edualc's sandbox menu.";
const NOTE_PRIVILEGES: &str = "Nothing gains privileges (no sudo). The network is open.";
const NOTE_MESSAGING: &str = "\
Other Claude Code sessions may message it, but it can't reach them itself: its own SendMessage \
and ListAgents reach only its own subagents. To message another session, or to answer one, it \
uses edualc's send_message tool with the session's name (list_sessions lists them); the user is \
asked to allow each message, and one not allowed isn't sent.";
const NOTE_ENFORCED: &str = "\
The kernel enforces this on the whole process, so `dangerouslyDisableSandbox` can't lift it. \
\"Permission denied\" on a path outside the project, or a socket that can't be reached, comes \
from this sandbox: don't look for a way around it. Tell the user what was blocked and what \
for, and let them choose: run the command themselves outside edualc, allow the path \
(`sandbox_read` or `sandbox_write` in ~/.config/edualc/settings.json, read when edualc's \
window opens), or turn the sandbox off with the shield button in edualc's window, which \
restarts the session.";

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

#[cfg(not(target_arch = "wasm32"))]
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
    /// USB devices and the serial ports there are as it starts (`ttyACM*`, `ttyUSB*`), with
    /// what names them (`/dev/serial`, udev's database): debug probes, boards, serial
    /// monitors. A port plugged in, or plugged in again (a new device node), needs the session
    /// started again. Every USB device the user may open is let in, a phone too. `/dev/hidraw*`
    /// stays closed on purpose, so HID probes (CMSIS-DAP v1) don't work: it is keyboards' and
    /// security keys' too.
    pub(crate) hardware: bool,
    /// More to read, as the user's settings name it.
    pub(crate) read: &'a [PathBuf],
    /// More to write.
    pub(crate) write: &'a [PathBuf],
}

/// What a sandboxed session has of its own (see `home`): its Claude Code home and its
/// temporary folder, which it may write as it likes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Own<'a> {
    pub(crate) home: &'a Path,
    pub(crate) scratch: &'a Path,
}

#[cfg(not(target_arch = "wasm32"))]
impl Policy {
    /// The policy for working in `project`.
    pub(crate) fn for_project(project: &Path, own: Own<'_>, allowances: Allowances<'_>) -> Self {
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
            ]
            .map(at_home),
        );
        policy.read.extend(claude_config(&home.join(".claude")));
        policy.read.extend(public_keys(&home.join(".ssh")));
        policy.read.extend(git_includes(&home));
        policy.write.extend([project.to_owned()]);
        policy.write.extend(git_dirs(project));
        // Not /tmp or /var/tmp: other sessions keep files there that they read back (their
        // tasks' output, the Claude Code daemon's socket). `TMPDIR` names the session's own.
        policy.write.push(PathBuf::from("/dev/shm"));
        policy
            .write
            .extend([own.home.to_owned(), own.scratch.to_owned()]);
        policy.write.extend(
            [
                ".cache",
                ".cargo/registry",
                ".cargo/git",
                ".npm",
                "go",
                ".m2",
                ".gradle",
            ]
            .map(at_home),
        );
        policy.read.extend(allowances.read.iter().cloned());
        policy.write.extend(allowances.write.iter().cloned());
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
        if allowances.hardware {
            // USB as a tree, so a probe plugged in later is reached; the ports are files, so
            // only those there now (see `Allowances::hardware`).
            policy.devices.push(PathBuf::from("/dev/bus/usb"));
            policy.devices.extend(serial_ports(Path::new("/dev")));
            policy
                .read
                .extend(["/dev/serial", "/run/udev"].map(PathBuf::from));
        }
        policy
    }

    /// The policy for agy working in `project` (see `agy`): what a sandboxed Claude reads, and
    /// writes in the project, its git folder, caches and `/dev/shm`; besides, agy's own data
    /// (`data`, its sign-in and conversations) and the session's temporary folder. It also
    /// reads `read`: agy itself, edualc for the messaging tool agy starts, and agy's
    /// configuration. Of socket files only `sockets`, which edualc's adapter serves it; it
    /// makes none for others.
    pub(crate) fn for_agy(
        project: &Path,
        data: &[PathBuf],
        read: &[PathBuf],
        scratch: &Path,
        sockets: &[PathBuf],
    ) -> Self {
        let mut policy = Self::for_project(
            project,
            Own {
                home: scratch,
                scratch,
            },
            Allowances {
                desktop: false,
                hardware: false,
                read,
                write: data,
            },
        );
        policy.inboxes.clear();
        policy.sockets.extend(sockets.iter().cloned());
        policy
    }

    /// Whether the user let the desktop in (see [`Allowances::desktop`]).
    fn lets_in_the_desktop(&self) -> bool {
        self.devices
            .iter()
            .any(|device| device == Path::new("/dev/dri"))
    }

    /// Whether the user let USB devices and serial ports in (see [`Allowances::hardware`]).
    fn lets_in_hardware(&self) -> bool {
        self.devices
            .iter()
            .any(|device| device == Path::new("/dev/bus/usb"))
    }

    /// What a sandboxed Claude is told about its sandbox (`--append-system-prompt`), as far as
    /// the kernel enforced it; nothing where it enforced none.
    pub(crate) fn note_for_claude(&self, enforcement: Enforcement) -> Option<String> {
        let hardware = if self.lets_in_hardware() {
            NOTE_HARDWARE
        } else {
            NOTE_NO_HARDWARE
        };
        match enforcement {
            Enforcement::None => None,
            // An older kernel: connecting to socket files, the newest of what is asked, isn't
            // guarded, so the note says nothing of them, nor of the other sessions' sockets.
            Enforcement::Partial => Some(format!(
                "{NOTE_FILES} {hardware} {NOTE_PRIVILEGES}\n\n{NOTE_ENFORCED}"
            )),
            Enforcement::Full => {
                let sockets = if self.lets_in_the_desktop() {
                    NOTE_SOCKETS_BUT_DISPLAY
                } else {
                    NOTE_SOCKETS
                };
                Some(format!(
                    "{NOTE_FILES} {sockets} {hardware} {NOTE_PRIVILEGES}\n\n{NOTE_MESSAGING}\n\n{NOTE_ENFORCED}"
                ))
            }
        }
    }

    /// Variables of this window's environment the child isn't given: ways out of the sandbox
    /// (the session bus starts programs; niri's socket does too) and displays it may not use.
    pub(crate) fn hidden_variables(&self) -> &'static [&'static str] {
        if self.lets_in_the_desktop() {
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

/// The USB serial ports in `dev` now (`ttyACM*`, `ttyUSB*`), as boards and probes show up.
fn serial_ports(dev: &Path) -> Vec<PathBuf> {
    std::fs::read_dir(dev)
        .into_iter()
        .flatten()
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("ttyACM") || name.starts_with("ttyUSB"))
        })
        .collect()
}

/// A rule for each of `paths` that exists; one that doesn't is no rule (and needs none). A file
/// gets only the rights a file can have: a folder's (making entries in it, say) on a file would
/// count as not enforced, and the whole sandbox as only partly.
#[cfg(not(target_arch = "wasm32"))]
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
#[cfg(not(target_arch = "wasm32"))]
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

/// What in the user's `~/.claude` is configuration, for the sandbox to read: what Claude Code
/// runs or obeys, and the scripts beside it (a status line's, say). Settings name these by
/// their place there, as an installed plugin does its folder. Not the sign-in, the session
/// records and their keys, nor the transcripts and memories of every project.
fn claude_config(claude: &Path) -> Vec<PathBuf> {
    let scripts = std::fs::read_dir(claude)
        .into_iter()
        .flatten()
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| {
            path.extension().is_some_and(|extension| extension == "sh") && path.is_file()
        });
    CLAUDE_CODE_RUNS
        .iter()
        .map(|name| claude.join(name))
        .chain(scripts)
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

    /// agy writes its own data, and reads its configuration without changing it.
    #[test]
    fn agy_keeps_its_data_and_reads_its_configuration() {
        let scratch = std::env::temp_dir().join(format!("edualc-agy-{}", std::process::id()));
        let [project, data, config, temporary] =
            ["project", "data", "config", "tmp"].map(|name| scratch.join(name));
        for folder in [&project, &data, &config, &temporary] {
            std::fs::create_dir_all(folder).expect("a scratch folder");
        }
        std::fs::write(config.join("mcp_config.json"), "{}").expect("a configuration");
        let policy = Policy::for_agy(
            &project,
            std::slice::from_ref(&data),
            std::slice::from_ref(&config),
            &temporary,
            &[],
        );
        let outcome = std::thread::scope(|scope| {
            scope
                .spawn(|| {
                    let enforcement = policy.restrict_this_thread().expect("a sandbox");
                    let shell = |script: &str| {
                        Command::new("sh")
                            .args(["-c", script])
                            .output()
                            .expect("sh runs")
                            .status
                            .success()
                    };
                    let data = data.display();
                    let config = config.display();
                    (
                        enforcement,
                        shell(&format!("echo state > '{data}/state'")),
                        shell(&format!("cat '{config}/mcp_config.json'")),
                        shell(&format!("echo '{{\"x\":1}}' > '{config}/mcp_config.json'")),
                        shell(&format!("echo planted > '{config}/new.json'")),
                    )
                })
                .join()
                .expect("the restricted thread")
        });
        let (enforcement, writes_data, reads_config, changes_config, adds_config) = outcome;
        if enforcement == Enforcement::None {
            return;
        }
        assert!(writes_data, "its own data is writable");
        assert!(reads_config, "its configuration is readable");
        assert!(!changes_config, "its configuration can't be changed");
        assert!(!adds_config, "nor added to");
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

    /// Of the user's own home, the sandbox reads the configuration, and nothing else.
    #[test]
    fn only_the_users_configuration_is_readable() {
        let claude = std::env::temp_dir().join(format!("edualc-claude-{}", std::process::id()));
        for name in [
            "projects",
            "skills",
            "sessions",
            "settings.json",
            "statusline.sh",
            "history.jsonl",
            ".credentials.json",
        ] {
            let path = claude.join(name);
            if name.contains('.') {
                std::fs::create_dir_all(&claude).expect("a folder");
                std::fs::write(&path, "").expect("a file");
            } else {
                std::fs::create_dir_all(&path).expect("a folder");
            }
        }
        let readable: Vec<PathBuf> = claude_config(&claude);
        let present: Vec<String> = readable
            .iter()
            .filter(|path| path.exists())
            .filter_map(|path| Some(path.file_name()?.to_string_lossy().into_owned()))
            .collect();
        assert_eq!(present, ["settings.json", "skills", "statusline.sh"]);
        std::fs::remove_dir_all(&claude).expect("the scratch folder goes");
    }

    /// The user's own home is out of reach: nothing in it, `~/.claude.json` included, is
    /// writable, and what isn't configuration (the sign-in, say) isn't readable.
    #[test]
    fn the_users_own_home_is_out_of_reach() {
        let scratch = std::env::temp_dir().join(format!("edualc-own-{}", std::process::id()));
        let project = scratch.join("project");
        let home = scratch.join("home");
        let session_scratch = scratch.join("tmp");
        for folder in [&project, &home, &session_scratch] {
            std::fs::create_dir_all(folder).expect("a folder");
        }
        let policy = Policy::for_project(
            &project,
            Own {
                home: &home,
                scratch: &session_scratch,
            },
            Allowances {
                desktop: false,
                hardware: false,
                read: &[],
                write: &[],
            },
        );
        let users = dirs::home_dir().expect("a home");
        let in_users = |path: &PathBuf| path.starts_with(users.join(".claude"));
        assert!(
            !policy
                .write
                .iter()
                .any(|path| in_users(path) || path.ends_with(".claude.json")),
            "{:?}",
            policy.write
        );
        assert!(
            !policy
                .write
                .iter()
                .any(|path| path == Path::new("/tmp") || path == Path::new("/var/tmp")),
            "{:?}",
            policy.write
        );
        assert!(policy.write.contains(&home) && policy.write.contains(&session_scratch));
        assert!(
            !policy.read.iter().any(|path| *path == users.join(".claude")
                || path.ends_with(".credentials.json")
                || path.ends_with("projects")),
            "{:?}",
            policy.read
        );
        std::fs::remove_dir_all(&scratch).expect("the scratch folder goes");
    }

    /// A sandboxed Claude is told where its own folders are, and only what the kernel guards:
    /// the display where the user let the desktop in, nothing about socket files or other
    /// sessions where the kernel can't keep them closed, and nothing without a sandbox.
    #[test]
    fn claude_is_told_only_what_the_kernel_guards() {
        let project = std::env::temp_dir().join(format!("edualc-note-{}", std::process::id()));
        let home = project.join("home");
        let session_scratch = project.join("tmp");
        let note_with = |desktop, hardware, enforcement| {
            let own = Own {
                home: &home,
                scratch: &session_scratch,
            };
            let allowances = Allowances {
                desktop,
                hardware,
                read: &[],
                write: &[],
            };
            Policy::for_project(&project, own, allowances).note_for_claude(enforcement)
        };
        let note = |desktop, enforcement| note_with(desktop, false, enforcement);
        let closed = note(false, Enforcement::Full).expect("a note");
        assert!(closed.contains("$CLAUDE_CONFIG_DIR") && closed.contains("$TMPDIR"));
        assert!(closed.contains("nothing in /run/user") && !closed.contains("Wayland"));
        assert!(closed.contains("can't reach them itself") && closed.contains("send_message"));
        assert!(closed.contains("USB devices and serial ports are closed"));
        let with_desktop = note(true, Enforcement::Full).expect("a note");
        assert!(with_desktop.contains("the Wayland display's"));
        assert!(with_desktop.contains("nothing else in /run/user"));
        let with_hardware = note_with(false, true, Enforcement::Full).expect("a note");
        assert!(with_hardware.contains("serial ports there were as it started are open"));
        let partial = note(false, Enforcement::Partial).expect("a note");
        assert!(partial.contains("$CLAUDE_CONFIG_DIR") && partial.contains("$TMPDIR"));
        assert!(!partial.contains("socket file") && !partial.contains("can't reach them"));
        assert!(note(false, Enforcement::None).is_none());
    }

    /// USB devices and serial ports are let in only when asked: USB as a tree, the ports that
    /// are there, and what names them, read only.
    #[test]
    fn hardware_is_let_in_only_when_asked() {
        let project = std::env::temp_dir().join(format!("edualc-usb-{}", std::process::id()));
        let own = Own {
            home: &project,
            scratch: &project,
        };
        let policy = |hardware| {
            let allowances = Allowances {
                desktop: false,
                hardware,
                read: &[],
                write: &[],
            };
            Policy::for_project(&project, own, allowances)
        };
        let closed = policy(false);
        assert!(
            !closed
                .devices
                .iter()
                .any(|device| device.starts_with("/dev/bus"))
        );
        assert!(!closed.read.contains(&PathBuf::from("/dev/serial")));
        let open = policy(true);
        assert!(open.devices.contains(&PathBuf::from("/dev/bus/usb")));
        assert!(open.read.contains(&PathBuf::from("/dev/serial")));
        assert!(open.read.contains(&PathBuf::from("/run/udev")));
        assert!(
            !open
                .write
                .iter()
                .any(|path| path.starts_with("/dev/serial"))
        );
    }

    /// Only USB serial ports are taken from a device folder, as they are there.
    #[test]
    fn serial_ports_are_the_usb_ones_there() {
        let dev = std::env::temp_dir().join(format!("edualc-dev-{}", std::process::id()));
        std::fs::create_dir_all(&dev).expect("a scratch folder");
        for name in ["ttyACM0", "ttyUSB1", "ttyS0", "tty1", "sda"] {
            std::fs::write(dev.join(name), "").expect("a stand-in device");
        }
        let mut ports = serial_ports(&dev);
        ports.sort();
        assert_eq!(ports, [dev.join("ttyACM0"), dev.join("ttyUSB1")]);
        std::fs::remove_dir_all(&dev).expect("the scratch folder goes");
    }
}
