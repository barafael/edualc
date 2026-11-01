//! The conversation as the window shows it: entries made from the CLI's messages (live,
//! partial messages included) or from a saved transcript, and which of them changed since the
//! window last looked.

use std::collections::{HashMap, HashSet};

use serde_json::{Map, Value};

use crate::external::wire::{
    ApiMessage, ContentBlock, Delta, MessageId, StreamEvent, System, ToolName, ToolUseId,
    TranscriptLine, UserContent, UserMessage,
};
use crate::tools;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ToolState {
    /// Its input is still arriving.
    Streaming,
    Running,
    /// Waiting for the user to allow or deny it.
    Awaiting,
    Done,
    Failed,
    Denied,
    /// The turn ended (interrupted) before it did.
    Cancelled,
}

#[derive(Debug, Clone)]
pub(crate) struct Tool {
    pub(crate) name: ToolName,
    pub(crate) input: Value,
    partial_input: String,
    pub(crate) output: String,
    pub(crate) state: ToolState,
}

#[derive(Debug, Clone)]
pub(crate) enum Body {
    User(String),
    Assistant(String),
    Thinking { text: String, expanded: bool },
    Tool { tool: Tool, expanded: bool },
    Info(String),
    Error(String),
}

#[derive(Debug, Clone)]
pub(crate) struct Entry {
    pub(crate) body: Body,
    /// Made by a subagent (its messages carry the tool call that started it).
    pub(crate) nested: bool,
}

