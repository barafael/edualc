//! The window, confined. It parses what Claude and other sessions write and draws it; for that
//! it reads the system, fonts and the user's files but their secrets, writes only its settings
//! and caches, reaches no socket file but Wayland's and its D-Bus proxies' (see `buses`),
//! signals nothing outside, and gains no privileges (Landlock). Of the network it reaches only
//! its edualcd: TCP to edualcd's port and no other (Landlock), no UDP, raw or other sockets, so
//! no DNS either (its edualcd's addresses were looked up before), nor io_uring, which could open
//! one past the filter (seccomp). What it can't do, it asks its edualcd, which does a few
//! things, checked.
//!
//! Applied to the main thread before any other thread or the window is made: what is made after
//! keeps it.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use std::net::SocketAddr;

use landlock::{
    ABI, Access, AccessFs, AccessNet, BitFlags, CompatLevel, Compatible, NetPort, PathBeneath,
    PathFd, Ruleset, RulesetAttr, RulesetCreatedAttr, RulesetStatus, Scope,
};
use seccompiler::{
    BpfProgram, SeccompAction, SeccompCmpArgLen, SeccompCmpOp, SeccompCondition, SeccompFilter,
    SeccompRule,
};

/// The newest Landlock this window knows.
const ABI_KNOWN: ABI = ABI::V9;

/// Under the home folder, kept from the window even where the rest is read: keys, passwords,
/// tokens, mail and browsing.
const SECRETS: &[&str] = &[
    ".ssh",
    ".gnupg",
    ".password-store",
    ".local/share/keyrings",
    ".mozilla",
    ".thunderbird",
    ".config/chromium",
    ".config/google-chrome",
    ".config/gh",
    ".aws",
    ".kube",
    ".docker",
    ".netrc",
    ".pki",
    ".cargo/credentials.toml",
    ".cargo/credentials",
    ".claude/.credentials.json",
    // MCP servers' environments, tokens among them; the daemon reads the trust kept there.
    ".claude.json",
    ".claude/backups",
];

