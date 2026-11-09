//! The conversation (`ui/transcript.slint`): the user's messages, Claude's Markdown, thinking,
//! tool calls with their input, output and diffs, runs of calls folded into one line, messages
//! from other sessions, notes and errors. GPUI's list draws only the rows in view, measures
//! each as it is drawn, and, aligned to its end, stays at the end while it is there: new output
//! is followed only while the end is in view.

use gpui::{
    AnyElement, Context, FontWeight, InteractiveElement as _, IntoElement, ObjectFit,
    ParentElement as _, SharedString, StatefulInteractiveElement as _, Styled as _,
    StyledImage as _, Window, div, img, list, px,
};

use super::heron;
use super::widgets::{self, CodeBox, Ink, icon, icon_button};
use super::{Palette, Shell};
use crate::app::Action;
use crate::present::model::{
    DiffKind, DiffLine, Entry, EntryKind, GroupRole, ImageState, ShownImage, ToolState,
};

/// The transcript's measure: prose at most this many base sizes wide.
const MEASURE: f32 = 46.0;

pub(super) fn transcript(shell: &Shell, window: &Window, cx: &Context<Shell>) -> AnyElement {
    let palette = shell.palette();
    let empty = shell.entries.is_empty();
    // The conversation's home for the keyboard (F6): it scrolls with the keys while it has the
    // focus itself.
    let mut area = div()
        .id("conversation")
        .relative()
        .flex_1()
        .min_h_0()
        .w_full()
        .track_focus(&shell.areas.conversation)
        .on_key_down(cx.listener(Shell::conversation_key));
    if !empty {
        area = area.child(
            list(
                shell.list.clone(),
                cx.processor(|shell: &mut Shell, index: usize, _, _| shell.row(index)),
            )
            .size_full(),
        );
    }
    // Over the list: the heron, which stays while the conversation is empty and flies off as
    // it begins, and the greeting's words, only while it is empty.
    area = area.child(greeting(shell, empty, heron::reach(window), &palette));
    if !empty && !shell.at_end {
        area = area.child(
            div()
                .absolute()
                .bottom(px(12.0))
                .right(px(24.0))
                .rounded_full()
                .bg(palette.surface_raised)
                .border_1()
                .border_color(palette.divider)
                .child(icon_button(
                    "to-end",
                    "arrow-down",
                    "Go to the end",
                    &palette,
                    shell.page.text_size,
                    cx.listener(|shell, _, _, cx| {
                        shell.scroll_to_end();
                        cx.notify();
                    }),
                )),
        );
    }
    area.into_any_element()
}

/// Over a new session's empty conversation: the heron, a greeting, and where it works; the
/// heron alone, flying off, as the conversation begins.
fn greeting(shell: &Shell, empty: bool, reach: (f32, f32), palette: &Palette) -> impl IntoElement {
    let mut greeting = div()
        .absolute()
        .top_0()
        .left_0()
        .size_full()
        .flex()
        .flex_col()
        .items_center()
        .justify_center()
        .gap(px(8.0))
        .child(shell.heron(reach, palette));
    if empty {
        greeting = greeting
            .child(
                div()
                    .text_size(px(shell.page.text_size * 1.6))
                    .font_weight(FontWeight::SEMIBOLD)
                    .child("What should we work on?"),
            )
            .child(widgets::small(
                format!("in {}", shell.session.cwd),
                palette.text_tertiary,
            ));
    }
    // It has no handlers, so it takes no pointer: the conversation under it does.
    greeting
}

