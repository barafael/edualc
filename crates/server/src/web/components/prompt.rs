//! What Claude waits for, above the composer (`ui/app.slint`'s `PromptBar`): may a tool run, its
//! questions answered, its plan approved. Its buttons take clicks only a moment after a request
//! appears: a click meant for what was under the pointer (another window, as this one opened)
//! must not answer it. The plan's review (`ui/plan.slint`) shows from here too, in the
//! transcript's place.

use dioxus::prelude::*;
use edualc_core::app::{Action, PermissionMode};
use edualc_core::external::wire::RequestId;
use edualc_core::present::model::{PlanSection, PromptKind, Question};

use crate::web::View;
use crate::web::dom::{focus, focus_at_end, grab_focus};
use crate::web::text::Block;
use crate::web::widgets::{Button, Icon, IconButton, Lucide};

/// How long a request shows before its buttons take clicks, in milliseconds.
const ARM_DELAY_MS: u32 = 700;

/// The focus coming into the prompt: as it opens (unless a draft is being written, where the
/// keys would go on into it), and as the arrow keys go to another question. It goes to what the
/// prompt asks for first: the question's options, the comment on the plan, or what to do instead
/// of a tool, never straight to a button. What takes it is made with the prompt or there
/// already, so it is asked by a flag and a count, and what takes it clears the flag.
#[derive(Clone, Copy, PartialEq)]
struct Grab {
    pending: Signal<bool>,
    serial: Signal<u32>,
}

impl Grab {
    fn ask(mut self) {
        self.pending.set(true);
        *self.serial.write() += 1;
    }

    /// Whether the focus is still to be taken; the effect asking runs again at every ask.
    fn asked(self) -> bool {
        (self.serial)();
        *self.pending.peek()
    }

    fn took(mut self) {
        self.pending.set(false);
    }
}

/// The request shown as `serial`, as an answer names it, if it is still the one waiting: an
/// answer meant for a request that went away is not given to the next.
fn request(view: View, serial: i32) -> Option<RequestId> {
    let prompt = view.prompt.peek();
    (prompt.serial == serial).then(|| RequestId::from(prompt.request.clone()))
}

/// Hands the app the answer `made` makes of the request shown as `serial`, if it still waits.
fn answer(view: View, serial: i32, made: impl FnOnce(RequestId) -> Action) {
    if let Some(request) = request(view, serial) {
        view.act(made(request));
    }
}

/// What Claude waits for: its heading, then what it asks.
#[component]
pub fn PromptBar() -> Element {
    let view = use_context::<View>();
    let serial = use_memo(move || view.prompt.read().serial);
    let reviewing = use_memo(move || view.page.read().reviewing);
    // The request whose buttons are armed.
    let mut armed_for = use_signal(|| None::<i32>);
    // The question shown: they come one at a time.
    let mut current = use_signal(|| 0_usize);
    let grab = Grab {
        pending: use_signal(|| false),
        serial: use_signal(|| 0),
    };
    // A fresh request: its buttons arm a moment later, its first question shows, and the focus
    // comes into it.
    use_effect(move || {
        let serial = serial();
        current.set(0);
        spawn(async move {
            crate::web::platform::sleep(ARM_DELAY_MS).await;
            if view.prompt.peek().serial == serial {
                armed_for.set(Some(serial));
            }
        });
        if view.draft.peek().is_empty() {
            grab.ask();
        }
    });
    // F6 coming into what is written: the prompt, while one is shown.
    let wanted = use_memo(move || view.page.read().focus_prompt);
    let mut seen = use_signal(|| *wanted.peek());
    use_effect(move || {
        let wanted = wanted();
        if wanted != *seen.peek() {
            seen.set(wanted);
            grab.ask();
        }
    });
    let prompt = view.prompt.read();
    let armed = armed_for() == Some(prompt.serial);
    let icon = match prompt.kind {
        PromptKind::Question => Lucide::MessageCircleQuestionMark,
        PromptKind::Plan => Lucide::ListChecks,
        PromptKind::Tool | PromptKind::None => Lucide::ShieldQuestionMark,
    };
    rsx! {
        if prompt.kind == PromptKind::Plan && reviewing() {
            PlanReview {}
        }
        section { class: "prompt", "aria-labelledby": "prompt-title",
            div { class: "prompt-heading",
                Icon { icon: icon }
                div { id: "prompt-title", class: "prompt-title", "{prompt.title}" }
                if prompt.queued > 0 {
                    div { class: "prompt-note", "{prompt.queued} more waiting" }
                }
            }
            {
                match prompt.kind {
                    PromptKind::Tool => rsx! {
                        ToolPrompt { armed, grab }
                    },
                    PromptKind::Question => rsx! {
                        QuestionsPrompt { armed, grab, current }
                    },
                    PromptKind::Plan => rsx! {
                        PlanPrompt { armed, grab }
                    },
                    PromptKind::None => rsx! {},
                }
            }
        }
    }
}

