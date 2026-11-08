//! The page: edualc's window, drawn by Dioxus. It runs the same app as the native window
//! (`edualc-core`): the presenter's frontend here (`view::PageFrontend`) puts what it shows in
//! signals, which the components draw; what the user does goes back as the app's actions. Its
//! line to the daemon is the server's (`line`), carried on to the daemon's socket.

// Dioxus's `rsx!` keeps its templates in `OnceLock`s on the server: its state, not ours.
#![allow(clippy::disallowed_types)]
// The server renders the page as it is before its line opens; what drives it (the frontend,
// the line) runs only in the browser.
#![cfg_attr(not(feature = "web"), allow(dead_code))]

use dioxus::prelude::*;

mod components;
pub mod text;
pub mod view;
pub mod widgets;

pub use view::View;

const STYLE: Asset = asset!("/assets/edualc.css");
/// Fira Sans and Fira Code, as the native window draws its text (SIL Open Font License).
const SANS: Asset = asset!("/assets/fonts/FiraSans-Regular.ttf");
const SANS_SEMIBOLD: Asset = asset!("/assets/fonts/FiraSans-SemiBold.ttf");
const SANS_ITALIC: Asset = asset!("/assets/fonts/FiraSans-Italic.ttf");
const MONO: Asset = asset!("/assets/fonts/FiraCode-Regular.ttf");

/// The page.
#[component]
pub fn App() -> Element {
    let view = use_hook(View::new);
    use_context_provider(|| view);
    // In the browser only: the server renders the page as it is before the line opens.
    #[cfg(feature = "web")]
    use_hook(move || {
        spawn(run(view));
    });
    let page = view.page.read();
    let theme = match page.look {
        edualc_core::present::Look::Gruvbox => "gruvbox",
        edualc_core::present::Look::Plain => "plain",
    };
    let style = format!("--base-size: {}px;", page.text_size);
    rsx! {
        document::Stylesheet { href: STYLE }
        document::Style {
            "@font-face {{ font-family: 'Fira Sans'; src: url({SANS}); font-weight: 400; }}
            @font-face {{ font-family: 'Fira Sans'; src: url({SANS_SEMIBOLD}); font-weight: 600; }}
            @font-face {{ font-family: 'Fira Sans'; src: url({SANS_ITALIC}); font-style: italic; }}
            @font-face {{ font-family: 'Fira Code'; src: url({MONO}); }}"
        }
        document::Title { "edualc" }
        div { class: "window {theme}", style,
            if let Some(lost) = &page.lost {
                div { class: "lost", role: "alert", "{lost}" }
            } else if page.closed {
                div { class: "lost", "This window was closed. Reload the page to open it again." }
            }
            components::Window {}
        }
    }
}

/// The page's address: its token, and the folder it is to open (`#token=…&dir=…`).
#[cfg(feature = "web")]
fn address() -> (String, Option<String>) {
    let hash = web_sys::window()
        .and_then(|window| window.location().hash().ok())
        .unwrap_or_default();
    let mut token = String::new();
    let mut dir = None;
    for pair in hash.trim_start_matches('#').split('&') {
        match pair.split_once('=') {
            Some(("token", value)) => token = value.to_owned(),
            Some(("dir", value)) => dir = Some(decoded(value)),
            Some(_) | None => {}
        }
    }
    (token, dir)
}

/// A value of the address, its `%`-escapes undone.
#[cfg(feature = "web")]
fn decoded(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut at = 0;
    while let Some(&byte) = bytes.get(at) {
        let escaped = (byte == b'%')
            .then(|| bytes.get(at + 1..at + 3))
            .flatten()
            .and_then(|hex| std::str::from_utf8(hex).ok())
            .and_then(|hex| u8::from_str_radix(hex, 16).ok());
        match escaped {
            Some(decoded) => {
                out.push(decoded);
                at += 3;
            }
            None => {
                out.push(if byte == b'+' { b' ' } else { byte });
                at += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Opens the line, greets the daemon, and runs the app until the line is lost.
#[cfg(feature = "web")]
async fn run(view: View) {
    use dioxus::fullstack::WebSocketOptions;
    use edualc_core::app::{App, Desktop, Event};
    use edualc_core::ipc::{PROTOCOL, ToDaemon};
    use edualc_core::link::{self, Inbox, QUEUE};
    use futures_util::future::{Either, select};

    let lost = move |why: String| {
        let mut page = view.page;
        page.write().lost = Some(why);
    };
    let (token, dir) = address();
    let socket = match crate::line::line(token, WebSocketOptions::new()).await {
        Ok(socket) => socket,
        Err(error) => return lost(format!("No line to edualc's daemon: {error}")),
    };
    if let Err(error) = socket.send(ToDaemon::Hello { protocol: PROTOCOL }).await {
        return lost(format!("The daemon didn't take the hello: {error}"));
    }
    let (hello, hosted) = match (socket.recv().await, socket.recv().await) {
        (Ok(hello), Ok(hosted)) => (hello, hosted),
        (Err(error), _) | (_, Err(error)) => {
            return lost(format!(
                "The daemon didn't answer: {error}. Is the page's token right?"
            ));
        }
    };
    let greeting = match link::greeted(hello, hosted) {
        Ok((_buses, greeting)) => greeting,
        Err(error) => return lost(error.to_string()),
    };
    let cwd = dir
        .map(std::path::PathBuf::from)
        .or_else(|| greeting.home.clone())
        .unwrap_or_else(|| std::path::PathBuf::from("/"));
    let (events, received) = async_channel::unbounded();
    let (line, queue) = async_channel::bounded(QUEUE);
    view.events.clone().set(Some(events.clone()));
    let text_size = view.page.peek().text_size;
    let frontend = view::PageFrontend::new(view);
    let mut app = App::new(
        frontend,
        events.clone(),
        line,
        Desktop::of(&greeting, text_size),
        greeting.settings.clone(),
        cwd,
        None,
    );
    app.begin(greeting.hosted, greeting.live, None, None, None);
    spawn(app.run(received));
    let mut inbox = Inbox::default();
    loop {
        let asked = std::pin::pin!(queue.recv());
        let said = std::pin::pin!(socket.recv());
        match select(asked, said).await {
            Either::Left((Ok(message), _)) => {
                if let Err(error) = socket.send(message).await {
                    tracing::warn!(%error, "lost the daemon");
                    break;
                }
            }
            // The app ended.
            Either::Left((Err(_closed), _)) => return,
            Either::Right((Ok(message), _)) => {
                if let Some(event) = inbox.take(message)
                    && events.send(event).await.is_err()
                {
                    return;
                }
            }
            Either::Right((Err(error), _)) => {
                tracing::warn!(%error, "lost the daemon");
                break;
            }
        }
    }
    if events.send(Event::DaemonLost).await.is_err() {
        tracing::debug!("the app ended with the line");
    }
    lost("The line to edualc's daemon was lost. Reload the page to connect again.".to_owned());
}