impl Shell {
    /// Row `index` of the conversation, in the column's measure; nothing for a hidden one (a
    /// folded run's later calls).
    fn row(&self, index: usize) -> AnyElement {
        let Some(entry) = self.entries.get(index) else {
            return div().into_any_element();
        };
        if entry.hidden {
            return div().into_any_element();
        }
        let palette = self.palette();
        let size = self.page.text_size;
        let body = match entry.kind {
            EntryKind::User => self.user_bubble(entry, &palette),
            EntryKind::Image => {
                let mut images = div().flex().flex_row().justify_end().gap(px(8.0));
                for shown in &entry.images {
                    images = images.child(self.image_view(shown, &palette));
                }
                images.into_any_element()
            }
            EntryKind::Peer => self.peer_bubble(entry, &palette),
            EntryKind::Assistant => self.assistant(entry, &palette),
            EntryKind::Thinking => self.thinking(entry, &palette),
            EntryKind::Tool => {
                let mut tool = div().flex().flex_col().gap(px(4.0));
                if entry.group == GroupRole::Head {
                    tool = tool.child(self.run_line(entry, &palette));
                }
                if entry.group == GroupRole::Alone || entry.group_open {
                    let card = self.tool_card(entry, &palette);
                    tool = tool.child(if entry.group == GroupRole::Alone {
                        div().child(card)
                    } else {
                        div().pl(px(20.0)).child(card)
                    });
                }
                tool.into_any_element()
            }
            EntryKind::Info => div()
                .text_color(palette.text_tertiary)
                .text_sm()
                .child(widgets::entry_text(
                    SharedString::from(format!("info-{index}")),
                    entry,
                    &self.actions,
                ))
                .into_any_element(),
            EntryKind::Error => div()
                .flex()
                .flex_row()
                .gap(px(8.0))
                .text_color(palette.danger_text)
                .child(icon("circle-alert", size, palette.danger_text))
                .child(div().flex_1().child(widgets::entry_text(
                    SharedString::from(format!("error-{index}")),
                    entry,
                    &self.actions,
                )))
                .into_any_element(),
        };
        let indent = if entry.nested { px(24.0) } else { px(0.0) };
        div()
            .w_full()
            .flex()
            .flex_row()
            .justify_center()
            .px(px(16.0))
            .py(px(size * 0.4))
            .child(
                div()
                    .w_full()
                    .max_w(px(size * MEASURE))
                    .pl(indent)
                    .child(body),
            )
            .into_any_element()
    }

    /// The user's message: a bubble on the right of the column.
    fn user_bubble(&self, entry: &Entry, palette: &Palette) -> AnyElement {
        div()
            .flex()
            .flex_row()
            .justify_end()
            .child(
                div()
                    .max_w(gpui::relative(0.85))
                    .rounded(px(16.0))
                    .bg(palette.bubble)
                    .px(px(14.0))
                    .py(px(8.0))
                    .child(widgets::entry_text(
                        SharedString::from(format!("user-{}", entry.index)),
                        entry,
                        &self.actions,
                    )),
            )
            .into_any_element()
    }