/// May a tool run: what it would do, then Allow, Always allow (the rule remembered) or Deny, with
/// what to do instead.
#[component]
fn ToolPrompt(armed: bool, grab: Grab) -> Element {
    let view = use_context::<View>();
    let mut feedback = use_signal(String::new);
    use_effect(move || {
        if grab.asked() {
            grab.took();
            grab_focus("prompt-feedback");
        }
    });
    let prompt = view.prompt.read();
    let summary_class = if prompt.summary_prose {
        "prompt-summary prose"
    } else {
        "prompt-summary code"
    };
    let always = prompt.always_label.clone();
    let serial = prompt.serial;
    rsx! {
        div { class: "prompt-part",
            if !prompt.summary.is_empty() {
                div { class: summary_class, "{prompt.summary}" }
            }
            if !prompt.detail.is_empty() {
                div { class: "prompt-note prompt-wrap", "{prompt.detail}" }
            }
            div { class: "prompt-actions",
                Button {
                    primary: true,
                    disabled: !armed,
                    onclick: move |_| answer(view, serial, Action::Allow),
                    "Allow"
                }
                // A long rule gives way in a narrow window, said in full on hover and to
                // assistive technology; Allow and Deny keep their words.
                if !always.is_empty() {
                    button {
                        class: "button shortens",
                        title: "{always}",
                        disabled: !armed,
                        onclick: move |_| answer(view, serial, Action::AllowAlways),
                        span { "{always}" }
                    }
                }
                Button {
                    danger: true,
                    disabled: !armed,
                    onclick: move |_| {
                        let feedback_text = feedback();
                        answer(view, serial, |request| Action::Deny {
                            request,
                            feedback: feedback_text,
                        });
                        feedback.set(String::new());
                    },
                    "Deny"
                }
            }
            // On a line of its own, so that a narrow or zoomed window doesn't push it out of
            // sight beside a long "Always allow". Enter denies with what was written, as Deny
            // does: once armed like the buttons, and not with nothing written.
            input {
                id: "prompt-feedback",
                class: "field prompt-field",
                r#type: "text",
                placeholder: "Or tell Claude what to do instead",
                "aria-label": "Or tell Claude what to do instead",
                value: "{feedback}",
                oninput: move |event| feedback.set(event.value()),
                onkeydown: move |event| {
                    if event.key() == Key::Enter && !event.is_composing() {
                        let text = feedback();
                        if armed && !text.is_empty() {
                            answer(view, serial, |request| Action::Deny {
                                request,
                                feedback: text,
                            });
                            feedback.set(String::new());
                        }
                    }
                },
            }
        }
    }
}

