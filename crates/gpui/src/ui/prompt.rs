//! What Claude asks (`ui/app.slint`'s `PromptBar`, `ui/plan.slint`): a tool's permission, its
//! questions with their options, or its plan to approve; the plan reviewed part by part, with a
//! comment on any. Its buttons take a press only a moment after it shows (`Shell::armed`), so
//! that a key meant for the composer doesn't answer it.

use gpui::{
    AnyElement, Context, FontWeight, InteractiveElement as _, IntoElement, ParentElement as _,
    SharedString, StatefulInteractiveElement as _, Styled as _, Window, div, px,
};
use gpui_component::input::{Input, Textarea};
use gpui_component::scroll::ScrollableElement as _;

use super::widgets::{self, Tone, icon, icon_button, text_button};
use super::{Palette, Shell};
use crate::app::{Action, PermissionMode};
use crate::external::wire::RequestId;
use crate::present::model::{PlanSection, PromptKind, Question};

pub(super) fn prompt_bar(shell: &Shell, window: &Window, cx: &Context<Shell>) -> AnyElement {
    let palette = shell.palette();
    let prompt = &shell.prompt;
    let size = shell.page.text_size;
    let name = match prompt.kind {
        PromptKind::Question => "message-circle-question-mark",
        PromptKind::Plan => "list-checks",
        PromptKind::Tool | PromptKind::None => "shield-question-mark",
    };
    let mut heading = div()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(8.0))
        .child(icon(name, size, palette.warning_text))
        .child(
            div()
                .flex_1()
                .font_weight(FontWeight::SEMIBOLD)
                .child(prompt.title.clone()),
        );
    if prompt.queued > 0 {
        heading = heading.child(widgets::small(
            format!("{} more waiting", prompt.queued),
            palette.text_tertiary,
        ));
    }
    let body = match prompt.kind {
        PromptKind::Tool => tool_prompt(shell, &palette),
        PromptKind::Question => questions_prompt(shell, &palette, window, cx),
        PromptKind::Plan => plan_prompt(shell, &palette, cx),
        PromptKind::None => div().into_any_element(),
    };
    div()
        .id("prompt")
        .flex()
        .flex_col()
        .gap(px(8.0))
        .mx(px(16.0))
        .mb(px(8.0))
        .p(px(12.0))
        .max_h(px(size * 30.0))
        .overflow_y_scroll()
        .rounded(px(12.0))
        .bg(palette.card)
        .border_1()
        .border_color(palette.warning)
        .child(heading)
        .child(body)
        .into_any_element()
}

/// The request the prompt shown answers.
fn request(shell: &Shell) -> RequestId {
    RequestId::from(shell.prompt.request.clone())
}

fn actions_row() -> gpui::Div {
    div().flex().flex_row().flex_wrap().gap(px(8.0))
}

/// A tool asks to run: allowed once, always (as the prompt words it), or denied, with what
/// to do instead.
fn tool_prompt(shell: &Shell, palette: &Palette) -> AnyElement {
    let prompt = &shell.prompt;
    let armed = shell.armed();
    let mut part = div().flex().flex_col().gap(px(8.0));
    if !prompt.summary.is_empty() {
        let summary = div()
            .rounded(px(6.0))
            .bg(palette.code)
            .px(px(8.0))
            .py(px(4.0))
            .whitespace_normal()
            .child(prompt.summary.clone());
        part = part.child(if prompt.summary_prose {
            summary
        } else {
            summary.font_family(shell.mono.clone())
        });
    }
    if !prompt.detail.is_empty() {
        part = part.child(widgets::small(
            prompt.detail.clone(),
            palette.text_secondary,
        ));
    }
    let (allow, always, deny) = (
        shell.actions.clone(),
        shell.actions.clone(),
        shell.actions.clone(),
    );
    let (request_allow, request_always) = (request(shell), request(shell));
    let feedback = shell.fields.feedback.clone();
    let mut row = actions_row().child(text_button(
        "allow",
        "Allow",
        Tone::Primary,
        armed,
        palette,
        move |_, _, _| allow.send(Action::Allow(request_allow.clone())),
    ));
    if !prompt.always_label.is_empty() {
        row = row.child(text_button(
            "allow-always",
            prompt.always_label.clone(),
            Tone::Plain,
            armed,
            palette,
            move |_, _, _| always.send(Action::AllowAlways(request_always.clone())),
        ));
    }
    let request_deny = request(shell);
    row = row.child(text_button(
        "deny",
        "Deny",
        Tone::Danger,
        armed,
        palette,
        move |_, window, cx| {
            let text = feedback.read(cx).value().to_string();
            deny.send(Action::Deny {
                request: request_deny.clone(),
                feedback: text,
            });
            feedback.update(cx, |state, cx| state.set_value("", window, cx));
        },
    ));
    part.child(row)
        .child(Input::new(&shell.fields.feedback))
        .into_any_element()
}

