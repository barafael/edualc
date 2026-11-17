//! The window's parts, as the native window lays them out (`ui/app.slint`'s `AppWindow`): the
//! bar across the top; under it the start page, a project's page, or a session: the
//! conversation's column (the transcript or what the session changed, what Claude asks and the
//! bars over the composer, the composer), the side panel or a subagent followed; the usage
//! details, the settings and a dialog over it all. From a session the switcher goes to any
//! other.

use dioxus::prelude::*;
use edualc_core::app::Action;
use edualc_core::present::model::{PlaceView, PromptKind, SideView};

use super::View;

mod bars;
mod changes;
mod composer;
mod header;
mod pages;
mod panels;
mod prompt;
mod sessions;
mod transcript;

/// A shortcut that works wherever the focus is, as the page's listener names it.
#[derive(Debug, Clone, Copy, serde::Deserialize)]
#[serde(deny_unknown_fields)]
enum Shortcut {
    /// Ctrl+,
    Settings,
    /// Esc, unless what has the focus took it.
    Stop,
    /// Ctrl+K: in a session, the switcher, shown or put away; on a page, its filter focused.
    Filter,
    /// Alt+Left or Alt+Up: back, from a session to its project, from a project to the projects.
    Back,
    /// F6 (`forward`) or Shift+F6, from the area last gone to or focused.
    Area { forward: bool },
    /// The focus went into this area (none: somewhere else): where F6 goes on from.
    Focus(Option<Area>),
}

/// The window's areas F6 goes round, in the native window's order (`ui/app.slint`'s
/// `next-area`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize)]
enum Area {
    Conversation,
    Writing,
    Side,
}

