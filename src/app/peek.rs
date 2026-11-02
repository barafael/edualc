//! Peeking into a subagent: what it was asked, what it says and each tool it calls, read from
//! the transcript Claude Code keeps for it beside the session's
//! (`<session>/subagents/agent-<task id>.jsonl`) and followed as it grows. Read only; the
//! side panel shows it.

use std::path::{Path, PathBuf};

use crate::external::wire::{ContentBlock, TaskId, TranscriptLine, UserContent};
use crate::sessions;
use crate::tools::{self, Depth};
use crate::watcher::Watched;

use super::{App, Event, Refresh};

impl App {
    /// Shows subagent `task`'s conversation in the side panel, followed as it goes.
    pub(super) fn open_peek(&mut self, task: TaskId) {
        let Some(running) = self
            .status
            .tasks
            .running()
            .find(|running| running.task.task_id == task)
        else {
            return;
        };
        let title = running.task.description.clone();
        let agent = running.task.subagent_type.clone();
        let Some(id) = self.status.session_id.as_ref() else {
            return;
        };
        // Not found, it is yet to be written where this folder's sessions go.
        let session = sessions::find_transcript(&self.cwd, id)
            .unwrap_or_else(|| sessions::transcript_path(&self.cwd, id));
        let path = transcript_of(&session, &task);
        self.close_peek();
        let mut peek = Peek::new(task, title, path);
        peek.agent = agent;
        peek.following = self.follow_peek(&peek.path);
        self.peek = Some(peek);
        self.redraw.peek = true;
        self.read_peek();
    }

    /// Watches the subagent's transcript, or until it is written, the nearest folder that will
    /// hold it.
    fn follow_peek(&self, path: &Path) -> Option<Following> {
        let watcher = self.watcher.as_ref()?;
        let following = if path.is_file() {
            watcher.file(path).map(Following::File)
        } else {
            let folder = path.ancestors().skip(1).take(2).find(|dir| dir.is_dir())?;
            watcher.folder(folder).map(Following::Folder)
        };
        following
            .inspect_err(|error| tracing::debug!(%error, "couldn't follow a subagent"))
            .ok()
    }

    /// Whether `watched` is the peek's watch.
    pub(super) fn peeks_at(&self, watched: Watched) -> bool {
        self.peek
            .as_ref()
            .and_then(|peek| peek.following)
            .is_some_and(|following| following.watched() == watched)
    }

    /// The subagent's transcript, or its folder, changed.
    pub(super) fn peek_changed(&mut self) {
        let Some(peek) = self.peek.as_ref() else {
            return;
        };
        // Watching a folder until the transcript is there: from now on the transcript.
        if let Some(Following::Folder(watched)) = peek.following
            && peek.path.is_file()
        {
            let path = peek.path.clone();
            if let Some(watcher) = &self.watcher {
                watcher.forget(watched);
            }
            let following = self.follow_peek(&path);
            if let Some(peek) = self.peek.as_mut() {
                peek.following = following;
            }
        }
        self.read_peek();
    }

    /// Reads what the subagent's transcript says after what is shown.
    fn read_peek(&mut self) {
        let Some(peek) = self.peek.as_mut() else {
            return;
        };
        let grown =
            std::fs::metadata(&peek.path).is_ok_and(|metadata| metadata.len() > peek.offset);
        if grown && peek.reading.wanted() {
            let (task, path, from) = (peek.task.clone(), peek.path.clone(), peek.offset);
            self.in_background(
                move || sessions::read_from(&path, from),
                move |result| Event::PeekGrew { task, result },
            );
        }
    }

    pub(super) fn peek_grew(
        &mut self,
        task: &TaskId,
        result: std::io::Result<(Vec<TranscriptLine>, u64)>,
    ) {
        let cwd = self.cwd.clone();
        let Some(peek) = self.peek.as_mut().filter(|peek| &peek.task == task) else {
            return;
        };
        let again = peek.reading.done();
        match result {
            Ok((lines, offset)) => {
                peek.offset = offset;
                peek.take(&lines, &cwd);
                self.redraw.peek = true;
            }
            Err(error) => tracing::debug!(%error, "couldn't read on in a subagent's transcript"),
        }
        if again {
            self.read_peek();
        }
    }

