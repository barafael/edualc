//! What the session changed in its folder, in place of the conversation (`ui/changes.slint`):
//! the files a turn, the turns since you looked, or the whole session changed, who changed
//! each, the diff of the one chosen, a mark for each file reviewed, and comments on lines, sent
//! to Claude together.

use dioxus::prelude::*;
use edualc_core::app::Action;
use edualc_core::changes::Scope;
use edualc_core::present::model::{
    ChangeKind, ChangeReview, ChangeScope, ChangeWho, ChangedFile, DiffRow, DiffRowKind,
};

use super::prompt::CommentEditor;
use crate::web::View;
use crate::web::widgets::{Button, Icon, IconButton, Lucide};

/// The view's id: where F6 brings the focus while it is over the conversation.
pub(super) const PANEL: &str = "changes";

/// Who changed a file, in words.
fn who_words(who: ChangeWho) -> &'static str {
    match who {
        ChangeWho::Claude => "Claude's edit",
        ChangeWho::Command => "by a command",
        ChangeWho::Both => "Claude and a command",
        ChangeWho::BetweenTurns => "between turns",
    }
}

/// The line a row's comment is on, by its number: the new file's, or the old one's for a line
/// taken out.
fn line_of(row: &DiffRow) -> &str {
    if row.new.is_empty() {
        &row.old
    } else {
        &row.new
    }
}

/// The view, over the conversation's place.
#[component]
pub fn ChangesPanel() -> Element {
    let view = use_context::<View>();
    let changes = view.changes.read();
    let scope = changes.scope;
    let send = match changes.comments {
        1 => "Send the comment to Claude".to_owned(),
        count => format!("Send {count} comments to Claude"),
    };
    rsx! {
        section {
            id: PANEL,
            class: "changes",
            tabindex: "-1",
            "aria-label": "Changes",
            div { class: "changes-head",
                div { class: "changes-names",
                    h2 { class: "changes-title", "Changes" }
                    span { class: "caption trimmed", "{changes.summary}" }
                }
                div { class: "changes-scopes", role: "group", "aria-label": "Which changes",
                    ScopeButton { scope: ChangeScope::Turn, chosen: scope, "Last turn" }
                    ScopeButton { scope: ChangeScope::SinceLooked, chosen: scope, "Since you looked" }
                    ScopeButton { scope: ChangeScope::Session, chosen: scope, "Whole session" }
                }
                IconButton {
                    icon: Lucide::X,
                    label: "Back to the conversation (Esc)",
                    onclick: move |_| view.act(Action::CloseChanges),
                }
            }
            if !changes.note.is_empty() {
                p { class: "changes-note", "{changes.note}" }
            }
            // Nothing changed: the summary says so, and there is nothing to choose from.
            if !changes.files.is_empty() {
                div { class: "changes-body",
                    div { class: "changes-files", role: "listbox", "aria-label": "Changed files",
                        for (index , file) in changes.files.iter().enumerate() {
                            FileRow { key: "{file.path}", file: file.clone(), index }
                        }
                    }
                    DiffPane {}
                }
            }
            if changes.comments > 0 {
                div { class: "changes-foot",
                    span { class: "caption", "Each goes with its file, line and the line's text." }
                    Button {
                        primary: true,
                        onclick: move |_| view.act(Action::SendLineComments),
                        "{send}"
                    }
                }
            }
        }
    }
}

/// One of the three scopes: pressed while it is the one shown.
#[component]
fn ScopeButton(scope: ChangeScope, chosen: ChangeScope, children: Element) -> Element {
    let view = use_context::<View>();
    let asked = match scope {
        ChangeScope::Turn => Scope::Turn,
        ChangeScope::SinceLooked => Scope::SinceLooked,
        ChangeScope::Session => Scope::Session,
    };
    rsx! {
        button {
            class: "changes-scope",
            r#type: "button",
            "aria-pressed": "{scope == chosen}",
            onclick: move |_| view.act(Action::ChangesScope(asked)),
            {children}
        }
    }
}

/// A changed file: what happened to it, who did it, how much, and whether you reviewed it.
#[component]
fn FileRow(file: ChangedFile, index: usize) -> Element {
    let view = use_context::<View>();
    let (icon, kind) = match file.kind {
        ChangeKind::Added => (Lucide::FilePlus, "added"),
        ChangeKind::Deleted => (Lucide::FileMinus, "deleted"),
        ChangeKind::Changed => (Lucide::FileDiff, "changed"),
    };
    let who = who_words(file.who);
    let reviewed = match file.review {
        ChangeReview::NotReviewed => "",
        ChangeReview::Reviewed => ", reviewed",
        ChangeReview::ChangedSince => ", changed since you reviewed it",
    };
    let comments = match file.comments {
        0 => String::new(),
        1 => "1 comment".to_owned(),
        count => format!("{count} comments"),
    };
    rsx! {
        button {
            class: "changes-file {kind}",
            class: if file.selected { "selected" },
            r#type: "button",
            role: "option",
            "aria-selected": "{file.selected}",
            "aria-label": "{file.path}",
            "aria-description": "{who}, +{file.added} −{file.removed}{reviewed}",
            title: "{file.path}",
            onclick: move |_| view.act(Action::SelectChangedFile(index)),
            span { class: "changes-file-kind",
                Icon { icon, class: "small" }
            }
            span { class: "changes-file-texts",
                span { class: "changes-file-path", "{file.path}" }
                span { class: "changes-file-facts caption",
                    span { "{who}" }
                    span { class: "changes-added", "+{file.added}" }
                    span { class: "changes-removed", "−{file.removed}" }
                    if !comments.is_empty() {
                        span { "{comments}" }
                    }
                }
            }
            match file.review {
                ChangeReview::NotReviewed => rsx! {},
                ChangeReview::Reviewed => rsx! {
                    span { class: "changes-reviewed",
                        Icon { icon: Lucide::Check, class: "small" }
                    }
                },
                ChangeReview::ChangedSince => rsx! {
                    span { class: "changes-reviewed stale",
                        Icon { icon: Lucide::RefreshCw, class: "small" }
                    }
                },
            }
        }
    }
}