/// Claude's questions, one at a time, as in the terminal: a tab for each, marked once answered,
/// and Back and Next between them. The others stay, folded away, with what was written in them.
#[component]
fn QuestionsPrompt(armed: bool, grab: Grab, current: Signal<usize>) -> Element {
    let view = use_context::<View>();
    let prompt = view.prompt.read();
    let count = prompt.questions.len();
    let shown = current().min(count.saturating_sub(1));
    let last = shown + 1 >= count;
    let serial = prompt.serial;
    rsx! {
        div { class: "prompt-part prompt-questions",
            if count > 1 {
                // Left and Right in the options go between them.
                div { class: "prompt-tabs", role: "tablist", "aria-label": "Questions",
                    for (q, question) in prompt.questions.iter().enumerate() {
                        button {
                            key: "{q}",
                            id: "prompt-tab-{q}",
                            class: if q == shown { "prompt-tab selected" } else { "prompt-tab" },
                            role: "tab",
                            tabindex: "-1",
                            "aria-selected": "{q == shown}",
                            "aria-controls": "prompt-question-{q}",
                            "aria-describedby": "prompt-tab-{q}-state",
                            onclick: move |_| current.set(q),
                            {answered_mark(question.answered)}
                            span { "{header(question, q)}" }
                            span { id: "prompt-tab-{q}-state", hidden: true,
                                if question.answered {
                                    "Answered"
                                } else {
                                    "Not answered yet"
                                }
                            }
                        }
                    }
                }
            }
            // What doesn't fit scrolls; the tabs and the buttons stay in view.
            div { class: "prompt-body",
                for (q, question) in prompt.questions.iter().enumerate() {
                    QuestionEntry {
                        key: "{serial}-{q}",
                        q,
                        question: question.clone(),
                        shown: q == shown,
                        tabbed: count > 1,
                        count,
                        current,
                        grab,
                    }
                }
            }
            // Next until the last question, which is answered; Skip leaves them all.
            div { class: "prompt-actions",
                if shown > 0 {
                    Button { onclick: move |_| current.set(shown - 1), "Back" }
                }
                if last {
                    Button {
                        primary: true,
                        disabled: !armed,
                        onclick: move |_| answer(view, serial, Action::AnswerQuestions),
                        "Answer"
                    }
                } else {
                    Button {
                        primary: true,
                        onclick: move |_| current.set(shown + 1),
                        "Next"
                    }
                }
                Button {
                    disabled: !armed,
                    onclick: move |_| {
                        answer(view, serial, |request| Action::Deny {
                            request,
                            feedback: String::new(),
                        });
                    },
                    if count > 1 {
                        "Skip all"
                    } else {
                        "Skip"
                    }
                }
            }
        }
    }
}

/// A tab's mark: whether its question is answered.
fn answered_mark(answered: bool) -> Element {
    if answered {
        rsx! {
            Icon { icon: Lucide::CircleCheck, class: "small answered" }
        }
    } else {
        rsx! {
            Icon { icon: Lucide::Circle, class: "small" }
        }
    }
}

/// A question's name on its tab: its header, or its place.
fn header(question: &Question, q: usize) -> String {
    if question.header.is_empty() {
        format!("Question {}", q + 1)
    } else {
        question.header.clone()
    }
}

/// To the question at `to` (the nearest there is), the keys with it.
fn go(mut current: Signal<usize>, count: usize, grab: Grab, to: usize) {
    current.set(to.min(count.saturating_sub(1)));
    grab.ask();
}

