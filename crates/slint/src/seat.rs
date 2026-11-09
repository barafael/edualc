//! What the window does on its own desktop, the user's (see `present::Seat`), each on a thread of
//! its own: a link opens through the desktop portal (`OpenURI`), outside the window's sandbox,
//! as a Flatpak's does; a notification goes to the notification service; the clipboard's image
//! is read with `wl-paste` and decoded in the confined helper (see `decode`). The window is
//! confined: these run so, and reach only what its D-Bus proxies let through (see `buses`).

use std::collections::HashMap;
use std::time::Duration;

use async_channel::Sender;
use tokio::io::AsyncReadExt as _;

use crate::app::Event;
use crate::decode::{self, PasteFailure, Pasted, Refusal};
use crate::desktop;
use crate::present::Seat;

/// How long the clipboard's owner gets to hand over what it holds.
const CLIPBOARD: Duration = Duration::from_secs(5);
/// The most of the clipboard's list of types read: a few dozen names.
const MAX_CLIPBOARD_TYPES_BYTES: usize = 64 * 1024;

/// Does `seat` on a thread of its own; what fails, or the image pasted, comes to the app as an
/// event.
pub(crate) fn act(seat: Seat, events: &Sender<Event>) {
    let events = events.clone();
    std::thread::spawn(move || {
        let event = match seat {
            Seat::OpenUrl(url) => open_url(&url)
                .err()
                .map(|failure| Event::SeatFailed(format!("couldn't open {url}: {failure}"))),
            Seat::Notify { summary, body } => {
                if let Err(error) = desktop::notify(&summary, &body) {
                    tracing::warn!(%error, "couldn't notify");
                }
                None
            }
            Seat::PasteImage => Some(Event::Pasted(paste())),
        };
        if let Some(event) = event
            && events.send_blocking(event).is_err()
        {
            tracing::debug!("the window closed");
        }
    });
}

/// Opens `url` in the desktop's browser, through the portal.
fn open_url(url: &str) -> Result<(), zbus::Error> {
    let connection = zbus::blocking::Connection::session()?;
    let options: HashMap<&str, zbus::zvariant::Value<'_>> = HashMap::new();
    connection.call_method(
        Some("org.freedesktop.portal.Desktop"),
        "/org/freedesktop/portal/desktop",
        Some("org.freedesktop.portal.OpenURI"),
        "OpenURI",
        &("", url, options),
    )?;
    Ok(())
}

/// The clipboard's image, decoded in the confined helper.
fn paste() -> Pasted {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|error| PasteFailure::Clipboard(error.to_string()))?;
    runtime.block_on(async {
        let listed = clipboard(&["--list-types"], MAX_CLIPBOARD_TYPES_BYTES).await?;
        let format = String::from_utf8_lossy(&listed)
            .lines()
            .find_map(|line| line.trim().parse::<decode::Format>().ok())
            .ok_or(PasteFailure::NoImage)?;
        let limit = decode::MAX_INPUT_BYTES + 1;
        let bytes = clipboard(&["--no-newline", "--type", format.as_ref()], limit).await?;
        if bytes.len() > decode::MAX_INPUT_BYTES {
            return Err(PasteFailure::Refused(Refusal::TooManyBytes));
        }
        decode::decode(&bytes).await.map_err(|error| match error {
            decode::Error::Refused(refusal) => PasteFailure::Refused(refusal),
            error @ (decode::Error::Start(_)
            | decode::Error::NoPipes
            | decode::Error::Hand(_)
            | decode::Error::Read(_)
            | decode::Error::Slow
            | decode::Error::Failed(_)
            | decode::Error::TooMuch
            | decode::Error::Unreadable(_)
            | decode::Error::Malformed) => PasteFailure::Decoder(error.to_string()),
        })
    })
}

/// What `wl-paste` gives with `args`, up to `limit` bytes (reading stops there).
async fn clipboard(args: &[&str], limit: usize) -> Result<Vec<u8>, PasteFailure> {
    let failed = |error: std::io::Error| PasteFailure::Clipboard(error.to_string());
    let mut child = tokio::process::Command::new("wl-paste")
        .args(args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .map_err(failed)?;
    let Some(output) = child.stdout.take() else {
        return Err(PasteFailure::Clipboard("wl-paste has no output".to_owned()));
    };
    let reading = async {
        let mut bytes = Vec::new();
        let cap = u64::try_from(limit).unwrap_or(u64::MAX);
        output
            .take(cap)
            .read_to_end(&mut bytes)
            .await
            .map_err(failed)?;
        if bytes.len() >= limit {
            // The caller refuses it; what is left isn't waited for (the child is killed as it
            // is dropped).
            return Ok(bytes);
        }
        let status = child.wait().await.map_err(failed)?;
        // It fails when the clipboard is empty, or holds nothing of the type asked.
        if status.success() {
            Ok(bytes)
        } else {
            Err(PasteFailure::NoImage)
        }
    };
    tokio::time::timeout(CLIPBOARD, reading)
        .await
        .map_err(|_elapsed| {
            PasteFailure::Clipboard("the clipboard's owner didn't hand it over in time".to_owned())
        })?
}
