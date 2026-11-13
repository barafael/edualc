//! What the window does on the platform it runs on, chosen here once: in a browser, the
//! browser's; in the Android app, Android's (`android::java`); rendered on the server, nothing.
//! The window's code calls each alike everywhere.

pub use here::*;

/// The Android app: Android's browser, notifications and clipboard, and its runtime's clock.
#[cfg(feature = "mobile")]
mod here {
    use dioxus::prelude::ReadableExt as _;
    use edualc_core::app::Event;
    use edualc_core::decode;
    use edualc_core::ipc::{ActId, PasteFailure};

    use crate::android::java;
    use crate::web::View;

    /// Opens `url` in Android's browser.
    pub fn open_url(url: &str) {
        if let Err(error) = webbrowser::open(url) {
            tracing::warn!(%error, "Android opened no browser");
        }
    }

    /// Shows Android's notification, if the user lets the app (asked as the window opens).
    pub fn notify(summary: &str, body: &str) {
        java::notify(summary.to_owned(), body.to_owned());
    }

    /// Puts `text` on Android's clipboard.
    pub fn copy(text: &str) {
        java::copy(text.to_owned());
    }

    /// The clipboard's image, read and made ready to send beside the window (the decoder's
    /// sandbox is Linux's), for the next message.
    pub fn paste_image(view: View) {
        let Some(events) = view.events.peek().clone() else {
            return;
        };
        java::clipboard_image(decode::MAX_INPUT_BYTES, move |read| {
            let pasted = match read {
                Ok(Some(bytes)) => decode::to_send(&bytes).map_err(PasteFailure::Refused),
                Ok(None) => Err(PasteFailure::NoImage),
                Err(error) => Err(PasteFailure::Clipboard(error.to_string())),
            };
            let pasted = Event::Pasted {
                request: ActId::default(),
                pasted,
            };
            if events.send_blocking(pasted).is_err() {
                tracing::debug!("the app ended");
            }
        });
    }

    /// Waits `ms` milliseconds.
    pub async fn sleep(ms: u32) {
        tokio::time::sleep(std::time::Duration::from_millis(u64::from(ms))).await;
    }

    /// Whether a keyboard with keys is attached, so that Enter sends: on the screen's keyboard
    /// alone, which has no Shift+Enter, Enter starts a new line. Asked of Android beside the
    /// window.
    pub async fn keyboard_attached() -> bool {
        let (answer, answered) = async_channel::bounded(1);
        java::has_keyboard(move |has| {
            if answer.send_blocking(has).is_err() {
                tracing::debug!("the keyboard was asked of for nothing");
            }
        });
        match answered.recv().await {
            Ok(Ok(has)) => has,
            Ok(Err(error)) => {
                tracing::debug!(%error, "no keyboard known of");
                false
            }
            Err(_gone) => false,
        }
    }
}

/// A browser's page: a tab, the browser's notifications and clipboard, and its timers.
#[cfg(all(feature = "web", not(feature = "mobile")))]
mod here {
    use crate::web::View;

    /// Opens `url` in a new tab, which can't reach back to the page.
    pub fn open_url(url: &str) {
        if let Some(window) = web_sys::window()
            && window
                .open_with_url_and_target_and_features(url, "_blank", "noopener,noreferrer")
                .is_err()
        {
            tracing::warn!("the browser opened no tab");
        }
    }

    /// Shows a notification, if the browser lets the page; asks it once, for the next.
    pub fn notify(summary: &str, body: &str) {
        use web_sys::{Notification, NotificationOptions, NotificationPermission};
        match Notification::permission() {
            NotificationPermission::Granted => {
                let options = NotificationOptions::new();
                options.set_body(body);
                if Notification::new_with_options(summary, &options).is_err() {
                    tracing::warn!("the browser showed no notification");
                }
            }
            NotificationPermission::Default => {
                if Notification::request_permission().is_err() {
                    tracing::debug!("the browser wasn't asked to notify");
                }
            }
            NotificationPermission::Denied | _ => {}
        }
    }

    /// Puts `text` on the browser's clipboard. Only a secure context (https, or the loopback)
    /// has one: elsewhere `navigator.clipboard` is undefined, and calling it would throw
    /// through the page's code.
    pub fn copy(text: &str) {
        let Some(window) = web_sys::window() else {
            return;
        };
        if !window.is_secure_context() {
            tracing::warn!("no clipboard here: the page isn't served over https or the loopback");
            return;
        }
        let written = window.navigator().clipboard().write_text(text);
        wasm_bindgen_futures::spawn_local(async move {
            if wasm_bindgen_futures::JsFuture::from(written).await.is_err() {
                tracing::warn!("the clipboard refused the text");
            }
        });
    }

    /// Nothing: a page's pasted image comes with the paste itself (see the composer).
    pub fn paste_image(_view: View) {}

    /// Waits `ms` milliseconds.
    pub async fn sleep(ms: u32) {
        gloo_timers::future::TimeoutFuture::new(ms).await;
    }

    /// A browser's page is typed into with a keyboard with keys: Enter sends.
    #[expect(clippy::unused_async, reason = "as the app's, which asks Android")]
    pub async fn keyboard_attached() -> bool {
        true
    }
}

/// Rendered on the server: nothing runs.
#[cfg(not(any(feature = "web", feature = "mobile")))]
mod here {
    pub fn copy(_text: &str) {}

    #[expect(clippy::unused_async, reason = "as the page's")]
    pub async fn sleep(_ms: u32) {}

    #[expect(clippy::unused_async, reason = "as the page's")]
    pub async fn keyboard_attached() -> bool {
        true
    }
}
