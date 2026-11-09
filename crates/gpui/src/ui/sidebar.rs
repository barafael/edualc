//! The sidebar (`ui/app.slint`'s `Sidebar`): a new session, the folders to work in, the
//! sessions (running ones under their project, then earlier ones), filtered; and at its foot,
//! the sandbox, and how much of the plan is used.

use gpui::{
    AnyElement, Context, FontWeight, InteractiveElement as _, IntoElement, ParentElement as _,
    SharedString, StatefulInteractiveElement as _, Styled as _, Window, div, px,
};
use gpui_component::input::Input;
use gpui_component::scroll::ScrollableElement as _;

use super::widgets::{self, icon, icon_button};
use super::{Palette, Shell};
use crate::app::Action;
use crate::external::wire::SessionId;
use crate::present::model::{LiveState, ProjectItem, SessionRow, SessionRowKind};

pub(super) fn sidebar(shell: &Shell, _window: &mut Window, cx: &Context<Shell>) -> AnyElement {
    let palette = shell.palette();
    let size = shell.page.text_size;
    let width = shell.sidebar_width();
    let actions = shell.actions.clone();
    let look = shell.actions.clone();
    let top = div()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(4.0))
        .px(px(8.0))
        .h(px((size * 3.1).max(46.0)))
        .child(shell.heron_mark(24.0, &palette))
        .child(
            div()
                .flex_1()
                .min_w_0()
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(palette.claude)
                .child("edualc"),
        )
        .child(icon_button(
            "look",
            "palette",
            "Switch the look",
            &palette,
            size,
            move |_, _, _| {
                look.send(Action::SwitchLook);
            },
        ))
        .child(icon_button(
            "folders",
            "folder-open",
            "Open a folder",
            &palette,
            size,
            cx.listener(|shell, _, _, cx| {
                shell.projects_open = !shell.projects_open;
                if shell.projects_open {
                    shell.actions.send(Action::ListProjects);
                }
                cx.notify();
            }),
        ))
        .child(icon_button(
            "new-session",
            "square-pen",
            "New session (Ctrl+N)",
            &palette,
            size,
            move |_, _, _| actions.send(Action::NewSession),
        ));
    let mut column = div()
        .relative()
        .track_focus(&shell.areas.sidebar)
        .flex()
        .flex_col()
        .flex_none()
        .w(px(width))
        .h_full()
        .bg(palette.surface_raised)
        .border_r_1()
        .border_color(palette.divider)
        .child(top);
    if shell.projects_open {
        column = column.child(folders(shell, &palette));
    }
    column = column.child(
        div()
            .px(px(8.0))
            .pb(px(8.0))
            .child(Input::new(&shell.fields.filter).cleanable(true)),
    );
    if !shell.sessions.empty.is_empty() {
        column = column.child(div().px(px(16.0)).py(px(8.0)).child(widgets::small(
            shell.sessions.empty.clone(),
            palette.text_tertiary,
        )));
    }
    column
        .child(
            div()
                .id("sessions")
                .flex_1()
                .min_h_0()
                .px(px(6.0))
                .overflow_y_scrollbar()
                .child(
                    div().flex().flex_col().gap(px(1.0)).children(
                        shell
                            .sessions
                            .rows
                            .iter()
                            .enumerate()
                            .map(|(at, row)| session_row(shell, at, row, &palette)),
                    ),
                ),
        )
        .child(foot(shell, &palette))
        .child(shell.sidebar_edge(cx))
        .into_any_element()
}

/// The folders to work in: a path typed, or one of those worked in lately.
fn folders(shell: &Shell, palette: &Palette) -> impl IntoElement {
    let mut panel = div()
        .flex()
        .flex_col()
        .gap(px(6.0))
        .mx(px(8.0))
        .mb(px(8.0))
        .p(px(8.0))
        .rounded(px(8.0))
        .bg(palette.card)
        .border_1()
        .border_color(palette.divider)
        .child(Input::new(&shell.fields.project));
    if !shell.sessions.problem.is_empty() {
        panel = panel.child(widgets::small(
            shell.sessions.problem.clone(),
            palette.danger_text,
        ));
    }
    panel
        .child(widgets::small("Recent", palette.text_tertiary))
        .child(
            div()
                .id("folders-list")
                .max_h(px(240.0))
                .overflow_y_scrollbar()
                .child(
                    div().flex().flex_col().children(
                        shell
                            .sessions
                            .projects
                            .iter()
                            .enumerate()
                            .map(|(at, project)| folder_row(shell, at, project, palette)),
                    ),
                ),
        )
}

