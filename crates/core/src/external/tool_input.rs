//! The inputs of the tools the window knows, with the field names Claude Code gives them, as
//! far as the window reads them. An agent's tool call is translated into one of these (see
//! `crates/host/src/acp`), so the window reads the same shape whichever agent made the call.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::external::wire::{Questions, ToolName};

/// `Bash`'s input, and `Monitor`'s, which runs a command too.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BashInput {
    pub command: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub run_in_background: Option<bool>,
    /// Asked for to run outside Claude Code's own sandbox.
    #[serde(
        rename = "dangerouslyDisableSandbox",
        skip_serializing_if = "Option::is_none"
    )]
    pub outside_sandbox: Option<bool>,
}

impl BashInput {
    /// A command alone, as the adapter or the user's `!` gives one.
    pub fn command(command: String, description: Option<String>) -> Self {
        Self {
            command,
            description,
            run_in_background: None,
            outside_sandbox: None,
        }
    }
}

/// `Read`'s input.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReadInput {
    pub file_path: PathBuf,
    /// The first line read, counted from 1.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub offset: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub limit: Option<u64>,
}

impl ReadInput {
    /// The whole file.
    pub fn whole(file_path: PathBuf) -> Self {
        Self {
            file_path,
            offset: None,
            limit: None,
        }
    }
}

/// `Write`'s input.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WriteInput {
    pub file_path: PathBuf,
    pub content: String,
}

/// `Edit`'s input.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EditInput {
    pub file_path: PathBuf,
    pub old_string: String,
    pub new_string: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub replace_all: Option<bool>,
}

impl EditInput {
    /// One occurrence replaced.
    pub fn once(file_path: PathBuf, old_string: String, new_string: String) -> Self {
        Self {
            file_path,
            old_string,
            new_string,
            replace_all: None,
        }
    }
}

/// `MultiEdit`'s input.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MultiEditInput {
    pub file_path: PathBuf,
    pub edits: Vec<OneEdit>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OneEdit {
    pub old_string: String,
    pub new_string: String,
}

/// `NotebookEdit`'s input; a deleted cell has no new source.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NotebookEditInput {
    pub notebook_path: PathBuf,
    #[serde(default)]
    pub new_source: String,
}

/// `Glob`'s input.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GlobInput {
    pub pattern: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<PathBuf>,
}

/// `Grep`'s input.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GrepInput {
    pub pattern: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<PathBuf>,
    /// The files searched, as a glob.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub glob: Option<String>,
}

/// `WebFetch`'s input.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WebFetchInput {
    pub url: String,
    /// What to take from the page.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prompt: Option<String>,
}

/// `WebSearch`'s input.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WebSearchInput {
    pub query: String,
}

/// `Agent`'s input (`Task`'s, before Claude Code renamed it).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentInput {
    pub description: String,
    pub prompt: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub subagent_type: Option<String>,
}

/// `Skill`'s input: the skill by name, under the key the release names it by.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkillInput {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub skill: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

/// `TodoWrite`'s input.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TodoWriteInput {
    pub todos: Vec<Todo>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Todo {
    pub content: String,
    pub status: TodoStatus,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TodoStatus {
    Pending,
    InProgress,
    Completed,
    #[serde(other)]
    Other,
}

/// edualc's messaging tool's input, and Claude Code's `SendMessage`'s.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SendMessageInput {
    pub to: String,
    pub message: String,
}

/// edualc's tool listing the sessions takes nothing: an empty object.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ListSessionsInput {}

/// A tool call the window knows, by the tool it is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KnownCall {
    Bash(BashInput),
    Read(ReadInput),
    Write(WriteInput),
    Edit(EditInput),
    Grep(GrepInput),
    WebFetch(WebFetchInput),
    Agent(AgentInput),
    Questions(Questions),
    SendMessage(SendMessageInput),
    ListSessions(ListSessionsInput),
}

impl KnownCall {
    /// The tool's name, as the window shows it.
    pub fn name(&self) -> ToolName {
        match self {
            Self::Bash(_) => ToolName::Bash,
            Self::Read(_) => ToolName::Read,
            Self::Write(_) => ToolName::Write,
            Self::Edit(_) => ToolName::Edit,
            Self::Grep(_) => ToolName::Grep,
            Self::WebFetch(_) => ToolName::WebFetch,
            Self::Agent(_) => ToolName::Agent,
            Self::Questions(_) => ToolName::AskUserQuestion,
            Self::SendMessage(_) => ToolName::EdualcSendMessage,
            Self::ListSessions(_) => ToolName::EdualcListSessions,
        }
    }

