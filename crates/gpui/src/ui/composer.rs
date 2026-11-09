//! Where the user writes (`ui/app.slint`'s `EditArea`): the draft, the images going with it, the
//! commands its `/` matches; under it the permission mode and the model, what runs, how full
//! the context is, and Send (or Stop, while Claude works).

use gpui::{
    AnyElement, Context, InteractiveElement as _, IntoElement, ObjectFit, ParentElement as _,
    SharedString, StatefulInteractiveElement as _, Styled as _, StyledImage as _, Window, div, img,
    px,
};
use gpui_component::button::{Button, ButtonVariants as _};
use gpui_component::input::Textarea;
use gpui_component::menu::{DropdownMenu as _, PopupMenuItem};

use super::widgets::{self, icon, icon_button};
use super::{Palette, Shell};
use crate::app::Action;
use crate::external::wire::TaskId;
use crate::present::model::{ComposerSize, MenuItem, PermissionKind, SendKey};

/// The commands the draft's `/` matches: picked with Tab or a click, run with Enter.
pub(super) fn command_list(shell: &Shell, _cx: &mut Context<Shell>) -> AnyElement {
    let palette = shell.palette();
    let chosen = shell
        .command_index
        .min(shell.composer.commands.len().saturating_sub(1));
    div()
        .flex()
        .flex_col()
        .mx(px(16.0))
        .mb(px(8.0))
        .p(px(4.0))
        .rounded(px(8.0))
        .bg(palette.surface_raised)
        .border_1()
        .border_color(palette.divider)
        .children(
            shell
                .composer
                .commands
                .iter()
                .enumerate()
                .map(|(at, command)| {
                    let actions = shell.actions.clone();
                    let mut row = div()
                        .id(SharedString::from(format!("command-{at}")))
                        .flex()
                        .flex_row()
                        .gap(px(8.0))
                        .px(px(8.0))
                        .py(px(4.0))
                        .rounded(px(6.0))
                        .cursor_pointer()
                        .hover(|style| style.bg(palette.hover()))
                        .on_click(move |_, _, _| actions.send(Action::PickCommand(at)))
                        .child(
                            div()
                                .font_family(shell.mono.clone())
                                .text_color(palette.text)
                                .child(format!("/{}", command.name)),
                        )
                        .child(widgets::small(command.hint.clone(), palette.text_tertiary))
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .truncate()
                                .text_sm()
                                .text_color(palette.text_secondary)
                                .child(command.description.clone()),
                        );
                    if at == chosen {
                        row = row.bg(palette.selected());
                    }
                    row
                }),
        )
        .into_any_element()
}

/// The composer; `stretched`, over the column (see `resize::over_column`), its text box as tall
/// as the room it has.
pub(super) fn composer(
    shell: &Shell,
    stretched: bool,
    _window: &mut Window,
    cx: &Context<Shell>,
) -> AnyElement {
    let palette = shell.palette();
    let size = shell.page.text_size;
    let composer = &shell.composer;
    let mut field = div()
        .relative()
        .flex()
        .flex_col()
        .gap(px(6.0))
        .p(px(8.0))
        .rounded(px(12.0))
        .bg(palette.field)
        .border_1()
        .border_color(if composer.shell {
            palette.warning
        } else {
            palette.control_border
        });
    if !composer.images.is_empty() {
        field = field.child(div().flex().flex_row().flex_wrap().gap(px(6.0)).children(
            composer.images.iter().enumerate().map(|(at, chip)| {
                let actions = shell.actions.clone();
                let mut thumbnail = div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(4.0))
                    .p(px(4.0))
                    .rounded(px(6.0))
                    .bg(palette.code);
                if let Some(image) = super::image_of(&chip.thumbnail, &shell.images) {
                    thumbnail = thumbnail.child(
                        img(image)
                            .size(px(size * 2.4))
                            .object_fit(ObjectFit::Cover)
                            .rounded(px(4.0)),
                    );
                }
                thumbnail
                    .child(widgets::small(chip.caption.clone(), palette.text_secondary))
                    .child(icon_button(
                        SharedString::from(format!("remove-image-{at}")),
                        "x",
                        "Take the image off",
                        &palette,
                        size * 0.7,
                        move |_, _, _| actions.send(Action::RemoveImage(at)),
                    ))
            }),
        ));
    }
    let input = Textarea::new(&shell.fields.draft).appearance(false);
    let (input, room) = match shell.page.composer_size {
        _ if stretched => (input.h(gpui::relative(1.0)), div().flex_1().min_h_0()),
        ComposerSize::Chosen => (input.h(px(shell.page.composer_chosen)), div()),
        ComposerSize::Fit | ComposerSize::Tall | ComposerSize::Full => (input, div()),
    };
    field = field.child(
        room.capture_key_down(cx.listener(Shell::composer_key))
            .child(input),
    );
    if !composer.argument_hint.is_empty() {
        field = field.child(widgets::small(
            composer.argument_hint.clone(),
            palette.text_tertiary,
        ));
    }
    field = field.child(tools(shell, &palette, cx));
    if stretched {
        field = field.flex_1().min_h_0();
    } else {
        field = field.child(shell.composer_grip(cx));
    }
    let outer = div().px(px(16.0)).pb(px(12.0)).flex().flex_col();
    let outer = if stretched {
        outer.flex_1().min_h_0()
    } else {
        outer.flex_none()
    };
    outer.child(field).into_any_element()
}