/// The area after `from` (or before, not `forward`), of those shown: the side panel while it
/// is. From nowhere, or from an area not shown, the round starts at what is written, as the
/// native window's focus does.
fn next_area(from: Option<Area>, forward: bool, side: bool) -> Area {
    let shown: Vec<Area> = [Area::Conversation, Area::Writing, Area::Side]
        .into_iter()
        .filter(|area| match area {
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

/// Puts the focus where `area` starts, as the native window's areas take it: the conversation
/// (what the session changed, while that is over it), what the prompt asks for first or else
/// the composer, the side panel's field.
fn go_to(view: View, area: Area) {
    let mut page = view.page;
    match area {
        Area::Conversation if view.changes.peek().open => crate::web::dom::focus(changes::PANEL),
        Area::Conversation => crate::web::dom::focus(transcript::LIST),
        Area::Writing if view.prompt.peek().kind != PromptKind::None => {
            page.write().focus_prompt += 1;
        }
        Area::Writing => page.write().focus_composer += 1,
        Area::Side => crate::web::dom::focus(panels::SIDE_ASK),
    }
}

/// Listens for the shortcuts on the whole page, and tells each; in place of the last window's
/// listener, if one was made before (the app reaching an edualcd again).
const SHORTCUTS: &str = r#"window.edualcShortcuts?.abort();
const listening = new AbortController();
window.edualcShortcuts = listening;
// On Apple's keyboards Option+Left and Option+Up move the caret in a text field: there they
// stay the field's.
const apple = /Mac|iPhone|iPad|iPod/.test(navigator.platform);
document.addEventListener("keydown", (event) => {
    if (event.metaKey || event.isComposing) {
        return;
    }
    if (event.altKey) {
        const typing = apple && event.target instanceof Element
            && event.target.closest("input, textarea, [contenteditable]");
        if (!event.ctrlKey && !event.shiftKey && !typing
            && (event.key === "ArrowLeft" || event.key === "ArrowUp")) {
            event.preventDefault();
            // Held down, it goes back once.
            if (!event.repeat) {
                dioxus.send("Back");
            }
        }
        return;
    }
    if (event.ctrlKey && event.key === ",") {
        event.preventDefault();
        dioxus.send("Settings");
    } else if (event.ctrlKey && !event.shiftKey && (event.key === "k" || event.key === "K")) {
        event.preventDefault();
        dioxus.send("Filter");
    } else if (!event.ctrlKey && event.key === "F6") {
        event.preventDefault();
        dioxus.send({ Area: { forward: !event.shiftKey } });
    } else if (!event.ctrlKey && !event.shiftKey && event.key === "Escape" && !event.defaultPrevented) {
        dioxus.send("Stop");
    }
}, { signal: listening.signal });
// Where the focus is, said as it goes there. F6 goes on from the last area it went to, which the
// focus isn't in yet when a second F6 comes before it has arrived.
document.addEventListener("focusin", (event) => {
    const at = event.target;
    const area = !(at instanceof Element) ? null
        : at.closest(".transcript-area, .changes") ? "Conversation"
        : at.closest(".composer, .prompt") ? "Writing"
        : at.closest(".side-panel") ? "Side"
        : null;
    dioxus.send({ Focus: area });
}, { signal: listening.signal });
await new Promise(() => {});"#;

/// Esc puts away what is over the window, the topmost first: the switcher, a dialog, then what
/// the session changed, unless a comment is being written there or something is over it;
/// otherwise it stops the turn while Claude works, asks nothing and nothing is written (with a
/// draft, Stop is beside it), and nothing is over the conversation, as in the native window.
/// Each of these takes an Esc from inside it itself: this is for one from elsewhere.
fn stop(
    view: View,
    settings: panels::SettingsOpen,
    mut switcher: Signal<bool>,
    mut dialog: Signal<Option<pages::Dialog>>,
) {
    if *switcher.peek() {
        switcher.set(false);
        return;
    }
    if dialog.peek().is_some() {
        dialog.set(None);
        return;
    }
    if view.nav.peek().view != PlaceView::Session {
        return;
    }
    let covered = view.usage.peek().open || *settings.0.peek();
    let changes = view.changes.peek();
    if changes.open {
        if !changes.editing_comment && !covered {
            view.act(Action::CloseChanges);
        }
        return;
    }
    let stops = view.session.peek().busy
        && view.prompt.peek().kind == PromptKind::None
        && view.draft.peek().is_empty()
        && !covered;
    if stops {
        view.act(Action::Interrupt);
    }
}

/// The whole window.
#[component]
pub fn Window() -> Element {
    let view = use_context::<View>();
    let settings = use_context_provider(|| panels::SettingsOpen(Signal::new(false)));
    // What is over a place: the switcher over a session, a dialog over any. Put away as the
    // window goes elsewhere.
    let mut switcher = use_context_provider(|| pages::SwitcherOpen(Signal::new(false))).0;
    let mut dialog = use_context_provider(|| pages::DialogOpen(Signal::new(None))).0;
    let place = use_memo(move || view.nav.read().view);
    use_effect(move || {
        place();
        switcher.set(false);
        dialog.set(None);
    });
    // The shortcuts the native window takes wherever the focus is, and the page's controls name:
    // the settings, the switcher or a page's filter, Back, F6 between areas, and Esc to stop.
    // Ctrl+N and the zoom are the browser's own.
    use_hook(move || {
        spawn(async move {
            let mut keys = document::eval(SHORTCUTS);
            // The area the focus is in, or F6 last went to.
            let mut current: Option<Area> = None;
            while let Ok(shortcut) = keys.recv::<Shortcut>().await {
                match shortcut {
                    Shortcut::Settings => settings.open(view),
                    Shortcut::Stop => stop(view, settings, switcher, dialog),
                    Shortcut::Filter if view.nav.peek().view == PlaceView::Session => {
                        pages::SwitcherOpen(switcher).toggle(view);
                    }
                    // A page has one area, which is also what Ctrl+K goes to: its filter,
                    // unless a dialog over it has the focus.
                    Shortcut::Filter => {
                        if dialog.peek().is_none() {
                            crate::web::dom::focus(pages::FILTER);
                        }
                    }
                    Shortcut::Back => {
                        if view.nav.peek().can_back {
                            view.act(Action::Back);
                        }
                    }
                    Shortcut::Focus(area) => current = area,
                    Shortcut::Area { .. } if view.nav.peek().view != PlaceView::Session => {
                        if dialog.peek().is_none() {
                            crate::web::dom::focus(pages::FILTER);
                        }
                    }
                    Shortcut::Area { forward } => {
                        let side = view.side.peek().view == SideView::Shown;
                        let area = next_area(current, forward, side);
                        // Before the focus moves: another F6 goes on from here.
                        current = Some(area);
                        go_to(view, area);
                    }
                }
            }
        });
    });
    rsx! {
        div { class: "layout",
            match place() {
                PlaceView::Start => rsx! {
                    Column { pages::StartPage {} }
                },
                PlaceView::Project => rsx! {
                    Column { pages::ProjectPage {} }
                },
                PlaceView::Session => rsx! {
                    Session {}
                },
            }
        }
    }
}

/// The window's column: the bar, a place under it, and what comes over it all, as the native
/// window lays them. `changes`: what the session changed is shown over the conversation.
#[component]
fn Column(children: Element, #[props(default)] changes: bool) -> Element {
    rsx! {
        div { class: if changes { "column showing-changes" } else { "column" },
            header::Header {}
            {children}
            panels::Usage {}
            panels::Settings {}
            pages::Dialogs {}
        }
    }
}

/// A session: its conversation, or what it changed over it, what Claude asks and the bars over
/// the composer, the composer; over them the switcher; beside them the side panel or a subagent
/// followed.
#[component]
fn Session() -> Element {
    let view = use_context::<View>();
    let switcher = use_context::<pages::SwitcherOpen>();
    let changes_open = use_memo(move || view.changes.read().open);
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
        Column { changes: changes_open(),
            // The conversation stays where it was read to, under what the session changed.
            div { class: "conversation",
                transcript::Transcript {}
                if changes_open() {
                    changes::ChangesPanel {}
                }
            }
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
            if (switcher.0)() {
                pages::Switcher { onclose: move |()| switcher.close(view) }
            }
        }
        if peek.open {
            panels::PeekPanel {}
        } else if side.view == SideView::Shown {
            panels::SidePanel {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// F6 goes round the areas shown, as in the native window; Shift+F6 the other way.
    #[test]
    fn f6_goes_round_the_areas_shown() {
        use Area::{Conversation, Side, Writing};
        assert_eq!(next_area(Some(Conversation), true, false), Writing);
        assert_eq!(next_area(Some(Writing), true, false), Conversation);
        assert_eq!(next_area(Some(Writing), true, true), Side);
        assert_eq!(next_area(Some(Side), true, true), Conversation);
        assert_eq!(next_area(Some(Conversation), false, false), Writing);
        assert_eq!(next_area(Some(Conversation), false, true), Side);
        assert_eq!(next_area(None, true, false), Conversation);
        assert_eq!(next_area(Some(Side), true, false), Conversation);
    }
}