/// Claude's questions, one at a time with tabs between them: options to pick, something else
/// typed, notes.
fn questions_prompt(
    shell: &Shell,
    palette: &Palette,
    window: &Window,
    cx: &Context<Shell>,
) -> AnyElement {
    let prompt = &shell.prompt;
    let count = prompt.questions.len();
    let shown = shell.question_shown.min(count.saturating_sub(1));
    let last = shown + 1 >= count;
    let armed = shell.armed();
    let mut part = div().flex().flex_col().gap(px(8.0));
    if count > 1 {
        part = part.child(div().flex().flex_row().flex_wrap().gap(px(4.0)).children(
            prompt.questions.iter().enumerate().map(|(at, question)| {
                let mut tab = div()
                    .id(SharedString::from(format!("question-tab-{at}")))
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(4.0))
                    .px(px(8.0))
                    .py(px(4.0))
                    .rounded(px(6.0))
                    .cursor_pointer()
                    .hover(|style| style.bg(palette.hover()))
                    .on_click(cx.listener(move |shell, _, _, cx| {
                        shell.show_question(at);
                        cx.notify();
                    }))
                    .child(icon(
                        if question.answered {
                            "circle-check"
                        } else {
                            "circle"
                        },
                        shell.page.text_size * 0.9,
                        if question.answered {
                            palette.success
                        } else {
                            palette.text_faint
                        },
                    ))
                    .child(header(question, at));
                if at == shown {
                    tab = tab.bg(palette.selected());
                }
                tab
            }),
        ));
    }
    if let Some(question) = prompt.questions.get(shown) {
        part = part.child(question_entry(shell, shown, question, palette, window, cx));
    }
    let mut row = actions_row();
    if shown > 0 {
        row = row.child(text_button(
            "back",
            "Back",
            Tone::Plain,
            true,
            palette,
            cx.listener(move |shell, _, _, cx| {
                shell.show_question(shown - 1);
                cx.notify();
            }),
        ));
    }
    if last {
        let answer = shell.actions.clone();
        let request_answer = request(shell);
        row = row.child(text_button(
            "answer",
            "Answer",
            Tone::Primary,
            armed,
            palette,
            move |_, _, _| answer.send(Action::AnswerQuestions(request_answer.clone())),
        ));
    } else {
        row = row.child(text_button(
            "next",
            "Next",
            Tone::Primary,
            true,
            palette,
            cx.listener(move |shell, _, _, cx| {
                shell.show_question(shown + 1);
                cx.notify();
            }),
        ));
    }
    let skip = shell.actions.clone();
    let request_skip = request(shell);
    row = row.child(text_button(
        "skip",
        if count > 1 { "Skip all" } else { "Skip" },
        Tone::Plain,
        armed,
        palette,
        move |_, _, _| {
            skip.send(Action::Deny {
                request: request_skip.clone(),
                feedback: String::new(),
            });
        },
    ));
    part.child(row).into_any_element()
}

fn header(question: &Question, at: usize) -> String {
    if question.header.is_empty() {
        format!("Question {}", at + 1)
    } else {
        question.header.clone()
    }
}

