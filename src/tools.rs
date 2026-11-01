//! How a tool call reads in the chat: its name, a one-line summary (the command, the file),
//! the input worth showing when expanded, and for edits a diff.

use std::path::Path;

use serde_json::Value;

use crate::external::wire::{ContentBlock, ToolName, UserContent};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DiffKind {
    Context,
    Added,
    Removed,
    Hunk,
}

#[derive(Debug, Default)]
pub(crate) struct ToolView {
    pub(crate) title: String,
    pub(crate) summary: String,
    /// Shown when expanded, as plain monospace text; empty if the summary says it all.
    pub(crate) detail: String,
    pub(crate) diff: Vec<(DiffKind, String)>,
    /// Markdown shown when expanded (ExitPlanMode's plan).
    pub(crate) markdown: String,
}

/// How much of a call to describe: its line alone (title and summary), or also what an
/// expanded card shows (a diff, a file, the input).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Depth {
    Headline,
    Full,
}

/// Diffs longer than this are cut; the full change is in the file.
const MAX_DIFF_LINES: usize = 400;

pub(crate) fn describe(name: &ToolName, input: &Value, cwd: &Path, depth: Depth) -> ToolView {
    let full = depth == Depth::Full;
    let field = |key: &str| str_field(input, key);
    let path = |key: &str| short_path(field(key), cwd);
    let mut view = ToolView {
        title: name.to_string(),
        ..ToolView::default()
    };
    match name {
        ToolName::Bash => {
            let command = field("command");
            view.summary = first_line(command);
            if command.contains('\n') || command.chars().count() > 90 {
                view.detail = command.to_owned();
            }
            if !field("description").is_empty() {
                view.detail = if view.detail.is_empty() {
                    format!("# {}", field("description"))
                } else {
                    format!("# {}\n{}", field("description"), view.detail)
                };
            }
            if input.get("run_in_background").and_then(Value::as_bool) == Some(true) {
                view.title = "Bash (background)".into();
            }
        }
        ToolName::Read => {
            view.summary = path("file_path");
            if let Some(offset) = input.get("offset").and_then(Value::as_u64) {
                let limit = input.get("limit").and_then(Value::as_u64).unwrap_or(0);
                let range = if limit > 0 {
                    format!("lines {offset}–{}", offset + limit)
                } else {
                    format!("from line {offset}")
                };
                view.summary = format!("{}  {range}", view.summary);
            }
        }
        // The whole file, as it will be: a wall of `+` lines would say the same, louder.
        ToolName::Write => {
            view.summary = path("file_path");
            if full {
                view.detail = field("content").to_owned();
            }
        }
        ToolName::Edit => {
            view.summary = path("file_path");
            if input.get("replace_all").and_then(Value::as_bool) == Some(true) {
                view.summary += "  (all occurrences)";
            }
            if full {
                view.diff = diff(field("old_string"), field("new_string"));
            }
        }
        ToolName::MultiEdit => {
            view.summary = path("file_path");
            for edit in input
                .get("edits")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter(|_| full)
            {
                let edited = |key: &str| edit.get(key).and_then(Value::as_str).unwrap_or("");
                view.diff
                    .extend(diff(edited("old_string"), edited("new_string")));
            }
        }
        // The cell's new source, as it will be: what is approved is all shown.
        ToolName::NotebookEdit => {
            view.summary = path("notebook_path");
            if full {
                view.detail = field("new_source").to_owned();
            }
        }
        ToolName::Glob => view.summary = with_place(field("pattern"), &path("path")),
        ToolName::Grep => {
            let mut what = field("pattern").to_owned();
            if !field("glob").is_empty() {
                what = format!("{what}  ({})", field("glob"));
            }
            view.summary = with_place(&what, &path("path"));
        }
        ToolName::WebFetch => {
            view.summary = field("url").to_owned();
            view.detail = field("prompt").to_owned();
        }
        ToolName::WebSearch => view.summary = field("query").to_owned(),
        ToolName::Task | ToolName::Agent => {
            view.title = match field("subagent_type") {
                "" => "Agent".into(),
                kind => format!("Agent · {kind}"),
            };
            view.summary = field("description").to_owned();
            if full {
                view.detail = field("prompt").to_owned();
            }
        }
        ToolName::Skill => {
            view.summary = first_non_empty(&[field("skill"), field("command"), field("name")])
        }
        ToolName::TodoWrite => {
            let todos = input.get("todos").and_then(Value::as_array);
            view.summary = format!("{} items", todos.map_or(0, Vec::len));
            view.detail = todos
                .into_iter()
                .flatten()
                .filter(|_| full)
                .map(|todo| {
                    let mark = match todo.get("status").and_then(Value::as_str) {
                        Some("completed") => "☑",
                        Some("in_progress") => "◐",
                        Some(_) | None => "☐",
                    };
                    let text = todo.get("content").and_then(Value::as_str).unwrap_or("");
                    format!("{mark} {text}")
                })
                .collect::<Vec<_>>()
                .join("\n");
        }
        ToolName::AskUserQuestion => {
            let questions = input.get("questions").and_then(Value::as_array);
            view.title = "Question".into();
            view.summary = questions
                .and_then(|questions| questions.first())
                .and_then(|question| question.get("question"))
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_owned();
        }
        ToolName::ExitPlanMode => {
            view.title = "Plan".into();
            view.summary = "Ready for review".into();
            if full {
                view.markdown = field("plan").to_owned();
            }
        }
        ToolName::Other(name) if name.starts_with("mcp__") => {
            let mut parts = name.trim_start_matches("mcp__").splitn(2, "__");
            let server = parts.next().unwrap_or("");
            view.title = parts.next().unwrap_or(name).to_owned();
            view.summary = format!("{server}  {}", compact(input, 120));
            if full {
                view.detail = pretty(input);
            }
        }
        ToolName::BashOutput
        | ToolName::KillShell
        | ToolName::Monitor
        | ToolName::NotebookRead
        | ToolName::Ls
        | ToolName::Other(_) => {
            view.summary = compact(input, 120);
            if full {
                view.detail = pretty(input);
            }
        }
    }
    if view.diff.len() > MAX_DIFF_LINES {
        let cut = view.diff.len() - MAX_DIFF_LINES;
        view.diff.truncate(MAX_DIFF_LINES);
        view.diff
            .push((DiffKind::Hunk, format!("… {cut} more lines")));
    }
    view
}

