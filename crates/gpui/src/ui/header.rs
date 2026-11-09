//! Above the conversation (`ui/app.slint`'s `Header`): its title, the worktree it works in and
//! what that opens in, the side questions and the settings; and the bars over the composer
//! (`WatchBar`, `TrustBar`, `SignInBar`): a session running elsewhere, a folder to trust, a
//! sign-in to make.

use gpui::{
    AnyElement, Context, FontWeight, InteractiveElement as _, IntoElement, ParentElement as _,
    SharedString, StatefulInteractiveElement as _, Styled as _, Window, div, px,
};
use gpui_component::button::{Button, ButtonVariants as _};
use gpui_component::menu::{DropdownMenu as _, PopupMenuItem};

use super::widgets::{self, Tone, icon, icon_button, text_button};
use super::{Palette, Shell};
use crate::app::Action;
use crate::present::model::SideView;

/// What `claude` is signed in with, as the sign-in bar has it run.
const SIGN_IN_COMMAND: &str = "claude auth login";

pub(super) fn header(shell: &Shell, _window: &mut Window, cx: &Context<Shell>) -> AnyElement {
    let palette = shell.palette();
    let size = shell.page.text_size;
    let session = &shell.session;
    let mut bar = div()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(8.0))
        .px(px(12.0))
        .h(px((size * 3.1).max(46.0)))
        .flex_none()
        .border_b_1()
        .border_color(palette.divider);
    if !shell.sidebar_shown {
        bar = bar.child(icon_button(
            "show-sidebar",
            "panel-left-open",
            "Show the sessions (Ctrl+B)",
            &palette,
            size,
            cx.listener(|shell, _, _, cx| {
                shell.sidebar_shown = true;
                cx.notify();
            }),
        ));
    } else {
        bar = bar.child(icon_button(
            "hide-sidebar",
            "panel-left-close",
            "Hide the sessions (Ctrl+B)",
            &palette,
            size,
            cx.listener(|shell, _, _, cx| {
                shell.sidebar_shown = false;
                cx.notify();
            }),
        ));
    }
    if session.busy {
        bar = bar.child(super::sidebar::live_dot(
            crate::present::model::LiveState::Busy,
            &palette,
            size,
        ));
    }
    bar = bar.child(
        div()
            .flex_1()
            .min_w_0()
            .truncate()
            .font_weight(FontWeight::SEMIBOLD)
            .text_size(px(size * 1.1))
            .child(session.title.clone()),
    );
    if !session.worktree_name.is_empty() {
        bar = bar.child(worktree(shell));
    }
    let side = &shell.side;
    let actions = shell.actions.clone();
    let mut btw = div()
        .id("btw")
        .flex()
        .flex_row()
        .items_center()
        .gap(px(4.0))
        .px(px(8.0))
        .py(px(4.0))
        .rounded(px(6.0))
        .cursor_pointer()
        .hover(|style| style.bg(palette.hover()))
        .on_click(move |_, _, _| actions.send(Action::ToggleSide))
        .child(icon(
            "message-circle-dashed",
            size * 0.9,
            if side.view == SideView::Hidden {
                palette.text_secondary
            } else {
                palette.accent
            },
        ))
        .child(widgets::small("btw", palette.text_secondary));
    if side.view == SideView::Shown {
        btw = btw.bg(palette.selected());
    }
    if side.unseen {
        btw = btw.child(div().size(px(6.0)).rounded_full().bg(palette.accent));
    }
    bar.child(btw)
        .child(icon_button(
            "settings",
            "settings",
            "Settings (Ctrl+,)",
            &palette,
            size,
            cx.listener(|shell, _, _, cx| {
                shell.settings_open = !shell.settings_open;
                cx.notify();
            }),
        ))
        .into_any_element()
}

/// The worktree the session works in, and a menu of what opens it.
fn worktree(shell: &Shell) -> impl IntoElement {
    let session = &shell.session;
    let branch_differs =
        !session.worktree_branch.is_empty() && session.worktree_branch != session.worktree_name;
    let label = if branch_differs {
        format!("{} ({})", session.worktree_name, session.worktree_branch)
    } else {
        session.worktree_name.clone()
    };
    let openers = session.openers.clone();
    let actions = shell.actions.clone();
    Button::new("worktree")
        .ghost()
        .label(label)
        .icon(super::widgets::lucide("git-branch"))
        .dropdown_menu(move |mut menu, _, _| {
            menu = menu.label("Open the worktree in");
            for (index, opener) in openers.iter().enumerate() {
                let actions = actions.clone();
                menu = menu.item(
                    PopupMenuItem::new(opener.clone())
                        .on_click(move |_, _, _| actions.send(Action::OpenWorktree(index))),
                );
            }
            menu
        })
}

/// The bars over the composer, those that show.
pub(super) fn bars(shell: &Shell, _cx: &mut Context<Shell>) -> Vec<AnyElement> {
    let palette = shell.palette();
    let session = &shell.session;
    let mut bars = Vec::new();
    if session.watching {
        bars.push(watch_bar(shell, &palette));
    }
    if session.needs_trust && !session.watching {
        bars.push(trust_bar(shell, &palette));
    }
    if session.shows_sign_in && !session.watching {
        bars.push(sign_in_bar(shell, &palette));
    }
    bars
}

