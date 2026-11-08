//! Peeking into a task, read only, in the side panel. A subagent: what it was asked, what it
//! says and each tool it calls, read from the transcript Claude Code keeps for it beside the
//! session's (`<session>/subagents/agent-<task id>.jsonl`) and followed as it grows. A
//! background command: the end of what it printed, asked of Claude Code every little while as
//! it runs and once after.

use crate::present::Frontend;
use std::path::Path;
use std::time::Duration;

use serde::Deserialize;
use serde_json::Value;

use crate::chat::strip_ansi;
use crate::external::wire::{
    ContentBlock, ControlRequest, TaskId, TaskKind, TaskOutput, TranscriptLine, UserContent,
};
use crate::ipc::{Place, Read, Target, Watched};
use crate::tools::{self, Depth};

use super::{App, Event, Refresh, Request, Sandboxing};

/// How often a followed command's output is asked for while it runs.
const POLL_OUTPUT: Duration = Duration::from_secs(1);

impl<F: Frontend> App<F> {
    /// Shows task `task` in the side panel, followed as it goes: a subagent's conversation, or
    /// what a command prints.
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
        let kind = running.task.task_type;
        let agent = running.task.subagent_type.clone();
        let view = match Followed::of(kind) {
            Some(Followed::Conversation) => {
                let Some(place) = self.subagent_transcript(&task) else {
                    return;
                };
                PeekView::Conversation(Conversation::new(agent, place))
            }
            Some(Followed::Output) => PeekView::Output(CommandOutput::default()),
            None => return,
        };
        self.close_peek();
        self.peek = Some(Peek {
            task,
            title,
            kind,
            view,
        });
        self.redraw.peek = true;
        // Its transcript watched, or until it is written, the folder it will be in.
        let following = self
            .conversation()
            .map(|conversation| conversation.place.clone())
            .and_then(|place| self.watch(Target::Transcript(place)));
        if let Some(conversation) = self.conversation() {
            conversation.following = following;
            self.read_peek();
        } else {
            self.start_polling();
            self.poll_output();
        }
    }

    /// Where subagent `task` of the session shown keeps its transcript: beside the session's in
    /// the home its process writes in, as the user's own follows a sandboxed session's
    /// transcript but not its subagents'.
    fn subagent_transcript(&self, task: &TaskId) -> Option<Place> {
        let id = self.status.session_id.clone()?;
        let sandboxed = match self.status.sandboxing {
            Sandboxing::Landlock(_) => true,
            Sandboxing::Pending | Sandboxing::Off | Sandboxing::ClaudeCodes => false,
        };
        Some(Place::Subagent {
            sandboxed,
            cwd: self.cwd.clone(),
            id,
            task: task.clone(),
        })
    }

    /// The subagent's conversation peeked into, if one is.
    fn conversation(&mut self) -> Option<&mut Conversation> {
        match self.peek.as_mut().map(|peek| &mut peek.view) {
            Some(PeekView::Conversation(conversation)) => Some(conversation),
            Some(PeekView::Output(_)) | None => None,
        }
    }

    /// The command's output peeked into, if one is.
    fn output(&mut self) -> Option<&mut CommandOutput> {
        match self.peek.as_mut().map(|peek| &mut peek.view) {
            Some(PeekView::Output(output)) => Some(output),
            Some(PeekView::Conversation(_)) | None => None,
        }
    }

    fn start_polling(&mut self) {
        if self.polling.running() {
            return;
        }
        self.polling
            .start(POLL_OUTPUT, self.events.clone(), || Event::PollOutput);
    }

    /// Asks Claude Code for the followed command's output, unless the last question is still
    /// out; stops polling once there is nothing more to ask.
    pub(super) fn poll_output(&mut self) {
        let task = self.peek.as_ref().map(|peek| peek.task.clone());
        let running = task.as_ref().is_some_and(|task| {
            self.status
                .tasks
                .running()
                .any(|running| &running.task.task_id == task)
        });
        let ask = self
            .output()
            .is_some_and(|output| output.ask(running))
            .then_some(task)
            .flatten();
        if let Some(task) = ask {
            let request = ControlRequest::GetTaskOutput {
                task_id: task.clone(),
            };
            if !self.request(&request, Request::TaskOutput(task))
                && let Some(output) = self.output()
            {
                output.give_up();
            }
        }
        if self.output().is_none_or(|output| output.finished()) {
            self.polling.stop();
        }
    }

    /// What Claude Code says the command printed, or why it doesn't.
    pub(super) fn output_told(&mut self, task: &TaskId, answer: Result<Value, String>) {
        if self.peek.as_ref().is_none_or(|peek| &peek.task != task) {
            return;
        }
        let Some(output) = self.output() else {
            return;
        };
        let changed = match answer.map(|value| TaskOutput::deserialize(&value)) {
            Ok(Ok(told)) => output.told(&told),
            Ok(Err(error)) => {
                tracing::warn!(%error, "a command's output that doesn't parse");
                output.give_up();
                false
            }
            Err(refusal) => {
                output.refused(refusal);
                true
            }
        };
        self.redraw.peek |= changed;
    }

    /// Whether `watched` is the peek's watch.
    pub(super) fn peeks_at(&self, watched: Watched) -> bool {
        match self.peek.as_ref().map(|peek| &peek.view) {
            Some(PeekView::Conversation(conversation)) => conversation.following == Some(watched),
            Some(PeekView::Output(_)) | None => false,
        }
    }

    /// Reads what the subagent's transcript says after what is shown.
    pub(super) fn read_peek(&mut self) {
        let Some(peek) = self.peek.as_mut() else {
            return;
        };
        let PeekView::Conversation(conversation) = &mut peek.view else {
            return;
        };
        if conversation.reading.wanted() {
            let (task, place, from) = (
                peek.task.clone(),
                conversation.place.clone(),
                conversation.offset,
            );
            self.read(
                Read::Transcript { place, from },
                super::Reading::Peek { task },
            );
        }
    }

    pub(super) fn peek_grew(
        &mut self,
        task: &TaskId,
        result: Result<(Vec<TranscriptLine>, u64), String>,
    ) {
        let cwd = self.cwd.clone();
        if self.peek.as_ref().is_none_or(|peek| &peek.task != task) {
            return;
        }
        let Some(conversation) = self.conversation() else {
            return;
        };
        let again = conversation.reading.done();
        match result {
            Ok((lines, offset)) => {
                conversation.offset = offset;
                conversation.take(&lines, &cwd);
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
        self.polling.stop();
        if let Some(peek) = self.peek.take() {
            if let PeekView::Conversation(conversation) = peek.view
                && let Some(following) = conversation.following
            {
                self.unwatch(following);
            }
            self.redraw.peek = true;
        }
    }
}

/// The most steps kept: the latest, a long-running agent's first ones dropped.
const MAX_STEPS: usize = 400;

/// The most characters of what it says shown per step.
const MAX_SAID_CHARS: usize = 600;

/// How a task's work is followed, for the kinds whose work can be.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Followed {
    /// Its conversation, from its transcript: a subagent on this machine.
    Conversation,
    /// What it prints, asked of Claude Code: a background command.
    Output,
}