/// One question: what is asked, its options (with previews beside them, where it has them),
/// something else written instead, and notes going with the answer.
#[component]
fn QuestionEntry(
    q: usize,
    question: Question,
    shown: bool,
    tabbed: bool,
    count: usize,
    current: Signal<usize>,
    grab: Grab,
) -> Element {
    let view = use_context::<View>();
    // The option the arrow keys are on.
    let mut at = use_signal(|| 0_usize);
    // The option last pointed at, and last picked: the preview shows the one pointed at, else
    // the one the keys are on, else the one picked, else the first.
    let mut pointed = use_signal(|| None::<usize>);
    let mut picked = use_signal(|| None::<usize>);
    let mut focused = use_signal(|| false);
    use_effect(move || {
        if grab.asked() && q == *current.peek() {
            grab.took();
            focus(&format!("prompt-options-{q}"));
        }
    });
    let multi = question.multi;
    let options = question.options.len();
    let selected: Vec<bool> = question
        .options
        .iter()
        .map(|option| option.selected)
        .collect();
    let mut pick = move |option: usize| {
        at.set(option);
        picked.set(Some(option));
        view.act(Action::ToggleOption {
            question: q,
            option,
        });
    };
    let keys_on = focused().then_some(at());
    let preview_at = pointed().or(keys_on).or(picked()).unwrap_or(0);
    let preview = question
        .options
        .get(preview_at)
        .map(|option| option.preview.clone())
        .unwrap_or_default();
    rsx! {
        div {
            id: "prompt-question-{q}",
            class: "prompt-question",
            role: if tabbed { "tabpanel" },
            "aria-labelledby": if tabbed { "prompt-tab-{q}" },
            hidden: !shown,
            div { class: "prompt-wrap", "{question.question}" }
            if multi {
                div { class: "caption", "Pick any" }
            }
            div { class: "prompt-choice",
                // One stop for Tab: Up and Down go through the options and Space picks one;
                // Enter picks one too, and for a single choice goes on to the next question.
                // Left and Right go between the questions. A screen reader is on the option the
                // arrow keys are on.
                div {
                    id: "prompt-options-{q}",
                    class: "prompt-options",
                    tabindex: "0",
                    role: if multi { "group" } else { "radiogroup" },
                    "aria-label": "{question.question}",
                    "aria-activedescendant": "prompt-option-{q}-{at}",
                    onfocus: move |_| focused.set(true),
                    onblur: move |_| focused.set(false),
                    onkeydown: move |event| {
                        let key = event.key();
                        let handled = if key == Key::ArrowDown {
                            at.set((at() + 1).min(options.saturating_sub(1)));
                            true
                        } else if key == Key::ArrowUp {
                            at.set(at().saturating_sub(1));
                            true
                        } else if key == Key::Character(" ".to_owned()) {
                            pick(at());
                            true
                        } else if key == Key::Enter {
                            if multi {
                                pick(at());
                            } else {
                                if !selected.get(at()).copied().unwrap_or(false) {
                                    pick(at());
                                }
                                go(current, count, grab, q + 1);
                            }
                            true
                        } else if key == Key::ArrowRight {
                            go(current, count, grab, q + 1);
                            true
                        } else if key == Key::ArrowLeft {
                            go(current, count, grab, q.saturating_sub(1));
                            true
                        } else {
                            false
                        };
                        if handled {
                            event.prevent_default();
                        }
                    },
                    for (o, option) in question.options.iter().enumerate() {
                        div {
                            key: "{o}",
                            id: "prompt-option-{q}-{o}",
                            class: option_class(option.selected, focused() && o == at()),
                            role: if multi { "checkbox" } else { "radio" },
                            "aria-checked": "{option.selected}",
                            "aria-label": "{option.label}",
                            "aria-describedby": if !option.description.is_empty() { "prompt-option-{q}-{o}-description" },
                            // A click doesn't take the focus, so writing goes on where it was.
                            onmousedown: move |event| event.prevent_default(),
                            onmouseenter: move |_| pointed.set(Some(o)),
                            onclick: move |_| pick(o),
                            Icon { icon: option_icon(multi, option.selected) }
                            div { class: "prompt-option-text",
                                div { class: "prompt-option-label", "{option.label}" }
                                if !option.description.is_empty() {
                                    div {
                                        id: "prompt-option-{q}-{o}-description",
                                        class: "prompt-note",
                                        "{option.description}"
                                    }
                                }
                            }
                        }
                    }
                }
                // As it is laid out (code, a mockup): not wrapped, and cut at the box's edge.
                if question.previews {
                    div { class: "prompt-preview",
                        pre { "{preview}" }
                    }
                }
            }
            input {
                class: "field prompt-field",
                r#type: "text",
                placeholder: "Something else",
                "aria-label": "Something else",
                oninput: move |event| {
                    view.act(Action::SetOther {
                        question: q,
                        text: event.value(),
                    });
                },
            }
            input {
                class: "field prompt-field",
                r#type: "text",
                placeholder: "Notes for Claude, with the answer (optional)",
                "aria-label": "Notes for Claude, with the answer (optional)",
                oninput: move |event| {
                    view.act(Action::SetNotes {
                        question: q,
                        text: event.value(),
                    });
                },
            }
        }
    }
}