    /// A message another session sent this one: on Claude's side, with who sent it above it.
    fn peer_bubble(&self, entry: &Entry, palette: &Palette) -> AnyElement {
        let size = self.page.text_size;
        div()
            .flex()
            .flex_row()
            .justify_start()
            .child(
                div()
                    .max_w(gpui::relative(0.85))
                    .rounded(px(16.0))
                    .border_1()
                    .border_color(palette.divider)
                    .px(px(14.0))
                    .py(px(8.0))
                    .flex()
                    .flex_col()
                    .gap(px(4.0))
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .gap(px(6.0))
                            .items_center()
                            .text_sm()
                            .text_color(palette.text_tertiary)
                            .child(icon("messages-square", size * 0.9, palette.text_tertiary))
                            .child(format!("Message from {}", entry.sender)),
                    )
                    .child(widgets::entry_text(
                        SharedString::from(format!("peer-{}", entry.index)),
                        entry,
                        &self.actions,
                    )),
            )
            .into_any_element()
    }

    /// Claude's message: its Markdown, and a button to copy it.
    fn assistant(&self, entry: &Entry, palette: &Palette) -> AnyElement {
        let size = self.page.text_size;
        let actions = self.actions.clone();
        let index = usize::try_from(entry.index).ok();
        let ink = Ink {
            palette,
            actions: &self.actions,
            size: size * 1.1,
        };
        div()
            .flex()
            .flex_col()
            .gap(px(4.0))
            .text_size(px(size * 1.1))
            .child(widgets::blocks(
                &format!("assistant-{}", entry.index),
                &entry.blocks,
                &ink,
            ))
            .child(icon_button(
                SharedString::from(format!("copy-{}", entry.index)),
                "copy",
                "Copy message",
                palette,
                size * 0.8,
                move |_, _, _| {
                    if let Some(index) = index {
                        actions.send(Action::CopyEntry(index));
                    }
                },
            ))
            .into_any_element()
    }

    /// Claude's thinking: folded under a line, opened by it.
    fn thinking(&self, entry: &Entry, palette: &Palette) -> AnyElement {
        let size = self.page.text_size;
        let actions = self.actions.clone();
        let index = usize::try_from(entry.index).ok();
        let chevron = if entry.expanded {
            "chevron-down"
        } else {
            "chevron-right"
        };
        let mut thinking = div().flex().flex_col().gap(px(4.0)).child(
            div()
                .id(SharedString::from(format!("thinking-{}", entry.index)))
                .flex()
                .flex_row()
                .items_center()
                .gap(px(6.0))
                .cursor_pointer()
                .text_color(palette.text_tertiary)
                .text_sm()
                .on_click(move |_, _, _| {
                    if let Some(index) = index {
                        actions.send(Action::Toggle(index));
                    }
                })
                .child(icon("brain", size * 0.9, palette.text_tertiary))
                .child("Thinking")
                .child(icon(chevron, size * 0.9, palette.text_tertiary)),
        );
        if entry.expanded {
            thinking = thinking.child(
                div()
                    .border_l_2()
                    .border_color(palette.divider)
                    .pl(px(12.0))
                    .text_color(palette.text_secondary)
                    .child(widgets::entry_text(
                        SharedString::from(format!("thought-{}", entry.index)),
                        entry,
                        &self.actions,
                    )),
            );
        }
        thinking.into_any_element()
    }

    /// The line a run of tool calls is folded into: what they did, and how it went.
    fn run_line(&self, entry: &Entry, palette: &Palette) -> impl IntoElement {
        let size = self.page.text_size;
        let actions = self.actions.clone();
        let index = usize::try_from(entry.index).ok();
        let chevron = if entry.group_open {
            "chevron-down"
        } else {
            "chevron-right"
        };
        let ink = if entry.group_state == ToolState::Failed {
            palette.danger_text
        } else {
            palette.text_secondary
        };
        let hover = palette.hover();
        div()
            .id(SharedString::from(format!("run-{}", entry.index)))
            .flex()
            .flex_row()
            .items_center()
            .gap(px(8.0))
            .cursor_pointer()
            .rounded(px(6.0))
            .px(px(4.0))
            .py(px(2.0))
            .hover(move |style| style.bg(hover))
            .on_click(move |_, _, _| {
                if let Some(index) = index {
                    actions.send(Action::ToggleRun(index));
                }
            })
            .child(icon(chevron, size * 0.9, palette.text_faint))
            .child(state_mark(entry.group_state, palette, size))
            .child(div().text_color(ink).child(entry.group_summary.clone()))
    }

    /// A tool call: its name and what it works on, how it went, and, opened, its input,
    /// output, diff and images.
    fn tool_card(&self, entry: &Entry, palette: &Palette) -> AnyElement {
        let size = self.page.text_size;
        let actions = self.actions.clone();
        let index = usize::try_from(entry.index).ok();
        let state = entry.tool_state;
        let state_label = match state {
            ToolState::Awaiting => "Waiting for you",
            ToolState::Failed => "Failed",
            ToolState::Denied => "Denied",
            ToolState::Cancelled => "Cancelled",
            ToolState::Streaming | ToolState::Running | ToolState::Done => "",
        };
        let chevron = if entry.expanded {
            "chevron-down"
        } else {
            "chevron-right"
        };
        let summary = div()
            .flex_1()
            .min_w_0()
            .truncate()
            .text_color(palette.text_secondary)
            .child(entry.tool_summary.clone());
        let summary = if entry.tool_summary_prose {
            summary
        } else {
            summary
                .font_family(self.mono.clone())
                .text_size(px(size * 0.95))
        };
        let mut head = div()
            .id(SharedString::from(format!("tool-{}", entry.index)))
            .flex()
            .flex_row()
            .items_center()
            .gap(px(8.0))
            .px(px(12.0))
            .py(px(8.0))
            .cursor_pointer()
            .on_click(move |_, _, _| {
                if let Some(index) = index {
                    actions.send(Action::Toggle(index));
                }
            })
            .child(state_mark(state, palette, size))
            .child(
                div()
                    .flex_none()
                    .font_weight(FontWeight::SEMIBOLD)
                    .child(entry.tool_title.clone()),
            )
            .child(summary);
        if !state_label.is_empty() {
            let ink = if state == ToolState::Failed {
                palette.danger_text
            } else {
                palette.text_tertiary
            };
            head = head.child(widgets::small(state_label, ink));
        }
        head = head.child(icon(chevron, size * 0.9, palette.text_tertiary));
        let border = if state == ToolState::Awaiting {
            palette.warning
        } else {
            palette.divider
        };
        let mut card = div()
            .flex()
            .flex_col()
            .rounded(px(8.0))
            .border_1()
            .border_color(border)
            .bg(palette.card)
            .child(head);
        if entry.expanded {
            let mut body = div()
                .flex()
                .flex_col()
                .gap(px(8.0))
                .px(px(12.0))
                .pb(px(12.0));
            let ink = Ink {
                palette,
                actions: &self.actions,
                size,
            };
            if !entry.diff.is_empty() {
                body = body.child(self.diff_view(&entry.diff, palette));
            }
            if entry.has_detail {
                body = body.child(
                    CodeBox {
                        id: SharedString::from(format!("detail-{}", entry.index)),
                        body: &entry.detail,
                        color: palette.text_secondary,
                        note: "",
                        copy: None,
                    }
                    .draw(&ink),
                );
            }
            if !entry.blocks.is_empty() {
                body = body.child(widgets::blocks(
                    &format!("tool-md-{}", entry.index),
                    &entry.blocks,
                    &ink,
                ));
            }
            if entry.has_output {
                let color = if state == ToolState::Failed {
                    palette.danger_text
                } else {
                    palette.text
                };
                body = body.child(
                    CodeBox {
                        id: SharedString::from(format!("output-{}", entry.index)),
                        body: &entry.output,
                        color,
                        note: &entry.output_note,
                        copy: index,
                    }
                    .draw(&ink),
                );
            }
            for shown in &entry.images {
                body = body.child(self.image_view(shown, palette));
            }
            card = card.child(body);
        }
        card.into_any_element()
    }

    /// An edit's lines, added and taken out.
    fn diff_view(&self, lines: &[DiffLine], palette: &Palette) -> impl IntoElement {
        div()
            .flex()
            .flex_col()
            .rounded(px(8.0))
            .bg(palette.code)
            .py(px(6.0))
            .font_family(self.mono.clone())
            .text_size(px(self.page.text_size * 0.95))
            .children(lines.iter().map(|line| {
                let (fill, ink, sign) = match line.kind {
                    DiffKind::Added => (palette.added(), palette.success, "+"),
                    DiffKind::Removed => (palette.removed(), palette.danger_text, "−"),
                    DiffKind::Context => (gpui::transparent_black(), palette.text_secondary, " "),
                    DiffKind::Hunk => (gpui::transparent_black(), palette.text_faint, " "),
                };
                div()
                    .flex()
                    .flex_row()
                    .bg(fill)
                    .px(px(8.0))
                    .child(div().w(px(16.0)).flex_none().text_color(ink).child(sign))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .text_color(ink)
                            .whitespace_normal()
                            .child(line.text.replace('\t', "    ")),
                    )
            }))
    }

    /// An image, at its size in logical pixels (narrowed to the column); a word for one not
    /// decoded yet, or not shown.
    fn image_view(&self, shown: &ShownImage, palette: &Palette) -> AnyElement {
        if shown.state == ImageState::Failed {
            return widgets::small(
                format!("Image not shown: {}", shown.note),
                palette.text_tertiary,
            )
            .into_any_element();
        }
        match super::image_of(&shown.image, &self.images) {
            Some(image) => {
                // Pixel sizes are small enough to be exact in `f32`.
                #[allow(clippy::cast_precision_loss)]
                let (width, height) = (shown.width_px.max(1) as f32, shown.height_px.max(1) as f32);
                img(image)
                    .w(px(width))
                    .max_w_full()
                    .h(px(height))
                    .object_fit(ObjectFit::Contain)
                    .rounded(px(8.0))
                    .into_any_element()
            }
            None => div()
                .w(px(120.0))
                .h(px(80.0))
                .rounded(px(8.0))
                .bg(palette.code)
                .flex()
                .items_center()
                .justify_center()
                .child(widgets::small("Image", palette.text_faint))
                .into_any_element(),
        }
    }
}

/// How a tool call went, as a mark: a dot while it works, an icon after.
fn state_mark(state: ToolState, palette: &Palette, size: f32) -> AnyElement {
    let (name, ink) = match state {
        ToolState::Streaming | ToolState::Running => {
            return div()
                .size(px(size * 0.5))
                .mx(px(size * 0.2))
                .rounded_full()
                .bg(palette.claude)
                .into_any_element();
        }
        ToolState::Awaiting => ("hand", palette.warning_text),
        ToolState::Done => ("check", palette.success),
        ToolState::Failed => ("x", palette.danger_text),
        ToolState::Denied => ("ban", palette.text_faint),
        ToolState::Cancelled => ("circle-slash", palette.text_faint),
    };
    icon(name, size * 0.9, ink).into_any_element()
}