/// One question: its options (one, or any), what else, notes, and the preview of the option
/// picked, if the options have them.
fn question_entry(
    shell: &Shell,
    at: usize,
    question: &Question,
    palette: &Palette,
    window: &Window,
    cx: &Context<Shell>,
) -> impl IntoElement {
    let size = shell.page.text_size;
    // The arrow keys' option, marked while the options have the focus.
    let keyed = shell
        .areas
        .options
        .is_focused(window)
        .then_some(shell.option_at);
    let mut entry = div()
        .flex()
        .flex_col()
        .gap(px(6.0))
        .child(div().whitespace_normal().child(question.question.clone()));
    if question.multi {
        entry = entry.child(widgets::small("Pick any", palette.text_tertiary));
    }
    // Gone through with the arrow keys, as one stop for Tab (see `Shell::options_key`).
    let options = div()
        .id(SharedString::from(format!("options-{at}")))
        .track_focus(&shell.areas.options)
        .on_key_down(cx.listener(Shell::options_key))
        .flex()
        .flex_col()
        .gap(px(2.0));
    entry = entry.child(options.children(question.options.iter().enumerate().map(
        |(option, choice)| {
            let actions = shell.actions.clone();
            let mark = match (question.multi, choice.selected) {
                (true, true) => "square-check",
                (true, false) => "square",
                (false, true) => "circle-dot",
                (false, false) => "circle",
            };
            let mut row = div()
                .id(SharedString::from(format!("option-{at}-{option}")))
                .flex()
                .flex_row()
                .gap(px(8.0))
                .px(px(8.0))
                .py(px(6.0))
                .rounded(px(6.0))
                .cursor_pointer()
                .hover(|style| style.bg(palette.hover()))
                .on_click(move |_, _, _| {
                    actions.send(Action::ToggleOption {
                        question: at,
                        option,
                    });
                })
                .child(icon(
                    mark,
                    size,
                    if choice.selected {
                        palette.accent
                    } else {
                        palette.text_tertiary
                    },
                ))
                .child(
                    div()
                        .flex_1()
                        .flex()
                        .flex_col()
                        .child(choice.label.clone())
                        .child(widgets::small(
                            choice.description.clone(),
                            palette.text_tertiary,
                        )),
                );
            if keyed == Some(option) {
                row = row.border_1().border_color(palette.accent);
            }
            if choice.selected {
                row = row.bg(palette.selected());
            }
            row
        },
    )));
    if question.previews {
        let preview = question
            .options
            .iter()
            .find(|option| option.selected)
            .or_else(|| question.options.first())
            .map(|option| option.preview.clone())
            .unwrap_or_default();
        if !preview.is_empty() {
            entry = entry.child(
                div()
                    .rounded(px(6.0))
                    .bg(palette.code)
                    .p(px(8.0))
                    .font_family(shell.mono.clone())
                    .text_size(px(size * 0.9))
                    .whitespace_normal()
                    .child(preview),
            );
        }
    }
    if let Some(other) = shell.fields.others.get(at) {
        entry = entry.child(Input::new(other));
    }
    if let Some(notes) = shell.fields.notes.get(at) {
        entry = entry.child(Input::new(notes));
    }
    entry
}

/// Claude's plan: approved (with edits accepted or asked for), or sent back to keep planning,
/// with a word on the whole of it; reviewed part by part over the conversation.
fn plan_prompt(shell: &Shell, palette: &Palette, cx: &Context<Shell>) -> AnyElement {
    let prompt = &shell.prompt;
    let armed = shell.armed();
    let mut part = div().flex().flex_col().gap(px(8.0));
    match prompt.comment_count {
        0 => {}
        1 => {
            part = part.child(widgets::small(
                "1 comment on the plan goes with your answer.",
                palette.text_secondary,
            ));
        }
        count => {
            part = part.child(widgets::small(
                format!("{count} comments on the plan go with your answer."),
                palette.text_secondary,
            ));
        }
    }
    part = part.child(Textarea::new(&shell.fields.overall));
    let overall = shell.fields.overall.clone();
    let approve = |mode: PermissionMode| {
        let actions = shell.actions.clone();
        let request = request(shell);
        let overall = overall.clone();
        move |_: &gpui::ClickEvent, _: &mut Window, cx: &mut gpui::App| {
            actions.send(Action::ApprovePlan {
                request: request.clone(),
                mode,
                overall: overall.read(cx).value().to_string(),
            });
        }
    };
    let keep = shell.actions.clone();
    let request_keep = request(shell);
    let keep_overall = overall.clone();
    let reviewing = shell.page.reviewing;
    part.child(
        actions_row()
            .child(text_button(
                "approve-edits",
                "Approve, accept edits",
                Tone::Primary,
                armed,
                palette,
                approve(PermissionMode::AcceptEdits),
            ))
            .child(text_button(
                "approve-ask",
                "Approve, ask before edits",
                Tone::Plain,
                armed,
                palette,
                approve(PermissionMode::Default),
            ))
            .child(text_button(
                "keep-planning",
                "Keep planning",
                Tone::Plain,
                armed,
                palette,
                move |_, _, cx| {
                    keep.send(Action::Deny {
                        request: request_keep.clone(),
                        feedback: keep_overall.read(cx).value().to_string(),
                    });
                },
            ))
            .child(text_button(
                "review",
                if reviewing {
                    "Show the conversation"
                } else {
                    "Review the plan"
                },
                Tone::Plain,
                true,
                palette,
                cx.listener(|shell, _, _, cx| {
                    shell.page.reviewing = !shell.page.reviewing;
                    cx.notify();
                }),
            )),
    )
    .into_any_element()
}

