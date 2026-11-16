//! agy's own records of a conversation (see [`crate::external::agy`]), read for the window as
//! Claude Code's: the subagents and swarm members a session started as its background tasks, a
//! member's steps as its transcript, and a swarm's board as what its members said. Nothing is
//! copied: the daemon translates them as a window reads them.

use std::path::Path;

use serde::de::DeserializeOwned;

use super::Agent;
use super::translate::as_claude_tool;
use crate::external::acp::ToolCall;
use crate::external::agy::{
    self, AgyTask, BoardPost, MemberRole, Step, StepKind, Subagent, SubagentState, Swarm,
};
use crate::external::wire::{
    ApiMessage, BackgroundTask, ContentBlock, MessageId, SessionId, TaskKind, ToolUseId,
    TranscriptLine, Usage, UserContent, UserMessage,
};

/// The subagents and swarm members conversation `own` started that run, and its swarm's board.
pub(super) fn tasks(own: &SessionId) -> Vec<BackgroundTask> {
    let Some(brain) = agy::brain() else {
        return Vec::new();
    };
    let generated = agy::generated(&brain, own);
    let swarm: Option<Swarm> = read(&generated.join("swarm/members.json"));
    let subagents = std::fs::read_dir(generated.join("subagents"))
        .into_iter()
        .flatten()
        .filter_map(|entry| read::<Subagent>(&entry.ok()?.path()))
        .collect();
    background_tasks(own, swarm, subagents)
}

/// The tasks a session's records tell of: its swarm's board and running workers, then the
/// subagents outside the swarm that are alive, each by its conversation.
fn background_tasks(
    own: &SessionId,
    swarm: Option<Swarm>,
    subagents: Vec<Subagent>,
) -> Vec<BackgroundTask> {
    let task = |id: AgyTask, kind: String, description: String| BackgroundTask {
        task_id: id.id(),
        task_type: TaskKind::LocalAgent,
        subagent_type: Some(kind),
        description,
        ambient: false,
    };
    let mut tasks = Vec::new();
    let mut members = Vec::new();
    if let Some(swarm) = swarm {
        tasks.push(task(
            AgyTask::Board(own.clone()),
            "Board".to_owned(),
            swarm.title,
        ));
        for (name, member) in swarm.members {
            members.push(member.conversation_id.clone());
            match member.role {
                MemberRole::Worker | MemberRole::Other if !member.killed => tasks.push(task(
                    AgyTask::Conversation(member.conversation_id),
                    name,
                    member.model.unwrap_or_default(),
                )),
                MemberRole::Worker | MemberRole::Other | MemberRole::Coordinator => {}
            }
        }
    }
    for subagent in subagents {
        match subagent.state {
            SubagentState::Alive if !members.contains(&subagent.conversation_id) => {
                tasks.push(task(
                    AgyTask::Conversation(subagent.conversation_id),
                    subagent.subagent_descriptor.type_name,
                    subagent.subagent_descriptor.role,
                ));
            }
            SubagentState::Alive | SubagentState::Other => {}
        }
    }
    tasks
}

/// A JSON file agy wrote, if it is there; one it can't read is logged.
fn read<Record: DeserializeOwned>(path: &Path) -> Option<Record> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return None,
        Err(error) => {
            tracing::warn!(%error, path = %path.display(), "an agy record edualc can't read");
            return None;
        }
    };
    serde_json::from_str(&text)
        .inspect_err(|error| {
            tracing::warn!(%error, path = %path.display(), "an agy record edualc can't read");
        })
        .ok()
}

