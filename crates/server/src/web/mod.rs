//! The page: edualc's window, drawn by Dioxus. It runs the same app as the native window
//! (`edualc-core`): the presenter's frontend here (`view::PageFrontend`) puts what it shows in
//! signals, which the components draw; what the user does goes back as the app's actions. In a
//! browser its line is a WebSocket to the server that served it (`line`); in the Android app
//! (`android`), which draws the same components, a native line over the network
//! (`edualc_core::line`).

use dioxus::prelude::*;

mod components;
pub mod dom;
pub mod platform;
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
    // Asked from the first thing you do in the window, which a browser takes the question from.
    let mut asked = use_signal(|| false);
    let mut ask = move || {
        if !asked() {
            asked.set(true);
            platform::ask_to_notify();
        }
    };
    rsx! {
        document::Stylesheet { href: STYLE }
        Fonts {}
        document::Title { "edualc" }
        div {
            class: "window {theme}",
            style,
            onclick: move |_| ask(),
            onkeydown: move |_| ask(),
            {children}
        }
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

/// Every part's stylesheet, linked here, from the start, rather than by each part: a link is
/// put in the page by an effect of the part that names it first, and only once, so a part shown
/// and hidden in the same moment would leave its stylesheet out for good. A part's new
/// stylesheet goes in this list.
#[component]
fn Stylesheets() -> Element {
    rsx! {
        document::Stylesheet { href: asset!("/assets/sessions.css") }
        document::Stylesheet { href: asset!("/assets/header.css") }
        document::Stylesheet { href: asset!("/assets/transcript.css") }
        document::Stylesheet { href: asset!("/assets/prompt.css") }
        document::Stylesheet { href: asset!("/assets/bars.css") }
        document::Stylesheet { href: asset!("/assets/composer.css") }
        document::Stylesheet { href: asset!("/assets/panels.css") }
        document::Stylesheet { href: asset!("/assets/pages.css") }
        document::Stylesheet { href: asset!("/assets/changes.css") }
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
    use edualc_core::ipc::pairing::decoded;
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

#[cfg(any(feature = "web", feature = "mobile"))]
/// Starts the window's app on a line edualcd greeted (`link`): in `cwd`, opening `resume` if
/// given, or with no folder on the start page of projects. What it shows goes to `view`, and it
/// hears `events`' receiver's (as `app.run` is given it). The page starts it so, and the
/// Android app on each edualcd it reaches.
pub fn begin(
    view: View,
    events: async_channel::Sender<edualc_core::app::Event>,
    link: edualc_core::link::Link,
    greeting: edualc_core::link::Greeting,
    cwd: Option<std::path::PathBuf>,
    resume: Option<edualc_core::external::wire::SessionId>,
) -> edualc_core::app::App<view::PageFrontend> {
    use edualc_core::app::{App, Desktop};
    let mut shown_events = view.events;
    shown_events.set(Some(events.clone()));
    // The page's own size, not the one shown: that is zoomed already, by the app this one
    // follows (another edualcd reached), and would be zoomed again.
    let mut app = App::new(
        view::PageFrontend::new(view),
        events,
        link,
        Desktop::of(&greeting, view::DESKTOP_TEXT_SIZE),
        greeting.settings.clone(),
        edualc_core::app::Start {
            cwd,
            backend: edualc_core::claude::Backend::Claude,
            model: None,
        },
    );
    app.begin(greeting.hosted, greeting.live, resume, None, None);
    app
}

/// Opens the line, greets the daemon, and runs the app until the line is lost.
#[cfg(feature = "web")]
async fn run(view: View) {
    use edualc_core::app::Event;
    use edualc_core::ipc::{PROTOCOL, ToDaemon};
    use edualc_core::link::{self, Inbox, QUEUE};
    use futures_util::future::{Either, select};

    let lost = move |why: String| {
        let mut page = view.page;
        page.write().lost = Some(why);
    };
    let (token, dir) = address();
    tracing::debug!("opening the line to the daemon");
    let (mut sender, mut receiver) = match crate::line::open(&token).await {
        Ok(line) => line,
        Err(error) => return lost(format!("No line to edualc's daemon: {error}")),
    };
    if let Err(error) = sender.send(&ToDaemon::Hello { protocol: PROTOCOL }).await {
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
    // The folder the address names, or the projects to choose from.
    let cwd = dir.map(std::path::PathBuf::from);
    let (events, received) = async_channel::unbounded();
    let (line, queue) = async_channel::bounded(QUEUE);
    let app = begin(view, events.clone(), line, greeting, cwd, None);
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
