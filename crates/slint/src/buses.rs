//! The D-Bus the confined window gets: not the session bus, which starts programs outside any
//! sandbox (`systemd-run --user`) and reaches every service of the user, but filtering proxies
//! (`xdg-dbus-proxy`, as Flatpak does). Through the session bus's, the window reaches only
//! accessibility's address, the desktop's settings (light or dark), the portal that opens a link
//! outside the window's sandbox, and the notification service; through the accessibility bus's,
//! a screen reader can read the window, but the window can't read or drive other programs.
//!
//! They are the window's desktop's, wherever its edualcd runs, so the window starts them
//! itself: edualc's first process, before anything else, starts the proxies, then the window
//! as its child with its bus the proxies' (a running process can't change its own bus without
//! `unsafe`), and waits for it. It holds one end of a socket pair with each proxy, which stops
//! as that end closes: as edualc ends, however it ends.

use std::io::Read as _;
use std::os::unix::fs::{DirBuilderExt as _, MetadataExt as _};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::Duration;

/// What a proxy took to listen, at most.
const READY: Duration = Duration::from_secs(2);

/// Set in the window's own environment, to the first process's id: the proxies are started, and
/// the window is that process's child.
pub(crate) const PROXIED: &str = "EDUALC_PROXIED";

/// The proxies' sockets, which the window's bus addresses name.
#[derive(Debug, Default)]
struct Buses {
    /// The session bus, filtered.
    session: Option<PathBuf>,
    /// The accessibility bus, filtered.
    accessibility: Option<PathBuf>,
}

/// A running proxy, and this process's end of the socket it says it is ready on and stops when
/// closed.
#[derive(Debug)]
struct Proxy {
    _child: Child,
    _sync: UnixStream,
}

/// Starts the proxies, then this program again as the window, its bus theirs, and waits for it:
/// returns its exit code, or why it couldn't be started.
pub(crate) fn run_window() -> Result<std::process::ExitCode, std::io::Error> {
    let folder = folder();
    let (buses, proxies) = match &folder {
        Some(folder) => start(folder),
        None => (Buses::default(), Vec::new()),
    };
    let mut command = Command::new(std::env::current_exe()?);
    command
        .args(std::env::args_os().skip(1))
        .env(PROXIED, std::process::id().to_string());
    match &buses.session {
        Some(socket) => command.env(
            "DBUS_SESSION_BUS_ADDRESS",
            format!("unix:path={}", socket.display()),
        ),
        None => command.env_remove("DBUS_SESSION_BUS_ADDRESS"),
    };
    match &buses.accessibility {
        Some(socket) => command.env(
            "AT_SPI_BUS_ADDRESS",
            format!("unix:path={}", socket.display()),
        ),
        None => command.env_remove("AT_SPI_BUS_ADDRESS"),
    };
    let status = command.status();
    drop(proxies);
    if let Some(folder) = folder
        && let Err(error) = std::fs::remove_dir_all(&folder)
    {
        tracing::debug!(%error, "the proxies' folder stays");
    }
    let code = status?
        .code()
        .and_then(|code| u8::try_from(code).ok())
        .unwrap_or(1);
    Ok(std::process::ExitCode::from(code))
}

/// `$XDG_RUNTIME_DIR/edualc/window-<pid>`, made for this window's proxies, the user's alone; none
/// where there is no such place (the window goes without D-Bus).
fn folder() -> Option<PathBuf> {
    let runtime = PathBuf::from(std::env::var_os("XDG_RUNTIME_DIR")?);
    let edualc = runtime.join("edualc");
    let ours = edualc.join(format!("window-{}", std::process::id()));
    let made = private(&edualc).and_then(|()| {
        clear_stale(&edualc);
        private(&ours)
    });
    match made {
        Ok(()) => Some(ours),
        Err(error) => {
            tracing::warn!(%error, "no private folder for the D-Bus proxies: the window goes without D-Bus");
            None
        }
    }
}

/// Removes the folders of windows whose first process is gone without removing its own (it was
/// killed).
fn clear_stale(edualc: &Path) {
    let entries = std::fs::read_dir(edualc).into_iter().flatten().flatten();
    for entry in entries {
        let name = entry.file_name();
        let Some(pid) = name
            .to_str()
            .and_then(|name| name.strip_prefix("window-"))
            .filter(|pid| pid.bytes().all(|byte| byte.is_ascii_digit()))
        else {
            continue;
        };
        if !Path::new("/proc").join(pid).exists()
            && let Err(error) = std::fs::remove_dir_all(entry.path())
        {
            tracing::debug!(%error, "a gone window's proxies' folder stays");
        }
    }
}

/// Ends this process, the window, as its first process does, which holds its D-Bus proxies:
/// killed alone, it would leave the window without them. False if it is gone already.
pub(crate) fn end_with_first_process() -> bool {
    let first = std::env::var(PROXIED)
        .ok()
        .and_then(|pid| pid.parse::<i32>().ok());
    if let Err(error) =
        rustix::process::set_parent_process_death_signal(Some(rustix::process::Signal::TERM))
    {
        tracing::warn!(%error, "the window outlives its first process if that is killed");
    }
    // Asked after the signal is set: a parent gone before is seen here.
    let parent = rustix::process::getppid().map(rustix::process::Pid::as_raw_nonzero);
    parent.map(std::num::NonZero::get) == first
}