    /// Stops peeking.
    pub(super) fn close_peek(&mut self) {
        if let Some(peek) = self.peek.take() {
            if let (Some(watcher), Some(following)) = (&self.watcher, peek.following) {
                watcher.forget(following.watched());
            }
            self.redraw.peek = true;
        }
    }
}

/// What is watched to follow a subagent's transcript.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Following {
    /// The folder it will be written in, until it is there.
    Folder(Watched),
    File(Watched),
}

impl Following {
    fn watched(self) -> Watched {
        match self {
            Self::Folder(watched) | Self::File(watched) => watched,
        }
    }
}

/// The most steps kept: the latest, a long-running agent's first ones dropped.
const MAX_STEPS: usize = 400;

/// The most characters of what it says shown per step.
const MAX_SAID_CHARS: usize = 600;

/// The subagent peeked into, and how far its transcript was read.
#[derive(Debug)]
pub(crate) struct Peek {
    pub(crate) task: TaskId,
    /// What it was started for.
    pub(crate) title: String,
    /// Its kind ("Explore"), kept for after it ended.
    pub(crate) agent: Option<String>,
    /// Its transcript; it may not be written yet.
    path: PathBuf,
    offset: u64,
    reading: Refresh,
    following: Option<Following>,
    pub(crate) steps: Vec<Step>,
}

/// A step of the subagent's.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Step {
    pub(crate) kind: StepKind,
    /// The tool's name, for a call.
    pub(crate) title: String,
    pub(crate) text: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum StepKind {
    /// What it was asked to do.
    Asked,
    Said,
    Called,
}

impl Peek {
    pub(crate) fn new(task: TaskId, title: String, path: PathBuf) -> Self {
        Self {
            task,
            title,
            agent: None,
            path,
            offset: 0,
            reading: Refresh::default(),
            following: None,
            steps: Vec::new(),
        }
    }

    /// Takes in lines read on from its transcript.
    pub(crate) fn take(&mut self, lines: &[TranscriptLine], cwd: &Path) {
        self.steps
            .extend(lines.iter().flat_map(|line| steps(line, cwd)));
        let over = self.steps.len().saturating_sub(MAX_STEPS);
        self.steps.drain(..over);
    }
}

/// Where Claude Code keeps the transcript of subagent `task` of the session whose transcript is
/// `session`.
pub(crate) fn transcript_of(session: &Path, task: &TaskId) -> PathBuf {
    session
        .with_extension("")
        .join("subagents")
        .join(format!("agent-{task}.jsonl"))
}