/// An option's look: picked, and ringed where the arrow keys are on it (the hover's colour
/// alone doesn't show on one picked).
fn option_class(selected: bool, ringed: bool) -> &'static str {
    match (selected, ringed) {
        (true, true) => "prompt-option selected ringed",
        (true, false) => "prompt-option selected",
        (false, true) => "prompt-option ringed",
        (false, false) => "prompt-option",
    }
}

/// An option's mark: a box for one of any number, a circle for a single choice.
fn option_icon(multi: bool, selected: bool) -> Lucide {
    match (multi, selected) {
        (true, true) => Lucide::SquareCheck,
        (true, false) => Lucide::Square,
        (false, true) => Lucide::CircleCheck,
        (false, false) => Lucide::Circle,
    }
}

/// Claude's plan: how many comments go with the answer, one on the whole plan, approving it
/// (with edits accepted, or asked before each) or planning on, and the review of it.
#[component]
fn PlanPrompt(armed: bool, grab: Grab) -> Element {
    let view = use_context::<View>();
    let mut page = view.page;
    let mut overall = use_signal(String::new);
    let reviewing = use_memo(move || view.page.read().reviewing);
    use_effect(move || {
        if grab.asked() {
            grab.took();
            grab_focus("prompt-overall");
        }
    });
    let (count, serial) = {
        let prompt = view.prompt.read();
        (prompt.comment_count, prompt.serial)
    };
    let mut approve = move |accept_edits: bool| {
        let mode = if accept_edits {
            PermissionMode::AcceptEdits
        } else {
            PermissionMode::Default
        };
        let comment = overall();
        answer(view, serial, |request| Action::ApprovePlan {
            request,
            mode,
            overall: comment,
        });
        overall.set(String::new());
    };
    rsx! {
        div { class: "prompt-part",
            if count == 1 {
                div { class: "prompt-note", "1 comment on the plan goes with your answer." }
            } else if count > 1 {
                div { class: "prompt-note", "{count} comments on the plan go with your answer." }
            }
            input {
                id: "prompt-overall",
                class: "field prompt-field",
                r#type: "text",
                placeholder: "A comment on the whole plan (optional)",
                "aria-label": "A comment on the whole plan (optional)",
                value: "{overall}",
                oninput: move |event| overall.set(event.value()),
            }
            div { class: "prompt-actions",
                Button {
                    primary: true,
                    disabled: !armed,
                    onclick: move |_| approve(true),
                    "Approve, accept edits"
                }
                Button {
                    disabled: !armed,
                    onclick: move |_| approve(false),
                    "Approve, ask before edits"
                }
                Button {
                    disabled: !armed,
                    onclick: move |_| {
                        let comment = overall();
                        answer(view, serial, |request| Action::Deny {
                            request,
                            feedback: comment,
                        });
                        overall.set(String::new());
                    },
                    "Keep planning"
                }
                div { class: "prompt-spacer" }
                Button {
                    onclick: move |_| {
                        let shown = page.peek().reviewing;
                        page.write().reviewing = !shown;
                    },
                    if reviewing() {
                        "Show the conversation"
                    } else {
                        "Review the plan"
                    }
                }
            }
        }
    }
}

