//! Above the conversation (`ui/app.slint`'s `Header`): its title, the worktree it works in and
//! what that opens in, what keeps Claude Code in while the sidebar is hidden, and the side
//! questions.

use dioxus::prelude::*;
use edualc_core::app::Action;
use edualc_core::present::model::SideView;

use crate::web::View;
use crate::web::widgets::{Icon, IconButton, Lucide};

/// The worktree's button's id, which its menu gives the focus back to.
const WORKTREE: &str = "worktree-button";

/// The header: a line under it sets it apart from the conversation.
#[component]
pub fn Header(sidebar_shown: bool, ontoggle: EventHandler<()>) -> Element {
    let view = use_context::<View>();
    let session = view.session.read();
    let side = view.side.read();
    let side_shown = side.view == SideView::Shown;
    let side_lit = side.view != SideView::Hidden;
    rsx! {
        document::Stylesheet { href: asset!("/assets/header.css") }
        header { class: if sidebar_shown { "header" } else { "header sidebar-hidden" },
            // Hidden, the sidebar is brought back from here; shown, it has its own button.
            if !sidebar_shown {
                IconButton {
                    icon: Lucide::PanelLeftOpen,
                    label: "Show the sessions (Ctrl+B)",
                    onclick: move |_| ontoggle.call(()),
                }
            }
            h1 { class: "header-title", "{session.title}" }
            if !session.worktree_name.is_empty() {
                Worktree {}
            }
            // In the sidebar's foot while that shows; here while it is hidden.
            if !sidebar_shown {
                super::sidebar::SandboxChip {}
            }
            // Side questions: labelled, as a bare question mark reads as help.
            button {
                class: if side_shown { "header-btw selected" } else { "header-btw" },
                class: if side_lit { "lit" },
                "aria-label": if side_shown { "Fold the side questions away" } else { "Side questions (/btw)" },
                onclick: move |_| view.act(Action::ToggleSide),
                Icon { icon: Lucide::MessageCircleDashed, class: "small" }
                span { class: "header-btw-text", "btw" }
                // An answer came while folded away.
                if side.unseen {
                    span { class: "header-unseen", "aria-hidden": "true" }
                }
            }
        }
    }
}

/// The git worktree the session works in, and its branch where that's named otherwise; its
/// menu opens it here or elsewhere.
#[component]
fn Worktree() -> Element {
    let view = use_context::<View>();
    let mut open = use_signal(|| false);
    let session = view.session.read();
    let branch_differs =
        !session.worktree_branch.is_empty() && session.worktree_branch != session.worktree_name;
    let mut close = move || {
        open.set(false);
        dom::focus(WORKTREE);
    };
    rsx! {
        div { class: "worktree",
            button {
                id: WORKTREE,
                class: "worktree-button",
                "aria-haspopup": "menu",
                "aria-expanded": "{open}",
                onclick: move |_| open.toggle(),
                onkeydown: move |event| {
                    if event.key() == Key::ArrowDown && !open() {
                        event.prevent_default();
                        open.set(true);
                    }
                },
                Icon { icon: Lucide::GitBranch, class: "small worktree-icon" }
                span { class: "worktree-name", "{session.worktree_name}" }
                if branch_differs {
                    span { class: "caption", "{session.worktree_branch}" }
                }
                Icon { icon: Lucide::ChevronDown, class: "small" }
            }
            if open() {
                div { class: "menu-backdrop", onclick: move |_| open.set(false) }
                div {
                    class: "worktree-menu",
                    role: "menu",
                    "aria-label": "Open the worktree in",
                    // Up and Down go through the openers; Esc closes the menu.
                    onkeydown: move |event| {
                        let key = event.key();
                        if key == Key::ArrowDown || key == Key::ArrowUp {
                            event.prevent_default();
                            dom::focus_next(key == Key::ArrowDown);
                        } else if key == Key::Escape {
                            event.prevent_default();
                            event.stop_propagation();
                            close();
                        } else if key == Key::Tab {
                            open.set(false);
                        }
                    },
                    div { class: "caption worktree-menu-heading", "aria-hidden": "true",
                        "Open the worktree in"
                    }
                    for (index, opener) in session.openers.iter().enumerate() {
                        button {
                            key: "{index}",
                            class: "worktree-opener",
                            role: "menuitem",
                            tabindex: "-1",
                            onmounted: move |event| async move {
                                if index == 0 && event.set_focus(true).await.is_err() {
                                    tracing::debug!("the worktree's menu took no focus");
                                }
                            },
                            onclick: move |_| {
                                view.act(Action::OpenWorktree(index));
                                close();
                            },
                            "{opener}"
                        }
                    }
                }
            }
        }
    }
}

/// The page's own parts the header reaches: the focus, in its menu.
#[cfg(feature = "web")]
mod dom {
    use web_sys::HtmlElement;
    use web_sys::wasm_bindgen::JsCast as _;

    fn document() -> Option<web_sys::Document> {
        web_sys::window()?.document()
    }

    fn take_focus(element: &HtmlElement) {
        if element.focus().is_err() {
            tracing::debug!("the header's control took no focus");
        }
    }

    /// Focuses the element with this id.
    pub fn focus(id: &str) {
        if let Some(element) = document()
            .and_then(|document| document.get_element_by_id(id))
            .and_then(|element| element.dyn_into::<HtmlElement>().ok())
        {
            take_focus(&element);
        }
    }

    /// Moves the focus to the next of the menu's items (`forward`) or the one before, staying
    /// at the ends.
    pub fn focus_next(forward: bool) {
        let Some(at) = document().and_then(|document| document.active_element()) else {
            return;
        };
        let next = if forward {
            at.next_element_sibling()
        } else {
            at.previous_element_sibling()
        };
        if let Some(element) = next
            .filter(|element| element.get_attribute("role").as_deref() == Some("menuitem"))
            .and_then(|element| element.dyn_into::<HtmlElement>().ok())
        {
            take_focus(&element);
        }
    }
}

/// Rendered on the server, the page has no focus to move yet.
#[cfg(not(feature = "web"))]
mod dom {
    pub fn focus(_id: &str) {}

    pub fn focus_next(_forward: bool) {}
}
