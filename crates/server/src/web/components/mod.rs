//! The window's parts, as the native window lays them out (`ui/app.slint`'s `AppWindow`): the
//! sidebar of sessions; the conversation's column (its header, the transcript, what Claude asks
//! and the bars over the composer, the composer); the side panel or a subagent followed; the
//! usage details and settings over the conversation.

use dioxus::prelude::*;
use edualc_core::app::Action;
use edualc_core::present::model::{PromptKind, SideView};

use super::View;

mod bars;
mod composer;
mod header;
mod panels;
mod prompt;
mod sidebar;
mod transcript;

/// A shortcut that works wherever the focus is, as the page's listener names it.
#[derive(Debug, Clone, Copy, serde::Deserialize)]
enum Shortcut {
    /// Ctrl+,
    Settings,
    /// Ctrl+B
    Sessions,
    /// Esc, unless what has the focus took it.
    Stop,
}

/// Listens for the shortcuts on the whole page, and tells each.
const SHORTCUTS: &str = r#"document.addEventListener("keydown", (event) => {
    if (event.altKey || event.metaKey || event.isComposing) {
        return;
    }
    if (event.ctrlKey && event.key === ",") {
        event.preventDefault();
        dioxus.send("Settings");
    } else if (event.ctrlKey && (event.key === "b" || event.key === "B")) {
        event.preventDefault();
        dioxus.send("Sessions");
    } else if (!event.ctrlKey && !event.shiftKey && event.key === "Escape" && !event.defaultPrevented) {
        dioxus.send("Stop");
    }
});
await new Promise(() => {});"#;

/// Esc stops the turn while Claude works, asks nothing and nothing is written (with a draft,
/// Stop is beside it), and nothing is over the conversation, as in the native window.
fn stop(view: View, settings: panels::SettingsOpen) {
    let stops = view.session.peek().busy
        && view.prompt.peek().kind == PromptKind::None
        && view.draft.peek().is_empty()
        && !view.usage.peek().open
        && !*settings.0.peek();
    if stops {
        view.act(Action::Interrupt);
    }
}

/// The whole window.
#[component]
pub fn Window() -> Element {
    let view = use_context::<View>();
    let settings = use_context_provider(|| panels::SettingsOpen(Signal::new(false)));
    let mut sidebar_shown = use_signal(|| true);
    // The shortcuts the native window takes wherever the focus is, and the page's controls name:
    // the settings, the sessions shown or hidden, and Esc to stop.
    use_hook(move || {
        spawn(async move {
            let mut keys = document::eval(SHORTCUTS);
            while let Ok(shortcut) = keys.recv::<Shortcut>().await {
                match shortcut {
                    Shortcut::Settings => settings.open(view),
                    Shortcut::Sessions => sidebar_shown.toggle(),
                    Shortcut::Stop => stop(view, settings),
                }
            }
        });
    });
    let session = view.session.read();
    let prompt = view.prompt.read();
    let composer = view.composer.read();
    let side = view.side.read();
    let peek = view.peek.read();
    let docked = prompt.kind != PromptKind::None
        || !composer.commands.is_empty()
        || session.watching
        || session.needs_trust
        || session.shows_sign_in;
    let writes = !session.watching && !session.needs_trust;
    rsx! {
        div { class: "layout",
            if sidebar_shown() {
                sidebar::Sidebar {
                    onhide: move |()| sidebar_shown.set(false),
                }
            }
            div { class: "column",
                header::Header {
                    sidebar_shown: sidebar_shown(),
                    ontoggle: move |()| sidebar_shown.toggle(),
                }
                transcript::Transcript {}
                if docked {
                    div { class: "dock",
                        if prompt.kind != PromptKind::None {
                            prompt::PromptBar {}
                        }
                        if !composer.commands.is_empty() {
                            composer::CommandList {}
                        }
                        if session.watching {
                            bars::WatchBar {}
                        }
                        if session.needs_trust && !session.watching {
                            bars::TrustBar {}
                        }
                        if session.shows_sign_in && !session.watching {
                            bars::SignInBar {}
                        }
                    }
                }
                if writes {
                    composer::Composer {}
                }
                // Over the conversation, as the native window lays them.
                panels::Usage {}
                panels::Settings {}
            }
            if peek.open {
                panels::PeekPanel {}
            } else if side.view == SideView::Shown {
                panels::SidePanel {}
            }
        }
    }
}