fn folder_row(
    shell: &Shell,
    at: usize,
    project: &ProjectItem,
    palette: &Palette,
) -> impl IntoElement {
    let actions = shell.actions.clone();
    let path = project.path.clone();
    let hover = palette.hover();
    div()
        .id(SharedString::from(format!("folder-{at}")))
        .flex()
        .flex_row()
        .items_center()
        .gap(px(8.0))
        .px(px(6.0))
        .py(px(4.0))
        .rounded(px(6.0))
        .cursor_pointer()
        .hover(move |style| style.bg(hover))
        .on_click(move |_, _, _| actions.send(Action::OpenProject(path.clone())))
        .child(
            div()
                .flex_1()
                .min_w_0()
                .flex()
                .flex_col()
                .child(div().truncate().child(project.name.clone()))
                .child(
                    div()
                        .truncate()
                        .text_sm()
                        .text_color(palette.text_tertiary)
                        .child(project.shown.clone()),
                ),
        )
        .child(widgets::small(project.age.clone(), palette.text_tertiary))
}

fn session_row(shell: &Shell, at: usize, row: &SessionRow, palette: &Palette) -> AnyElement {
    let size = shell.page.text_size;
    if row.kind == SessionRowKind::Heading {
        let mut heading = div()
            .flex()
            .flex_row()
            .items_baseline()
            .gap(px(6.0))
            .px(px(10.0))
            .pt(px(12.0))
            .pb(px(4.0))
            .text_sm()
            .font_weight(FontWeight::SEMIBOLD)
            .text_color(palette.text_secondary)
            .child(div().truncate().child(row.title.clone()));
        if !row.detail.is_empty() {
            heading = heading.child(
                div()
                    .font_weight(FontWeight::NORMAL)
                    .text_color(palette.text_tertiary)
                    .child(row.detail.clone()),
            );
        }
        return heading.into_any_element();
    }
    let actions = shell.actions.clone();
    let id = row.id.clone();
    let hover = palette.hover();
    let mut pressable = div()
        .id(SharedString::from(format!("session-{at}")))
        .flex()
        .flex_row()
        .items_center()
        .gap(px(8.0))
        .px(px(10.0))
        .min_h(px((size * 2.3).max(34.0)))
        .py(px(4.0))
        .rounded(px(6.0))
        .cursor_pointer()
        .hover(move |style| style.bg(hover))
        .on_click(move |_, _, _| match SessionId::try_from(id.clone()) {
            Ok(id) => actions.send(Action::OpenSession(id)),
            Err(error) => tracing::warn!(%error, "a session the lists shouldn't hold"),
        });
    if row.selected {
        pressable = pressable.bg(palette.selected());
    }
    match row.kind {
        SessionRowKind::Running => {
            let detail_ink = if row.state == LiveState::Waiting {
                palette.warning_text
            } else {
                palette.text_tertiary
            };
            pressable = pressable.child(live_dot(row.state, palette, size)).child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .child(div().truncate().child(row.title.clone()))
                    .child(
                        div()
                            .truncate()
                            .text_sm()
                            .text_color(detail_ink)
                            .child(row.detail.clone()),
                    ),
            );
            if row.agents > 0 {
                pressable = pressable.child(
                    div()
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap(px(2.0))
                        .text_sm()
                        .text_color(palette.text_tertiary)
                        .child(icon("bot", size * 0.9, palette.text_tertiary))
                        .child(row.agents.to_string()),
                );
            }
        }
        SessionRowKind::Earlier | SessionRowKind::Heading => {
            if !row.worktree.is_empty() {
                pressable = pressable.child(icon("git-branch", size * 0.9, palette.text_tertiary));
            }
            pressable = pressable
                .child(div().flex_1().min_w_0().truncate().child(row.title.clone()))
                .child(widgets::small(row.age.clone(), palette.text_tertiary));
        }
    }
    pressable.into_any_element()
}

