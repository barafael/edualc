//! The conversation as the window shows it: entries made from the CLI's messages (live,
//! partial messages included) or from a saved transcript, and which of them changed since the
//! window last looked.

use std::collections::{HashMap, HashSet};

use serde_json::{Map, Value};

use crate::decode::Picture;
use crate::external::wire::{
    ApiMessage, ContentBlock, Delta, MessageId, MessageOrigin, ReceivedImage, StreamEvent, System,
    ToolName, ToolUseId, TranscriptLine, UserContent, UserMessage,
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
    /// The images it returned (a file read, a screenshot).
    pub(crate) images: Vec<ImageKey>,
    pub(crate) state: ToolState,
}

/// An image in the conversation, known by its content: the same image twice is one image, and
/// one's pixels, decoded late, can't come to another.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct ImageKey(u64);

impl ImageKey {
    fn of(bytes: &[u8]) -> Self {
        use std::hash::{Hash, Hasher};
        let mut hasher = std::hash::DefaultHasher::new();
        bytes.hash(&mut hasher);
        Self(hasher.finish())
    }
}

/// An image's bytes, to be decoded for showing: as a message holds them, or as the composer
/// sent them (already decoded and encoded again once).
#[derive(Debug, Clone)]
pub(crate) enum ImageBytes {
    Base64(String),
    Sent(Vec<u8>),
}

/// A command the user ran with `!`, as Claude Code ran it, and what it printed.
#[derive(Debug, Clone)]
pub(crate) struct Shell {
    pub(crate) command: String,
    pub(crate) output: String,
    pub(crate) state: ShellState,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ShellState {
    Running,
    /// It exited with 0 (or, as the terminal saves it, without telling how).
    Done,
    /// It exited otherwise, or wasn't run.
    Failed,
    /// An interrupt stopped it; or its process ended, or its session was saved, first.
    Stopped,
}

#[derive(Debug, Clone)]
pub(crate) enum Body {
    User(String),
    Assistant(String),
    Thinking {
        text: String,
        expanded: bool,
    },
    Tool {
        tool: Tool,
        expanded: bool,
    },
    /// Beside Claude's tool calls, never folded into their runs, and not ended with a turn:
    /// it runs beside the turns.
    Shell {
        shell: Shell,
        expanded: bool,
    },
    /// An image the user sent.
    Image(ImageKey),
    /// A message another session sent, by the name it gave, if any.
    Peer {
        from: Option<String>,
        text: String,
    },
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
            | Body::Thinking { text, .. }
            | Body::Peer { text, .. } => text,
            Body::Tool { tool, .. } => &tool.output,
            Body::Shell { shell, .. } => &shell.output,
            Body::Image(_) => "",
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
    /// Images come since the window last took them, to be decoded for showing.
    new_images: Vec<(ImageKey, ImageBytes)>,
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
    /// How a `!` command ended. No `stdout` is a command Claude Code didn't run, its `stderr`
    /// saying why; the terminal saves no exit code.
    ShellOutput {
        stdout: Option<&'a str>,
        stderr: &'a str,
        exit_code: Option<&'a str>,
    },
    /// A slash command's output.
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
        } else if text.starts_with("<bash-stdout>") || text.starts_with("<bash-stderr>") {
            Self::ShellOutput {
                stdout: between(text, "<bash-stdout>", "</bash-stdout>"),
                stderr: between(text, "<bash-stderr>", "</bash-stderr>").unwrap_or(""),
                exit_code: between(text, "<bash-exit-code>", "</bash-exit-code>"),
            }
        } else if let Some(output) =
            between(text, "<local-command-stdout>", "</local-command-stdout>")
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

