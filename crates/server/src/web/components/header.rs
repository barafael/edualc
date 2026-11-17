//! The bar across the top (`ui/topbar.slint`): where the window is (the projects, a project, a
//! session) and the way back, the sessions that wait for you, and, in a session, the worktree
//! it works in and what that opens in, what it changed, and the side questions.

use dioxus::prelude::*;
use edualc_core::app::Action;
use edualc_core::present::Look;
use edualc_core::present::model::{PlaceView, SideView};

use crate::web::View;
use crate::web::widgets::{Icon, IconButton, Lucide, level, percent};

/// The worktree's button's id, which its menu gives the focus back to.
const WORKTREE: &str = "worktree-button";

/// The heron's drawings: standing, and the frames of what it does now and then.
const HERON: Asset = asset!("/assets/heron/heron.svg");
const HERON_BACK: Asset = asset!("/assets/heron/heron-looking-back.svg");
const HERON_PREEN_1: Asset = asset!("/assets/heron/heron-preening-1.svg");
const HERON_PREEN_2: Asset = asset!("/assets/heron/heron-preening-2.svg");
const HERON_HALF: Asset = asset!("/assets/heron/heron-flapping-half.svg");
const HERON_UP: Asset = asset!("/assets/heron/heron-flapping-up.svg");

