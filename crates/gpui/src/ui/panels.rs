//! The panels beside and over the conversation: side questions (`ui/side.slint`), a subagent
//! or command followed (`ui/peek.slint`), the usage details (`ui/usage.slint`) and the settings
//! (`ui/settings.slint`).

use gpui::{
    AnyElement, Context, Entity, FontWeight, InteractiveElement as _, IntoElement,
    ParentElement as _, SharedString, StatefulInteractiveElement as _, Styled as _, Window, div,
    px,
};
use gpui_component::input::{Input, InputState};
use gpui_component::scroll::ScrollableElement as _;

use super::widgets::{self, Tone, icon, icon_button, text_button};
use super::{Actions, Palette, Shell};
use crate::app::Action;
use crate::cjk::CjkLanguage;
use crate::ipc::SandboxAccess;
use crate::present::model::{HanLanguage, SendKey, SideState, StepKind};
use crate::settings;

/// A panel on the right of the conversation, `width` base sizes wide.
fn side_column(palette: &Palette, size: f32, width: f32) -> gpui::Div {
    div()
        .flex()
        .flex_col()
        .flex_none()
        .w(px((size * width).max(400.0)))
        .h_full()
        .bg(palette.surface_raised)
        .border_l_1()
        .border_color(palette.divider)
}

fn panel_head(title: impl Into<SharedString>, palette: &Palette, size: f32) -> gpui::Div {
    div()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(8.0))
        .px(px(12.0))
        .h(px((size * 3.1).max(46.0)))
        .flex_none()
        .border_b_1()
        .border_color(palette.divider)
        .child(
            div()
                .flex_1()
                .min_w_0()
                .truncate()
                .font_weight(FontWeight::SEMIBOLD)
                .child(title.into()),
        )
}

/// Questions on the side (`/btw`): asked of a copy of the conversation, answered without
/// touching it; an answer put in the conversation, or the side branched into a session.
pub(super) fn side_panel(shell: &Shell) -> AnyElement {
    let palette = shell.palette();
    let size = shell.page.text_size;
    let side = &shell.side;
    let (fold, dismiss) = (shell.actions.clone(), shell.actions.clone());
    let mut head = panel_head("Side questions", &palette, size);
    if side.can_branch {
        let branch = shell.actions.clone();
        head = head.child(icon_button(
            "branch-side",
            "git-branch",
            "Continue these in a new session",
            &palette,
            size,
            move |_, _, _| branch.send(Action::BranchSide),
        ));
    }
    if side.has_answers {
        let to_conversation = shell.actions.clone();
        head = head.child(icon_button(
            "side-to-conversation",
            "corner-down-left",
            "Put the answers in the conversation",
            &palette,
            size,
            move |_, _, _| to_conversation.send(Action::SideToConversation),
        ));
    }
    head = head
        .child(icon_button(
            "fold-side",
            "minus",
            "Fold away",
            &palette,
            size,
            move |_, _, _| {
                fold.send(Action::ToggleSide);
            },
        ))
        .child(icon_button(
            "dismiss-side",
            "x",
            "Close",
            &palette,
            size,
            move |_, _, _| {
                dismiss.send(Action::DismissSide);
            },
        ));
    let ink = widgets::Ink {
        palette: &palette,
        actions: &shell.actions,
        size,
    };
    let mut exchanges = div().flex().flex_col().gap(px(16.0)).p(px(12.0));
    for (at, exchange) in side.exchanges.iter().enumerate() {
        let copy = shell.actions.clone();
        let mut one = div().flex().flex_col().gap(px(6.0)).child(
            div().flex().flex_row().justify_end().child(
                div()
                    .max_w(gpui::relative(0.9))
                    .rounded(px(12.0))
                    .bg(palette.bubble)
                    .px(px(10.0))
                    .py(px(6.0))
                    .child(exchange.question.clone()),
            ),
        );
        one = match exchange.state {
            SideState::Pending => one.child(widgets::small("Thinking…", palette.text_tertiary)),
            SideState::Retrying => {
                one.child(widgets::small("Trying again…", palette.text_tertiary))
            }
            SideState::Failed => {
                one.child(widgets::small(exchange.error.clone(), palette.danger_text))
            }
            SideState::Answered => one
                .child(widgets::blocks(
                    &format!("side-{at}"),
                    &exchange.answer,
                    &ink,
                ))
                .child(icon_button(
                    SharedString::from(format!("copy-side-{at}")),
                    "copy",
                    "Copy the answer",
                    &palette,
                    size * 0.8,
                    move |_, _, _| copy.send(Action::CopySide(at)),
                )),
        };
        exchanges = exchanges.child(one);
    }
    let mut column = side_column(&palette, size, 26.0)
        .track_focus(&shell.areas.side)
        .child(head)
        .child(
            div()
                .id("side-exchanges")
                .flex_1()
                .min_h_0()
                .overflow_y_scrollbar()
                .child(exchanges),
        );
    if side.can_ask {
        column = column.child(div().p(px(12.0)).child(Input::new(&shell.fields.side)));
    }
    column.into_any_element()
}