/// Whether a session works, waits for the user, or is idle, as a dot.
pub(super) fn live_dot(state: LiveState, palette: &Palette, size: f32) -> impl IntoElement {
    let fill = match state {
        LiveState::Busy => palette.claude,
        LiveState::Waiting => palette.warning,
        LiveState::Idle => palette.text_faint,
        LiveState::Other => palette.info,
    };
    div()
        .flex_none()
        .size(px(size * 0.55))
        .rounded_full()
        .bg(fill)
}

/// The sidebar's foot: the sandbox, and how much of the plan is used (a click shows the
/// details).
fn foot(shell: &Shell, palette: &Palette) -> impl IntoElement {
    let session = &shell.session;
    let size = shell.page.text_size;
    let actions = shell.actions.clone();
    let sandbox = shell.actions.clone();
    let hover = palette.hover();
    let mut usage = div()
        .id("usage")
        .flex()
        .flex_col()
        .gap(px(4.0))
        .p(px(8.0))
        .rounded(px(6.0))
        .cursor_pointer()
        .hover(move |style| style.bg(hover))
        .on_click(move |_, _, _| actions.send(Action::ShowUsage));
    if session.limit_five_hour >= 0.0 {
        usage = usage.child(usage_bar("5h", session.limit_five_hour, palette));
    }
    if !session.limit_five_hour_resets.is_empty() {
        usage = usage.child(widgets::small(
            session.limit_five_hour_resets.clone(),
            palette.text_tertiary,
        ));
    }
    if session.limit_seven_day >= 0.0 {
        usage = usage.child(usage_bar("7d", session.limit_seven_day, palette));
    }
    if session.limit_five_hour < 0.0 && session.limit_seven_day < 0.0 {
        usage = usage.child(widgets::small("Usage", palette.text_tertiary));
    }
    let (shield, ink) = if session.sandboxed {
        ("shield-check", palette.success)
    } else {
        ("shield-off", palette.warning_text)
    };
    let mut chip = div()
        .id("sandbox")
        .flex()
        .flex_row()
        .items_center()
        .gap(px(6.0))
        .px(px(8.0))
        .py(px(4.0))
        .rounded(px(6.0))
        .cursor_pointer()
        .hover(move |style| style.bg(hover))
        .on_click(move |_, _, _| sandbox.send(Action::ToggleSandbox))
        .child(icon(shield, size, ink))
        .child(
            div()
                .flex_1()
                .truncate()
                .child(session.sandbox_label.clone()),
        );
    if session.sandbox_pending {
        chip = chip.child(widgets::small("…", palette.text_tertiary));
    }
    div()
        .flex()
        .flex_col()
        .gap(px(2.0))
        .p(px(6.0))
        .border_t_1()
        .border_color(palette.divider)
        .child(chip)
        .child(usage)
}

/// A share of the plan used, as a bar and a number.
fn usage_bar(label: impl Into<SharedString>, used: f32, palette: &Palette) -> impl IntoElement {
    let fraction = used.clamp(0.0, 1.0);
    // A share as a whole percentage, from 0 to 100.
    #[allow(clippy::cast_possible_truncation)]
    let share = (fraction * 100.0).round() as i32;
    div()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(8.0))
        .child(
            div()
                .w(px(24.0))
                .text_sm()
                .text_color(palette.text_tertiary)
                .child(label.into()),
        )
        .child(
            div()
                .flex_1()
                .h(px(6.0))
                .rounded_full()
                .bg(palette.hover())
                .child(
                    div()
                        .h_full()
                        .w(gpui::relative(fraction))
                        .rounded_full()
                        .bg(palette.fill(fraction)),
                ),
        )
        .child(
            div()
                .w(px(36.0))
                .text_sm()
                .text_color(palette.text_tertiary)
                .child(format!("{share}%")),
        )
}