/// A unified line diff with three lines of context around each change.
fn diff(old: &str, new: &str) -> Vec<(DiffKind, String)> {
    let text_diff = similar::TextDiff::from_lines(old, new);
    let mut lines = Vec::new();
    for (i, group) in text_diff.grouped_ops(3).iter().enumerate() {
        if i > 0 {
            lines.push((DiffKind::Hunk, "⋯".to_owned()));
        }
        for op in group {
            for change in text_diff.iter_changes(op) {
                let kind = match change.tag() {
                    similar::ChangeTag::Equal => DiffKind::Context,
                    similar::ChangeTag::Delete => DiffKind::Removed,
                    similar::ChangeTag::Insert => DiffKind::Added,
                };
                let text = change.value();
                lines.push((kind, text.strip_suffix('\n').unwrap_or(text).to_owned()));
            }
        }
    }
    lines
}

/// `path` relative to the working directory when inside it.
pub(crate) fn short_path(path: &str, cwd: &Path) -> String {
    Path::new(path)
        .strip_prefix(cwd)
        .ok()
        .filter(|relative| !relative.as_os_str().is_empty())
        .map_or_else(
            || path.to_owned(),
            |relative| relative.display().to_string(),
        )
}

fn with_place(what: &str, place: &str) -> String {
    if place.is_empty() {
        what.to_owned()
    } else {
        format!("{what}  in {place}")
    }
}