/// A subagent or command followed as it runs: what it was asked, said and called, or its
/// output.
pub(super) fn peek_panel(shell: &Shell) -> AnyElement {
    let palette = shell.palette();
    let size = shell.page.text_size;
    let peek = &shell.peek;
    let close = shell.actions.clone();
    let mut head = panel_head(peek.title.clone(), &palette, size);
    if peek.running {
        head = head.child(super::sidebar::live_dot(
            crate::present::model::LiveState::Busy,
            &palette,
            size,
        ));
    }
    head = head.child(icon_button(
        "close-peek",
        "x",
        "Close",
        &palette,
        size,
        move |_, _, _| {
            close.send(Action::ClosePeek);
        },
    ));
    let mut body = div().flex().flex_col().gap(px(10.0)).p(px(12.0));
    if !peek.detail.is_empty() {
        body = body.child(widgets::small(peek.detail.clone(), palette.text_secondary));
    }
    for (at, step) in peek.steps.iter().enumerate() {
        let (name, ink) = match step.kind {
            StepKind::Asked => ("message-square", palette.text_secondary),
            StepKind::Said => ("bot", palette.claude),
            StepKind::Called => ("wrench", palette.text_tertiary),
        };
        body = body.child(
            div()
                .flex()
                .flex_col()
                .gap(px(4.0))
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap(px(6.0))
                        .child(icon(name, size * 0.9, ink))
                        .child(
                            div()
                                .font_weight(FontWeight::MEDIUM)
                                .truncate()
                                .child(step.title.clone()),
                        ),
                )
                .child(widgets::markdown(
                    SharedString::from(format!("peek-{at}")),
                    &step.text,
                    &shell.actions,
                )),
        );
    }
    if !peek.output.is_empty() {
        let output = crate::present::Styled::Plain(peek.output.join("\n"));
        body = body.child(
            widgets::CodeBox {
                id: SharedString::from("peek-output"),
                body: &output,
                color: palette.text,
                note: "",
                copy: None,
            }
            .draw(&widgets::Ink {
                palette: &palette,
                actions: &shell.actions,
                size,
            }),
        );
    }
    if !peek.note.is_empty() {
        body = body.child(widgets::small(peek.note.clone(), palette.text_tertiary));
    }
    side_column(&palette, size, 26.0)
        .child(head)
        .child(
            div()
                .id("peek-body")
                .flex_1()
                .min_h_0()
                .overflow_y_scrollbar()
                .child(body),
        )
        .into_any_element()
}

/// Over the conversation, centred, with the conversation dimmed behind.
fn over(
    palette: &Palette,
    size: f32,
    id: impl Into<gpui::ElementId>,
) -> (gpui::Div, gpui::Stateful<gpui::Div>) {
    let backdrop = div()
        .absolute()
        .top_0()
        .left_0()
        .size_full()
        .bg(palette.dim)
        .flex()
        .justify_center()
        .items_start()
        .pt(px(48.0));
    let card = div()
        .id(id)
        .flex()
        .flex_col()
        .gap(px(12.0))
        .w(px(size * 34.0))
        .max_h(gpui::relative(0.85))
        .p(px(16.0))
        .rounded(px(12.0))
        .bg(palette.surface_raised)
        .border_1()
        .border_color(palette.divider)
        .overflow_y_scroll();
    (backdrop, card)
}

