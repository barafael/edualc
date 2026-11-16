//! How a tool call reads in the chat: its name, a one-line summary (the command, the file),
//! the input worth showing when expanded, and for edits a diff.

use std::path::{Path, PathBuf};

use serde::de::DeserializeOwned;
use serde_json::Value;

use crate::external::tool_input::{
    AgentInput, BashInput, EditInput, GlobInput, GrepInput, IncomingMessageInput, MultiEditInput,
    NotebookEditInput, ReadInput, SendMessageInput, SkillInput, TodoStatus, TodoWriteInput,
    WebFetchInput, WebSearchInput, WriteInput,
};
use crate::external::wire::{ContentBlock, PlanInput, Questions, ToolName, UserContent};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiffKind {
    Context,
    Added,
    Removed,
    Hunk,
}

/// How a call's summary reads, and so how it is set: as code (a command, a path, a pattern, a
/// URL, the input), or as words (a question, what an agent is to do).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Writing {
    #[default]
    Code,
    Prose,
}

#[derive(Debug, Default)]
pub struct ToolView {
    pub title: String,
    pub summary: String,
    pub summary_reads_as: Writing,
    /// Shown when expanded, as plain monospace text; empty if the summary says it all.
    pub detail: String,
    pub diff: Vec<(DiffKind, String)>,
    /// Markdown shown when expanded (ExitPlanMode's plan).
    pub markdown: String,
}

/// How much of a call to describe: its line alone (title and summary), or also what an
/// expanded card shows (a diff, a file, the input).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Depth {
    Headline,
    Full,
}

/// Diffs longer than this are cut; the full change is in the file.
const MAX_DIFF_LINES: usize = 400;