    /// The call's input, as Claude Code's field names give it. Fails only for a path that
    /// isn't UTF-8, which JSON can't hold.
    pub fn input(&self) -> Result<Value, serde_json::Error> {
        match self {
            Self::Bash(input) => serde_json::to_value(input),
            Self::Read(input) => serde_json::to_value(input),
            Self::Write(input) => serde_json::to_value(input),
            Self::Edit(input) => serde_json::to_value(input),
            Self::Grep(input) => serde_json::to_value(input),
            Self::WebFetch(input) => serde_json::to_value(input),
            Self::Agent(input) => serde_json::to_value(input),
            Self::Questions(input) => serde_json::to_value(input),
            Self::SendMessage(input) => serde_json::to_value(input),
            Self::ListSessions(input) => serde_json::to_value(input),
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn a_command_without_a_description_leaves_it_out() {
        let call = KnownCall::Bash(BashInput::command("ls -la".to_owned(), None));
        assert_eq!(call.name(), ToolName::Bash);
        assert_eq!(
            call.input().expect("an input"),
            json!({"command": "ls -la"})
        );
    }

    #[test]
    fn a_command_with_a_description_keeps_it() {
        let call = KnownCall::Bash(BashInput::command(
            "wc -l notes.txt".to_owned(),
            Some("Count lines".to_owned()),
        ));
        assert_eq!(
            call.input().expect("an input"),
            json!({"command": "wc -l notes.txt", "description": "Count lines"})
        );
    }

    #[test]
    fn file_tools_use_claude_codes_field_names() {
        let read = KnownCall::Read(ReadInput::whole(PathBuf::from("/w/f.txt")));
        assert_eq!(
            read.input().expect("an input"),
            json!({"file_path": "/w/f.txt"})
        );
        let write = KnownCall::Write(WriteInput {
            file_path: PathBuf::from("/w/f.txt"),
            content: "hi\n".to_owned(),
        });
        assert_eq!(
            write.input().expect("an input"),
            json!({"file_path": "/w/f.txt", "content": "hi\n"})
        );
        let edit = KnownCall::Edit(EditInput::once(
            PathBuf::from("/w/f.txt"),
            "two".to_owned(),
            "TWO".to_owned(),
        ));
        assert_eq!(edit.name(), ToolName::Edit);
        assert_eq!(
            edit.input().expect("an input"),
            json!({"file_path": "/w/f.txt", "old_string": "two", "new_string": "TWO"})
        );
    }

    #[test]
    fn search_and_fetch_use_claude_codes_field_names() {
        let grep = KnownCall::Grep(GrepInput {
            pattern: "needle".to_owned(),
            path: Some(PathBuf::from("/w/src")),
            glob: None,
        });
        assert_eq!(grep.name(), ToolName::Grep);
        assert_eq!(
            grep.input().expect("an input"),
            json!({"pattern": "needle", "path": "/w/src"})
        );
        let anywhere = KnownCall::Grep(GrepInput {
            pattern: "needle".to_owned(),
            path: None,
            glob: None,
        });
        assert_eq!(
            anywhere.input().expect("an input"),
            json!({"pattern": "needle"})
        );
        let fetch = KnownCall::WebFetch(WebFetchInput {
            url: "https://example.com".to_owned(),
            prompt: None,
        });
        assert_eq!(fetch.name(), ToolName::WebFetch);
        assert_eq!(
            fetch.input().expect("an input"),
            json!({"url": "https://example.com"})
        );
    }

    #[test]
    fn edualcs_tools_have_claude_codes_names() {
        let send = KnownCall::SendMessage(SendMessageInput {
            to: "calm-heron".to_owned(),
            message: "hello".to_owned(),
        });
        assert_eq!(send.name().to_string(), "mcp__edualc__send_message");
        assert_eq!(
            send.input().expect("an input"),
            json!({"to": "calm-heron", "message": "hello"})
        );
        let list = KnownCall::ListSessions(ListSessionsInput {});
        assert_eq!(list.name().to_string(), "mcp__edualc__list_sessions");
        assert_eq!(list.input().expect("an input"), json!({}));
    }
}