fn dock_bar(palette: &Palette, warned: bool) -> gpui::Div {
    div()
        .flex()
        .flex_col()
        .gap(px(8.0))
        .mx(px(16.0))
        .mb(px(8.0))
        .p(px(12.0))
        .rounded(px(12.0))
        .bg(palette.card)
        .border_1()
        .border_color(if warned {
            palette.warning
        } else {
            palette.divider
        })
}

fn actions_row() -> gpui::Div {
    div()
        .flex()
        .flex_row()
        .flex_wrap()
        .justify_end()
        .gap(px(8.0))
}

/// A session running in a terminal elsewhere: watched here, continued here as a copy, or
/// brought here.
fn watch_bar(shell: &Shell, palette: &Palette) -> AnyElement {
    let session = &shell.session;
    let continue_here = shell.actions.clone();
    let bring_here = shell.actions.clone();
    dock_bar(palette, false)
        .child(
            div()
                .flex()
                .flex_row()
                .gap(px(8.0))
                .items_center()
                .child(icon("eye", shell.page.text_size, palette.text_secondary))
                .child(div().flex_1().child(session.watch_text.clone())),
        )
        .child(
            actions_row()
                .child(text_button(
                    "continue-here",
                    "Continue here as a copy",
                    Tone::Plain,
                    true,
                    palette,
                    move |_, _, _| continue_here.send(Action::ContinueHere),
                ))
                .child(text_button(
                    "bring-here",
                    "Stop it there, continue here",
                    Tone::Primary,
                    true,
                    palette,
                    move |_, _, _| bring_here.send(Action::BringHere),
                )),
        )
        .into_any_element()
}

/// The folder's settings run as Claude Code starts: trusted, or left out.
fn trust_bar(shell: &Shell, palette: &Palette) -> AnyElement {
    let session = &shell.session;
    let without = shell.actions.clone();
    let trust = shell.actions.clone();
    let runs = if session.trust_runs.is_empty() {
        "Its .claude/settings.json, settings.local.json and .mcp.json run nothing and set no environment."
    } else {
        "Its .claude/settings.json, settings.local.json and .mcp.json would have Claude Code run or set:"
    };
    dock_bar(palette, true)
        .child(
            div()
                .flex()
                .flex_row()
                .gap(px(8.0))
                .child(icon("shield-alert", shell.page.text_size, palette.warning_text))
                .child(div().flex_1().child(format!(
                    "Do you trust the files in {}? Claude Code runs this folder's hooks and settings as soon as it starts here, before you send anything.",
                    session.project
                ))),
        )
        .child(widgets::small(runs, palette.text_secondary))
        .child(
            div()
                .flex()
                .flex_col()
                .font_family(shell.mono.clone())
                .text_sm()
                .children(session.trust_runs.iter().map(|line| div().child(line.clone()))),
        )
        .child(widgets::small(
            "Without its settings, Claude Code starts with only your own: none of this runs, and neither do any MCP servers or claude.ai connectors, yours included. You're asked again next time.",
            palette.text_tertiary,
        ))
        .child(
            actions_row()
                .child(text_button(
                    "without-settings",
                    "Start without its settings",
                    Tone::Plain,
                    true,
                    palette,
                    move |_, _, _| without.send(Action::StartWithoutFolderSettings),
                ))
                .child(text_button(
                    "trust",
                    "Trust this folder",
                    Tone::Primary,
                    true,
                    palette,
                    move |_, _, _| trust.send(Action::TrustFolder),
                )),
        )
        .into_any_element()
}

/// Claude Code isn't signed in: the command that signs it in, to run in a terminal.
fn sign_in_bar(shell: &Shell, palette: &Palette) -> AnyElement {
    let copy = shell.actions.clone();
    let dismiss = shell.actions.clone();
    let again = shell.actions.clone();
    dock_bar(palette, true)
        .child(
            div()
                .flex()
                .flex_row()
                .gap(px(8.0))
                .items_center()
                .child(icon("log-in", shell.page.text_size, palette.warning_text))
                .child(
                    div()
                        .font_weight(FontWeight::SEMIBOLD)
                        .child("Sign in to Claude Code"),
                ),
        )
        .child(widgets::small(
            "Run this in a terminal and follow its steps, then try again. Claude Code starts again with your sign-in, and a message that failed is sent again.",
            palette.text_secondary,
        ))
        .child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap(px(8.0))
                .child(
                    div()
                        .px(px(8.0))
                        .py(px(4.0))
                        .rounded(px(6.0))
                        .bg(palette.code)
                        .font_family(shell.mono.clone())
                        .child(SIGN_IN_COMMAND),
                )
                .child(icon_button(
                    SharedString::from("copy-sign-in"),
                    "copy",
                    "Copy the command",
                    palette,
                    shell.page.text_size * 0.8,
                    move |_, _, _| copy.send(Action::Copy(SIGN_IN_COMMAND.to_owned())),
                )),
        )
        .child(
            actions_row()
                .child(text_button(
                    "not-now",
                    "Not now",
                    Tone::Plain,
                    true,
                    palette,
                    move |_, _, _| dismiss.send(Action::DismissSignIn),
                ))
                .child(text_button(
                    "try-again",
                    "Try again",
                    Tone::Primary,
                    true,
                    palette,
                    move |_, _, _| again.send(Action::TrySignInAgain),
                )),
        )
        .into_any_element()
}