impl Followed {
    pub fn of(kind: TaskKind) -> Option<Self> {
        match kind {
            TaskKind::LocalAgent => Some(Self::Conversation),
            TaskKind::LocalBash => Some(Self::Output),
            TaskKind::RemoteAgent
            | TaskKind::InProcessTeammate
            | TaskKind::LocalWorkflow
            | TaskKind::MonitorMcp
            | TaskKind::MonitorWs
            | TaskKind::Other => None,
        }
    }
}

/// The task peeked into.
#[derive(Debug)]
pub struct Peek {
    pub task: TaskId,
    /// What it was started for.
    pub title: String,
    pub kind: TaskKind,
    pub view: PeekView,
}

#[derive(Debug)]
pub enum PeekView {
    Conversation(Conversation),
    Output(CommandOutput),
}

/// A subagent's conversation, and how far its transcript was read.
#[derive(Debug)]
pub struct Conversation {
    /// Its kind ("Explore"), kept for after it ended.
    pub agent: Option<String>,
    /// Its transcript; it may not be written yet.
    place: Place,
    offset: u64,
    reading: Refresh,
    /// The watch on its transcript.
    following: Option<Watched>,
    pub steps: Vec<Step>,
}

/// What a background command printed, as Claude Code last told it.
#[derive(Debug, Default)]
pub struct CommandOutput {
    /// The end of it, escape sequences taken out.
    pub text: String,
    /// The size of all it printed, of which `text` is only the end if `truncated`.
    pub total_bytes: u64,
    pub truncated: bool,
    asking: Asking,
}