/// The row under the draft.
fn tools(shell: &Shell, palette: &Palette, cx: &Context<Shell>) -> impl IntoElement {
    let session = &shell.session;
    let size = shell.page.text_size;
    let mut row = div()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(4.0))
        .child(menu(
            "mode",
            super::widgets::lucide(mode_icon(session.mode)),
            &session.modes,
            shell.page.mode_index,
            shell,
            Action::ChooseMode,
        ))
        .child(menu(
            "model",
            super::widgets::lucide("bot"),
            &session.models,
            shell.page.model_index,
            shell,
            Action::ChooseModel,
        ));
    if !session.tasks.is_empty() {
        row = row.child(tasks(shell));
    }
    row = row.child(div().flex_1());
    if !session.context.is_empty() {
        row = row.child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap(px(6.0))
                .child(
                    div()
                        .w(px(48.0))
                        .h(px(4.0))
                        .rounded_full()
                        .bg(palette.hover())
                        .child(
                            div()
                                .h_full()
                                .w(gpui::relative(session.context_fraction.clamp(0.0, 1.0)))
                                .rounded_full()
                                .bg(palette.fill(session.context_fraction)),
                        ),
                )
                .child(widgets::small(
                    session.context.clone(),
                    palette.text_tertiary,
                )),
        );
    }
    let shell_toggle = shell.actions.clone();
    let paste = shell.actions.clone();
    let chosen = shell.page.composer_size;
    let (tall_icon, tall_label) = if chosen == ComposerSize::Tall {
        ("minimize-2", "Make the message box small again")
    } else {
        ("maximize-2", "Make the message box taller")
    };
    let (full_icon, full_label) = if chosen == ComposerSize::Full {
        ("shrink", "Show the conversation again")
    } else {
        (
            "expand",
            "Give the message the whole column, to see all of a long paste",
        )
    };
    row = row
        .child(icon_button(
            "composer-tall",
            tall_icon,
            tall_label,
            palette,
            size * 0.9,
            cx.listener(|shell, _, _, cx| {
                shell.toggle_composer(ComposerSize::Tall);
                cx.notify();
            }),
        ))
        .child(icon_button(
            "composer-full",
            full_icon,
            full_label,
            palette,
            size * 0.9,
            cx.listener(|shell, _, _, cx| {
                shell.toggle_composer(ComposerSize::Full);
                cx.notify();
            }),
        ))
        .child(icon_button(
            "toggle-shell",
            "square-terminal",
            "Run as a shell command (!)",
            palette,
            size * 0.9,
            move |_, _, _| shell_toggle.send(Action::ToggleShell),
        ))
        .child(icon_button(
            "paste-image",
            "image-plus",
            "Paste the clipboard's image (Ctrl+V)",
            palette,
            size * 0.9,
            move |_, _, _| paste.send(Action::PasteImage),
        ));
    let stop = session.busy;
    let label = if stop {
        "Stop (Esc)"
    } else if shell.preferences.send_key == SendKey::CtrlEnter {
        "Send (Ctrl+Enter)"
    } else {
        "Send (Enter)"
    };
    let (fill, ink, name) = if stop {
        (palette.danger, palette.on_accent, "square")
    } else {
        (palette.accent, palette.on_accent, "arrow-up")
    };
    row.child(
        div()
            .id("send")
            .flex()
            .items_center()
            .justify_center()
            .size(px((size * 2.0).max(28.0)))
            .rounded_full()
            .bg(fill)
            .cursor_pointer()
            .hover(|style| style.opacity(0.9))
            .tooltip(move |window, cx| {
                gpui_component::tooltip::Tooltip::new(label).build(window, cx)
            })
            .on_click(cx.listener(move |shell, _, window, cx| {
                if stop {
                    shell.actions.send(Action::Interrupt);
                } else {
                    shell.send_draft(window, cx);
                }
            }))
            .child(icon(name, size, ink)),
    )
}