/// A line of a conversation's steps as a transcript line: a message to the model as a prompt,
/// the model's answer with its tool calls by Claude Code's names. A tool's result and agy's
/// checkpoints give none, as a subagent's peek shows none.
pub(crate) fn step_line(line: &str) -> Option<TranscriptLine> {
    let step: Step = serde_json::from_str(line)
        .inspect_err(|error| tracing::debug!(%error, "an agy step edualc can't read"))
        .ok()?;
    match step.kind {
        StepKind::SystemMessage | StepKind::UserInput => {
            let text = step.content?;
            Some(TranscriptLine::User {
                message: UserMessage {
                    content: UserContent::Text(unwrapped(&text).to_owned()),
                },
                is_meta: false,
                is_sidechain: false,
                origin: None,
            })
        }
        StepKind::PlannerResponse => {
            let mut content = Vec::new();
            content.extend(
                step.thinking
                    .map(|thinking| ContentBlock::Thinking { thinking }),
            );
            content.extend(step.content.map(|text| ContentBlock::Text { text }));
            content.extend(step.tool_calls.into_iter().zip(0_u32..).map(|(call, n)| {
                let id = format!("agy-{}-{n}", step.step_index);
                let (name, input) = as_claude_tool(
                    Agent::Agy,
                    &ToolCall {
                        tool_call_id: id.clone(),
                        name: Some(call.name),
                        raw_input: Some(call.args),
                        ..ToolCall::default()
                    },
                );
                ContentBlock::ToolUse {
                    id: ToolUseId::from(id),
                    name,
                    input,
                }
            }));
            Some(TranscriptLine::Assistant {
                message: ApiMessage {
                    id: MessageId::from(format!("agy-step-{}", step.step_index)),
                    content,
                    usage: Usage::default(),
                },
                is_sidechain: false,
            })
        }
        StepKind::Generic | StepKind::Checkpoint | StepKind::Other => None,
    }
}

/// A message agy puts to the model, without the wrapper agy puts around it (whose preamble
/// names the tag too), and without the fields before another conversation's message
/// (`[Message] timestamp=… sender=… priority=… content=`).
fn unwrapped(text: &str) -> &str {
    let inner = text
        .rsplit_once("<SYSTEM_MESSAGE>")
        .and_then(|(_, inner)| inner.split_once("</SYSTEM_MESSAGE>"))
        .map_or(text, |(inner, _)| inner.trim());
    inner
        .strip_prefix("[Message] ")
        .and_then(|fields| fields.split_once(" content="))
        .map_or(inner, |(_, message)| message)
}