/// A folder made (or found) the user's alone: theirs, not a link, closed to others.
fn private(folder: &Path) -> Result<(), std::io::Error> {
    match std::fs::DirBuilder::new().mode(0o700).create(folder) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(error),
    }
    let metadata = std::fs::symlink_metadata(folder)?;
    let owned = metadata.uid() == rustix::process::getuid().as_raw();
    if !metadata.is_dir() || !owned || metadata.mode() & 0o077 != 0 {
        return Err(std::io::Error::other(format!(
            "{} isn't the user's alone",
            folder.display()
        )));
    }
    Ok(())
}

/// Starts the proxies in `folder`; those that can't be started are left out.
fn start(folder: &Path) -> (Buses, Vec<Proxy>) {
    let mut running = Vec::new();
    let mut buses = Buses::default();
    if let Some(address) = std::env::var_os("DBUS_SESSION_BUS_ADDRESS") {
        let socket = folder.join("session-bus");
        let portal = "org.freedesktop.portal.Desktop";
        let path = "/org/freedesktop/portal/desktop";
        let rules = [
            "--talk=org.a11y.Bus".to_owned(),
            format!("--call={portal}=org.freedesktop.portal.Settings.*@{path}"),
            format!(
                "--broadcast={portal}=org.freedesktop.portal.Settings.SettingChanged@{path}"
            ),
            // A link from the conversation opens in the desktop's browser, outside the window's
            // sandbox; a notification shows when a session elsewhere needs the user.
            format!("--call={portal}=org.freedesktop.portal.OpenURI.OpenURI@{path}"),
            "--call=org.freedesktop.Notifications=org.freedesktop.Notifications.*@/org/freedesktop/Notifications".to_owned(),
        ];
        if let Some(proxy) = proxy(&address.to_string_lossy(), &socket, &rules) {
            running.push(proxy);
            buses.session = Some(socket);
        }
    }
    if let Some(address) = accessibility_address() {
        let socket = folder.join("accessibility-bus");
        // Flatpak's: a program announces itself to the registry and answers what a screen
        // reader asks it; it reaches no other program.
        let registry = "org.a11y.atspi.Registry";
        let controller = "org.a11y.atspi.DeviceEventController";
        let rules = [
            format!(
                "--call={registry}=org.a11y.atspi.Socket.Embed@/org/a11y/atspi/accessible/root"
            ),
            format!(
                "--call={registry}=org.a11y.atspi.Socket.Unembed@/org/a11y/atspi/accessible/root"
            ),
            format!(
                "--call={registry}=org.a11y.atspi.Registry.GetRegisteredEvents@/org/a11y/atspi/registry"
            ),
            format!(
                "--call={registry}={controller}.GetKeystrokeListeners@/org/a11y/atspi/registry/deviceeventcontroller"
            ),
            format!(
                "--call={registry}={controller}.GetDeviceEventListeners@/org/a11y/atspi/registry/deviceeventcontroller"
            ),
            format!(
                "--call={registry}={controller}.NotifyListenersSync@/org/a11y/atspi/registry/deviceeventcontroller"
            ),
            format!(
                "--call={registry}={controller}.NotifyListenersAsync@/org/a11y/atspi/registry/deviceeventcontroller"
            ),
        ];
        if let Some(proxy) = proxy(&address, &socket, &rules) {
            running.push(proxy);
            buses.accessibility = Some(socket);
        }
    }
    (buses, running)
}

/// One proxy, once it listens. Its `--fd` is one end of a socket pair: it writes a byte there
/// when it listens, and stops when this process's end closes.
fn proxy(address: &str, socket: &Path, rules: &[String]) -> Option<Proxy> {
    let (mut sync, theirs) = match UnixStream::pair() {
        Ok(paired) => paired,
        Err(error) => {
            tracing::warn!(%error, "no socket for a D-Bus proxy: the window goes without D-Bus");
            return None;
        }
    };
    let child = Command::new("xdg-dbus-proxy")
        .arg(address)
        .arg(socket)
        .arg("--filter")
        .arg("--sloppy-names")
        .args(rules)
        .arg("--fd=0")
        .stdin(Stdio::from(std::os::fd::OwnedFd::from(theirs)))
        .stdout(Stdio::null())
        .spawn();
    let mut child = match child {
        Ok(child) => child,
        Err(error) => {
            tracing::warn!(%error, "no D-Bus proxy: the window goes without D-Bus");
            return None;
        }
    };
    let mut ready = [0u8; 1];
    let read = sync
        .set_read_timeout(Some(READY))
        .and_then(|()| sync.read(&mut ready));
    match read {
        Ok(1) => Some(Proxy {
            _child: child,
            _sync: sync,
        }),
        Ok(_) | Err(_) => {
            tracing::warn!(socket = %socket.display(), "a D-Bus proxy didn't listen");
            if let Err(error) = child.kill() {
                tracing::debug!(%error, "the proxy had ended");
            }
            None
        }
    }
}

/// The accessibility bus's address, as its launcher says; none without one.
fn accessibility_address() -> Option<String> {
    let output = Command::new("busctl")
        .args([
            "--user",
            "call",
            "org.a11y.Bus",
            "/org/a11y/bus",
            "org.a11y.Bus",
            "GetAddress",
        ])
        .stderr(Stdio::null())
        .output()
        .inspect_err(|error| tracing::debug!(%error, "no busctl"))
        .ok()?;
    if !output.status.success() {
        tracing::debug!("no accessibility bus");
        return None;
    }
    // `s "unix:path=…"`
    let said = String::from_utf8_lossy(&output.stdout);
    let address = said.trim().strip_prefix("s \"")?.strip_suffix('"')?;
    Some(address.to_owned())
}
