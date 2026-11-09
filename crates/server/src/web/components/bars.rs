//! The bars over the composer that ask something of the session (`ui/app.slint`): a session
//! another process runs, watched; a folder not trusted yet; signing in to Claude Code.

use dioxus::prelude::*;
use edualc_core::app::Action;

use crate::web::View;
use crate::web::widgets::{Button, Icon, IconButton, Lucide, take_focus};

/// What signs in to Claude Code, run in a terminal.
const SIGN_IN_COMMAND: &str = "claude auth login";

/// In place of the composer while a session another process runs is shown. Bringing it here
/// stops it in its window, so it asks first.
#[component]
pub fn WatchBar() -> Element {
    let view = use_context::<View>();
    let mut confirming = use_signal(|| false);
    // Asked and called off: the focus goes back to what asked.
    let mut called_off = use_signal(|| false);
    let session = view.session.read();
    rsx! {
        document::Stylesheet { href: asset!("/assets/bars.css") }
        div { class: if confirming() { "dock-bar warned" } else { "dock-bar" },
            div { class: "dock-bar-heading",
                Icon { icon: Lucide::Eye }
                if confirming() {
                    div { class: "dock-bar-text",
                        "Stop it in its window and continue it here? If it is working, its current step is interrupted."
                    }
                } else {
                    div { class: "dock-bar-text secondary", "{session.watch_text}" }
                }
            }
            if confirming() {
                div { class: "dock-bar-actions",
                    button {
                        class: "button",
                        onmounted: take_focus,
                        onclick: move |_| {
                            called_off.set(true);
                            confirming.set(false);
                        },
                        "Cancel"
                    }
                    Button {
                        primary: true,
                        onclick: move |_| {
                            confirming.set(false);
                            view.act(Action::BringHere);
                        },
                        "Stop it there, continue here"
                    }
                }
            } else {
                div { class: "dock-bar-actions",
                    Button {
                        icon: Lucide::Copy,
                        onclick: move |_| view.act(Action::ContinueHere),
                        "Continue here as a copy"
                    }
                    button {
                        class: "button primary",
                        onmounted: move |mounted| async move {
                            if called_off() {
                                called_off.set(false);
                                take_focus(mounted).await;
                            }
                        },
                        onclick: move |_| confirming.set(true),
                        Icon { icon: Lucide::ArrowDownToLine }
                        "Bring it here"
                    }
                }
            }
        }
    }
}

/// Before Claude Code starts in a folder not trusted yet: what it would run there.
#[component]
pub fn TrustBar() -> Element {
    let view = use_context::<View>();
    let session = view.session.read();
    let runs = !session.trust_runs.is_empty();
    rsx! {
        document::Stylesheet { href: asset!("/assets/bars.css") }
        div { class: "dock-bar warned",
            div { class: "dock-bar-heading",
                Icon { icon: Lucide::ShieldAlert, class: "warning" }
                div { class: "dock-bar-text",
                    "Do you trust the files in {session.project}? Claude Code runs this folder's hooks and settings as soon as it starts here, before you send anything."
                }
            }
            // What it would run, as read from its settings and .mcp.json (plain text, as
            // written there); a long list scrolls.
            div { class: "dock-bar-note",
                if runs {
                    "Its .claude/settings.json, settings.local.json and .mcp.json would have Claude Code run or set:"
                } else {
                    "Its .claude/settings.json, settings.local.json and .mcp.json run nothing and set no environment."
                }
            }
            if runs {
                div { class: "dock-bar-runs",
                    for (index, line) in session.trust_runs.iter().enumerate() {
                        div { key: "{index}", "{line}" }
                    }
                }
            }
            // As the terminal's dialog lets one go on without them; asked again at the next start.
            div { class: "dock-bar-note",
                "Without its settings, Claude Code starts with only your own: none of this runs, and neither do any MCP servers or claude.ai connectors, yours included. You're asked again next time."
            }
            div { class: "dock-bar-actions",
                Button {
                    onclick: move |_| {
                        let mut page = view.page;
                        page.write().projects_asked += 1;
                    },
                    "Open another folder"
                }
                // The focus comes here as the question shows, never to "Trust this folder": a
                // stray Enter starts Claude Code with none of the folder's settings.
                button {
                    class: "button",
                    onmounted: take_focus,
                    onclick: move |_| view.act(Action::StartWithoutFolderSettings),
                    "Start without its settings"
                }
                Button {
                    primary: true,
                    onclick: move |_| view.act(Action::TrustFolder),
                    "Trust this folder"
                }
            }
        }
    }
}

/// How to sign in: Claude Code behind a pipe can't, though it tells the user to run `/login`. It
/// reads the sign-in as it starts, so trying again starts it again.
#[component]
pub fn SignInBar() -> Element {
    let view = use_context::<View>();
    rsx! {
        document::Stylesheet { href: asset!("/assets/bars.css") }
        div { class: "dock-bar warned",
            div { class: "dock-bar-heading",
                Icon { icon: Lucide::LogIn, class: "warning" }
                div { class: "dock-bar-text strong", "Sign in to Claude Code" }
            }
            div { class: "dock-bar-note",
                "Run this in a terminal and follow its steps, then try again. Claude Code starts again with your sign-in, and a message that failed is sent again."
            }
            div { class: "dock-bar-command",
                code { "{SIGN_IN_COMMAND}" }
                IconButton {
                    icon: Lucide::Copy,
                    label: "Copy the command".to_owned(),
                    onclick: move |_| view.act(Action::Copy(SIGN_IN_COMMAND.to_owned())),
                }
            }
            div { class: "dock-bar-actions",
                Button {
                    onclick: move |_| view.act(Action::DismissSignIn),
                    "Not now"
                }
                Button {
                    primary: true,
                    onclick: move |_| view.act(Action::TrySignInAgain),
                    "Try again"
                }
            }
        }
    }
}