    /// Marks every entry changed, to be drawn again (in another font, say).
    pub(crate) fn touch_all(&mut self) {
        self.changes
            .extend((0..self.entries.len()).map(Change::Updated));
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

    /// A message another session sent: its `body`, or, for one that came without an envelope
    /// to take the body from, the whole text Claude was given.
    pub(crate) fn peer_message(
        &mut self,
        message: &UserMessage,
        from: Option<&str>,
        body: Option<&str>,
    ) {
        let text = body.map_or_else(|| tools::result_text(Some(&message.content)), str::to_owned);
        self.push(
            Body::Peer {
                from: from.map(str::to_owned),
                text,
            },
            false,
        );
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
            images: Vec::new(),
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
            | Body::Shell { .. }
            | Body::Image(_)
            | Body::Peer { .. }
            | Body::Info(_)
            | Body::Error(_) => None,
        }
    }

    /// Images the user sent from the composer, each a row of its own.
    pub(crate) fn user_images(&mut self, pictures: &[Picture]) {
        for picture in pictures {
            let key = ImageKey::of(&picture.bytes);
            self.new_images
                .push((key, ImageBytes::Sent(picture.bytes.clone())));
            self.push(Body::Image(key), false);
        }
    }

    /// The images come since the last call, to be decoded for showing.
    pub(crate) fn take_images(&mut self) -> Vec<(ImageKey, ImageBytes)> {
        std::mem::take(&mut self.new_images)
    }

    /// Image `key`'s pixels came (or it can't be shown): the rows showing it show it anew.
    pub(crate) fn image_shown(&mut self, key: ImageKey) {
        for (index, entry) in self.entries.iter().enumerate() {
            let shows = match &entry.body {
                Body::Image(shown) => *shown == key,
                Body::Tool { tool, .. } => tool.images.contains(&key),
                Body::User(_)
                | Body::Assistant(_)
                | Body::Thinking { .. }
                | Body::Shell { .. }
                | Body::Peer { .. }
                | Body::Info(_)
                | Body::Error(_) => false,
            };
            if shows {
                self.changes.push(Change::Updated(index));
            }
        }
    }

    /// The images the conversation shows.
    pub(crate) fn image_keys(&self) -> HashSet<ImageKey> {
        self.entries
            .iter()
            .flat_map(|entry| match &entry.body {
                Body::Image(key) => std::slice::from_ref(key),
                Body::Tool { tool, .. } => tool.images.as_slice(),
                Body::User(_)
                | Body::Assistant(_)
                | Body::Thinking { .. }
                | Body::Shell { .. }
                | Body::Peer { .. }
                | Body::Info(_)
                | Body::Error(_) => &[],
            })
            .copied()
            .collect()
    }

    /// A `!` command, as Claude Code echoed it: it runs until its output comes. Its card is
    /// open, as the command was run to see what it prints.
    fn shell(&mut self, input: &str) {
        let shell = Shell {
            command: unescaped(input),
            output: String::new(),
            state: ShellState::Running,
        };
        self.push(
            Body::Shell {
                shell,
                expanded: true,
            },
            false,
        );
    }

    /// A `!` command's end, for the latest one running: one runs at a time, and one refused
    /// meanwhile is told of at once, before the one running ends. `stdout` is none for one
    /// that wasn't run, its `stderr` saying why.
    fn shell_ended(&mut self, stdout: Option<&str>, stderr: &str, exit_code: Option<&str>) {
        let state = match (stdout, exit_code.map(str::trim)) {
            (Some(_), None | Some("0")) => ShellState::Done,
            (Some(_), Some("130")) if stderr.trim() == "[interrupted]" => ShellState::Stopped,
            (None, _) | (Some(_), Some(_)) => ShellState::Failed,
        };
        let mut output: Vec<String> = [stdout.unwrap_or(""), stderr]
            .into_iter()
            .map(|text| strip_ansi(&unescaped(text.trim_end())))
            .filter(|text| !text.is_empty())
            .collect();
        if let (ShellState::Failed, Some(code)) = (state, exit_code) {
            output.push(format!("Exit code {}", code.trim()));
        }
        let output = output.join("\n");
        let running = self
            .entries
            .iter_mut()
            .enumerate()
            .rev()
            .find_map(|(index, entry)| match &mut entry.body {
                Body::Shell { shell, .. } if shell.state == ShellState::Running => {
                    Some((index, shell))
                }
                Body::Shell { .. }
                | Body::User(_)
                | Body::Assistant(_)
                | Body::Thinking { .. }
                | Body::Tool { .. }
                | Body::Image(_)
                | Body::Peer { .. }
                | Body::Info(_)
                | Body::Error(_) => None,
            });
        match running {
            Some((index, shell)) => {
                shell.output = output;
                shell.state = state;
                self.changes.push(Change::Updated(index));
            }
            None if output.is_empty() => {}
            None if state == ShellState::Failed => self.error(output),
            None => self.info(output),
        }
    }