/// The plan over the conversation, part by part, each with a comment to make or change.
pub(super) fn plan_review(shell: &Shell, cx: &Context<Shell>) -> AnyElement {
    let palette = shell.palette();
    let size = shell.page.text_size;
    let mut sections = Vec::new();
    for (at, section) in shell.prompt.plan.iter().enumerate() {
        sections.push(section_row(shell, at, section, &palette, cx));
    }
    div()
        .absolute()
        .top_0()
        .left_0()
        .size_full()
        .bg(palette.dim)
        .flex()
        .justify_center()
        .p(px(24.0))
        .child(
            div()
                .id("plan-review")
                .flex()
                .flex_col()
                .gap(px(12.0))
                .w_full()
                .max_w(px(size * 50.0))
                .h_full()
                .p(px(16.0))
                .rounded(px(12.0))
                .bg(palette.surface)
                .border_1()
                .border_color(palette.divider)
                .overflow_y_scrollbar()
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .items_center()
                        .child(
                            div()
                                .flex_1()
                                .text_size(px(size * 1.3))
                                .font_weight(FontWeight::SEMIBOLD)
                                .child("Claude's plan"),
                        )
                        .child(icon_button(
                            "close-review",
                            "x",
                            "Back to the conversation",
                            &palette,
                            size,
                            cx.listener(|shell, _, _, cx| {
                                shell.page.reviewing = false;
                                cx.notify();
                            }),
                        )),
                )
                .child(widgets::small(
                    "Comment on any part with the button beside it. Your comments go back with what you choose below.",
                    palette.text_secondary,
                ))
                .children(sections),
        )
        .into_any_element()
}

fn section_row(
    shell: &Shell,
    at: usize,
    section: &PlanSection,
    palette: &Palette,
    cx: &Context<Shell>,
) -> AnyElement {
    let size = shell.page.text_size;
    let commented = !section.comment.is_empty();
    let comment = section.comment.clone();
    let mut text = div()
        .flex_1()
        .min_w_0()
        .flex()
        .flex_col()
        .gap(px(6.0))
        .child(widgets::blocks(
            &format!("plan-{at}"),
            std::slice::from_ref(&section.block),
            &widgets::Ink {
                palette,
                actions: &shell.actions,
                size,
            },
        ));
    if commented && !section.editing {
        let remove = shell.actions.clone();
        text = text.child(
            div()
                .flex()
                .flex_row()
                .gap(px(6.0))
                .p(px(8.0))
                .rounded(px(6.0))
                .bg(palette.selected())
                .child(icon("message-square-text", size * 0.9, palette.accent))
                .child(div().flex_1().child(section.comment.clone()))
                .child(icon_button(
                    SharedString::from(format!("remove-comment-{at}")),
                    "trash-2",
                    "Remove the comment",
                    palette,
                    size * 0.8,
                    move |_, _, _| {
                        remove.send(Action::SetComment {
                            section: at,
                            text: String::new(),
                        });
                    },
                )),
        );
    }
    if section.editing
        && let Some(editor) = shell.fields.comments.get(at)
    {
        let save = shell.actions.clone();
        let cancel = shell.actions.clone();
        let saved = editor.clone();
        text = text.child(
            div()
                .flex()
                .flex_col()
                .gap(px(6.0))
                .child(Textarea::new(editor))
                .child(
                    actions_row()
                        .child(text_button(
                            SharedString::from(format!("cancel-comment-{at}")),
                            "Cancel",
                            Tone::Plain,
                            true,
                            palette,
                            move |_, _, _| cancel.send(Action::CancelComment),
                        ))
                        .child(text_button(
                            SharedString::from(format!("save-comment-{at}")),
                            "Comment",
                            Tone::Primary,
                            true,
                            palette,
                            move |_, _, cx| {
                                save.send(Action::SetComment {
                                    section: at,
                                    text: saved.read(cx).value().to_string(),
                                });
                            },
                        )),
                ),
        );
    }
    let editor = shell.fields.comments.get(at).cloned();
    div()
        .flex()
        .flex_row()
        .gap(px(8.0))
        .child(text)
        .child(icon_button(
            SharedString::from(format!("comment-{at}")),
            if commented {
                "message-square-text"
            } else {
                "message-square-plus"
            },
            if commented {
                "Edit the comment"
            } else {
                "Comment on this part"
            },
            palette,
            size * 0.9,
            cx.listener(move |shell, _, window, cx| {
                if let Some(editor) = &editor {
                    let comment = comment.clone();
                    editor.update(cx, |state, cx| {
                        state.set_value(comment, window, cx);
                        state.focus(window, cx);
                    });
                }
                shell.actions.send(Action::EditComment(at));
            }),
        ))
        .into_any_element()
}