/// Reviewing Claude's plan before approving it, in the transcript's place: the plan part by
/// part, a comment on any part, and the comments going back with the answer chosen in the bar
/// below.
#[component]
fn PlanReview() -> Element {
    let view = use_context::<View>();
    let mut page = view.page;
    let prompt = view.prompt.read();
    rsx! {
        section { class: "plan-review", "aria-labelledby": "plan-review-title",
            div { class: "plan-review-header",
                Icon { icon: Lucide::ListChecks, class: "plan-review-icon" }
                div { class: "plan-review-titles",
                    h2 { id: "plan-review-title", class: "plan-review-title", "Claude's plan" }
                    div { class: "caption plan-review-caption",
                        "Comment on any part with the button beside it. Your comments go back with what you choose below."
                    }
                }
                IconButton {
                    icon: Lucide::X,
                    label: "Back to the conversation".to_owned(),
                    onclick: move |_| page.write().reviewing = false,
                }
            }
            div { class: "plan-review-sections",
                for (index, section) in prompt.plan.iter().enumerate() {
                    SectionRow { key: "{index}", index, section: section.clone() }
                }
            }
        }
    }
}

/// A part of the plan: its block, the comment on it (or the comment being written), and the
/// button beside it to comment.
#[component]
fn SectionRow(index: usize, section: PlanSection) -> Element {
    let view = use_context::<View>();
    let commented = !section.comment.is_empty();
    let excerpt = section.excerpt.clone();
    let (comment_icon, comment_class, comment_label) = if commented {
        (
            Lucide::MessageSquareText,
            "plan-comment-button commented",
            format!("Edit the comment on: {excerpt}"),
        )
    } else {
        (
            Lucide::MessageSquarePlus,
            "plan-comment-button",
            format!("Comment on: {excerpt}"),
        )
    };
    rsx! {
        div { class: "plan-section",
            div { class: "plan-section-body",
                Block { block: section.block.clone() }
                if commented && !section.editing {
                    div { class: "plan-comment",
                        Icon { icon: Lucide::MessageSquareText, class: "small plan-comment-mark" }
                        div { class: "plan-comment-text", "{section.comment}" }
                        IconButton {
                            icon: Lucide::Pencil,
                            label: format!("Edit the comment on: {excerpt}"),
                            class: "small",
                            onclick: move |_| view.act(Action::EditComment(index)),
                        }
                        IconButton {
                            icon: Lucide::Trash,
                            label: format!("Remove the comment on: {excerpt}"),
                            class: "small",
                            onclick: move |_| {
                                view.act(Action::SetComment {
                                    section: index,
                                    text: String::new(),
                                });
                            },
                        }
                    }
                }
                if section.editing {
                    CommentEditor {
                        index,
                        text: section.comment.clone(),
                        about: section.excerpt,
                    }
                }
            }
            IconButton {
                icon: comment_icon,
                label: comment_label,
                class: comment_class,
                onclick: move |_| view.act(Action::EditComment(index)),
            }
        }
    }
}

/// Writing a comment on a part of the plan: Ctrl+Enter keeps it, Esc drops the edit. `about` is
/// the part's first words, naming the editor for assistive technology.
#[component]
fn CommentEditor(index: usize, text: String, about: String) -> Element {
    let view = use_context::<View>();
    let mut written = use_signal(|| text);
    use_effect(move || focus_at_end(&format!("plan-comment-{index}")));
    let save = move || {
        view.act(Action::SetComment {
            section: index,
            text: written.peek().clone(),
        });
    };
    let cancel = move || view.act(Action::CancelComment);
    rsx! {
        div { class: "plan-editor",
            textarea {
                id: "plan-comment-{index}",
                class: "plan-editor-input",
                rows: "2",
                "aria-label": "Comment on: {about}",
                "aria-placeholder": "Your comment on this part",
                value: "{written}",
                oninput: move |event| written.set(event.value()),
                onkeydown: move |event| {
                    let key = event.key();
                    if key == Key::Enter && event.modifiers().ctrl() {
                        event.prevent_default();
                        save();
                    } else if key == Key::Escape {
                        event.prevent_default();
                        cancel();
                    }
                },
            }
            div { class: "plan-editor-actions",
                div { class: "caption", "Ctrl+Enter to keep, Esc to drop" }
                Button { onclick: move |_| cancel(), "Cancel" }
                Button { primary: true, onclick: move |_| save(), "Comment" }
            }
        }
    }
}
