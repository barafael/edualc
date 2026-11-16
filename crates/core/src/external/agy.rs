//! What agy (Google's Antigravity CLI, 1.5.9) keeps of a conversation in its brain folder,
//! `~/.gemini/antigravity-cli/brain/<conversation>/.system_generated/`, in the fields edualc reads:
//! the conversation's steps (`logs/transcript.jsonl`, one a line, appended), the subagents it
//! started (`subagents/<conversation>.json`), and a swarm's members (`swarm/members.json`) and
//! board (`swarm/board.jsonl`, a post a line). A subagent, or a swarm's member, is a
//! conversation of its own, with a brain folder of its own.

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::Deserialize;
use serde_json::Value;

use crate::external::wire::{SessionId, TaskId};

/// Where agy keeps its conversations.
pub fn brain() -> Option<PathBuf> {
    dirs::home_dir().map(|home| home.join(".gemini/antigravity-cli/brain"))
}

/// Conversation `id`'s folder of what agy wrote for it.
pub fn generated(brain: &std::path::Path, id: &SessionId) -> PathBuf {
    brain.join(id.as_str()).join(".system_generated")
}

/// One step of a conversation: the model's answer (its thinking, text and tool calls), a tool
/// call's result, or a message to the model.
#[derive(Debug, Deserialize)]
pub struct Step {
    pub step_index: u64,
    #[serde(rename = "type")]
    pub kind: StepKind,
    pub status: StepStatus,
    #[serde(default)]
    pub content: Option<String>,
    #[serde(default)]
    pub thinking: Option<String>,
    #[serde(default)]
    pub tool_calls: Vec<StepCall>,
    #[serde(default)]
    pub error: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum StepKind {
    /// The model's answer; each of its tool calls has a result step after it, in turn.
    PlannerResponse,
    /// A tool call's result.
    Generic,
    /// A message to the model from agy: an assignment, a board's news.
    SystemMessage,
    UserInput,
    Checkpoint,
    #[serde(other)]
    Other,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum StepStatus {
    Done,
    /// Still under way, or a command that went on in the background.
    Running,
    Error,
    #[serde(other)]
    Other,
}

/// A tool call, by agy's name for the tool and its arguments.
#[derive(Debug, Deserialize)]
pub struct StepCall {
    pub name: String,
    #[serde(default)]
    pub args: Value,
}

/// A subagent a conversation started.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Subagent {
    pub conversation_id: SessionId,
    pub subagent_descriptor: SubagentDescriptor,
    pub state: SubagentState,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SubagentDescriptor {
    /// Its kind ("worker").
    pub type_name: String,
    /// What it does, in words ("Swarm Worker (verifier)").
    #[serde(default)]
    pub role: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
pub enum SubagentState {
    #[serde(rename = "SUBAGENT_STATE_ALIVE")]
    Alive,
    #[serde(other)]
    Other,
}

/// A swarm's members, by name.
#[derive(Debug, Deserialize)]
pub struct Swarm {
    #[serde(default)]
    pub title: String,
    pub members: BTreeMap<String, Member>,
}

#[derive(Debug, Deserialize)]
pub struct Member {
    pub conversation_id: SessionId,
    pub role: MemberRole,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub killed: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MemberRole {
    /// The conversation that started the swarm.
    Coordinator,
    Worker,
    #[serde(other)]
    Other,
}

/// A post on a swarm's board.
#[derive(Debug, Deserialize)]
pub struct BoardPost {
    pub seq: u64,
    /// The member's name.
    pub agent: String,
    pub text: String,
}

/// A background task of an agy session, as edualc names it to the window: one of its swarm's
/// members (or another subagent), or the swarm's board. The conversation's id is in the name,
/// so the daemon finds what to read from the name alone.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgyTask {
    Conversation(SessionId),
    Board(SessionId),
}

const CONVERSATION: &str = "agy-";
const BOARD: &str = "agy-board-";

impl AgyTask {
    /// The task this is, if it is one of an agy session's.
    pub fn of(task: &TaskId) -> Option<Self> {
        let name = task.to_string();
        if let Some(id) = name.strip_prefix(BOARD) {
            return SessionId::try_from(id.to_owned()).ok().map(Self::Board);
        }
        name.strip_prefix(CONVERSATION)
            .and_then(|id| SessionId::try_from(id.to_owned()).ok())
            .map(Self::Conversation)
    }

    pub fn id(&self) -> TaskId {
        match self {
            Self::Conversation(id) => TaskId::from(format!("{CONVERSATION}{}", id.as_str())),
            Self::Board(id) => TaskId::from(format!("{BOARD}{}", id.as_str())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const WORKER: &str = "1e16b130-ab24-45b6-8ebf-b573ab82eb7f";

    /// Steps as agy 1.5.9 writes them: an answer with a tool call, then its result.
    #[test]
    fn steps_read() {
        let answer: Step = serde_json::from_str(
            r#"{"step_index":2,"source":"MODEL","type":"PLANNER_RESPONSE","status":"DONE","created_at":"2026-11-15T21:28:47Z","model":"m","input_tokens":1,"cache_read_tokens":0,"output_tokens":1,"thinking":"First, the brief.","tool_calls":[{"name":"run_command","args":{"CommandLine":"ls","toolSummary":"List"}}]}"#,
        )
        .expect("an answer");
        assert_eq!(answer.kind, StepKind::PlannerResponse);
        assert_eq!(answer.thinking.as_deref(), Some("First, the brief."));
        assert_eq!(
            answer.tool_calls.first().map(|call| call.name.as_str()),
            Some("run_command")
        );
        let failed: Step = serde_json::from_str(
            r#"{"step_index":4,"source":"MODEL","type":"GENERIC","status":"ERROR","created_at":"2026-11-15T21:29:00Z","error":"permission check failed"}"#,
        )
        .expect("a result");
        assert_eq!(
            (failed.kind, failed.status),
            (StepKind::Generic, StepStatus::Error)
        );
    }

    /// A swarm's files as agy 1.5.9 writes them.
    #[test]
    fn a_swarm_reads() {
        let swarm: Swarm = serde_json::from_str(&format!(
            r#"{{"title":"Two tracks","members":{{"acp-worker":{{"conversation_id":"{WORKER}","role":"worker","desired_state":"RUNNING","phase":"RUNNING","wake_mode":"mentions","killed":false,"model":"gemini-4-hydrogen-xhigh"}}}}}}"#
        ))
        .expect("members");
        let worker = swarm.members.get("acp-worker").expect("the worker");
        assert_eq!(worker.role, MemberRole::Worker);
        assert_eq!(worker.conversation_id.as_str(), WORKER);
        let subagent: Subagent = serde_json::from_str(&format!(
            r#"{{"conversationId":"{WORKER}","subagentDescriptor":{{"typeName":"worker","role":"Swarm Worker (acp-worker)"}},"state":"SUBAGENT_STATE_ALIVE","spawnStepIndex":366,"workspaceUris":[]}}"#
        ))
        .expect("a subagent");
        assert_eq!(subagent.state, SubagentState::Alive);
        let post: BoardPost = serde_json::from_str(
            r#"{"seq":2,"agent":"coordinator","text":"@acp-worker Start.","time":"2026-11-15 22:28:45","kind":"brief"}"#,
        )
        .expect("a post");
        assert_eq!((post.seq, post.agent.as_str()), (2, "coordinator"));
    }

    /// A task's name gives back the conversation it is; another session's tasks, and a name
    /// that would lead out of the brain folder, aren't agy's.
    #[test]
    fn task_names_round_trip() {
        let id = SessionId::try_from(WORKER.to_owned()).expect("an id");
        for task in [AgyTask::Conversation(id.clone()), AgyTask::Board(id)] {
            assert_eq!(AgyTask::of(&task.id()), Some(task));
        }
        assert_eq!(AgyTask::of(&TaskId::from("a1b2c3d4".to_owned())), None);
        assert_eq!(AgyTask::of(&TaskId::from("agy-../../etc".to_owned())), None);
    }
}