fn first_line(text: &str) -> String {
    let mut lines = text.lines();
    let first = lines.next().unwrap_or("").to_owned();
    if lines.next().is_some() {
        format!("{first} …")
    } else {
        first
    }
}

fn first_non_empty(options: &[&str]) -> String {
    options
        .iter()
        .find(|option| !option.is_empty())
        .copied()
        .unwrap_or("")
        .to_owned()
}

fn compact(input: &Value, max: usize) -> String {
    let text = input.to_string();
    if text == "{}" {
        return String::new();
    }
    truncate_chars(&text, max)
}

fn pretty(input: &Value) -> String {
    serde_json::to_string_pretty(input).unwrap_or_default()
}

pub(crate) fn truncate_chars(text: &str, max: usize) -> String {
    match text.char_indices().nth(max) {
        Some((i, _)) => format!("{}…", &text[..i]),
        None => text.to_owned(),
    }
}

/// A string field of a tool's input; "" when missing.
pub(crate) fn str_field<'a>(input: &'a Value, key: &str) -> &'a str {
    input.get(key).and_then(Value::as_str).unwrap_or("")
}

/// A tool result as text: its text blocks joined, pictures named, Claude Code's error wrapper
/// taken off.
pub(crate) fn result_text(content: Option<&UserContent>) -> String {
    let text = match content {
        Some(UserContent::Text(text)) => text.clone(),
        Some(UserContent::Blocks(blocks)) => blocks
            .iter()
            .filter_map(|block| match block {
                ContentBlock::Text { text } if !text.is_empty() => Some(text.as_str()),
                // Shown in the card as images.
                ContentBlock::Image { .. }
                | ContentBlock::Text { .. }
                | ContentBlock::Thinking { .. }
                | ContentBlock::ToolUse { .. }
                | ContentBlock::ServerToolUse { .. }
                | ContentBlock::ToolResult { .. }
                | ContentBlock::Other => None,
            })
            .collect::<Vec<_>>()
            .join("\n"),
        None => String::new(),
    };
    let trimmed = text.trim();
    trimmed
        .strip_prefix("<tool_use_error>")
        .and_then(|inner| inner.strip_suffix("</tool_use_error>"))
        .unwrap_or(trimmed)
        .to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn edit_shows_a_diff_with_context() {
        let input = json!({
            "file_path": "/p/src/main.rs",
            "old_string": "a\nb\nc\n",
            "new_string": "a\nB\nc\n",
        });
        let view = describe(&ToolName::Edit, &input, Path::new("/p"), Depth::Full);
        assert_eq!(view.summary, "src/main.rs");
        assert_eq!(
            view.diff,
            vec![
                (DiffKind::Context, "a".into()),
                (DiffKind::Removed, "b".into()),
                (DiffKind::Added, "B".into()),
                (DiffKind::Context, "c".into()),
            ]
        );
    }

    #[test]
    fn bash_summary_is_first_line() {
        let input = json!({ "command": "cd x\nmake", "description": "Build" });
        let view = describe(&ToolName::Bash, &input, Path::new("/"), Depth::Full);
        assert_eq!(view.summary, "cd x …");
        assert_eq!(view.detail, "# Build\ncd x\nmake");
    }

    #[test]
    fn mcp_tools_name_their_server() {
        let view = describe(
            &ToolName::from("mcp__github__create_issue".to_owned()),
            &json!({ "title": "t" }),
            Path::new("/"),
            Depth::Full,
        );
        assert_eq!(view.title, "create_issue");
        assert!(view.summary.starts_with("github  "));
    }

    #[test]
    fn errors_lose_their_wrapper() {
        let content = UserContent::Text("<tool_use_error>File not found</tool_use_error>".into());
        assert_eq!(result_text(Some(&content)), "File not found");
    }
}