/// A post on a swarm's board, as what its member said.
pub(crate) fn post_line(line: &str) -> Option<TranscriptLine> {
    let post: BoardPost = serde_json::from_str(line)
        .inspect_err(|error| tracing::debug!(%error, "a board post edualc can't read"))
        .ok()?;
    Some(TranscriptLine::Assistant {
        message: ApiMessage {
            id: MessageId::from(format!("agy-post-{}", post.seq)),
            content: vec![ContentBlock::Text {
                text: format!("{} (#{}): {}", post.agent, post.seq, post.text),
            }],
            usage: Usage::default(),
        },
        is_sidechain: false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::external::agy::Member;
    use crate::external::agy::SubagentDescriptor;
    use crate::external::wire::ToolName;

    fn id(text: &str) -> SessionId {
        SessionId::try_from(text.to_owned()).expect("an id")
    }

    const OWN: &str = "6361b5fc-2b57-415e-b512-bdd977278077";
    const WORKER: &str = "1e16b130-ab24-45b6-8ebf-b573ab82eb7f";
    const ENDED: &str = "a539fe4a-54a1-469c-bc13-066bcacfba57";
    const HELPER: &str = "55282ae3-ca5a-4b50-a583-7ca7ffa6c63b";

    /// A swarm shows its board and its running workers, not its coordinator or a killed
    /// worker; a subagent outside the swarm shows while it is alive, a member only once.
    #[test]
    fn a_swarm_and_its_subagents_are_tasks() {
        let member = |conversation: &str, role, killed| Member {
            conversation_id: id(conversation),
            role,
            model: Some("gemini-4".to_owned()),
            killed,
        };
        let swarm = Swarm {
            title: "Two tracks".to_owned(),
            members: [
                (
                    "coordinator".to_owned(),
                    member(OWN, MemberRole::Coordinator, false),
                ),
                (
                    "acp-worker".to_owned(),
                    member(WORKER, MemberRole::Worker, false),
                ),
                (
                    "verifier".to_owned(),
                    member(ENDED, MemberRole::Worker, true),
                ),
            ]
            .into(),
        };
        let subagent = |conversation: &str| Subagent {
            conversation_id: id(conversation),
            subagent_descriptor: SubagentDescriptor {
                type_name: "worker".to_owned(),
                role: "Helper".to_owned(),
            },
            state: SubagentState::Alive,
        };
        let tasks = background_tasks(
            &id(OWN),
            Some(swarm),
            vec![subagent(WORKER), subagent(HELPER)],
        );
        let shown: Vec<(Option<AgyTask>, Option<&str>)> = tasks
            .iter()
            .map(|task| (AgyTask::of(&task.task_id), task.subagent_type.as_deref()))
            .collect();
        assert_eq!(
            shown,
            [
                (Some(AgyTask::Board(id(OWN))), Some("Board")),
                (Some(AgyTask::Conversation(id(WORKER))), Some("acp-worker")),
                (Some(AgyTask::Conversation(id(HELPER))), Some("worker")),
            ]
        );
        assert!(
            tasks
                .iter()
                .all(|task| task.task_type == TaskKind::LocalAgent)
        );
    }

    /// A member's steps read as its prompt and its answers, the tools by Claude Code's names;
    /// a tool's result gives no line.
    #[test]
    fn steps_read_as_a_transcript() {
        let told = step_line(
            r#"{"step_index":1,"source":"SYSTEM","type":"SYSTEM_MESSAGE","status":"DONE","content":"The following is a <SYSTEM_MESSAGE> not actually sent by the user.\n\n<SYSTEM_MESSAGE>\n[Message] timestamp=2026-11-15T21:28:47Z sender=6361b5fc-2b57-415e-b512-bdd977278077 priority=MESSAGE_PRIORITY_HIGH content=Start with the brief.\n</SYSTEM_MESSAGE>"}"#,
        );
        let Some(TranscriptLine::User { message, .. }) = told else {
            panic!("a prompt, not {told:?}");
        };
        assert!(
            matches!(&message.content, UserContent::Text(text) if text == "Start with the brief.")
        );
        let answered = step_line(
            r#"{"step_index":2,"source":"MODEL","type":"PLANNER_RESPONSE","status":"DONE","thinking":"First the files.","tool_calls":[{"name":"read_file","args":{"Path":"/w/notes.txt"}}]}"#,
        );
        let Some(TranscriptLine::Assistant { message, .. }) = answered else {
            panic!("an answer, not {answered:?}");
        };
        let called = message.content.iter().find_map(|block| match block {
            ContentBlock::ToolUse { name, input, .. } => Some((name.clone(), input.clone())),
            ContentBlock::Text { .. }
            | ContentBlock::Thinking { .. }
            | ContentBlock::ServerToolUse { .. }
            | ContentBlock::ToolResult { .. }
            | ContentBlock::Image { .. }
            | ContentBlock::Other => None,
        });
        assert_eq!(
            called,
            Some((
                ToolName::Read,
                serde_json::json!({ "file_path": "/w/notes.txt" })
            ))
        );
        assert!(
            step_line(
                r#"{"step_index":3,"source":"MODEL","type":"GENERIC","status":"DONE","content":"notes"}"#
            )
            .is_none()
        );
    }

    /// A board's post reads as what its member said, with its number.
    #[test]
    fn a_post_reads_as_its_members() {
        let said = post_line(
            r#"{"seq":4,"agent":"acp-worker","text":"Ready for review.","time":"2026-11-15 23:00:00","kind":"post"}"#,
        );
        let Some(TranscriptLine::Assistant { message, .. }) = said else {
            panic!("a post, not {said:?}");
        };
        assert!(matches!(
            message.content.first(),
            Some(ContentBlock::Text { text }) if text == "acp-worker (#4): Ready for review."
        ));
    }
}