/// Asking for a command's output: again once each answer is in while it runs, and a last time
/// after it ended.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
enum Asking {
    /// No question out: the next poll asks.
    #[default]
    Due,
    /// A question out, asked while it ran.
    Asked,
    /// The last question out, asked after it ended.
    AskedLast,
    /// Nothing more to ask: the answer after its end is in, or there is no process to ask.
    Done,
    /// Claude Code doesn't tell it, in its words.
    Refused(String),
}

impl CommandOutput {
    /// At a poll, the command `running` or ended: whether to ask for its output now.
    fn ask(&mut self, running: bool) -> bool {
        match self.asking {
            Asking::Due => {
                self.asking = if running {
                    Asking::Asked
                } else {
                    Asking::AskedLast
                };
                true
            }
            Asking::Asked | Asking::AskedLast | Asking::Done | Asking::Refused(_) => false,
        }
    }

    /// Takes Claude Code's answer: whether what is shown changed.
    fn told(&mut self, told: &TaskOutput) -> bool {
        self.asking = match std::mem::take(&mut self.asking) {
            Asking::Asked => Asking::Due,
            Asking::AskedLast => Asking::Done,
            unasked @ (Asking::Due | Asking::Done | Asking::Refused(_)) => unasked,
        };
        let changed = told.total_bytes != self.total_bytes || told.truncated != self.truncated;
        if changed {
            self.text = strip_ansi(&told.output);
            self.total_bytes = told.total_bytes;
            self.truncated = told.truncated;
        }
        changed
    }

    fn refused(&mut self, refusal: String) {
        self.asking = Asking::Refused(refusal);
    }

    /// No process to ask any more: what was told last stays.
    fn give_up(&mut self) {
        if !self.finished() {
            self.asking = Asking::Done;
        }
    }

    fn finished(&self) -> bool {
        matches!(self.asking, Asking::Done | Asking::Refused(_))
    }

    /// Why Claude Code doesn't tell it, if it refused.
    pub fn refusal(&self) -> Option<&str> {
        match &self.asking {
            Asking::Refused(refusal) => Some(refusal),
            Asking::Due | Asking::Asked | Asking::AskedLast | Asking::Done => None,
        }
    }
}

/// A step of the subagent's.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Step {
    pub kind: StepKind,
    /// The tool's name, for a call.
    pub title: String,
    pub text: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StepKind {
    /// What it was asked to do.
    Asked,
    Said,
    Called,
}

impl Conversation {
    fn new(agent: Option<String>, place: Place) -> Self {
        Self {
            agent,
            place,
            offset: 0,
            reading: Refresh::default(),
            following: None,
            steps: Vec::new(),
        }
    }

    /// Takes in lines read on from its transcript.
    pub fn take(&mut self, lines: &[TranscriptLine], cwd: &Path) {
        self.steps
            .extend(lines.iter().flat_map(|line| steps(line, cwd)));
        let over = self.steps.len().saturating_sub(MAX_STEPS);
        self.steps.drain(..over);
    }
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
    use std::path::PathBuf;

