//! The inputs of the tools the window knows, with the field names Claude Code gives them. An
//! agent's tool call is translated into one of these (see `crates/host/src/acp`), so the window
//! reads the same shape whichever agent made the call.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::external::wire::ToolName;

/// `Bash`'s input.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BashInput {
    pub command: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

/// `Read`'s input.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReadInput {
    pub file_path: PathBuf,
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
}

/// edualc's messaging tool's input.
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
        let call = KnownCall::Bash(BashInput {
            command: "ls -la".to_owned(),
            description: None,
        });
        assert_eq!(call.name(), ToolName::Bash);
        assert_eq!(
            call.input().expect("an input"),
            json!({"command": "ls -la"})
        );
    }

    #[test]
    fn a_command_with_a_description_keeps_it() {
        let call = KnownCall::Bash(BashInput {
            command: "wc -l notes.txt".to_owned(),
            description: Some("Count lines".to_owned()),
        });
        assert_eq!(
            call.input().expect("an input"),
            json!({"command": "wc -l notes.txt", "description": "Count lines"})
        );
    }

    #[test]
    fn file_tools_use_claude_codes_field_names() {
        let read = KnownCall::Read(ReadInput {
            file_path: PathBuf::from("/w/f.txt"),
        });
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
        let edit = KnownCall::Edit(EditInput {
            file_path: PathBuf::from("/w/f.txt"),
            old_string: "two".to_owned(),
            new_string: "TWO".to_owned(),
        });
        assert_eq!(edit.name(), ToolName::Edit);
        assert_eq!(
            edit.input().expect("an input"),
            json!({"file_path": "/w/f.txt", "old_string": "two", "new_string": "TWO"})
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
