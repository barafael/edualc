//! The page: edualc's window, drawn by Dioxus. It runs the same app as the native window
//! (`edualc-core`): the presenter's frontend here (`view::PageFrontend`) puts what it shows in
//! signals, which the components draw; what the user does goes back as the app's actions. In a
//! browser its line is a WebSocket to the server that served it (`line`); in the Android app
//! (`android`), which draws the same components, the native window's line (`edualc_core::line`).

// Dioxus's `rsx!` keeps its templates in `OnceLock`s on the server: its state, not ours.
#![allow(clippy::disallowed_types)]
// The server renders the page as it is before its line opens; what drives it (the frontend,
// the line) runs only in the browser and the app.
#![cfg_attr(not(any(feature = "web", feature = "mobile")), allow(dead_code))]

use dioxus::prelude::*;

mod components;
pub mod dom;
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
    // The line opens in the browser only: the server renders the page as it is before. The
    // hook is there in both, so the page's hooks are the same ones, in the same order.
    use_hook(move || {
        #[cfg(feature = "web")]
        spawn(run(view));
    });
    rsx! {
        Shell {}
    }
}

/// What everything the page shows is drawn in: the look and the text size chosen, the
/// stylesheet and the fonts. The window draws in it, and the app's start screen.
#[component]
pub fn Frame(children: Element) -> Element {
    let view = use_context::<View>();
    let page = view.page.read();
    let theme = match page.look {
        edualc_core::present::Look::Gruvbox => "gruvbox",
        edualc_core::present::Look::Plain => "plain",
    };
    let style = format!("--base-size: {}px;", page.text_size);
    rsx! {
        document::Stylesheet { href: STYLE }
        Fonts {}
        document::Title { "edualc" }
        div { class: "window {theme}", style, {children} }
    }
}

/// The window, and what keeps it from working, if anything does.
#[component]
pub fn Shell() -> Element {
    let view = use_context::<View>();
    let page = view.page.read();
    rsx! {
        Frame {
            Stylesheets {}
            if let Some(lost) = &page.lost {
                div { class: "lost", role: "alert", "{lost}" }
            } else if page.closed && cfg!(not(feature = "mobile")) {
                div { class: "lost", "This window was closed. Reload the page to open it again." }
            }
            components::Window {}
        }
    }
}

/// Every part's stylesheet, from the start. Each part names its own too, but a link is put in
/// the page by an effect of the part that names it first, and only once: a part shown and
/// hidden in the same moment (the sidebar, as a narrow window starts) would leave its
/// stylesheet out for good.
#[component]
fn Stylesheets() -> Element {
    rsx! {
        document::Stylesheet { href: asset!("/assets/sidebar.css") }
        document::Stylesheet { href: asset!("/assets/header.css") }
        document::Stylesheet { href: asset!("/assets/transcript.css") }
        document::Stylesheet { href: asset!("/assets/prompt.css") }
        document::Stylesheet { href: asset!("/assets/bars.css") }
        document::Stylesheet { href: asset!("/assets/composer.css") }
        document::Stylesheet { href: asset!("/assets/panels.css") }
    }
}

/// The fonts' faces, declared once: their own component, so the page's changes don't touch
/// them.
#[component]
pub fn Fonts() -> Element {
    rsx! {
        document::Style {
            "@font-face {{ font-family: 'Fira Sans'; src: url({SANS}); font-weight: 400; }}
            @font-face {{ font-family: 'Fira Sans'; src: url({SANS_SEMIBOLD}); font-weight: 600; }}
            @font-face {{ font-family: 'Fira Sans'; src: url({SANS_ITALIC}); font-style: italic; }}
            @font-face {{ font-family: 'Fira Code'; src: url({MONO}); }}"
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
            // As the browser keeps it: what a URL may not hold (a space, say) `%`-escaped.
            Some(("token", value)) => token = decoded(value),
            Some(("dir", value)) => dir = Some(decoded(value)),
            Some(_) | None => {}
        }
    }
    (token, dir)
}

/// A value of the address, its `%`-escapes undone. A `+` stays one: in a URL's fragment it is
/// no space (a folder may be named `c++`).
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
                out.push(byte);
                at += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Opens the line, greets the daemon, and runs the app until the line is lost.
#[cfg(feature = "web")]
async fn run(view: View) {
    use edualc_core::app::{App, Desktop, Event};
    use edualc_core::ipc::{PROTOCOL, ToDaemon};
    use edualc_core::link::{self, Inbox, QUEUE};
    use futures_util::future::{Either, select};

    let lost = move |why: String| {
        let mut page = view.page;
        page.write().lost = Some(why);
    };
    let (token, dir) = address();
    tracing::debug!("opening the line to the daemon");
    let (mut sender, mut receiver) = match crate::line::open(&token) {
        Ok(line) => line,
        Err(error) => return lost(format!("No line to edualc's daemon: {error}")),
    };
    if let Err(error) = sender
        .send(&ToDaemon::Hello {
            protocol: PROTOCOL,
            window: edualc_core::ipc::WindowKey::new(),
        })
        .await
    {
        return lost(format!("The daemon didn't take the hello: {error}"));
    }
    let hello = receiver.recv().await;
    let hosted = receiver.recv().await;
    let (hello, hosted) = match (hello, hosted) {
        (Ok(hello), Ok(hosted)) => (hello, hosted),
        (Err(error), _) | (_, Err(error)) => {
            return lost(format!(
                "The daemon didn't answer: {error}. Is the page's token right?"
            ));
        }
    };
    tracing::debug!("the daemon answered");
    let greeting = match link::greeted(hello, hosted) {
        Ok(greeting) => greeting,
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
    tracing::debug!("the app began");
    spawn(app.run(received));
    let mut inbox = Inbox::default();
    loop {
        let asked = std::pin::pin!(queue.recv());
        let said = std::pin::pin!(receiver.recv());
        match select(asked, said).await {
            Either::Left((Ok(message), _)) => {
                if let Err(error) = sender.send(&message).await {
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