    use super::*;
    use crate::sessions;
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
        let mut conversation = Conversation::new(None, Place::Path(PathBuf::new()));
        conversation.take(&lines, cwd);
        let [asked, said, called] = conversation.steps.as_slice() else {
            panic!("three steps: {:?}", conversation.steps);
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
        let mut conversation = Conversation::new(None, Place::Path(PathBuf::new()));
        conversation.take(&lines, Path::new("/"));
        assert_eq!(conversation.steps.len(), MAX_STEPS);
        assert_eq!(
            conversation.steps.first().map(|step| step.text.as_str()),
            Some("5")
        );
    }

    fn told(output: &str, total_bytes: u64, truncated: bool) -> TaskOutput {
        serde_json::from_value(json!({
            "output": output, "total_bytes": total_bytes, "truncated": truncated,
        }))
        .expect("an answer")
    }

    /// A running command's output is asked for again once each answer is in, never twice at
    /// once; after it ended, once more, and then no more.
    #[test]
    fn a_commands_output_is_asked_for_one_answer_at_a_time() {
        let mut output = CommandOutput::default();
        assert!(output.ask(true), "the first poll asks");
        assert!(!output.ask(true), "not again while the question is out");
        assert!(output.told(&told("\u{1b}[32mok\u{1b}[0m\n", 12, false)));
        assert_eq!(output.text, "ok\n", "escape sequences taken out");
        assert!(output.ask(true), "the answer is in: the next poll asks");
        assert!(
            !output.told(&told("\u{1b}[32mok\u{1b}[0m\n", 12, false)),
            "nothing more printed: nothing to show anew"
        );
        assert!(output.ask(true));
        assert!(output.told(&told("ok\nmore\n", 8, false)));
        assert!(output.ask(false), "it ended: asked a last time");
        assert!(!output.ask(false));
        assert!(!output.finished(), "until that answer is in");
        output.told(&told("ok\nmore\ndone\n", 13, false));
        assert!(output.finished());
        assert!(!output.ask(false), "nothing more to ask");
        assert_eq!(output.text, "ok\nmore\ndone\n");
    }

    /// It ending while a question is out still gets it a last question once that is answered.
    #[test]
    fn a_command_ending_meanwhile_is_asked_once_more() {
        let mut output = CommandOutput::default();
        assert!(output.ask(true));
        output.told(&told("half", 4, false));
        assert!(!output.finished());
        assert!(output.ask(false), "the poll after its end asks");
        output.told(&told("whole", 5, false));
        assert!(output.finished());
    }

    /// A refusal ends the asking, and says why; without a process, what was told stays.
    #[test]
    fn a_refused_or_unanswerable_output_is_asked_no_more() {
        let mut output = CommandOutput::default();
        assert!(output.ask(true));
        output.refused("no such shell task".to_owned());
        assert_eq!(output.refusal(), Some("no such shell task"));
        assert!(output.finished() && !output.ask(true));
        output.give_up();
        assert_eq!(output.refusal(), Some("no such shell task"), "kept");
        let mut gone = CommandOutput::default();
        assert!(gone.ask(true));
        gone.told(&told("last words", 10, true));
        gone.give_up();
        assert!(gone.finished() && !gone.ask(true));
        assert_eq!(
            (gone.text.as_str(), gone.truncated, gone.refusal()),
            ("last words", true, None)
        );
    }

    /// The transcript sits in the session's folder of subagents, named by the task.
    #[test]
    fn a_subagents_transcript_is_beside_its_sessions() {
        let session = Path::new("/home/u/.claude/projects/-p/0b8f.jsonl");
        assert_eq!(
            sessions::transcript_of(session, &TaskId::from("a50b8b2d1bc3507ae".to_owned())),
            Path::new("/home/u/.claude/projects/-p/0b8f/subagents/agent-a50b8b2d1bc3507ae.jsonl")
        );
    }
}
