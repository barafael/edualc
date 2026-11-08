//! The window's parts, as the native window lays them out (`ui/app.slint`'s `AppWindow`): the
//! sidebar of sessions; the conversation's column (its header, the transcript, what Claude asks
//! and the bars over the composer, the composer); the side panel or a subagent followed; the
//! usage details and settings over the conversation.

use dioxus::prelude::*;
use edualc_core::present::model::{ComposerSize, PromptKind, SideView};

use super::View;

mod bars;
mod composer;
mod header;
mod panels;
mod prompt;
mod sidebar;
mod transcript;

/// The whole window.
#[component]
pub fn Window() -> Element {
    let view = use_context::<View>();
    let mut sidebar_shown = use_signal(|| true);
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
                sidebar::Sidebar { onhide: move |()| sidebar_shown.set(false) }
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
            }
            if peek.open {
                panels::PeekPanel {}
            } else if side.view == SideView::Shown {
                panels::SidePanel {}
            }
            panels::Usage {}
            panels::Settings {}
        }
    }
}

// Silences the import until the composer's sizes are used here.
#[allow(dead_code)]
const _SIZES: [ComposerSize; 1] = [ComposerSize::Fit];