/// Claude Code's own secrets where `CLAUDE_CONFIG_DIR` puts its folder (`folder`) and
/// `.claude.json` (`global_config`): the sign-in, MCP servers' environments, and backups of both.
fn claude_secrets(folder: &Path, global_config: &Path) -> [PathBuf; 3] {
    [
        folder.join(".credentials.json"),
        global_config.to_owned(),
        folder.join("backups"),
    ]
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum Error {
    #[error("couldn't confine the window: {0}")]
    Landlock(#[from] landlock::RulesetError),
    #[error("couldn't confine the window's sockets: {0}")]
    Seccomp(#[from] seccompiler::Error),
    #[error("couldn't confine the window's sockets: {0}")]
    SeccompBackend(#[from] seccompiler::BackendError),
}

/// How far the kernel confines the window.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Confinement {
    Full,
    /// An older kernel: some of it isn't enforced (socket files, say); the socket filter is.
    Partial,
}

/// Confines the calling thread, and what it starts from now on; of the network, it reaches
/// `edualcd`'s port alone.
pub(crate) fn confine(edualcd: &[SocketAddr]) -> Result<Confinement, Error> {
    let home = dirs::home_dir().unwrap_or_default();
    let claude = crate::sessions::config_dir();
    let mut secrets: Vec<PathBuf> = SECRETS.iter().map(|secret| home.join(secret)).collect();
    secrets.extend(claude_secrets(
        &claude,
        &crate::sessions::global_config_path(),
    ));
    let mut read: Vec<PathBuf> = ["/usr", "/etc", "/opt", "/bin", "/sbin", "/lib", "/lib64"]
        .iter()
        .chain(&["/proc", "/sys"])
        .map(PathBuf::from)
        .collect();
    read.extend(all_but(&home, &secrets));
    // Itself, which it starts to decode images (`decode`), wherever it is installed or built.
    read.extend(std::env::current_exe().ok());
    // The sessions and transcripts the window lists, where `CLAUDE_CONFIG_DIR` keeps them
    // outside the home folder.
    if !claude.starts_with(&home) {
        read.extend(all_but(&claude, &secrets));
    }
    // Its settings and caches, where `XDG_CONFIG_HOME` and `XDG_CACHE_HOME` put them, as
    // `settings` finds them.
    let write: Vec<PathBuf> = [
        dirs::config_dir().map(|config| config.join("edualc")),
        dirs::cache_dir(),
    ]
    .into_iter()
    .flatten()
    .collect();
    let devices: Vec<PathBuf> = ["/dev/dri", "/dev/null", "/dev/urandom"]
        .iter()
        .map(PathBuf::from)
        .collect();
    // Wayland's, and its D-Bus proxies' (see `buses`).
    let runtime = std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from);
    let sockets: Vec<PathBuf> = runtime
        .iter()
        .flat_map(|runtime| {
            let display = std::env::var_os("WAYLAND_DISPLAY").map(|display| runtime.join(display));
            display.into_iter().chain([runtime.join("edualc")])
        })
        .collect();

    let all = AccessFs::from_all(ABI_KNOWN);
    let reading = AccessFs::from_read(ABI_KNOWN);
    let writing = all & !AccessFs::ResolveUnix;
    let device = reading | AccessFs::WriteFile | AccessFs::IoctlDev;
    let status = Ruleset::default()
        .set_compatibility(CompatLevel::BestEffort)
        .handle_access(all)?
        // Handled, and only connecting to edualcd's port allowed (the socket filter below
        // refuses the rest of the network besides).
        .handle_access(AccessNet::BindTcp | AccessNet::ConnectTcp)?
        .scope(Scope::AbstractUnixSocket | Scope::Signal)?
        .create()?
        .add_rules(rules(&read, reading))?
        .add_rules(rules(&write, writing))?
        .add_rules(rules(&devices, device))?
        .add_rules(rules(&sockets, AccessFs::ResolveUnix.into()))?
        .add_rules(ports(edualcd))?
        .restrict_self()?;
    seccompiler::apply_filter(&socket_filter()?)?;
    Ok(match status.ruleset {
        RulesetStatus::FullyEnforced => Confinement::Full,
        RulesetStatus::PartiallyEnforced | RulesetStatus::NotEnforced => Confinement::Partial,
    })
}

/// Connecting to `edualcd`'s ports, by TCP.
fn ports(
    edualcd: &[SocketAddr],
) -> impl Iterator<Item = Result<NetPort, landlock::RulesetError>> + '_ {
    let mut ports: Vec<u16> = edualcd.iter().map(SocketAddr::port).collect();
    ports.sort_unstable();
    ports.dedup();
    ports
        .into_iter()
        .map(|port| Ok(NetPort::new(port, AccessNet::ConnectTcp)))
}

/// Refuses every socket but Unix ones and TCP's (streams of IPv4 and IPv6), and io_uring, for
/// the calling thread and what it starts.
fn socket_filter() -> Result<BpfProgram, Error> {
    let family = |op, family: i32| {
        SeccompCondition::new(
            0,
            SeccompCmpArgLen::Dword,
            op,
            u64::from(family.unsigned_abs()),
        )
    };
    // Neither Unix nor IP.
    let mut refused = vec![SeccompRule::new(vec![
        family(SeccompCmpOp::Ne, libc::AF_UNIX)?,
        family(SeccompCmpOp::Ne, libc::AF_INET)?,
        family(SeccompCmpOp::Ne, libc::AF_INET6)?,
    ])?];
    // IP, but not a stream: datagrams (UDP, DNS among them), raw sockets, and the rest. The
    // type's low bits name it; its flags (non-blocking, close-on-exec) are above.
    for ip in [libc::AF_INET, libc::AF_INET6] {
        for kind in
            (0..=0xf_u64).filter(|&kind| kind != u64::from(libc::SOCK_STREAM.unsigned_abs()))
        {
            refused.push(SeccompRule::new(vec![
                family(SeccompCmpOp::Eq, ip)?,
                SeccompCondition::new(
                    1,
                    SeccompCmpArgLen::Dword,
                    SeccompCmpOp::MaskedEq(0xf),
                    kind,
                )?,
            ])?);
        }
    }
    let mut rules: BTreeMap<i64, Vec<SeccompRule>> = BTreeMap::new();
    rules.insert(libc::SYS_socket, refused);
    // No condition: refused whatever is asked.
    for io_uring in [
        libc::SYS_io_uring_setup,
        libc::SYS_io_uring_enter,
        libc::SYS_io_uring_register,
    ] {
        rules.insert(io_uring, Vec::new());
    }
    let refused = SeccompAction::Errno(libc::EAFNOSUPPORT.unsigned_abs());
    let filter = SeccompFilter::new(
        rules,
        SeccompAction::Allow,
        refused,
        std::env::consts::ARCH.try_into()?,
    )?;
    Ok(filter.try_into()?)
}

/// `root` and what is under it, but `secrets`: where one is inside an entry, that entry's own
/// entries are taken instead, down to the secret, which is left out.
fn all_but(root: &Path, secrets: &[PathBuf]) -> Vec<PathBuf> {
    if secrets.iter().any(|secret| secret == root) {
        return Vec::new();
    }
    if !secrets.iter().any(|secret| secret.starts_with(root)) {
        return vec![root.to_owned()];
    }
    std::fs::read_dir(root)
        .into_iter()
        .flatten()
        .filter_map(Result::ok)
        .flat_map(|entry| all_but(&entry.path(), secrets))
        .collect()
}

/// A rule for each of `paths` that exists; a file gets only the rights a file can have.
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secrets_are_left_out_however_deep() {
        let root = std::env::temp_dir().join(format!("edualc-confine-{}", std::process::id()));
        for place in ["a/b/secret", "a/b/open", "a/c", "d"] {
            std::fs::create_dir_all(root.join(place)).expect("a folder");
        }
        let mut allowed: Vec<PathBuf> = all_but(&root, &[root.join("a/b/secret")])
            .into_iter()
            .map(|path| path.strip_prefix(&root).expect("under the root").to_owned())
            .collect();
        allowed.sort();
        assert_eq!(
            allowed,
            ["a/b/open", "a/c", "d"].map(PathBuf::from).to_vec()
        );
        std::fs::remove_dir_all(&root).expect("the scratch folder goes");
    }

    /// A Claude Code folder moved by `CLAUDE_CONFIG_DIR` is read, its sessions and transcripts,
    /// but not its sign-in, `.claude.json` or backups.
    #[test]
    fn a_moved_claude_folder_keeps_its_secrets() {
        let folder = std::env::temp_dir().join(format!("edualc-claude-{}", std::process::id()));
        for place in ["projects/-home-x", "sessions", "backups"] {
            std::fs::create_dir_all(folder.join(place)).expect("a folder");
        }
        for file in [".credentials.json", ".claude.json", "settings.json"] {
            std::fs::write(folder.join(file), "{}").expect("a file");
        }
        let secrets = claude_secrets(&folder, &folder.join(".claude.json"));
        let mut allowed: Vec<PathBuf> = all_but(&folder, &secrets)
            .into_iter()
            .map(|path| {
                path.strip_prefix(&folder)
                    .expect("in the folder")
                    .to_owned()
            })
            .collect();
        allowed.sort();
        assert_eq!(
            allowed,
            ["projects", "sessions", "settings.json"]
                .map(PathBuf::from)
                .to_vec()
        );
        std::fs::remove_dir_all(&folder).expect("the scratch folder goes");
    }

    /// Filtered, a thread opens Unix sockets and TCP's (which Landlock keeps to edualcd's
    /// port), but no UDP: no DNS, nor anything else.
    #[test]
    fn the_filter_refuses_all_but_tcp() {
        let filter = socket_filter().expect("a filter");
        std::thread::scope(|scope| {
            scope
                .spawn(|| {
                    seccompiler::apply_filter(&filter).expect("applied");
                    assert!(std::net::TcpListener::bind("127.0.0.1:0").is_ok());
                    assert!(std::net::TcpListener::bind("[::1]:0").is_ok() || !ipv6());
                    assert!(std::net::UdpSocket::bind("127.0.0.1:0").is_err());
                    assert!(std::net::UdpSocket::bind("[::1]:0").is_err());
                    assert!(std::os::unix::net::UnixDatagram::unbound().is_ok());
                })
                .join()
                .expect("the filtered thread");
        });
        // This thread is free.
        assert!(std::net::UdpSocket::bind("127.0.0.1:0").is_ok());
    }

    /// Whether this machine has IPv6's loopback.
    fn ipv6() -> bool {
        std::net::TcpListener::bind("[::1]:0").is_ok()
    }
}