/// The steps a line of a subagent's transcript tells.
fn steps(line: &TranscriptLine, cwd: &Path) -> Vec<Step> {
    match line {
        TranscriptLine::User {
            message,
            is_meta: false,
            ..
        } => match &message.content {
            UserContent::Text(text) if !text.trim().is_empty() => vec![Step {
                kind: StepKind::Asked,
                title: String::new(),
                text: tools::truncate_chars(text.trim(), MAX_SAID_CHARS),
            }],
            UserContent::Text(_) | UserContent::Blocks(_) => Vec::new(),
        },
        TranscriptLine::Assistant { message, .. } => message
            .content
            .iter()
            .filter_map(|block| match block {
                ContentBlock::Text { text } if !text.trim().is_empty() => Some(Step {
                    kind: StepKind::Said,
                    title: String::new(),
                    text: tools::truncate_chars(text.trim(), MAX_SAID_CHARS),
                }),
                ContentBlock::ToolUse { name, input, .. }
                | ContentBlock::ServerToolUse { name, input, .. } => {
                    let view = tools::describe(name, input, cwd, Depth::Headline);
                    Some(Step {
                        kind: StepKind::Called,
                        title: view.title,
                        text: view.summary,
                    })
                }
                ContentBlock::Text { .. }
                | ContentBlock::Thinking { .. }
                | ContentBlock::ToolResult { .. }
                | ContentBlock::Image { .. }
                | ContentBlock::Other => None,
            })
            .collect(),
        TranscriptLine::User { .. }
        | TranscriptLine::System(_)
        | TranscriptLine::CustomTitle { .. }
        | TranscriptLine::AiTitle { .. }
        | TranscriptLine::Summary { .. }
        | TranscriptLine::WorktreeState { .. }
        | TranscriptLine::Other => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn line(value: serde_json::Value) -> TranscriptLine {
        serde_json::from_value(value).expect("a transcript line")
    }

    /// As Claude Code 2.1.316 writes a subagent's transcript: its prompt, then what it says and
    /// calls; tool results and meta lines tell nothing of their own.
    #[test]
    fn a_subagents_transcript_reads_as_steps() {
        let cwd = Path::new("/home/u/project");
        let lines = [
            line(
                json!({ "type": "user", "isSidechain": true, "agentId": "a1",
                "message": { "role": "user", "content": "Find the parser" }}),
            ),
            line(json!({ "type": "assistant", "isSidechain": true,
                "message": { "id": "m1", "role": "assistant", "content": [
                    { "type": "thinking", "thinking": "hm" },
                    { "type": "text", "text": "Looking for it." },
                    { "type": "tool_use", "id": "t1", "name": "Grep", "input": { "pattern": "fn parse" } },
                ] } })),
            line(json!({ "type": "user", "isSidechain": true,
                "message": { "role": "user", "content": [
                    { "type": "tool_result", "tool_use_id": "t1", "content": "src/a.rs" },
                ] } })),
            line(json!({ "type": "user", "isMeta": true,
                "message": { "role": "user", "content": "<system-reminder>x</system-reminder>" } })),
        ];
        let mut peek = Peek::new(
            TaskId::from("a1".to_owned()),
            "Find the parser".to_owned(),
            PathBuf::new(),
        );
        peek.take(&lines, cwd);
        let [asked, said, called] = peek.steps.as_slice() else {
            panic!("three steps: {:?}", peek.steps);
        };
        assert_eq!(asked.kind, StepKind::Asked);
        assert_eq!(
            (said.kind, said.text.as_str()),
            (StepKind::Said, "Looking for it.")
        );
        assert_eq!(
            (called.kind, called.title.as_str()),
            (StepKind::Called, "Grep")
        );
        assert!(called.text.contains("fn parse"), "{called:?}");
    }

    /// Only the latest steps are kept.
    #[test]
    fn a_long_agent_keeps_its_latest_steps() {
        let said = |text: String| {
            line(json!({ "type": "assistant",
                "message": { "id": "m", "role": "assistant", "content": [
                    { "type": "text", "text": text },
                ] } }))
        };
        let lines: Vec<TranscriptLine> = (0..MAX_STEPS + 5).map(|n| said(n.to_string())).collect();
        let mut peek = Peek::new(TaskId::from("a1".to_owned()), String::new(), PathBuf::new());
        peek.take(&lines, Path::new("/"));
        assert_eq!(peek.steps.len(), MAX_STEPS);
        assert_eq!(peek.steps.first().map(|step| step.text.as_str()), Some("5"));
    }

    /// The transcript sits in the session's folder of subagents, named by the task.
    #[test]
    fn a_subagents_transcript_is_beside_its_sessions() {
        let session = Path::new("/home/u/.claude/projects/-p/0b8f.jsonl");
        assert_eq!(
            transcript_of(session, &TaskId::from("a50b8b2d1bc3507ae".to_owned())),
            Path::new("/home/u/.claude/projects/-p/0b8f/subagents/agent-a50b8b2d1bc3507ae.jsonl")
        );
    }
}