    /// Whether a `!` command runs.
    pub(crate) fn shell_running(&self) -> bool {
        self.entries.iter().any(|entry| {
            matches!(&entry.body, Body::Shell { shell, .. } if shell.state == ShellState::Running)
        })
    }

    /// The `!` commands still running run no more: their process ended, or their session was
    /// saved without their end.
    pub(crate) fn stop_shells(&mut self) {
        for (index, entry) in self.entries.iter_mut().enumerate() {
            if let Body::Shell { shell, .. } = &mut entry.body
                && shell.state == ShellState::Running
            {
                shell.state = ShellState::Stopped;
                self.changes.push(Change::Updated(index));
            }
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
            Body::Thinking { expanded, .. }
            | Body::Tool { expanded, .. }
            | Body::Shell { expanded, .. } => *expanded = !*expanded,
            Body::User(_)
            | Body::Assistant(_)
            | Body::Image(_)
            | Body::Peer { .. }
            | Body::Info(_)
            | Body::Error(_) => return,
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
                    | ContentBlock::Image { .. }
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
                | ContentBlock::Image { .. }
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
                        ContentBlock::Image { source } => match received(source.as_ref()) {
                            Some((key, bytes)) => {
                                self.new_images.push((key, bytes));
                                self.push(Body::Image(key), false);
                            }
                            None => self.user("[image]"),
                        },
                        ContentBlock::Thinking { .. }
                        | ContentBlock::ToolUse { .. }
                        | ContentBlock::ServerToolUse { .. }
                        | ContentBlock::Other => {}
                    }
                }
            }
        }
    }

    /// A tool's result; a card with images in it opens, to show them.
    fn tool_result(&mut self, id: &ToolUseId, content: Option<&UserContent>, failed: bool) {
        let images: Vec<(ImageKey, ImageBytes)> = match content {
            Some(UserContent::Blocks(blocks)) => blocks
                .iter()
                .filter_map(|block| match block {
                    ContentBlock::Image { source } => received(source.as_ref()),
                    ContentBlock::Text { .. }
                    | ContentBlock::Thinking { .. }
                    | ContentBlock::ToolUse { .. }
                    | ContentBlock::ServerToolUse { .. }
                    | ContentBlock::ToolResult { .. }
                    | ContentBlock::Other => None,
                })
                .collect(),
            Some(UserContent::Text(_)) | None => Vec::new(),
        };
        let Some(index) = self.tools.get(id).copied() else {
            return;
        };
        let Some(Entry {
            body: Body::Tool { tool, expanded },
            ..
        }) = self.entries.get_mut(index)
        else {
            return;
        };
        tool.output = tools::result_text(content);
        tool.state = match (failed, tool.state) {
            (true, ToolState::Denied) => ToolState::Denied,
            (true, _) => ToolState::Failed,
            (false, _) => ToolState::Done,
        };
        if !images.is_empty() {
            tool.images = images.iter().map(|(key, _)| *key).collect();
            *expanded = true;
            self.new_images.extend(images);
        }
        self.changes.push(Change::Updated(index));
    }

    fn user_text(&mut self, text: &str, origin: Origin) {
        match UserText::of(text) {
            UserText::Interrupted => self.info("Interrupted"),
            UserText::Stdout(output) if !output.is_empty() => self.info(strip_ansi(output)),
            UserText::Stderr(output) if !output.is_empty() => self.error(strip_ansi(output)),
            UserText::Command { name, args } => self.user(format!("{name} {args}").trim()),
            UserText::Shell(input) => self.shell(input),
            UserText::ShellOutput {
                stdout,
                stderr,
                exit_code,
            } => self.shell_ended(stdout, stderr, exit_code),
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
        chat.stop_shells();
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
                    is_sidechain: false,
                    origin: Some(MessageOrigin::Peer { name, body }),
                    ..
                } => self.peer_message(message, name.as_deref(), body.as_deref()),
                TranscriptLine::User {
                    message,
                    is_meta: false,
                    is_sidechain: false,
                    ..
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

/// An image a message holds, if it holds its bytes (and not, say, a link).
fn received(source: Option<&ReceivedImage>) -> Option<(ImageKey, ImageBytes)> {
    match source? {
        ReceivedImage::Base64 { data } => Some((
            ImageKey::of(data.as_bytes()),
            ImageBytes::Base64(data.clone()),
        )),
        ReceivedImage::Other => None,
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

/// Text of a `!` row as Claude Code writes it, with `&`, `<` and `>` escaped (so that what a
/// command printed can't close the row's tags), as it was.
fn unescaped(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some((before, after)) = rest.split_once('&') {
        out.push_str(before);
        let (character, after) = [("lt;", '<'), ("gt;", '>'), ("amp;", '&')]
            .into_iter()
            .find_map(|(entity, character)| Some((character, after.strip_prefix(entity)?)))
            .unwrap_or(('&', after));
        out.push(character);
        rest = after;
    }
    out.push_str(rest);
    out
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

    fn said(chat: &mut Chat, content: Value, origin: Origin) {
        let message = Map::from_iter([("content".to_owned(), content)]);
        let message: UserMessage =
            serde_json::from_value(Value::Object(message)).expect("a user message");
        chat.user_message(&message, origin);
    }

    fn shells(chat: &Chat) -> Vec<(&str, &str, ShellState)> {
        chat.entries
            .iter()
            .filter_map(|entry| match &entry.body {
                Body::Shell { shell, .. } => {
                    Some((shell.command.as_str(), shell.output.as_str(), shell.state))
                }
                Body::User(_)
                | Body::Assistant(_)
                | Body::Thinking { .. }
                | Body::Tool { .. }
                | Body::Image(_)
                | Body::Peer { .. }
                | Body::Info(_)
                | Body::Error(_) => None,
            })
            .collect()
    }

    /// `!` rows as Claude Code 2.1.316 replays them: each output ends the latest command still
    /// running, so one refused while another runs is told of in its own card.
    #[test]
    fn shell_commands_run_until_their_output_comes() {
        let mut chat = Chat::default();
        said(
            &mut chat,
            json!("<bash-input>sleep 4; echo first</bash-input>"),
            Origin::Live,
        );
        assert!(chat.shell_running());
        said(
            &mut chat,
            json!("<bash-input>printf '&lt;b&gt;&amp;amp;&lt;/b&gt;\\n'; true</bash-input>"),
            Origin::Live,
        );
        said(
            &mut chat,
            json!("<bash-stderr>Not run: another '!' command is still running.</bash-stderr>"),
            Origin::Live,
        );
        assert!(chat.shell_running(), "the first still runs");
        chat.end_turn();
        assert!(chat.shell_running(), "a turn's end doesn't end it");
        said(
            &mut chat,
            json!(
                "<bash-stdout>first</bash-stdout><bash-stderr></bash-stderr><bash-exit-code>0</bash-exit-code>"
            ),
            Origin::Live,
        );
        assert!(!chat.shell_running());
        assert_eq!(
            shells(&chat),
            [
                ("sleep 4; echo first", "first", ShellState::Done),
                (
                    "printf '<b>&amp;</b>\\n'; true",
                    "Not run: another '!' command is still running.",
                    ShellState::Failed
                ),
            ]
        );
    }

    /// A command that failed shows its output, its errors and its exit code; one an interrupt
    /// stopped is stopped; one whose process ended runs no more.
    #[test]
    fn shell_commands_end_as_they_ended() {
        let mut chat = Chat::default();
        for (command, output) in [
            (
                "false",
                "<bash-stdout>out\nno newline</bash-stdout><bash-stderr>err &amp; more</bash-stderr><bash-exit-code>3</bash-exit-code>",
            ),
            (
                "sleep 20",
                "<bash-stdout></bash-stdout><bash-stderr>[interrupted]</bash-stderr><bash-exit-code>130</bash-exit-code>",
            ),
        ] {
            said(
                &mut chat,
                json!(format!("<bash-input>{command}</bash-input>")),
                Origin::Live,
            );
            said(&mut chat, json!(output), Origin::Live);
        }
        said(
            &mut chat,
            json!("<bash-input>swayimg</bash-input>"),
            Origin::Live,
        );
        chat.stop_shells();
        assert_eq!(
            shells(&chat),
            [
                (
                    "false",
                    "out\nno newline\nerr & more\nExit code 3",
                    ShellState::Failed
                ),
                ("sleep 20", "[interrupted]", ShellState::Stopped),
                ("swayimg", "", ShellState::Stopped),
            ]
        );
    }

    /// Another session's message shows as its own kind of entry, by the name the sender gave:
    /// live, its body without the envelope, or the whole text when it came without one; from
    /// a saved transcript, where Claude Code keeps it as a meta line, all the same.
    #[test]
    fn messages_from_other_sessions_show_who_sent_them() {
        let message = |content: &str| -> UserMessage {
            serde_json::from_value(json!({ "content": content })).expect("a user message")
        };
        let peers = |chat: &Chat| -> Vec<(Option<String>, String)> {
            chat.entries
                .iter()
                .map(|entry| {
                    let Body::Peer { from, text } = &entry.body else {
                        panic!("another session's message, not {:?}", entry.body);
                    };
                    (from.clone(), text.clone())
                })
                .collect()
        };
        let mut chat = Chat::default();
        chat.peer_message(
            &message("<cross-session-message from-name=\"b\">\nhi\n</cross-session-message>"),
            Some("b"),
            Some("hi"),
        );
        chat.peer_message(&message("[A message from c]\n\nhello"), None, None);
        assert_eq!(
            peers(&chat),
            [
                (Some("b".to_owned()), "hi".to_owned()),
                (None, "[A message from c]\n\nhello".to_owned()),
            ]
        );

        let lines: Vec<TranscriptLine> = [json!({
            "type": "user",
            "isMeta": true,
            "message": { "role": "user", "content": "Another Claude session sent a message: …" },
            "origin": { "kind": "peer", "from": "unknown", "name": "b", "body": "hi" },
        })]
        .into_iter()
        .map(|line| serde_json::from_value(line).expect("a transcript line"))
        .collect();
        let saved = Chat::from_transcript(&lines);
        assert_eq!(peers(&saved), [(Some("b".to_owned()), "hi".to_owned())]);
    }

    /// Saved `!` rows: the terminal's, with no exit code, are commands that ended; edualc's,
    /// sent ahead of a prompt in one message, come before it; one never told of ending, as a
    /// session was saved, runs no more.
    #[test]
    fn saved_shell_commands_show_as_they_ran() {
        let lines: Vec<TranscriptLine> = [
            json!({ "type": "user", "message": { "content": "<bash-input>rg \"let _\"</bash-input>" } }),
            json!({ "type": "user", "message": {
                "content": "<bash-stdout>stream.write_all(&amp;response);</bash-stdout><bash-stderr></bash-stderr>" } }),
            json!({ "type": "user", "message": { "content": [
                { "type": "text", "text": "<bash-input>ls</bash-input>" },
                { "type": "text", "text": "<bash-stdout>a</bash-stdout><bash-stderr></bash-stderr><bash-exit-code>0</bash-exit-code>" },
                { "type": "text", "text": "Why a?" },
            ] } }),
            json!({ "type": "user", "message": { "content": "<bash-input>swayimg</bash-input>" } }),
        ]
        .into_iter()
        .map(|line| serde_json::from_value(line).expect("a transcript line"))
        .collect();
        let chat = Chat::from_transcript(&lines);
        assert_eq!(
            shells(&chat),
            [
                (
                    "rg \"let _\"",
                    "stream.write_all(&response);",
                    ShellState::Done
                ),
                ("ls", "a", ShellState::Done),
                ("swayimg", "", ShellState::Stopped),
            ]
        );
        assert_eq!(
            texts(&chat),
            ["stream.write_all(&response);", "a", "Why a?", ""]
        );
    }

    fn image_block(data: &str) -> Value {
        json!({ "type": "image", "source": { "type": "base64", "media_type": "image/png", "data": data } })
    }

    /// Images the user sent are rows of their own, before the text they came with; those a
    /// tool returned go into its card, which opens. Each is to be decoded once taken, known by
    /// its content; one without its bytes says so.
    #[test]
    fn images_go_where_they_were_sent_and_are_decoded_by_content() {
        let lines: Vec<TranscriptLine> = [
            json!({ "type": "user", "message": { "content": [
                image_block("QUJD"),
                { "type": "text", "text": "What is this?" },
            ] } }),
            json!({ "type": "assistant", "message": { "id": "m1", "content": [
                { "type": "tool_use", "id": "t1", "name": "Read", "input": { "file_path": "/a.png" } },
            ] } }),
            json!({ "type": "user", "message": { "content": [
                { "type": "tool_result", "tool_use_id": "t1",
                  "content": [image_block("REVG"), { "type": "text", "text": "a.png" }] },
            ] } }),
            json!({ "type": "user", "message": { "content": [
                image_block("QUJD"),
                { "type": "image", "source": { "type": "url", "url": "https://example.com/a.png" } },
            ] } }),
        ]
        .into_iter()
        .map(|line| serde_json::from_value(line).expect("a transcript line"))
        .collect();
        let mut chat = Chat::from_transcript(&lines);
        let sent = ImageKey::of(b"QUJD");
        let read = ImageKey::of(b"REVG");
        let bodies: Vec<String> = chat
            .entries
            .iter()
            .map(|entry| match &entry.body {
                Body::Image(key) if *key == sent => "sent image".to_owned(),
                Body::Tool { tool, expanded } => {
                    format!(
                        "tool {:?} {expanded} {}",
                        tool.images == [read],
                        tool.output
                    )
                }
                Body::User(_)
                | Body::Assistant(_)
                | Body::Thinking { .. }
                | Body::Shell { .. }
                | Body::Image(_)
                | Body::Peer { .. }
                | Body::Info(_)
                | Body::Error(_) => format!("{:?}", entry.copy_text()),
            })
            .collect();
        assert_eq!(
            bodies,
            [
                "sent image",
                "\"What is this?\"",
                "tool true true a.png",
                "sent image",
                "\"[image]\"",
            ]
        );
        let taken: Vec<ImageKey> = chat.take_images().into_iter().map(|(key, _)| key).collect();
        assert_eq!(taken, [sent, read, sent]);
        assert!(chat.take_images().is_empty(), "taken once");
        assert_eq!(chat.image_keys(), HashSet::from([sent, read]));
        chat.take_changes();
        chat.image_shown(sent);
        assert_eq!(
            chat.take_changes(),
            [Change::Updated(0), Change::Updated(3)],
            "the rows showing it"
        );
        chat.image_shown(read);
        assert_eq!(chat.take_changes(), [Change::Updated(2)]);
    }

    /// Escapes undone once: what a command printed as `&lt;` stays `&lt;`.
    #[test]
    fn rows_are_unescaped_once() {
        assert_eq!(
            unescaped("&amp;lt; &lt;b&gt; & &x; &amp"),
            "&lt; <b> & &x; &amp"
        );
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