/// The file chosen: its path, the mark that you reviewed it, and how it changed.
#[component]
fn DiffPane() -> Element {
    let view = use_context::<View>();
    let changes = view.changes.read();
    let (mark, mark_label) = match changes.file_review {
        ChangeReview::Reviewed => ("Reviewed", "Reviewed: press to take the mark off"),
        ChangeReview::ChangedSince => ("Changed since: mark again", "Changed since: mark again"),
        ChangeReview::NotReviewed => ("Mark reviewed", "Mark reviewed"),
    };
    rsx! {
        div { class: "changes-diff",
            if !changes.file_path.is_empty() {
                div { class: "changes-diff-head",
                    span { class: "changes-diff-path trimmed", title: "{changes.file_path}",
                        "{changes.file_path}"
                    }
                    button {
                        class: "button",
                        r#type: "button",
                        "aria-label": "{mark_label}",
                        "aria-pressed": "{changes.file_review == ChangeReview::Reviewed}",
                        onclick: move |_| view.act(Action::ToggleReviewed),
                        if changes.file_review == ChangeReview::Reviewed {
                            Icon { icon: Lucide::Check }
                        }
                        "{mark}"
                    }
                }
            }
            if !changes.diff_note.is_empty() {
                p { class: "changes-note", "{changes.diff_note}" }
            }
            div { class: "changes-rows",
                for (index , row) in changes.diff.iter().enumerate() {
                    Row { key: "{index}", row: row.clone(), index }
                }
            }
        }
    }
}

/// A line of the diff, with its numbers in the old file and the new; your comment on it under
/// it, or the editor it is written in.
#[component]
fn Row(row: DiffRow, index: usize) -> Element {
    let view = use_context::<View>();
    let (kind, sign) = match row.kind {
        DiffRowKind::Same => ("", ""),
        DiffRowKind::Added => ("added", "+"),
        DiffRowKind::Removed => ("removed", "−"),
        DiffRowKind::Gap => ("gap", ""),
    };
    let line = line_of(&row).to_owned();
    let label = if row.comment.is_empty() {
        format!("Comment on line {line}")
    } else {
        format!("Edit the comment on line {line}")
    };
    rsx! {
        div { class: "changes-row {kind}",
            span { class: "changes-number", "{row.old}" }
            span { class: "changes-number", "{row.new}" }
            span { class: "changes-sign", "aria-hidden": "true", "{sign}" }
            if row.kind == DiffRowKind::Gap {
                span { class: "changes-text", "⋯" }
            } else {
                span { class: "changes-text", "{row.text}" }
                IconButton {
                    icon: Lucide::MessageSquarePlus,
                    class: if row.comment.is_empty() { "tiny changes-comment-button" } else { "tiny changes-comment-button commented" },
                    label,
                    onclick: move |_| view.act(Action::EditLineComment(index)),
                }
            }
        }
        if row.editing {
            CommentEditor {
                id: "line-comment-{index}",
                class: "changes-comment",
                text: row.comment,
                label: "Comment on line {line}",
                placeholder: "Your comment on this line",
                onsave: move |text| view.act(Action::SetLineComment(index, text)),
                oncancel: move |()| view.act(Action::CancelLineComment),
            }
        } else if !row.comment.is_empty() {
            div { class: "plan-comment changes-comment",
                span { class: "plan-comment-mark",
                    Icon { icon: Lucide::MessageSquare, class: "small" }
                }
                span { class: "plan-comment-text", "{row.comment}" }
                IconButton {
                    icon: Lucide::X,
                    label: "Remove the comment",
                    onclick: move |_| view.act(Action::SetLineComment(index, String::new())),
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A comment is on the new file's line, or the old one's for a line taken out.
    #[test]
    fn a_comment_names_the_new_line_or_the_old_one() {
        let row = |old: &str, new: &str| DiffRow {
            old: old.to_owned(),
            new: new.to_owned(),
            ..DiffRow::default()
        };
        assert_eq!(line_of(&row("3", "4")), "4");
        assert_eq!(line_of(&row("3", "")), "3");
    }
}
