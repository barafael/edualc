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
#[serde(deny_unknown_fields)]
enum Shortcut {
    /// Ctrl+,
    Settings,
    /// Ctrl+B
    Sessions,
    /// Esc, unless what has the focus took it.
    Stop,
    /// Ctrl+K: the sessions shown, and their filter focused.
    Filter,
    /// F6 (`forward`) or Shift+F6, from the area the focus is in, if any.
    Area { forward: bool, from: Option<Area> },
}

/// The window's areas F6 goes round, in the native window's order (`ui/app.slint`'s
/// `next-area`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize)]
enum Area {
    Sidebar,
    Conversation,
    Writing,
    Side,
}

/// The area after `from` (or before, not `forward`), of those shown: the sidebar while it is,
/// the side panel while it is. From nowhere, or from an area not shown, the round starts at
/// what is written, as the native window's focus does.
fn next_area(from: Option<Area>, forward: bool, sidebar: bool, side: bool) -> Area {
    let shown: Vec<Area> = [Area::Sidebar, Area::Conversation, Area::Writing, Area::Side]
        .into_iter()
        .filter(|area| match area {
            Area::Sidebar => sidebar,
            Area::Side => side,
            Area::Conversation | Area::Writing => true,
        })
        .collect();
    let at = from
        .and_then(|from| shown.iter().position(|area| *area == from))
        .or_else(|| shown.iter().position(|area| *area == Area::Writing))
        .unwrap_or_default();
    let next = if forward {
        (at + 1) % shown.len()
    } else {
        (at + shown.len() - 1) % shown.len()
    };
    shown.get(next).copied().unwrap_or(Area::Writing)
}

/// Puts the focus where `area` starts, as the native window's areas take it: the sessions'
/// filter, the conversation, what the prompt asks for first or else the composer, the side
/// panel's field.
fn go_to(view: View, area: Area) {
    let mut page = view.page;
    match area {
        Area::Sidebar => crate::web::dom::focus(sidebar::FILTER),
        Area::Conversation => crate::web::dom::focus(transcript::LIST),
        Area::Writing if view.prompt.peek().kind != PromptKind::None => {
            page.write().focus_prompt += 1;
        }
        Area::Writing => page.write().focus_composer += 1,
        Area::Side => crate::web::dom::focus(panels::SIDE_ASK),
    }
}

/// Tells whether the window is narrow (a phone's), as it starts and as that changes; the
/// stylesheet's own breakpoint (`edualc.css`).
const NARROW: &str = r#"const query = matchMedia("(max-width: 640px)");
dioxus.send(query.matches);
query.addEventListener("change", (event) => dioxus.send(event.matches));
await new Promise(() => {});"#;

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
    } else if (event.ctrlKey && !event.shiftKey && (event.key === "k" || event.key === "K")) {
        event.preventDefault();
        dioxus.send("Filter");
    } else if (!event.ctrlKey && event.key === "F6") {
        event.preventDefault();
        const at = document.activeElement;
        const from = !at ? null
            : at.closest(".sidebar") ? "Sidebar"
            : at.closest(".transcript-area") ? "Conversation"
            : at.closest(".composer, .prompt") ? "Writing"
            : at.closest(".side-panel") ? "Side"
            : null;
        dioxus.send({ Area: { forward: !event.shiftKey, from } });
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
    // Narrow, the sidebar comes over the conversation: hidden as the window starts or narrows,
    // shown again as it widens.
    let mut narrow = use_signal(|| false);
    use_hook(move || {
        spawn(async move {
            let mut changes = document::eval(NARROW);
            while let Ok(now) = changes.recv::<bool>().await {
                if now != *narrow.peek() {
                    narrow.set(now);
                    sidebar_shown.set(!now);
                }
            }
        });
    });
    // Narrow, a session opened or a new one begun goes on without the sidebar over it.
    let opened = use_memo(move || {
        let page = view.page.read();
        (page.opened, page.focus_composer)
    });
    use_effect(move || {
        opened();
        if *narrow.peek() && *sidebar_shown.peek() {
            sidebar_shown.set(false);
        }
    });
    // The shortcuts the native window takes wherever the focus is, and the page's controls name:
    // the settings, the sessions shown or hidden or filtered, F6 between areas, and Esc to stop.
    // Ctrl+N and the zoom are the browser's own.
    use_hook(move || {
        spawn(async move {
            let mut keys = document::eval(SHORTCUTS);
            while let Ok(shortcut) = keys.recv::<Shortcut>().await {
                match shortcut {
                    Shortcut::Settings => settings.open(view),
                    Shortcut::Sessions => sidebar_shown.toggle(),
                    Shortcut::Stop => stop(view, settings),
                    Shortcut::Filter => {
                        sidebar_shown.set(true);
                        let mut page = view.page;
                        page.write().filter_wanted = true;
                    }
                    Shortcut::Area { forward, from } => {
                        let side = view.side.peek().view == SideView::Shown;
                        go_to(view, next_area(from, forward, *sidebar_shown.peek(), side));
                    }
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
                if narrow() {
                    div {
                        class: "scrim",
                        "aria-hidden": "true",
                        onclick: move |_| sidebar_shown.set(false),
                    }
                }
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

#[cfg(test)]
mod tests {
    use super::*;

    /// F6 goes round the areas shown, as in the native window; Shift+F6 the other way.
    #[test]
    fn f6_goes_round_the_areas_shown() {
        use Area::{Conversation, Side, Sidebar, Writing};
        assert_eq!(next_area(Some(Sidebar), true, true, false), Conversation);
        assert_eq!(next_area(Some(Writing), true, true, false), Sidebar);
        assert_eq!(next_area(Some(Writing), true, true, true), Side);
        assert_eq!(next_area(Some(Side), true, true, true), Sidebar);
        assert_eq!(next_area(Some(Writing), true, false, false), Conversation);
        assert_eq!(next_area(Some(Conversation), false, false, false), Writing);
        assert_eq!(next_area(Some(Sidebar), false, true, true), Side);
        assert_eq!(next_area(None, true, true, false), Sidebar);
        assert_eq!(next_area(Some(Side), true, true, false), Sidebar);
    }
}