/// The bar: a line under it sets it apart from what is under it.
#[component]
pub fn Header() -> Element {
    let view = use_context::<View>();
    let settings = use_context::<super::panels::SettingsOpen>();
    let switcher = use_context::<super::pages::SwitcherOpen>();
    // What a narrow window keeps behind one button: shown as it is pressed.
    let mut more = use_signal(|| false);
    // Of all the page holds, only these: its other changes don't draw the bar again.
    let unencrypted = use_memo(move || view.page.read().unencrypted);
    let look = use_memo(move || view.page.read().look);
    let nav = view.nav.read();
    let session = view.session.read();
    let side = view.side.read();
    let changes = view.changes.read();
    let side_shown = side.view == SideView::Shown;
    let side_lit = side.view != SideView::Hidden;
    let in_session = nav.view == PlaceView::Session;
    let title = if nav.session_title.is_empty() {
        session.title.clone()
    } else {
        nav.session_title.clone()
    };
    let project = nav.project_folder.clone();
    let needs = match nav.needs_you {
        1 => "1 needs you".to_owned(),
        count => format!("{count} need you"),
    };
    let needs_label = match nav.needs_you {
        1 => "1 session needs you".to_owned(),
        count => format!("{count} sessions need you"),
    };
    let changes_label = if changes.open {
        "Back to the conversation".to_owned()
    } else if changes.last_turn.is_empty() {
        "Changes".to_owned()
    } else {
        format!("Changes: the last turn changed {}", changes.last_turn)
    };
    // The five hours' limit used, once it is known.
    let usage = if session.limit_five_hour >= 0.0 {
        format!("5h {}%", percent(session.limit_five_hour))
    } else {
        "Usage".to_owned()
    };
    let heron = format!(
        "--heron: url('{HERON}'); --heron-back: url('{HERON_BACK}'); --heron-preen-1: url('{HERON_PREEN_1}'); --heron-preen-2: url('{HERON_PREEN_2}'); --heron-half: url('{HERON_HALF}'); --heron-up: url('{HERON_UP}');"
    );
    rsx! {
        header {
            class: "header",
            // The narrow window's menu is put away from wherever in the bar the focus is.
            onkeydown: move |event: KeyboardEvent| {
                if event.key() == Key::Escape && *more.peek() {
                    event.prevent_default();
                    event.stop_propagation();
                    more.set(false);
                }
            },
            if nav.can_back {
                IconButton {
                    icon: Lucide::ArrowLeft,
                    label: "Back (Alt+Left)",
                    onclick: move |_| view.act(Action::Back),
                }
            }
            // edualc's heron, at the size of the grid it is drawn on.
            span { class: "heron-mark", "aria-hidden": "true", style: heron }
            // Where the window is, each place before it a way back; narrow, only the place.
            nav { class: "crumbs", "aria-label": "Where the window is",
                if nav.view != PlaceView::Start {
                    button {
                        class: "crumb",
                        r#type: "button",
                        onclick: move |_| view.act(Action::OpenStart),
                        "Projects"
                    }
                    span { class: "crumb-rule", "aria-hidden": "true", "/" }
                }
                if in_session {
                    button {
                        class: "crumb trimmed",
                        r#type: "button",
                        "aria-label": "Project {nav.project_name}",
                        onclick: move |_| view.act(Action::EnterProject(project.clone().into())),
                        "{nav.project_name}"
                    }
                    span { class: "crumb-rule", "aria-hidden": "true", "/" }
                }
                h1 { class: "header-title",
                    match nav.view {
                        PlaceView::Start => "Projects".to_owned(),
                        PlaceView::Project => nav.project_name.clone(),
                        PlaceView::Session => title,
                    }
                }
            }
            if nav.needs_you > 0 {
                button {
                    class: "header-needs",
                    r#type: "button",
                    "aria-label": "{needs_label}",
                    title: "Go to a session that waits for you",
                    onclick: move |_| view.act(Action::NextWaiting),
                    Icon { icon: Lucide::Hand, class: "small" }
                    span { class: "header-needs-text", "{needs}" }
                }
            }
            if in_session && !session.worktree_name.is_empty() {
                Worktree {}
            }
            // A demo's line is plain: what is typed crosses the network as it is.
            if unencrypted() {
                span {
                    class: "header-unencrypted",
                    role: "status",
                    title: "A demo's line isn't encrypted: whoever is on the network between can read and change what goes over it",
                    Icon { icon: Lucide::ShieldExclamationPoint, class: "small" }
                    span { class: "header-btw-text", "Unencrypted" }
                }
            }
            if in_session {
                // Any other session, of any project: where a touch screen has no Ctrl+K.
                IconButton {
                    icon: Lucide::ArrowLeftRight,
                    label: "Switch session (Ctrl+K)",
                    pressed: (switcher.0)(),
                    onclick: move |_| switcher.toggle(view),
                }
                button {
                    class: if changes.open { "header-btw selected" } else { "header-btw" },
                    class: if !changes.last_turn.is_empty() { "lit" },
                    r#type: "button",
                    "aria-label": "{changes_label}",
                    "aria-pressed": "{changes.open}",
                    onclick: move |_| {
                        let open = view.changes.peek().open;
                        view.act(if open { Action::CloseChanges } else { Action::ShowChanges });
                    },
                    Icon { icon: Lucide::FileDiff, class: "small" }
                    span { class: "header-btw-text",
                        if changes.last_turn.is_empty() {
                            "Changes"
                        } else {
                            "{changes.last_turn}"
                        }
                    }
                }
                // Side questions: labelled, as a bare question mark reads as help.
                button {
                    class: if side_shown { "header-btw selected" } else { "header-btw" },
                    class: if side_lit { "lit" },
                    r#type: "button",
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
            // Wide, in the bar; narrow, a menu behind the button before them, put away by Esc, a
            // press beside it, or by what it does.
            IconButton {
                icon: Lucide::Ellipsis,
                class: "header-more-button",
                label: "More",
                pressed: more(),
                onclick: move |_| more.toggle(),
            }
            if more() {
                div {
                    class: "header-more-backdrop",
                    onclick: move |_| more.set(false),
                }
            }
            div {
                class: if more() { "header-more open" } else { "header-more" },
                onclick: move |_| more.set(false),
                if in_session {
                    IconButton {
                        icon: Lucide::SquarePen,
                        label: "New session",
                        onclick: move |_| view.new_session(),
                    }
                }
                if in_session || session.limit_five_hour >= 0.0 {
                    button {
                        class: "header-btw {level(session.limit_five_hour.max(0.0))}",
                        r#type: "button",
                        "aria-label": "Usage details",
                        onclick: move |_| view.act(Action::ShowUsage),
                        Icon { icon: Lucide::Gauge, class: "small" }
                        span { "{usage}" }
                    }
                }
                IconButton {
                    icon: Lucide::Palette,
                    label: if look() == Look::Gruvbox { "Plain colours" } else { "Gruvbox colours" },
                    onclick: move |_| view.act(Action::SwitchLook),
                }
                IconButton {
                    icon: Lucide::Settings,
                    label: "Settings (Ctrl+,)",
                    onclick: move |_| settings.open(view),
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
        crate::web::dom::focus(WORKTREE);
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
                            crate::web::dom::focus_next(key == Key::ArrowDown);
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