impl Entry {
    /// What copying the whole entry takes: a message's Markdown, a tool call's whole output.
    pub(crate) fn copy_text(&self) -> &str {
        match &self.body {
            Body::User(text)
            | Body::Assistant(text)
            | Body::Info(text)
            | Body::Error(text)
            | Body::Thinking { text, .. } => text,
            Body::Tool { tool, .. } => &tool.output,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Change {
    Reset,
    Pushed(usize),
    /// Only the text grew (streaming).
    Grew(usize),
    Updated(usize),
}

#[derive(Debug, Default)]
pub(crate) struct Chat {
    pub(crate) entries: Vec<Entry>,
    changes: Vec<Change>,
    tools: HashMap<ToolUseId, usize>,
    /// Streamed content blocks: (message, block index) → entry.
    blocks: HashMap<(MessageId, u64), usize>,
    /// Messages seen as stream events: their complete copies only add tool input.
    streamed: HashSet<MessageId>,
    /// The message being streamed, by subagent (`None` for the main conversation).
    current: HashMap<Option<ToolUseId>, MessageId>,
}

/// Where messages come from: the process as it runs, or a saved transcript, where the user's
/// own prompts are among them and a call without a result was cut off.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Origin {
    Live,
    Transcript,
}

/// What the text of a user message is: a prompt, or one of the wrappers Claude Code puts its
/// own doings in.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum UserText<'a> {
    Prompt(&'a str),
    Command {
        name: &'a str,
        args: &'a str,
    },
    /// A shell command the user ran with `!`.
    Shell(&'a str),
    Stdout(&'a str),
    Stderr(&'a str),
    Interrupted,
    /// Caveats, system reminders, task notifications: meant for the model.
    ForModel,
    Empty,
}

impl<'a> UserText<'a> {
    pub(crate) fn of(text: &'a str) -> Self {
        let text = text.trim();
        if text.is_empty() {
            Self::Empty
        } else if text.starts_with("[Request interrupted") {
            Self::Interrupted
        } else if let Some(output) =
            between(text, "<local-command-stdout>", "</local-command-stdout>")
                .or_else(|| between(text, "<bash-stdout>", "</bash-stdout>"))
        {
            Self::Stdout(output.trim())
        } else if let Some(output) =
            between(text, "<local-command-stderr>", "</local-command-stderr>")
        {
            Self::Stderr(output.trim())
        } else if let Some(name) = between(text, "<command-name>", "</command-name>") {
            let args = between(text, "<command-args>", "</command-args>").unwrap_or("");
            Self::Command { name, args }
        } else if let Some(input) = between(text, "<bash-input>", "</bash-input>") {
            Self::Shell(input)
        } else if text.starts_with('<') {
            Self::ForModel
        } else {
            Self::Prompt(text)
        }
    }
}

impl Chat {
    pub(crate) fn take_changes(&mut self) -> Vec<Change> {
        std::mem::take(&mut self.changes)
    }

    pub(crate) fn clear(&mut self) {
        *self = Chat::default();
        self.changes.push(Change::Reset);
    }

    fn push(&mut self, body: Body, nested: bool) -> usize {
        let index = self.entries.len();
        self.entries.push(Entry { body, nested });
        self.changes.push(Change::Pushed(index));
        index
    }

    pub(crate) fn user(&mut self, text: impl Into<String>) {
        self.push(Body::User(text.into()), false);
    }

    pub(crate) fn info(&mut self, text: impl Into<String>) {
        self.push(Body::Info(text.into()), false);
    }

    pub(crate) fn error(&mut self, text: impl Into<String>) {
        self.push(Body::Error(text.into()), false);
    }

    fn push_tool(
        &mut self,
        id: &ToolUseId,
        name: &ToolName,
        input: Value,
        state: ToolState,
        nested: bool,
    ) -> usize {
        let expanded = name.opens_expanded();
        let tool = Tool {
            name: name.clone(),
            input,
            partial_input: String::new(),
            output: String::new(),
            state,
        };
        let index = self.push(Body::Tool { tool, expanded }, nested);
        self.tools.insert(id.clone(), index);
        index
    }

    /// The tool call a permission request is about, showing the input asked about: made if the
    /// stream hasn't shown it yet, and taking that input if it differs from what streamed (a
    /// hook may have changed it), so the card shows what is approved.
    pub(crate) fn show_asked(&mut self, id: &ToolUseId, name: &ToolName, input: &Value) {
        match self.tool_mut(id) {
            Some((index, tool)) => {
                if tool.input != *input {
                    tool.input = input.clone();
                    self.changes.push(Change::Updated(index));
                }
            }
            None => {
                self.push_tool(id, name, input.clone(), ToolState::Running, false);
            }
        }
    }

    fn tool_mut(&mut self, id: &ToolUseId) -> Option<(usize, &mut Tool)> {
        let index = *self.tools.get(id)?;
        match &mut self.entries.get_mut(index)?.body {
            Body::Tool { tool, .. } => Some((index, tool)),
            Body::User(_)
            | Body::Assistant(_)
            | Body::Thinking { .. }
            | Body::Info(_)
            | Body::Error(_) => None,
        }
    }

    pub(crate) fn set_tool_state(&mut self, id: &ToolUseId, state: ToolState) {
        if let Some((index, tool)) = self.tool_mut(id)
            && tool.state != state
        {
            tool.state = state;
            self.changes.push(Change::Updated(index));
        }
    }

    /// At the end of a turn: what never finished was cut off.
    pub(crate) fn end_turn(&mut self) {
        for (index, entry) in self.entries.iter_mut().enumerate() {
            if let Body::Tool { tool, .. } = &mut entry.body
                && matches!(
                    tool.state,
                    ToolState::Streaming | ToolState::Running | ToolState::Awaiting
                )
            {
                tool.state = ToolState::Cancelled;
                self.changes.push(Change::Updated(index));
            }
        }
    }

    pub(crate) fn toggle(&mut self, index: usize) {
        let Some(entry) = self.entries.get_mut(index) else {
            return;
        };
        match &mut entry.body {
            Body::Thinking { expanded, .. } | Body::Tool { expanded, .. } => *expanded = !*expanded,
            Body::User(_) | Body::Assistant(_) | Body::Info(_) | Body::Error(_) => return,
        }
        self.changes.push(Change::Updated(index));
    }

    fn grow(&mut self, slot: &(MessageId, u64), text: &str) {
        let Some(&index) = self.blocks.get(slot) else {
            return;
        };
        if let Some(Entry {
            body: Body::Assistant(body) | Body::Thinking { text: body, .. },
            ..
        }) = self.entries.get_mut(index)
        {
            body.push_str(text);
            self.changes.push(Change::Grew(index));
        }
    }

    /// A raw API stream event, from `--include-partial-messages`.
    pub(crate) fn stream_event(&mut self, event: &StreamEvent, parent: Option<&ToolUseId>) {
        let key = parent.cloned();
        let nested = parent.is_some();
        match event {
            StreamEvent::MessageStart { message } => {
                self.streamed.insert(message.id.clone());
                self.current.insert(key, message.id.clone());
            }
            StreamEvent::ContentBlockStart {
                index,
                content_block,
            } => {
                let Some(message) = self.current.get(&key).cloned() else {
                    return;
                };
                let entry = match content_block {
                    ContentBlock::Text { text } => {
                        Some(self.push(Body::Assistant(text.clone()), nested))
                    }
                    ContentBlock::ToolUse { id, name, .. }
                    | ContentBlock::ServerToolUse { id, name, .. } => Some(self.push_tool(
                        id,
                        name,
                        Value::Object(Map::new()),
                        ToolState::Streaming,
                        nested,
                    )),
                    // Thinking gets an entry once it has text: often it arrives redacted.
                    ContentBlock::Thinking { .. }
                    | ContentBlock::ToolResult { .. }
                    | ContentBlock::Image
                    | ContentBlock::Other => None,
                };
                if let Some(entry) = entry {
                    self.blocks.insert((message, *index), entry);
                }
            }
            StreamEvent::ContentBlockDelta { index, delta } => {
                let Some(message) = self.current.get(&key).cloned() else {
                    return;
                };
                let slot = (message, *index);
                match delta {
                    Delta::Text { text } => self.grow(&slot, text),
                    Delta::Thinking { thinking } if !thinking.is_empty() => {
                        if !self.blocks.contains_key(&slot) {
                            let body = Body::Thinking {
                                text: String::new(),
                                expanded: false,
                            };
                            let entry = self.push(body, nested);
                            self.blocks.insert(slot.clone(), entry);
                        }
                        self.grow(&slot, thinking);
                    }
                    Delta::InputJson { partial_json } => {
                        if let Some(&entry) = self.blocks.get(&slot)
                            && let Some(Entry {
                                body: Body::Tool { tool, .. },
                                ..
                            }) = self.entries.get_mut(entry)
                        {
                            tool.partial_input.push_str(partial_json);
                        }
                    }
                    Delta::Thinking { .. } | Delta::Other => {}
                }
            }
            StreamEvent::ContentBlockStop { index } => {
                let Some(message) = self.current.get(&key).cloned() else {
                    return;
                };
                let Some(&entry) = self.blocks.get(&(message, *index)) else {
                    return;
                };
                if let Some(Entry {
                    body: Body::Tool { tool, .. },
                    ..
                }) = self.entries.get_mut(entry)
                {
                    match serde_json::from_str(&tool.partial_input) {
                        Ok(input) => tool.input = input,
                        Err(error) => {
                            tracing::debug!(%error, "a streamed tool input that doesn't parse");
                        }
                    }
                    if tool.state == ToolState::Streaming {
                        tool.state = ToolState::Running;
                    }
                    self.changes.push(Change::Updated(entry));
                }
            }
            StreamEvent::Other => {}
        }
    }

    /// A complete assistant message: one content block each, as the CLI sends them.
    pub(crate) fn assistant_message(
        &mut self,
        message: &ApiMessage,
        parent: Option<&ToolUseId>,
        origin: Origin,
    ) {
        let nested = parent.is_some();
        let streamed = self.streamed.contains(&message.id);
        for block in &message.content {
            match block {
                ContentBlock::Text { text } if !streamed && !text.trim().is_empty() => {
                    self.push(Body::Assistant(text.clone()), nested);
                }
                ContentBlock::Thinking { thinking } if !streamed && !thinking.trim().is_empty() => {
                    let body = Body::Thinking {
                        text: thinking.clone(),
                        expanded: false,
                    };
                    self.push(body, nested);
                }
                ContentBlock::ToolUse { id, name, input }
                | ContentBlock::ServerToolUse { id, name, input } => {
                    match self.tool_mut(id) {
                        Some((index, tool)) => {
                            tool.input = input.clone();
                            if tool.state == ToolState::Streaming {
                                tool.state = ToolState::Running;
                            }
                            self.changes.push(Change::Updated(index));
                        }
                        None => {
                            // In a transcript, a call without a result was cut off.
                            let state = match origin {
                                Origin::Transcript => ToolState::Cancelled,
                                Origin::Live => ToolState::Running,
                            };
                            self.push_tool(id, name, input.clone(), state, nested);
                        }
                    }
                }
                ContentBlock::Text { .. }
                | ContentBlock::Thinking { .. }
                | ContentBlock::ToolResult { .. }
                | ContentBlock::Image
                | ContentBlock::Other => {}
            }
        }
    }

    /// Tool results, interruptions, what slash commands print; in a transcript, the prompts.
    pub(crate) fn user_message(&mut self, message: &UserMessage, origin: Origin) {
        match &message.content {
            UserContent::Text(text) => self.user_text(text, origin),
            UserContent::Blocks(blocks) => {
                for block in blocks {
                    match block {
                        ContentBlock::ToolResult {
                            tool_use_id,
                            content,
                            is_error,
                        } => self.tool_result(tool_use_id, content.as_ref(), *is_error),
                        ContentBlock::Text { text } => self.user_text(text, origin),
                        ContentBlock::Image if origin == Origin::Transcript => self.user("[image]"),
                        ContentBlock::Image
                        | ContentBlock::Thinking { .. }
                        | ContentBlock::ToolUse { .. }
                        | ContentBlock::ServerToolUse { .. }
                        | ContentBlock::Other => {}
                    }
                }
            }
        }
    }

    fn tool_result(&mut self, id: &ToolUseId, content: Option<&UserContent>, failed: bool) {
        if let Some((index, tool)) = self.tool_mut(id) {
            tool.output = tools::result_text(content);
            tool.state = match (failed, tool.state) {
                (true, ToolState::Denied) => ToolState::Denied,
                (true, _) => ToolState::Failed,
                (false, _) => ToolState::Done,
            };
            self.changes.push(Change::Updated(index));
        }
    }

    fn user_text(&mut self, text: &str, origin: Origin) {
        match UserText::of(text) {
            UserText::Interrupted => self.info("Interrupted"),
            UserText::Stdout(output) if !output.is_empty() => self.info(strip_ansi(output)),
            UserText::Stderr(output) if !output.is_empty() => self.error(strip_ansi(output)),
            UserText::Command { name, args } => self.user(format!("{name} {args}").trim()),
            UserText::Shell(input) => self.user(format!("! {input}")),
            UserText::Prompt(text) => match origin {
                Origin::Transcript => self.user(text),
                Origin::Live => self.info(text),
            },
            UserText::Stdout(_) | UserText::Stderr(_) | UserText::ForModel | UserText::Empty => {}
        }
    }

    /// A saved session's messages, as a conversation to show in place of what is.
    pub(crate) fn from_transcript(lines: &[TranscriptLine]) -> Self {
        let mut chat = Self::default();
        chat.replay(lines);
        // Loading built every row: the window rebuilds once instead.
        chat.changes = vec![Change::Reset];
        chat
    }

    /// More of a saved session's messages, after those shown (a session running elsewhere).
    pub(crate) fn replay(&mut self, lines: &[TranscriptLine]) {
        for line in lines {
            match line {
                TranscriptLine::User {
                    message,
                    is_meta: false,
                    is_sidechain: false,
                } => self.user_message(message, Origin::Transcript),
                TranscriptLine::Assistant {
                    message,
                    is_sidechain: false,
                } => self.assistant_message(message, None, Origin::Transcript),
                TranscriptLine::System(System::CompactBoundary) => {
                    self.info("Conversation compacted");
                }
                TranscriptLine::User { .. }
                | TranscriptLine::Assistant { .. }
                | TranscriptLine::System(_)
                | TranscriptLine::CustomTitle { .. }
                | TranscriptLine::AiTitle { .. }
                | TranscriptLine::Summary { .. }
                | TranscriptLine::WorktreeState { .. }
                | TranscriptLine::Other => {}
            }
        }
    }
}

fn between<'a>(text: &'a str, open: &str, close: &str) -> Option<&'a str> {
    let start = text.find(open)? + open.len();
    let rest = text.get(start..)?;
    Some(
        rest.find(close)
            .and_then(|end| rest.get(..end))
            .unwrap_or(rest),
    )
}

/// Slash command output is meant for a terminal.
fn strip_ansi(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(character) = chars.next() {
        if character != '\u{1b}' {
            out.push(character);
        } else if chars.peek() == Some(&'[') {
            chars.next();
            for code in chars.by_ref() {
                if code.is_ascii_alphabetic() {
                    break;
                }
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn stream(chat: &mut Chat, event: Value) {
        let event: StreamEvent = serde_json::from_value(event).expect("a stream event");
        chat.stream_event(&event, None);
    }

    fn texts(chat: &Chat) -> Vec<&str> {
        chat.entries.iter().map(Entry::copy_text).collect()
    }

    fn first_tool(chat: &Chat) -> &Tool {
        match chat.entries.first().map(|entry| &entry.body) {
            Some(Body::Tool { tool, .. }) => tool,
            other => panic!("a tool call, not {other:?}"),
        }
    }

    #[test]
    fn streamed_text_is_not_duplicated_by_the_complete_message() {
        let mut chat = Chat::default();
        stream(
            &mut chat,
            json!({ "type": "message_start", "message": { "id": "m1" } }),
        );
        stream(
            &mut chat,
            json!({
                "type": "content_block_start",
                "index": 0,
                "content_block": { "type": "text", "text": "" },
            }),
        );
        for text in ["Hel", "lo"] {
            stream(
                &mut chat,
                json!({
                    "type": "content_block_delta",
                    "index": 0,
                    "delta": { "type": "text_delta", "text": text },
                }),
            );
        }
        let message: ApiMessage = serde_json::from_value(json!({
            "id": "m1",
            "content": [{ "type": "text", "text": "Hello" }],
        }))
        .expect("a message");
        chat.assistant_message(&message, None, Origin::Live);
        stream(
            &mut chat,
            json!({ "type": "content_block_stop", "index": 0 }),
        );
        assert_eq!(texts(&chat), ["Hello"]);
    }

    #[test]
    fn tool_calls_stream_their_input_then_take_their_result() {
        let mut chat = Chat::default();
        stream(
            &mut chat,
            json!({ "type": "message_start", "message": { "id": "m1" } }),
        );
        stream(
            &mut chat,
            json!({
                "type": "content_block_start",
                "index": 0,
                "content_block": { "type": "tool_use", "id": "t1", "name": "Bash", "input": {} },
            }),
        );
        // The input arrives as pieces of its JSON text.
        let input = json!({ "command": "ls" });
        for piece in input.to_string().split_inclusive(':') {
            stream(
                &mut chat,
                json!({
                    "type": "content_block_delta",
                    "index": 0,
                    "delta": { "type": "input_json_delta", "partial_json": piece },
                }),
            );
        }
        stream(
            &mut chat,
            json!({ "type": "content_block_stop", "index": 0 }),
        );
        assert_eq!(first_tool(&chat).input, input);
        assert_eq!(first_tool(&chat).state, ToolState::Running);

        let result: UserMessage = serde_json::from_value(json!({
            "content": [{
                "type": "tool_result",
                "tool_use_id": "t1",
                "content": "a.txt",
                "is_error": false,
            }],
        }))
        .expect("a tool result");
        chat.user_message(&result, Origin::Live);
        assert_eq!(first_tool(&chat).output, "a.txt");
        assert_eq!(first_tool(&chat).state, ToolState::Done);
    }

    #[test]
    fn interruptions_and_command_output_become_notes() {
        let mut chat = Chat::default();
        for message in [
            json!({ "content": [{ "type": "text", "text": "[Request interrupted by user]" }] }),
            json!({ "content": "<local-command-stdout>Set model to `sonnet`</local-command-stdout>" }),
        ] {
            chat.user_message(
                &serde_json::from_value(message).expect("a user message"),
                Origin::Live,
            );
        }
        assert!(
            chat.entries
                .iter()
                .all(|entry| matches!(entry.body, Body::Info(_)))
        );
        assert_eq!(texts(&chat), ["Interrupted", "Set model to `sonnet`"]);
    }

    #[test]
    fn transcripts_show_prompts_and_skip_meta() {
        let lines: Vec<TranscriptLine> = [
            json!({
                "type": "user",
                "isMeta": true,
                "message": { "content": "<local-command-caveat>x</local-command-caveat>" },
            }),
            json!({ "type": "user", "message": { "content": "Fix the bug" } }),
            json!({
                "type": "assistant",
                "message": {
                    "id": "m1",
                    "content": [{
                        "type": "tool_use",
                        "id": "t1",
                        "name": "Read",
                        "input": { "file_path": "/a" },
                    }],
                },
            }),
            json!({
                "type": "user",
                "message": {
                    "content": [{ "type": "tool_result", "tool_use_id": "t1", "content": "..." }],
                },
            }),
            json!({
                "type": "assistant",
                "message": { "id": "m2", "content": [{ "type": "text", "text": "Done." }] },
            }),
            json!({
                "type": "user",
                "isSidechain": true,
                "message": { "content": "subagent prompt" },
            }),
            json!({ "type": "ai-title", "aiTitle": "Bug fix" }),
        ]
        .into_iter()
        .map(|line| serde_json::from_value(line).expect("a transcript line"))
        .collect();
        let mut chat = Chat::from_transcript(&lines);
        assert_eq!(texts(&chat), ["Fix the bug", "...", "Done."]);
        assert!(matches!(
            chat.entries.get(1).map(|entry| &entry.body),
            Some(Body::Tool { tool, .. }) if tool.state == ToolState::Done
        ));
        assert_eq!(chat.take_changes(), [Change::Reset]);
    }
}