/// How much of the plan and the context is used, section by section.
pub(super) fn usage_panel(shell: &Shell, _cx: &mut Context<Shell>) -> AnyElement {
    let palette = shell.palette();
    let size = shell.page.text_size;
    let usage = &shell.usage;
    let (close, refresh) = (shell.actions.clone(), shell.actions.clone());
    let (backdrop, mut card) = over(&palette, size, "usage-panel");
    card = card.child(
        div()
            .flex()
            .flex_row()
            .items_center()
            .child(
                div()
                    .flex_1()
                    .text_size(px(size * 1.2))
                    .font_weight(FontWeight::SEMIBOLD)
                    .child("Usage"),
            )
            .child(icon_button(
                "refresh-usage",
                "refresh-cw",
                "Refresh",
                &palette,
                size,
                move |_, _, _| {
                    refresh.send(Action::ShowUsage);
                },
            ))
            .child(icon_button(
                "close-usage",
                "x",
                "Close",
                &palette,
                size,
                move |_, _, _| {
                    close.send(Action::HideUsage);
                },
            )),
    );
    if usage.loading {
        card = card.child(widgets::small("Asking…", palette.text_tertiary));
    }
    if !usage.note.is_empty() {
        card = card.child(widgets::small(usage.note.clone(), palette.text_secondary));
    }
    card = card.children(usage.sections.iter().map(|section| {
        let mut part = div().flex().flex_col().gap(px(6.0)).child(
            div()
                .font_weight(FontWeight::SEMIBOLD)
                .child(section.title.clone()),
        );
        if !section.note.is_empty() {
            part = part.child(widgets::small(section.note.clone(), palette.text_tertiary));
        }
        part.children(section.rows.iter().map(|row| {
            let mut line = div().flex().flex_col().gap(px(2.0)).child(
                div()
                    .flex()
                    .flex_row()
                    .child(div().flex_1().child(row.label.clone()))
                    .child(
                        div()
                            .text_color(palette.text_secondary)
                            .child(row.value.clone()),
                    ),
            );
            if row.fraction >= 0.0 {
                line = line.child(
                    div().h(px(4.0)).rounded_full().bg(palette.hover()).child(
                        div()
                            .h_full()
                            .w(gpui::relative(row.fraction.clamp(0.0, 1.0)))
                            .rounded_full()
                            .bg(palette.fill(row.fraction)),
                    ),
                );
            }
            if !row.detail.is_empty() {
                line = line.child(widgets::small(row.detail.clone(), palette.text_tertiary));
            }
            line
        }))
    }));
    backdrop.child(card).into_any_element()
}

/// The settings: how a message is sent, the look, the sandbox and its folders, messages from
/// other sessions, the language of Han characters alone; and who is signed in.
pub(super) fn settings_panel(
    shell: &Shell,
    _window: &mut Window,
    cx: &Context<Shell>,
) -> AnyElement {
    let palette = shell.palette();
    let size = shell.page.text_size;
    let preferences = &shell.preferences;
    let session = &shell.session;
    let (backdrop, mut card) = over(&palette, size, "settings-panel");
    card = card.child(
        div()
            .flex()
            .flex_row()
            .items_center()
            .child(
                div()
                    .flex_1()
                    .text_size(px(size * 1.2))
                    .font_weight(FontWeight::SEMIBOLD)
                    .child("Settings"),
            )
            .child(icon_button(
                "close-settings",
                "x",
                "Close",
                &palette,
                size,
                cx.listener(|shell, _, _, cx| {
                    shell.settings_open = false;
                    cx.notify();
                }),
            )),
    );
    card = card
        .child(heading("Send with"))
        .child(
            div()
                .flex()
                .flex_row()
                .gap(px(8.0))
                .child(choice(
                    "send-enter",
                    "Enter",
                    preferences.send_key == SendKey::Enter,
                    &palette,
                    &shell.actions,
                    || Action::SendWith(settings::SendKey::Enter),
                ))
                .child(choice(
                    "send-ctrl-enter",
                    "Ctrl+Enter",
                    preferences.send_key == SendKey::CtrlEnter,
                    &palette,
                    &shell.actions,
                    || Action::SendWith(settings::SendKey::CtrlEnter),
                )),
        )
        .child(heading("Look"))
        .child(choice(
            "switch-look",
            "Switch between Gruvbox and plain",
            false,
            &palette,
            &shell.actions,
            || Action::SwitchLook,
        ))
        .child(heading("Sandbox"))
        .child(check(
            "sandbox-on",
            "Run Claude Code in the sandbox",
            session.sandbox_on,
            &palette,
            &shell.actions,
            || Action::ToggleSandbox,
        ))
        .child(check(
            "sandbox-desktop",
            "Let it reach the desktop (its windows, notifications)",
            session.sandbox_desktop,
            &palette,
            &shell.actions,
            || Action::ToggleSandboxDesktop,
        ))
        .child(check(
            "sandbox-hardware",
            "Let it reach hardware (GPUs, cameras, USB)",
            session.sandbox_hardware,
            &palette,
            &shell.actions,
            || Action::ToggleSandboxHardware,
        ))
        .child(widgets::small(
            session.sandbox_detail.clone(),
            palette.text_tertiary,
        ))
        .child(folders(
            "Folders it may read",
            &preferences.sandbox_read,
            &shell.fields.sandbox_read,
            SandboxAccess::Read,
            &palette,
            &shell.actions,
            size,
        ))
        .child(folders(
            "Folders it may write",
            &preferences.sandbox_write,
            &shell.fields.sandbox_write,
            SandboxAccess::Write,
            &palette,
            &shell.actions,
            size,
        ))
        .child(heading("Messages between sessions"))
        .child(check(
            "messages-unasked",
            "Let other sessions message this one without asking",
            preferences.messages_unasked,
            &palette,
            &shell.actions,
            || Action::ToggleMessagesUnasked,
        ))
        .child(heading("Han characters alone are"))
        .child(
            div().flex().flex_row().flex_wrap().gap(px(8.0)).children(
                [
                    (HanLanguage::Japanese, CjkLanguage::Japanese, "Japanese"),
                    (
                        HanLanguage::SimplifiedChinese,
                        CjkLanguage::SimplifiedChinese,
                        "Simplified Chinese",
                    ),
                    (
                        HanLanguage::TraditionalChinese,
                        CjkLanguage::TraditionalChinese,
                        "Traditional Chinese",
                    ),
                    (HanLanguage::Korean, CjkLanguage::Korean, "Korean"),
                ]
                .into_iter()
                .map(|(shown, language, label)| {
                    choice(
                        SharedString::from(format!("han-{label}")),
                        label,
                        preferences.han_language == shown,
                        &palette,
                        &shell.actions,
                        move || Action::HanLanguage(language),
                    )
                }),
            ),
        )
        .child(heading("Account"));
    for (label, value) in [
        ("Email", &session.email),
        ("Organization", &session.organization),
        ("Provider", &session.provider),
        ("Claude Code", &session.version),
        ("Spent", &session.cost),
    ] {
        if !value.is_empty() {
            card = card.child(
                div()
                    .flex()
                    .flex_row()
                    .gap(px(8.0))
                    .child(
                        div()
                            .w(px(120.0))
                            .text_color(palette.text_tertiary)
                            .child(label),
                    )
                    .child(div().flex_1().truncate().child(value.clone())),
            );
        }
    }
    backdrop.child(card).into_any_element()
}

