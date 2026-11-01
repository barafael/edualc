//! Runs of tool calls, folded into one line: "Read 2 files, ran 1 command". Long sessions are
//! mostly tool calls; one line per run keeps what Claude said readable, and a click opens the
//! run. A call waiting for the user is never folded away: it stands alone, and parts the run.

use std::collections::HashSet;
use std::path::Path;

use serde_json::Value;

use crate::chat::{Body, Entry, Tool, ToolState};
use crate::tools;
use crate::wire::ToolKind;

/// How an entry is shown with respect to runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Grouping {
    /// Not in a run: shown as itself.
    Alone,
    /// The first call of a run: it carries the run's line.
    Head(RunLine),
    /// A later call of a run: shown only while the run is open.
    Member { open: bool },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RunLine {
    pub(crate) summary: String,
    pub(crate) state: RunState,
    pub(crate) open: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RunState {
    Working,
    Done,
    /// Some call failed (and none still works).
    Failed,
}

/// How each of `entries` is shown; `open` holds the runs opened, by their first entry.
pub(crate) fn group(entries: &[Entry], open: &HashSet<usize>, cwd: &Path) -> Vec<Grouping> {
    let mut shown = vec![Grouping::Alone; entries.len()];
    let mut start = 0;
    while start < entries.len() {
        let end = run_end(entries, start);
        if end - start >= 2 {
            let tools: Vec<&Tool> = entries
                .get(start..end)
                .unwrap_or_default()
                .iter()
                .filter_map(foldable)
                .collect();
            let is_open = open.contains(&start);
            if let Some(slot) = shown.get_mut(start) {
                *slot = Grouping::Head(line(&tools, is_open, cwd));
            }
            for slot in shown.get_mut(start + 1..end).unwrap_or_default() {
                *slot = Grouping::Member { open: is_open };
            }
        }
        start = end.max(start + 1);
    }
    shown
}

/// A tool call that may fold into a run: any but one waiting for the user.
fn foldable(entry: &Entry) -> Option<&Tool> {
    match &entry.body {
        Body::Tool { tool, .. } if tool.state != ToolState::Awaiting => Some(tool),
        Body::Tool { .. }
        | Body::User(_)
        | Body::Assistant(_)
        | Body::Thinking { .. }
        | Body::Info(_)
        | Body::Error(_) => None,
    }
}

/// Where the run starting at `start` ends: foldable calls made by the same agent (the main
/// conversation, or one subagent's calls nested under it).
fn run_end(entries: &[Entry], start: usize) -> usize {
    let Some(first) = entries.get(start).filter(|entry| foldable(entry).is_some()) else {
        return start;
    };
    let length = entries
        .get(start..)
        .unwrap_or_default()
        .iter()
        .take_while(|entry| foldable(entry).is_some() && entry.nested == first.nested)
        .count();
    start + length
}

fn line(tools: &[&Tool], open: bool, cwd: &Path) -> RunLine {
    let working = tools
        .iter()
        .rev()
        .find(|tool| matches!(tool.state, ToolState::Streaming | ToolState::Running));
    let failed = tools
        .iter()
        .filter(|tool| tool.state == ToolState::Failed)
        .count();
    let mut summary = summary(tools);
    if failed > 0 {
        summary = format!("{summary}, {failed} failed");
    }
    let state = match (working, failed) {
        (Some(tool), _) => {
            let now = tools::describe(&tool.name, &tool.input, cwd, tools::Depth::Headline);
            summary = format!("{summary}  ·  {} {}", now.title, now.summary);
            RunState::Working
        }
        (None, 0) => RunState::Done,
        (None, _) => RunState::Failed,
    };
    RunLine {
        summary,
        state,
        open,
    }
}

fn summary(tools: &[&Tool]) -> String {
    let mut kinds: Vec<ToolKind> = Vec::new();
    for tool in tools {
        let found = tool.name.kind();
        if !kinds.contains(&found) {
            kinds.push(found);
        }
    }
    let phrases: Vec<String> = kinds
        .iter()
        .map(|&wanted| {
            let of_kind: Vec<&&Tool> = tools
                .iter()
                .filter(|tool| tool.name.kind() == wanted)
                .collect();
            phrase(wanted, &of_kind)
        })
        .collect();
    let joined = phrases.join(", ");
    let mut characters = joined.chars();
    characters.next().map_or_else(String::new, |first| {
        first.to_uppercase().chain(characters).collect()
    })
}

fn phrase(kind: ToolKind, tools: &[&&Tool]) -> String {
    let count = tools.len();
    let plural = |one: &str, many: &str| {
        if count == 1 {
            one.to_owned()
        } else {
            many.to_owned()
        }
    };
    match kind {
        // The files themselves: what changed matters most.
        ToolKind::Edit => files_phrase("edited", tools),
        ToolKind::Write => files_phrase("wrote", tools),
        ToolKind::Read => format!("read {count} {}", plural("file", "files")),
        ToolKind::Command => format!("ran {count} {}", plural("command", "commands")),
        ToolKind::Search => format!("searched {count} {}", plural("time", "times")),
        ToolKind::Web => format!("looked up {count} {}", plural("page", "pages")),
        ToolKind::Agent => format!("ran {count} {}", plural("agent", "agents")),
        ToolKind::Other => format!("used {count} {}", plural("tool", "tools")),
    }
}

/// "edited main.rs", "edited main.rs and lib.rs", "edited 4 files".
fn files_phrase(verb: &str, tools: &[&&Tool]) -> String {
    let mut files: Vec<String> = tools
        .iter()
        .filter_map(|tool| file_name(&tool.input))
        .collect();
    files.dedup();
    match files.as_slice() {
        [one] => format!("{verb} {one}"),
        [first, second] => format!("{verb} {first} and {second}"),
        _ => format!("{verb} {} files", tools.len()),
    }
}

fn file_name(input: &Value) -> Option<String> {
    let path = input
        .get("file_path")
        .or_else(|| input.get("notebook_path"))
        .and_then(Value::as_str)?;
    Path::new(path)
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};

    use super::*;
    use crate::chat::{Chat, Origin};
    use crate::wire::{ApiMessage, UserMessage};

    /// A chat with one completed call per `(name, input)`, in order.
    fn calls(calls: &[(&str, Value)]) -> Chat {
        let mut chat = Chat::default();
        for (index, (name, input)) in calls.iter().enumerate() {
            let message: ApiMessage = serde_json::from_value(json!({
                "id": format!("m{index}"),
                "content": [{
                    "type": "tool_use",
                    "id": format!("t{index}"),
                    "name": name,
                    "input": input,
                }],
            }))
            .expect("a message");
            chat.assistant_message(&message, None, Origin::Live);
            let result: UserMessage = serde_json::from_value(json!({
                "content": [{
                    "type": "tool_result",
                    "tool_use_id": format!("t{index}"),
                    "content": "ok",
                }],
            }))
            .expect("a result");
            chat.user_message(&result, Origin::Live);
        }
        chat
    }

    #[test]
    fn a_run_folds_into_its_first_call() {
        let chat = calls(&[
            ("Bash", json!({ "command": "ls" })),
            ("Read", json!({ "file_path": "/p/a.md" })),
            ("Read", json!({ "file_path": "/p/b.rs" })),
            ("Edit", json!({ "file_path": "/p/src/main.rs" })),
            ("Write", json!({ "file_path": "/p/NOTES.md" })),
        ]);
        let shown = group(&chat.entries, &HashSet::new(), Path::new("/p"));
        let Some(Grouping::Head(line)) = shown.first() else {
            panic!("a run's line, not {shown:?}");
        };
        assert_eq!(
            line.summary,
            "Ran 1 command, read 2 files, edited main.rs, wrote NOTES.md"
        );
        assert_eq!(line.state, RunState::Done);
        assert!(!line.open);
        assert!(
            shown
                .iter()
                .skip(1)
                .all(|grouping| *grouping == Grouping::Member { open: false })
        );
    }

    #[test]
    fn single_calls_and_waiting_calls_stand_alone() {
        let mut chat = calls(&[("Read", json!({ "file_path": "/p/a" }))]);
        chat.info("Interrupted");
        let more = calls(&[
            ("Bash", json!({ "command": "x" })),
            ("Bash", json!({ "command": "y" })),
        ]);
        chat.entries.extend(more.entries);
        if let Some(Entry {
            body: Body::Tool { tool, .. },
            ..
        }) = chat.entries.get_mut(2)
        {
            tool.state = ToolState::Awaiting;
        }
        let shown = group(&chat.entries, &HashSet::from([0]), Path::new("/p"));
        assert!(
            shown.iter().all(|grouping| *grouping == Grouping::Alone),
            "{shown:?}"
        );
    }

    #[test]
    fn an_open_run_shows_its_calls() {
        let chat = calls(&[
            ("Grep", json!({ "pattern": "x" })),
            ("Glob", json!({ "pattern": "*" })),
        ]);
        let shown = group(&chat.entries, &HashSet::from([0]), Path::new("/p"));
        assert!(matches!(
            shown.first(),
            Some(Grouping::Head(RunLine { open: true, .. }))
        ));
        assert_eq!(shown.get(1), Some(&Grouping::Member { open: true }));
    }
}