pub fn describe(name: &ToolName, input: &Value, cwd: &Path, depth: Depth) -> ToolView {
    let full = depth == Depth::Full;
    let path = |path: &Path| short_path(path, cwd);
    let place = |place: Option<&PathBuf>| place.map(|place| path(place)).unwrap_or_default();
    let mut view = ToolView {
        title: name.to_string(),
        ..ToolView::default()
    };
    match name {
        // A Monitor runs a command too, in the background, and says what it watches for.
        ToolName::Bash | ToolName::Monitor => {
            if let Some(bash) = read::<BashInput>(name, input) {
                command_view(&mut view, &bash);
                if *name == ToolName::Bash && bash.run_in_background == Some(true) {
                    view.title = "Bash (background)".into();
                }
            }
        }
        ToolName::Read => {
            if let Some(read) = read::<ReadInput>(name, input) {
                view.summary = path(&read.file_path);
                if let Some(offset) = read.offset {
                    let range = match read.limit.filter(|&limit| limit > 0) {
                        Some(limit) => format!("lines {offset}–{}", offset.saturating_add(limit)),
                        None => format!("from line {offset}"),
                    };
                    view.summary = format!("{}  {range}", view.summary);
                }
            }
        }
        // The whole file, as it will be: a wall of `+` lines would say the same, louder.
        ToolName::Write => {
            if let Some(write) = read::<WriteInput>(name, input) {
                view.summary = path(&write.file_path);
                if full {
                    view.detail = write.content;
                }
            }
        }
        ToolName::Edit => {
            if let Some(edit) = read::<EditInput>(name, input) {
                view.summary = path(&edit.file_path);
                if edit.replace_all == Some(true) {
                    view.summary += "  (all occurrences)";
                }
                if full {
                    view.diff = diff(&edit.old_string, &edit.new_string);
                }
            }
        }
        ToolName::MultiEdit => {
            if let Some(edits) = read::<MultiEditInput>(name, input) {
                view.summary = path(&edits.file_path);
                for edit in edits.edits.iter().filter(|_| full) {
                    view.diff.extend(diff(&edit.old_string, &edit.new_string));
                }
            }
        }
        // The cell's new source, as it will be, shown whole.
        ToolName::NotebookEdit => {
            if let Some(edit) = read::<NotebookEditInput>(name, input) {
                view.summary = path(&edit.notebook_path);
                if full {
                    view.detail = edit.new_source;
                }
            }
        }
        ToolName::Glob => {
            if let Some(glob) = read::<GlobInput>(name, input) {
                view.summary = with_place(&glob.pattern, &place(glob.path.as_ref()));
            }
        }
        ToolName::Grep => {
            if let Some(grep) = read::<GrepInput>(name, input) {
                let what = match grep.glob.filter(|glob| !glob.is_empty()) {
                    Some(glob) => format!("{}  ({glob})", grep.pattern),
                    None => grep.pattern,
                };
                view.summary = with_place(&what, &place(grep.path.as_ref()));
            }
        }
        ToolName::WebFetch => {
            if let Some(fetch) = read::<WebFetchInput>(name, input) {
                view.summary = fetch.url;
                view.detail = fetch.prompt.unwrap_or_default();
            }
        }
        ToolName::WebSearch => {
            if let Some(search) = read::<WebSearchInput>(name, input) {
                view.summary = search.query;
            }
        }
        ToolName::Task | ToolName::Agent => {
            view.title = "Agent".into();
            if let Some(agent) = read::<AgentInput>(name, input) {
                if let Some(kind) = agent.subagent_type.filter(|kind| !kind.is_empty()) {
                    view.title = format!("Agent · {kind}");
                }
                view.summary = agent.description;
                if full {
                    view.detail = agent.prompt;
                }
            }
        }
        ToolName::Skill => {
            if let Some(skill) = read::<SkillInput>(name, input) {
                view.summary = [skill.skill, skill.command, skill.name]
                    .into_iter()
                    .flatten()
                    .find(|named| !named.is_empty())
                    .unwrap_or_default();
            }
        }
        ToolName::TodoWrite => {
            if let Some(list) = read::<TodoWriteInput>(name, input) {
                view.summary = format!("{} items", list.todos.len());
                view.detail = list
                    .todos
                    .iter()
                    .filter(|_| full)
                    .map(|todo| {
                        let mark = match todo.status {
                            TodoStatus::Completed => "☑",
                            TodoStatus::InProgress => "◐",
                            TodoStatus::Pending | TodoStatus::Other => "☐",
                        };
                        format!("{mark} {}", todo.content)
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
            }
        }
        ToolName::AskUserQuestion => {
            view.title = "Question".into();
            if let Some(asked) = read::<Questions>(name, input) {
                view.summary = asked
                    .questions
                    .into_iter()
                    .next()
                    .map(|question| question.question)
                    .unwrap_or_default();
            }
        }
        ToolName::ExitPlanMode => {
            view.title = "Plan".into();
            view.summary = "Ready for review".into();
            if full && let Some(plan) = read::<PlanInput>(name, input) {
                view.markdown = plan.plan;
            }
        }
        ToolName::SendMessage => {
            if let Some(SendMessageInput { to, message }) = read(name, input) {
                // An inbox's address (`uds:…`, as Claude Code tells a session to answer by)
                // says nothing to the user; it stays in the detail.
                let addressed = ["uds:", "bridge:", "did:"]
                    .iter()
                    .any(|scheme| to.starts_with(scheme));
                view.title = if addressed {
                    "Message to another session".to_owned()
                } else {
                    format!("Message to {to}")
                };
                view.summary = first_line(&message);
                if full {
                    view.detail = if addressed {
                        format!("{message}\n\nTo {to}")
                    } else {
                        message
                    };
                }
            }
        }
        ToolName::EdualcSendMessage => {
            if let Some(SendMessageInput { to, message }) = read(name, input) {
                view.title = format!("Message to {to}");
                view.summary = first_line(&message);
                if full {
                    view.detail = message;
                }
            }
        }
        ToolName::EdualcIncomingMessage => {
            if let Some(IncomingMessageInput { from, message }) = read(name, input) {
                view.title = format!("Message from {from}");
                view.summary = first_line(&message);
                if full {
                    view.detail = message;
                }
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
        // Shown as the MCP tool it is, as any other server's.
        ToolName::EdualcListSessions => {
            view.title = "list_sessions".to_owned();
            view.summary = format!("edualc  {}", compact(input, 120));
            if full {
                view.detail = pretty(input);
            }
        }
        ToolName::BashOutput
        | ToolName::KillShell
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
    view.summary_reads_as = match name {
        ToolName::WebSearch
        | ToolName::Task
        | ToolName::Agent
        | ToolName::TodoWrite
        | ToolName::AskUserQuestion
        | ToolName::ExitPlanMode
        | ToolName::SendMessage
        | ToolName::EdualcSendMessage
        | ToolName::EdualcIncomingMessage => Writing::Prose,
        ToolName::Bash
        | ToolName::BashOutput
        | ToolName::KillShell
        | ToolName::Monitor
        | ToolName::Read
        | ToolName::NotebookRead
        | ToolName::Write
        | ToolName::Edit
        | ToolName::MultiEdit
        | ToolName::NotebookEdit
        | ToolName::Glob
        | ToolName::Grep
        | ToolName::Ls
        | ToolName::WebFetch
        | ToolName::Skill
        | ToolName::EdualcListSessions
        | ToolName::Other(_) => Writing::Code,
    };
    view
}

/// The file a call reads, writes or edits.
pub fn file_of(name: &ToolName, input: &Value) -> Option<PathBuf> {
    match name {
        ToolName::Read => read::<ReadInput>(name, input).map(|read| read.file_path),
        ToolName::Write => read::<WriteInput>(name, input).map(|write| write.file_path),
        ToolName::Edit => read::<EditInput>(name, input).map(|edit| edit.file_path),
        ToolName::MultiEdit => read::<MultiEditInput>(name, input).map(|edits| edits.file_path),
        ToolName::NotebookEdit => {
            read::<NotebookEditInput>(name, input).map(|edit| edit.notebook_path)
        }
        ToolName::Bash
        | ToolName::BashOutput
        | ToolName::KillShell
        | ToolName::Monitor
        | ToolName::NotebookRead
        | ToolName::Glob
        | ToolName::Grep
        | ToolName::Ls
        | ToolName::WebFetch
        | ToolName::WebSearch
        | ToolName::Task
        | ToolName::Agent
        | ToolName::Skill
        | ToolName::TodoWrite
        | ToolName::AskUserQuestion
        | ToolName::ExitPlanMode
        | ToolName::SendMessage
        | ToolName::EdualcSendMessage
        | ToolName::EdualcListSessions
        | ToolName::EdualcIncomingMessage
        | ToolName::Other(_) => None,
    }
}

/// A command's card: the `!` command the user ran, as Bash's.
pub fn describe_command(command: &BashInput) -> ToolView {
    let mut view = ToolView {
        title: ToolName::Bash.to_string(),
        ..ToolView::default()
    };
    command_view(&mut view, command);
    view
}

/// A command's first line, the whole of a long one, and its description.
fn command_view(view: &mut ToolView, bash: &BashInput) {
    let command = &bash.command;
    view.summary = first_line(command);
    if command.contains('\n') || command.chars().count() > 90 {
        view.detail.clone_from(command);
    }
    if let Some(description) = bash.description.as_deref().filter(|text| !text.is_empty()) {
        view.detail = if view.detail.is_empty() {
            format!("# {description}")
        } else {
            format!("# {description}\n{}", view.detail)
        };
    }
}

/// A tool's input as its type; none for one that isn't, such as the empty input of a call
/// still streaming in.
pub(crate) fn read<Input: DeserializeOwned>(name: &ToolName, input: &Value) -> Option<Input> {
    match Input::deserialize(input) {
        Ok(read) => Some(read),
        Err(error) => {
            if !input.as_object().is_some_and(serde_json::Map::is_empty) {
                tracing::debug!(%error, tool = %name, "a tool's input that isn't its shape");
            }
            None
        }
    }
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
/// The most a path outside the working directory takes on a card before its middle goes.
const CARD_PATH_CHARS: usize = 60;

/// A path as a card shows it: relative to the working directory inside it; otherwise as
/// [`shortened_path`] gives it, so a long one keeps its file's name in view.
pub fn short_path(path: &Path, cwd: &Path) -> String {
    path.strip_prefix(cwd)
        .ok()
        .filter(|relative| !relative.as_os_str().is_empty())
        .map_or_else(
            || shortened_path(path, CARD_PATH_CHARS),
            |relative| relative.display().to_string(),
        )
}

/// `path` relative to home, its middle left out if it is longer than `max_chars`: the start
/// says where it is, the end which it is.
pub fn shortened_path(path: &Path, max_chars: usize) -> String {
    let full = home_relative(path);
    if full.chars().count() <= max_chars {
        return full;
    }
    let parts: Vec<&str> = full.split('/').collect();
    match (
        parts.first(),
        parts.len().checked_sub(2).and_then(|at| parts.get(at..)),
    ) {
        (Some(first), Some(last)) if parts.len() > 3 => format!("{first}/…/{}", last.join("/")),
        _ => full,
    }
}

/// `path` from `~` when it is in the home folder.
pub fn home_relative(path: &Path) -> String {
    let home = dirs::home_dir();
    match home
        .as_deref()
        .and_then(|home| path.strip_prefix(home).ok())
    {
        Some(rest) if rest.as_os_str().is_empty() => "~".into(),
        Some(rest) => format!("~/{}", rest.display()),
        None => path.display().to_string(),
    }
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

pub fn truncate_chars(text: &str, max: usize) -> String {
    match text.char_indices().nth(max) {
        Some((i, _)) => format!("{}…", &text[..i]),
        None => text.to_owned(),
    }
}

/// A tool result as text: its text blocks joined, pictures named, Claude Code's error wrapper
/// taken off.
pub fn result_text(content: Option<&UserContent>) -> String {
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
    fn long_paths_lose_their_middle() {
        assert_eq!(
            shortened_path(Path::new("/var/tmp/claude-1000/scratchpad/review"), 24),
            "/…/scratchpad/review"
        );
        assert_eq!(shortened_path(Path::new("/srv/app"), 24), "/srv/app");
    }

    /// A card shows a path inside the project from the project; one outside, if long, keeps
    /// its file's name and folder, which used to be cut off at the end.
    #[test]
    fn a_card_keeps_the_file_name_of_a_long_path() {
        let cwd = Path::new("/srv/project");
        let short = |path: &str| short_path(Path::new(path), cwd);
        assert_eq!(short("/srv/project/src/main.rs"), "src/main.rs");
        assert_eq!(
            short(
                "/var/tmp/claude-1000/-home-someone-code-a-project/0a1b2c3d/scratchpad/shell.png"
            ),
            "/…/scratchpad/shell.png"
        );
        assert_eq!(short("/etc/hosts"), "/etc/hosts");
    }

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

    /// Each card reads its tool's input by Claude Code's names, the optional ones included;
    /// a call still streaming in, its input empty, shows its name alone.
    #[test]
    fn cards_read_claude_codes_inputs() {
        let cwd = Path::new("/p");
        let line = |name: ToolName, input: Value| {
            let view = describe(&name, &input, cwd, Depth::Full);
            (view.title, view.summary, view.detail)
        };
        assert_eq!(
            line(
                ToolName::Read,
                json!({ "file_path": "/p/a.rs", "offset": 10, "limit": 5 })
            ),
            ("Read".into(), "a.rs  lines 10–15".into(), String::new())
        );
        assert_eq!(
            line(
                ToolName::Grep,
                json!({ "pattern": "fn main", "path": "/p/src", "glob": "*.rs" })
            ),
            (
                "Grep".into(),
                "fn main  (*.rs)  in src".into(),
                String::new()
            )
        );
        assert_eq!(
            line(
                ToolName::Agent,
                json!({ "description": "Look around", "prompt": "List the files.", "subagent_type": "Explore" })
            ),
            (
                "Agent · Explore".into(),
                "Look around".into(),
                "List the files.".into()
            )
        );
        assert_eq!(
            line(
                ToolName::WebFetch,
                json!({ "url": "https://example.com", "prompt": "The title" })
            ),
            (
                "WebFetch".into(),
                "https://example.com".into(),
                "The title".into()
            )
        );
        assert_eq!(
            line(
                ToolName::TodoWrite,
                json!({ "todos": [
                    { "content": "Read", "status": "completed", "activeForm": "Reading" },
                    { "content": "Write", "status": "in_progress", "activeForm": "Writing" },
                ] })
            ),
            (
                "TodoWrite".into(),
                "2 items".into(),
                "☑ Read\n◐ Write".into()
            )
        );
        assert_eq!(
            line(
                ToolName::AskUserQuestion,
                json!({ "questions": [{ "question": "Which?", "header": "Pick", "options": [], "multiSelect": false }] })
            ),
            ("Question".into(), "Which?".into(), String::new())
        );
        assert_eq!(
            line(ToolName::Grep, json!({})),
            ("Grep".into(), String::new(), String::new())
        );
    }

    #[test]
    fn bash_summary_is_first_line() {
        let input = json!({ "command": "cd x\nmake", "description": "Build" });
        let view = describe(&ToolName::Bash, &input, Path::new("/"), Depth::Full);
        assert_eq!(view.summary, "cd x …");
        assert_eq!(view.detail, "# Build\ncd x\nmake");
    }

    /// A Monitor's card reads as its command, as Claude Code 2.1.316 sends it, not as its
    /// input's JSON; it isn't titled as a background Bash.
    #[test]
    fn a_monitor_reads_as_its_command() {
        let input = json!({
            "command": "for i in $(seq 1 60); do echo beat $i; sleep 2; done",
            "description": "Heartbeat lines every two seconds",
            "run_in_background": true,
        });
        let view = describe(&ToolName::Monitor, &input, Path::new("/"), Depth::Full);
        assert_eq!(view.title, "Monitor");
        assert_eq!(
            view.summary,
            "for i in $(seq 1 60); do echo beat $i; sleep 2; done"
        );
        assert_eq!(view.detail, "# Heartbeat lines every two seconds");
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

    /// Claude Code's own SendMessage reads as a message, not as its input's JSON: to a session
    /// by name, or, by an inbox's address (as Claude Code tells a session to answer), to
    /// "another session", the address kept in the detail.
    #[test]
    fn messages_to_sessions_read_as_messages() {
        let card = |to: &str| {
            let input = json!({ "to": to, "summary": "Ping", "message": "Ping 2 from A.\nMore." });
            describe(
                &ToolName::from("SendMessage".to_owned()),
                &input,
                Path::new("/"),
                Depth::Full,
            )
        };
        let by_name = card("decent-perch-55");
        assert_eq!(by_name.title, "Message to decent-perch-55");
        assert_eq!(by_name.summary, "Ping 2 from A. …");
        assert_eq!(by_name.detail, "Ping 2 from A.\nMore.");
        let by_address = card("uds:/run/user/1000/cc-socks/2164.sock");
        assert_eq!(by_address.title, "Message to another session");
        assert_eq!(
            by_address.detail,
            "Ping 2 from A.\nMore.\n\nTo uds:/run/user/1000/cc-socks/2164.sock"
        );
        assert_eq!(by_address.summary_reads_as, Writing::Prose);
    }

    #[test]
    fn errors_lose_their_wrapper() {
        let content = UserContent::Text("<tool_use_error>File not found</tool_use_error>".into());
        assert_eq!(result_text(Some(&content)), "File not found");
    }
}