/// The permission mode's icon, as the Slint window's: by how much it lets through.
fn mode_icon(mode: PermissionKind) -> &'static str {
    match mode {
        PermissionKind::AcceptEdits => "file-pen-line",
        PermissionKind::Plan => "map",
        PermissionKind::Auto => "zap",
        PermissionKind::Bypass => "shield-alert",
        PermissionKind::Unknown => "shield-question-mark",
        PermissionKind::Default => "shield-check",
    }
}

/// A menu of `items`, `chosen` checked, which sends `action` with the index picked.
fn menu(
    id: impl Into<gpui::ElementId>,
    icon: gpui_component::Icon,
    items: &[MenuItem],
    chosen: i32,
    shell: &Shell,
    action: fn(usize) -> Action,
) -> impl IntoElement {
    let chosen = usize::try_from(chosen).ok();
    let label = chosen
        .and_then(|at| items.get(at))
        .map_or_else(String::new, |item| item.title.clone());
    let items = items.to_vec();
    let actions = shell.actions.clone();
    Button::new(id)
        .ghost()
        .icon(icon)
        .label(label)
        .dropdown_menu(move |mut menu, _, _| {
            for (at, item) in items.iter().enumerate() {
                let actions = actions.clone();
                let title = if item.description.is_empty() {
                    item.title.clone()
                } else {
                    format!("{} — {}", item.title, item.description)
                };
                menu = menu.item(
                    PopupMenuItem::new(title)
                        .checked(chosen == Some(at))
                        .on_click(move |_, _, _| actions.send(action(at))),
                );
            }
            menu
        })
}

/// What runs beside the conversation (agents, commands, monitors): followed, or stopped.
fn tasks(shell: &Shell) -> impl IntoElement {
    let tasks = shell.session.tasks.clone();
    let actions = shell.actions.clone();
    Button::new("tasks")
        .ghost()
        .icon(super::widgets::lucide("loader-circle"))
        .label(shell.session.tasks_summary.clone())
        .dropdown_menu(move |mut menu, _, _| {
            for task in &tasks {
                let follow = actions.clone();
                let id = task.id.clone();
                let mut title = task.title.clone();
                if !task.now.is_empty() {
                    title = format!("{title} — {}", task.now);
                }
                menu = menu.item(PopupMenuItem::new(title).disabled(!task.peekable).on_click(
                    move |_, _, _| follow.send(Action::OpenPeek(TaskId::from(id.clone()))),
                ));
            }
            menu = menu.separator();
            for task in &tasks {
                let stop = actions.clone();
                let id = task.id.clone();
                menu = menu.item(PopupMenuItem::new(format!("Stop {}", task.title)).on_click(
                    move |_, _, _| stop.send(Action::StopTask(TaskId::from(id.clone()))),
                ));
            }
            let all = actions.clone();
            menu.item(
                PopupMenuItem::new("Stop all")
                    .on_click(move |_, _, _| all.send(Action::StopAllTasks)),
            )
        })
}
