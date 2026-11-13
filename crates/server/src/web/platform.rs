//! What the window does on the platform it runs on, chosen here once: in a browser, the
//! browser's; rendered on the server, nothing.
//! The window's code calls each alike everywhere.

pub use here::*;

#[cfg(feature = "web")]
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
#[cfg(not(feature = "web"))]
mod here {
    pub fn copy(_text: &str) {}

    #[expect(clippy::unused_async, reason = "as the page's")]
    pub async fn sleep(_ms: u32) {}

    #[expect(clippy::unused_async, reason = "as the page's")]
    pub async fn keyboard_attached() -> bool {
        true
    }
}