fn heading(text: impl Into<SharedString>) -> impl IntoElement {
    div()
        .pt(px(6.0))
        .font_weight(FontWeight::SEMIBOLD)
        .child(text.into())
}

/// One of a few choices: chosen, or chosen by a press, which sends `action`.
fn choice(
    id: impl Into<gpui::ElementId>,
    label: impl Into<SharedString>,
    chosen: bool,
    palette: &Palette,
    actions: &Actions,
    action: impl Fn() -> Action + 'static,
) -> impl IntoElement {
    let actions = actions.clone();
    text_button(
        id,
        label,
        if chosen { Tone::Primary } else { Tone::Plain },
        true,
        palette,
        move |_, _, _| actions.send(action()),
    )
}

/// A box ticked or not, which sends `action` as it is pressed.
fn check(
    id: impl Into<gpui::ElementId>,
    label: impl Into<SharedString>,
    checked: bool,
    palette: &Palette,
    actions: &Actions,
    action: fn() -> Action,
) -> impl IntoElement {
    let actions = actions.clone();
    div()
        .id(id)
        .flex()
        .flex_row()
        .items_center()
        .gap(px(8.0))
        .cursor_pointer()
        .on_click(move |_, _, _| actions.send(action()))
        .child(icon(
            if checked { "square-check" } else { "square" },
            16.0,
            if checked {
                palette.accent
            } else {
                palette.text_tertiary
            },
        ))
        .child(label.into())
}

/// The folders the sandbox opens for `access`, each to forget, and a field to add one.
fn folders(
    title: impl Into<SharedString>,
    folders: &[String],
    field: &Entity<InputState>,
    access: SandboxAccess,
    palette: &Palette,
    actions: &Actions,
    size: f32,
) -> impl IntoElement {
    let add = actions.clone();
    let typed = field.clone();
    div()
        .flex()
        .flex_col()
        .gap(px(4.0))
        .child(widgets::small(title, palette.text_secondary))
        .children(folders.iter().enumerate().map(|(at, folder)| {
            let forget = actions.clone();
            let gone = folder.clone();
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap(px(6.0))
                .child(
                    div()
                        .flex_1()
                        .truncate()
                        .font_family("monospace")
                        .text_sm()
                        .child(folder.clone()),
                )
                .child(icon_button(
                    SharedString::from(format!("forget-{access:?}-{at}")),
                    "x",
                    "Forget this folder",
                    palette,
                    size * 0.7,
                    move |_, _, _| {
                        forget.send(Action::ForgetSandboxFolder {
                            access,
                            folder: gone.clone(),
                        });
                    },
                ))
        }))
        .child(
            div()
                .flex()
                .flex_row()
                .gap(px(6.0))
                .child(div().flex_1().child(Input::new(field)))
                .child(text_button(
                    SharedString::from(format!("add-{access:?}")),
                    "Add",
                    Tone::Plain,
                    true,
                    palette,
                    move |_, window, cx| {
                        let text = typed.read(cx).value().to_string();
                        if !text.trim().is_empty() {
                            add.send(Action::AddSandboxFolder {
                                access,
                                typed: text,
                            });
                            typed.update(cx, |state, cx| state.set_value("", window, cx));
                        }
                    },
                )),
        )
}
