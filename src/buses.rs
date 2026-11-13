//! The D-Bus the confined window gets: not the session bus, which starts programs outside any
//! sandbox (`systemd-run --user`) and reaches every service of the user, but filtering proxies
//! the daemon runs (`xdg-dbus-proxy`, as Flatpak does). Through the session bus's, the window
//! reaches only accessibility's address and the desktop's settings (light or dark); through the
//! accessibility bus's, a screen reader can read the window, but the window can't read or drive
//! other programs.

use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use tokio::io::AsyncReadExt;
use tokio::process::{Child, Command};

use crate::ipc::Buses;

/// What a proxy took to listen, at most.
const READY: Duration = Duration::from_secs(2);

/// The running proxies: they end with the daemon (killed with it, and stopping when their
/// end of the sync socket closes, should it be killed itself).
#[derive(Debug)]
pub(crate) struct Proxies {
    _running: Vec<Proxy>,
}

/// A proxy, and the daemon's end of the socket it says it is ready on and stops when closed.
#[derive(Debug)]
struct Proxy {
    _child: Child,
    _sync: tokio::net::UnixStream,
}

/// Starts the proxies in the daemon's folder; those that can't be started are left out.
pub(crate) async fn start(folder: &Path) -> (Buses, Proxies) {
    let mut running = Vec::new();
    let mut buses = Buses::default();
    if let Some(address) = std::env::var_os("DBUS_SESSION_BUS_ADDRESS") {
        let socket = folder.join("session-bus");
        let rules = [
            "--talk=org.a11y.Bus".to_owned(),
            "--call=org.freedesktop.portal.Desktop=org.freedesktop.portal.Settings.*@/org/freedesktop/portal/desktop".to_owned(),
            "--broadcast=org.freedesktop.portal.Desktop=org.freedesktop.portal.Settings.SettingChanged@/org/freedesktop/portal/desktop".to_owned(),
        ];
        if let Some(proxy) = proxy(&address.to_string_lossy(), &socket, &rules).await {
            running.push(proxy);
            buses.session = Some(socket);
        }
    }
    if let Some(address) = accessibility_address().await {
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
        if let Some(proxy) = proxy(&address, &socket, &rules).await {
            running.push(proxy);
            buses.accessibility = Some(socket);
        }
    }
    (buses, Proxies { _running: running })
}

/// One proxy, once it listens. Its `--fd` is one end of a socket pair: it writes a byte there
/// when it listens, and stops when the daemon's end closes, however the daemon ends.
async fn proxy(address: &str, socket: &Path, rules: &[String]) -> Option<Proxy> {
    if let Err(error) = std::fs::remove_file(socket)
        && error.kind() != std::io::ErrorKind::NotFound
    {
        tracing::warn!(%error, "an old proxy's socket stays");
        return None;
    }
    let paired = std::os::unix::net::UnixStream::pair().and_then(|(ours, theirs)| {
        ours.set_nonblocking(true)?;
        Ok((tokio::net::UnixStream::from_std(ours)?, theirs))
    });
    let (mut sync, theirs) = match paired {
        Ok(paired) => paired,
        Err(error) => {
            tracing::warn!(%error, "no socket for a D-Bus proxy: the window goes without D-Bus");
            return None;
        }
    };
    let mut command = Command::new("xdg-dbus-proxy");
    command
        .arg(address)
        .arg(socket)
        .arg("--filter")
        .arg("--sloppy-names")
        .args(rules)
        .arg("--fd=0")
        .stdin(Stdio::from(std::os::fd::OwnedFd::from(theirs)))
        .stdout(Stdio::null())
        .kill_on_drop(true);
    let child = match command.spawn() {
        Ok(child) => child,
        Err(error) => {
            tracing::warn!(%error, "no D-Bus proxy: the window goes without D-Bus");
            return None;
        }
    };
    let mut ready = [0u8; 1];
    match tokio::time::timeout(READY, sync.read(&mut ready)).await {
        Ok(Ok(1)) => Some(Proxy {
            _child: child,
            _sync: sync,
        }),
        Ok(Ok(_)) | Ok(Err(_)) | Err(_) => {
            tracing::warn!(socket = %socket.display(), "a D-Bus proxy didn't listen");
            None
        }
    }
}

/// The accessibility bus's address, as its launcher says; none without one.
async fn accessibility_address() -> Option<String> {
    let output = Command::new("busctl")
        .args([
            "--user",
            "call",
            "org.a11y.Bus",
            "/org/a11y/bus",
            "org.a11y.Bus",
            "GetAddress",
        ])
        .output()
        .await
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
