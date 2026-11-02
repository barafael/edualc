//! Claude Code's sessions on disk: saved ones as `~/.claude/projects/<project>/<id>.jsonl`,
//! one directory per working directory, named after its path with every character other than
//! letters and digits made a `-`; running ones as `~/.claude/sessions/<pid>.json`.

use std::collections::HashMap;
use std::fs::File;
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use serde::Deserialize;

use crate::chat::UserText;
use crate::external::claude_files::{AgentRecord, LiveRecord};
use crate::external::wire::{self, SessionId, TranscriptCwd, TranscriptLine, UserContent};
use crate::home::Home;
use crate::process;

/// A session's two words, an adjective and a noun picked by its id, so the same each time it
/// runs: edualc names a session's process by them (see `spawn`), and Claude Code keeps that name
/// as the session's custom title too, which is a name then, not a title.
#[derive(Debug, Clone, PartialEq, Eq, derive_more::Display)]
pub(crate) struct PetName(String);

impl PetName {
    pub(crate) fn of(id: &SessionId) -> Self {
        let words = petname::Petnames::small();
        let word = |list: &[&'static str], number: usize| {
            list.get(number % list.len().max(1))
                .copied()
                .unwrap_or("session")
        };
        Self(format!(
            "{}-{}",
            word(&words.adjectives, id_number(id, 0)),
            word(&words.nouns, id_number(id, 8))
        ))
    }

    /// Whether `text` holds the two words, as each name edualc gave the session has.
    fn is_in(&self, text: &str) -> bool {
        text.contains(self.0.as_str())
    }
}

/// A number a session's id holds: eight of its hex digits, from the `at`th on.
pub(crate) fn id_number(id: &SessionId, at: usize) -> usize {
    let digits: String = id
        .as_str()
        .chars()
        .filter(char::is_ascii_hexdigit)
        .collect();
    digits
        .get(at..at + 8)
        .and_then(|hex| u32::from_str_radix(hex, 16).ok())
        .and_then(|number| usize::try_from(number).ok())
        .unwrap_or(0)
}

#[derive(Debug, Clone)]
pub(crate) struct SessionInfo {
    pub(crate) id: SessionId,
    pub(crate) title: String,
    pub(crate) modified: SystemTime,
    /// The worktree it last worked in, if that still exists.
    pub(crate) worktree: Option<Worktree>,
}

/// A git worktree a session works in (`claude --worktree`, the EnterWorktree tool).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Worktree {
    pub(crate) path: PathBuf,
    pub(crate) name: String,
    pub(crate) branch: String,
}

impl Worktree {
    fn existing(session: wire::WorktreeSession) -> Option<Self> {
        session.path.is_dir().then(|| Self {
            name: if session.name.is_empty() {
                folder_name(&session.path)
            } else {
                session.name
            },
            path: session.path,
            branch: session.branch,
        })
    }
}

/// `path` as a worktree, if it is a linked one: its `.git` is a file naming the real one.
pub(crate) fn linked_worktree(path: &Path) -> Option<Worktree> {
    let pointer = read_small(&path.join(".git"), GIT_POINTER_BYTES)?;
    let gitdir = PathBuf::from(pointer.strip_prefix("gitdir:")?.trim());
    let gitdir = if gitdir.is_absolute() {
        gitdir
    } else {
        path.join(gitdir)
    };
    let head = read_small(&gitdir.join("HEAD"), GIT_POINTER_BYTES).unwrap_or_default();
    let branch = head
        .trim()
        .strip_prefix("ref: refs/heads/")
        .unwrap_or("(detached)")
        .to_owned();
    Some(Worktree {
        path: path.to_owned(),
        name: folder_name(path),
        branch,
    })
}

/// The last part of a path, for a title; the whole path for `/`.
pub(crate) fn folder_name(path: &Path) -> String {
    path.file_name().map_or_else(
        || path.display().to_string(),
        |name| name.to_string_lossy().into_owned(),
    )
}

#[derive(Debug, Clone)]
pub(crate) struct ProjectInfo {
    pub(crate) path: PathBuf,
    pub(crate) modified: SystemTime,
}

/// A session some Claude Code process is running right now (a terminal, an editor, the
/// background), as that process reports itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LiveSession {
    pub(crate) pid: u32,
    /// When its process started, to tell it from a later one with the same pid.
    pub(crate) started: String,
    pub(crate) id: SessionId,
    pub(crate) cwd: PathBuf,
    /// The name other sessions message it by.
    pub(crate) name: String,
    /// As its transcript names it; none before it has one.
    pub(crate) title: Option<String>,
    pub(crate) kind: LiveKind,
    pub(crate) status: LiveStatus,
    pub(crate) worktree: Option<Worktree>,
    /// Its subagents running now, as their records tell (see [`running_agents`]).
    pub(crate) agents: usize,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum LiveKind {
    Interactive,
    /// Started with `--bg`.
    Bg,
    #[default]
    #[serde(other)]
    Other,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum LiveStatus {
    Idle,
    Busy,
    /// Waiting for its user: a permission request or a question.
    Waiting,
    #[default]
    #[serde(other)]
    Other,
}

pub(crate) fn config_dir() -> PathBuf {
    std::env::var_os("CLAUDE_CONFIG_DIR").map_or_else(
        || dirs::home_dir().unwrap_or_default().join(".claude"),
        PathBuf::from,
    )
}

/// `~/.claude.json` runs to hundreds of kilobytes; anything far larger isn't that file.
pub(crate) const GLOBAL_CONFIG_BYTES: u64 = 64 * 1024 * 1024;

/// Where Claude Code keeps `.claude.json`: beside its configuration folder, or in it when that
/// is moved.
pub(crate) fn global_config_path() -> PathBuf {
    std::env::var_os("CLAUDE_CONFIG_DIR").map_or_else(
        || dirs::home_dir().unwrap_or_default().join(".claude.json"),
        |dir| PathBuf::from(dir).join(".claude.json"),
    )
}

fn projects_dir() -> PathBuf {
    config_dir().join("projects")
}

/// A project's folder name as Claude Code writes it: its path, each character but a letter or
/// a digit a dash.
pub(crate) fn project_name(cwd: &Path) -> String {
    cwd.to_string_lossy()
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() {
                character
            } else {
                '-'
            }
        })
        .collect()
}

fn project_dir(cwd: &Path) -> PathBuf {
    projects_dir().join(project_name(cwd))
}

/// The folders a project's transcripts are in: the user's own home's, and the home of its
/// sandboxed sessions (see `home`).
fn project_dirs(cwd: &Path) -> Vec<PathBuf> {
    let name = project_name(cwd);
    std::iter::once(projects_dir())
        .chain(Home::of(cwd).map(|home| home.projects()))
        .map(|projects| projects.join(&name))
        .collect()
}

/// Every folder of transcripts' folders: the user's own home's and every sandboxed home's.
fn all_projects_dirs() -> Vec<PathBuf> {
    std::iter::once(projects_dir())
        .chain(Home::all().iter().map(Home::projects))
        .collect()
}

/// Where a session in `cwd` that runs outside the sandbox keeps its transcript.
pub(crate) fn transcript_path(cwd: &Path, id: &SessionId) -> PathBuf {
    project_dir(cwd).join(format!("{}.jsonl", id.as_str()))
}

/// Where a session's transcript is: under its working directory, or under another project's
/// when it moved there (a session entering a worktree is relocated to the worktree's); in the
/// user's own home or in the sandboxed sessions'. The newer one, if both have it: a session
/// from the user's home goes on in the sandboxed one when resumed there.
pub(crate) fn find_transcript(cwd: &Path, id: &SessionId) -> Option<PathBuf> {
    let name = format!("{}.jsonl", id.as_str());
    newest(project_dirs(cwd).into_iter().map(|dir| dir.join(&name))).or_else(|| {
        newest(
            all_projects_dirs()
                .iter()
                .flat_map(|projects| std::fs::read_dir(projects).into_iter().flatten())
                .filter_map(Result::ok)
                .map(|entry| entry.path().join(&name)),
        )
    })
}

/// A session's transcript in the user's own home, whatever its project.
pub(crate) fn find_user_transcript(id: &SessionId) -> Option<PathBuf> {
    let name = format!("{}.jsonl", id.as_str());
    newest(
        std::fs::read_dir(projects_dir())
            .into_iter()
            .flatten()
            .filter_map(Result::ok)
            .map(|entry| entry.path().join(&name)),
    )
}

/// The most recently changed of the files there are.
fn newest(paths: impl Iterator<Item = PathBuf>) -> Option<PathBuf> {
    paths
        .filter_map(|path| {
            let metadata = path.metadata().ok().filter(std::fs::Metadata::is_file)?;
            Some((metadata.modified().ok()?, path))
        })
        .max_by_key(|(modified, _)| *modified)
        .map(|(_, path)| path)
}

/// Where a session's transcript is, or would be if it is not written yet.
pub(crate) fn locate(cwd: &Path, id: &SessionId) -> PathBuf {
    find_transcript(cwd, id).unwrap_or_else(|| transcript_path(cwd, id))
}

/// The transcripts in a directory, newest first.
fn transcripts(dir: &Path) -> Vec<(PathBuf, SystemTime)> {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(error) => {
            tracing::debug!(%error, dir = %dir.display(), "no transcripts");
            return Vec::new();
        }
    };
    let mut files: Vec<_> = entries
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| {
            path.extension()
                .is_some_and(|extension| extension == "jsonl")
        })
        .filter_map(|path| {
            let modified = path.metadata().and_then(|metadata| metadata.modified());
            match modified {
                Ok(modified) => Some((path, modified)),
                Err(error) => {
                    tracing::debug!(%error, path = %path.display(), "a transcript without a time");
                    None
                }
            }
        })
        .collect();
    files.sort_by_key(|(_, modified)| std::cmp::Reverse(*modified));
    files
}

/// The sessions started in `cwd`, newest first. Sessions without a prompt (started, never
/// used) are left out.
/// The working directory's sessions, newest first. Those in `known` whose transcript hasn't
/// changed since are taken as they are, not read again.
pub(crate) fn list(cwd: &Path, limit: usize, known: Vec<SessionInfo>) -> Vec<SessionInfo> {
    let mut known: HashMap<SessionId, SessionInfo> = known
        .into_iter()
        .map(|session| (session.id.clone(), session))
        .collect();
    let mut files: Vec<(PathBuf, SystemTime)> = project_dirs(cwd)
        .iter()
        .flat_map(|dir| transcripts(dir))
        .collect();
    files.sort_by_key(|(_, modified)| std::cmp::Reverse(*modified));
    // A session in both homes is listed once, as its newer transcript has it.
    let mut seen = std::collections::HashSet::new();
    files.retain(|(path, _)| seen.insert(path.file_name().map(std::ffi::OsStr::to_owned)));
    files
        .into_iter()
        .filter_map(|(path, modified)| {
            let id = SessionId::try_from(path.file_stem()?.to_string_lossy().into_owned()).ok()?;
            if let Some(same) = known
                .remove(&id)
                .filter(|session| session.modified == modified)
            {
                return Some(same);
            }
            let (title, worktree) = summary(&path)?;
            Some(SessionInfo {
                id,
                title,
                modified,
                worktree,
            })
        })
        .take(limit)
        .collect()
}

/// Every working directory Claude Code was used in that still exists, most recent first.
pub(crate) fn projects() -> Vec<ProjectInfo> {
    let mut projects: Vec<_> = all_projects_dirs()
        .iter()
        .flat_map(|projects| match std::fs::read_dir(projects) {
            Ok(entries) => entries.filter_map(Result::ok).collect(),
            Err(error) => {
                tracing::debug!(%error, dir = %projects.display(), "no projects");
                Vec::new()
            }
        })
        .filter_map(|entry| {
            let (newest, modified) = transcripts(&entry.path()).into_iter().next()?;
            let path = recorded_cwd(&newest)?;
            path.is_dir().then_some(ProjectInfo { path, modified })
        })
        .collect();
    projects.sort_by_key(|project| std::cmp::Reverse(project.modified));
    // A project with sessions in both homes is listed once.
    let mut seen = std::collections::HashSet::new();
    projects.retain(|project| seen.insert(project.path.clone()));
    projects
}

/// The sessions Claude Code processes are running now, other than our own (`claude -p`).
pub(crate) fn live() -> Vec<LiveSession> {
    let dir = config_dir().join("sessions");
    let entries = match std::fs::read_dir(&dir) {
        Ok(entries) => entries,
        Err(error) => {
            tracing::debug!(%error, "no running sessions");
            return Vec::new();
        }
    };
    let mut sessions: Vec<LiveSession> = entries
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|extension| extension == "json"))
        .filter_map(|path| {
            let text = read_small(&path, RECORD_BYTES)?;
            let record: LiveRecord = match serde_json::from_str(&text) {
                Ok(record) => record,
                Err(error) => {
                    tracing::debug!(%error, path = %path.display(), "a session record that doesn't parse");
                    return None;
                }
            };
            // A record that can't tell its process from a later one with the same pid isn't
            // taken; nor this window's own.
            let started = record.proc_start?;
            let shown = matches!(record.kind, LiveKind::Interactive | LiveKind::Bg)
                && record.pid != std::process::id();
            (shown && process::is_running(record.pid, &started)).then(|| {
                let transcript = find_transcript(&record.cwd, &record.session_id);
                LiveSession {
                    pid: record.pid,
                    started,
                    worktree: linked_worktree(&record.cwd),
                    title: transcript
                        .as_deref()
                        .and_then(summary)
                        .map(|(title, _)| title),
                    agents: transcript
                        .as_deref()
                        .map_or(0, |transcript| running_agents(transcript, SystemTime::now())),
                    id: record.session_id,
                    cwd: record.cwd,
                    name: record.name,
                    kind: record.kind,
                    status: record.status,
                }
            })
        })
        .collect();
    // Those waiting for their user first, then those at work, then the idle.
    sessions.sort_by_key(|session| {
        let urgency = match session.status {
            LiveStatus::Waiting => 0,
            LiveStatus::Busy => 1,
            LiveStatus::Idle => 2,
            LiveStatus::Other => 3,
        };
        (urgency, session.name.clone())
    });
    sessions
}

/// How long a subagent's transcript may go unwritten and the agent still count as running.
/// Claude Code marks an agent finished as it ends; one whose end went unsaid (its session was
/// killed) is no longer counted once it has been quiet this long.
const AGENT_QUIET: std::time::Duration = std::time::Duration::from_secs(30 * 60);

/// The subagents of the session whose transcript is `transcript` that run at `now`: those whose
/// record beside it (`<session>/subagents/agent-<id>.meta.json`) isn't marked finished, and
/// whose transcript was written within [`AGENT_QUIET`].
pub(crate) fn running_agents(transcript: &Path, now: SystemTime) -> usize {
    let Ok(entries) = std::fs::read_dir(transcript.with_extension("").join("subagents")) else {
        return 0;
    };
    entries
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let name = entry
                .file_name()
                .to_str()?
                .strip_suffix(".meta.json")?
                .to_owned();
            Some((
                entry.path(),
                entry.path().with_file_name(format!("{name}.jsonl")),
            ))
        })
        .filter(|(record, agent_transcript)| {
            let finished = read_small(record, RECORD_BYTES)
                .and_then(|text| serde_json::from_str::<AgentRecord>(&text).ok())
                .is_some_and(|record| record.finished);
            // Written after `now` (a clock set back, or `now` taken first): just written.
            let quiet_for = std::fs::metadata(agent_transcript)
                .and_then(|metadata| metadata.modified())
                .ok()
                .map(|written| now.duration_since(written).unwrap_or_default());
            !finished && quiet_for.is_some_and(|quiet| quiet < AGENT_QUIET)
        })
        .count()
}

/// A `.git` file or a `HEAD` names a path or a branch.
const GIT_POINTER_BYTES: u64 = 4096;
/// A session record is a few hundred bytes; more is not one.
const RECORD_BYTES: u64 = 64 * 1024;
/// What is read of a transcript at once: whole sessions run to tens of megabytes.
const TRANSCRIPT_BYTES: u64 = 512 * 1024 * 1024;

/// A small regular file's text: not a FIFO, which would block, nor a device or a huge file
/// (anything running as the user can put those where a record or `.git` is expected).
fn read_small(path: &Path, limit: u64) -> Option<String> {
    let metadata = std::fs::metadata(path).ok()?;
    if !metadata.is_file() || metadata.len() > limit {
        return None;
    }
    let mut text = String::new();
    File::open(path)
        .ok()?
        .take(limit)
        .read_to_string(&mut text)
        .ok()?;
    Some(text)
}

/// The whole lines of a transcript from `offset` on, and the offset after the last.
pub(crate) fn read_from(path: &Path, offset: u64) -> std::io::Result<(Vec<TranscriptLine>, u64)> {
    if !std::fs::metadata(path)?.is_file() {
        return Err(std::io::Error::other("not a regular file"));
    }
    let mut file = File::open(path)?;
    file.seek(SeekFrom::Start(offset))?;
    let mut bytes = Vec::new();
    file.take(TRANSCRIPT_BYTES).read_to_end(&mut bytes)?;
    // A line still being written is left for the next read.
    let whole = bytes
        .iter()
        .rposition(|&byte| byte == b'\n')
        .map_or(0, |at| at + 1);
    let text = String::from_utf8_lossy(bytes.get(..whole).unwrap_or_default());
    let lines = text.lines().filter_map(parse_line).collect();
    Ok((lines, offset + whole as u64))
}

fn parse_line(line: &str) -> Option<TranscriptLine> {
    match serde_json::from_str(line) {
        Ok(parsed) => Some(parsed),
        Err(error) => {
            tracing::debug!(%error, "a transcript line that doesn't parse");
            None
        }
    }
}

/// Titles are appended as the session goes on; prompts are at the start. Neither needs the
/// whole file, which can be many megabytes.
const PEEK_BYTES: u64 = 128 * 1024;

/// A session's title and the worktree it last worked in.
fn summary(path: &Path) -> Option<(String, Option<Worktree>)> {
    let mut file = File::open(path).ok()?;
    let length = file.metadata().ok()?.len();
    let tail = read_range(&mut file, length.saturating_sub(PEEK_BYTES), length)?;
    let (mut custom, mut generated, mut summary, mut worktree) = (None, None, None, None);
    let candidates = tail.lines().filter(|line| {
        line.contains("itle\"") || line.contains("\"summary\"") || line.contains("worktree-state")
    });
    for line in candidates {
        match serde_json::from_str(line) {
            Ok(TranscriptLine::CustomTitle { custom_title }) => custom = Some(custom_title),
            Ok(TranscriptLine::AiTitle { ai_title }) => generated = Some(ai_title),
            Ok(TranscriptLine::Summary { summary: text }) => summary = Some(text),
            Ok(TranscriptLine::WorktreeState { worktree: state }) => worktree = Some(state),
            // The first line of the tail may be cut; other lines aren't titles.
            Ok(_) | Err(_) => {}
        }
    }
    let worktree = worktree.flatten().and_then(Worktree::existing);
    // A custom title holding the session's two words is the name edualc gave its process.
    let own_name = path
        .file_stem()
        .and_then(|stem| SessionId::try_from(stem.to_string_lossy().into_owned()).ok())
        .map(|id| PetName::of(&id));
    let custom = custom.filter(|title| !own_name.as_ref().is_some_and(|name| name.is_in(title)));
    if let Some(title) = custom.or(generated).or(summary) {
        return Some((title, worktree));
    }
    file.seek(SeekFrom::Start(0)).ok()?;
    head_lines(file)
        .filter_map(|line| match serde_json::from_str(&line) {
            Ok(TranscriptLine::User {
                message,
                is_meta: false,
                is_sidechain: false,
            }) => Some(message),
            Ok(_) | Err(_) => None,
        })
        .find_map(|message| prompt_text(&message.content))
        .map(|prompt| (crate::tools::truncate_chars(prompt.trim(), 80), worktree))
}

/// The worktree a session works in now, as its transcript last said.
pub(crate) fn worktree_of(transcript: &Path) -> Option<Worktree> {
    summary(transcript).and_then(|(_, worktree)| worktree)
}

fn recorded_cwd(path: &Path) -> Option<PathBuf> {
    head_lines(File::open(path).ok()?)
        .filter(|line| line.contains("\"cwd\""))
        .find_map(|line| serde_json::from_str::<TranscriptCwd>(&line).ok())
        .map(|record| record.cwd)
}

/// The prompt a user message holds, if it holds one: a prompt sent after `!` commands comes
/// after their rows.
fn prompt_text(content: &UserContent) -> Option<String> {
    let (whole, blocks) = match content {
        UserContent::Text(text) => (Some(text.as_str()), [].as_slice()),
        UserContent::Blocks(blocks) => (None, blocks.as_slice()),
    };
    let block_texts = blocks.iter().filter_map(|block| match block {
        wire::ContentBlock::Text { text } => Some(text.as_str()),
        wire::ContentBlock::Thinking { .. }
        | wire::ContentBlock::ToolUse { .. }
        | wire::ContentBlock::ServerToolUse { .. }
        | wire::ContentBlock::ToolResult { .. }
        | wire::ContentBlock::Image { .. }
        | wire::ContentBlock::Other => None,
    });
    whole
        .into_iter()
        .chain(block_texts)
        .find_map(|text| match UserText::of(text) {
            UserText::Prompt(text) => Some(text.to_owned()),
            UserText::Command { .. }
            | UserText::Shell(_)
            | UserText::ShellOutput { .. }
            | UserText::Stdout(_)
            | UserText::Stderr(_)
            | UserText::Interrupted
            | UserText::ForModel
            | UserText::Empty => None,
        })
}

/// The lines at the start of a transcript, read only as far as they are wanted.
fn head_lines(file: File) -> impl Iterator<Item = String> {
    BufReader::new(file.take(PEEK_BYTES))
        .lines()
        .map_while(Result::ok)
}

fn read_range(file: &mut File, start: u64, end: u64) -> Option<String> {
    file.seek(SeekFrom::Start(start)).ok()?;
    let mut bytes = Vec::new();
    file.take(end - start).read_to_end(&mut bytes).ok()?;
    Some(String::from_utf8_lossy(&bytes).into_owned())
}

/// How long ago, briefly: "now", "5m", "3h", "2d", "6w".
pub(crate) fn age(modified: SystemTime) -> String {
    let seconds = SystemTime::now()
        .duration_since(modified)
        .map_or(0, |elapsed| elapsed.as_secs());
    match seconds {
        0..60 => "now".into(),
        60..3600 => format!("{}m", seconds / 60),
        3600..86400 => format!("{}h", seconds / 3600),
        86400..604_800 => format!("{}d", seconds / 86400),
        _ => format!("{}w", seconds / 604_800),
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn project_dirs_are_named_like_claude_code_names_them() {
        let dir = project_dir(Path::new("/home/someone/code/my_project.v2"));
        assert!(dir.ends_with("-home-someone-code-my-project-v2"));
    }

    #[test]
    fn a_linked_worktree_names_its_branch() {
        let dir = std::env::temp_dir().join(format!("edualc-worktree-{}", std::process::id()));
        let gitdir = dir.join("repo-git-worktrees-w");
        let worktree = dir.join("w");
        std::fs::create_dir_all(&gitdir).expect("a scratch git dir");
        std::fs::create_dir_all(&worktree).expect("a scratch worktree");
        std::fs::write(gitdir.join("HEAD"), "ref: refs/heads/feature\n").expect("a HEAD");
        std::fs::write(
            worktree.join(".git"),
            format!("gitdir: {}\n", gitdir.display()),
        )
        .expect("a .git file");
        let found = linked_worktree(&worktree).expect("a linked worktree");
        assert_eq!(
            (found.name.as_str(), found.branch.as_str()),
            ("w", "feature")
        );
        assert_eq!(linked_worktree(&dir), None);
        std::fs::remove_dir_all(&dir).expect("the scratch directory goes");
    }

    /// A session's subagents run while their record isn't marked finished and their transcript
    /// was written lately; one quiet for long, finished, or without a transcript doesn't count.
    #[test]
    fn running_subagents_are_counted_from_their_records() {
        let dir = std::env::temp_dir().join(format!("edualc-agents-{}", std::process::id()));
        let subagents = dir.join("s1").join("subagents");
        std::fs::create_dir_all(&subagents).expect("a scratch directory");
        let agent = |id: &str, record: &str, transcript: bool| {
            std::fs::write(subagents.join(format!("agent-{id}.meta.json")), record)
                .expect("a record");
            if transcript {
                std::fs::write(subagents.join(format!("agent-{id}.jsonl")), "{}\n")
                    .expect("a transcript");
            }
        };
        agent("a1", r#"{"agentType":"Explore","description":"d"}"#, true);
        agent("a2", r#"{"agentType":"Explore","finished":true}"#, true);
        agent("a3", r#"{"agentType":"Explore","finished":false}"#, true);
        agent("a4", r#"{"agentType":"Explore"}"#, false);
        let session = dir.join("s1.jsonl");
        let written = std::fs::metadata(subagents.join("agent-a1.jsonl"))
            .and_then(|metadata| metadata.modified())
            .expect("a time it was written");
        assert_eq!(running_agents(&session, written), 2, "a1 and a3");
        // Written after `now` reads as just written, whatever order the files were made in.
        let before = written - std::time::Duration::from_secs(1);
        assert_eq!(running_agents(&session, before), 2, "a1 and a3");
        let later = written + AGENT_QUIET + std::time::Duration::from_secs(1);
        assert_eq!(running_agents(&session, later), 0, "quiet too long");
        assert_eq!(running_agents(&dir.join("none.jsonl"), written), 0);
        std::fs::remove_dir_all(&dir).expect("the scratch directory goes");
    }

    #[test]
    fn a_line_still_being_written_waits() {
        let dir = std::env::temp_dir().join(format!("edualc-transcript-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("a scratch directory");
        let path = dir.join("t.jsonl");
        let whole = format!("{}\n", json!({ "type": "summary", "summary": "a" }));
        let next = json!({ "type": "summary", "summary": "b" }).to_string();
        let unfinished: String = next.chars().take(next.len() / 2).collect();
        std::fs::write(&path, format!("{whole}{unfinished}")).expect("a transcript");
        let (lines, offset) = read_from(&path, 0).expect("readable");
        assert_eq!(lines.len(), 1);
        assert_eq!(offset, whole.len() as u64);
        std::fs::remove_dir_all(&dir).expect("the scratch directory goes");
    }

    /// The name edualc gave a session's process, in each of its shapes so far, isn't taken for
    /// its title, which Claude Code's own then is; a title the user gave it is.
    #[test]
    fn edualcs_name_for_a_session_is_no_title() {
        let id = "291365c8-6d45-4dba-ba19-41a5f0eccd6b";
        let pet = PetName::of(&SessionId::try_from(id.to_owned()).expect("an id"));
        let dir = std::env::temp_dir().join(format!("edualc-titles-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("a scratch directory");
        let path = dir.join(format!("{id}.jsonl"));
        let title_after = |custom: &str| {
            let lines = [
                json!({ "type": "ai-title", "aiTitle": "Floating NPU screen describer" }),
                json!({ "type": "custom-title", "customTitle": custom }),
            ];
            let text: String = lines.iter().map(|line| format!("{line}\n")).collect();
            std::fs::write(&path, text).expect("a transcript");
            summary(&path).map(|(title, _)| title)
        };
        for name in [
            format!("edualc-{pet}"),
            format!("edualc-local-npu-screen-con-{pet}-81"),
            format!("{pet}-local-npu-screen-con-81"),
        ] {
            assert_eq!(
                title_after(&name).as_deref(),
                Some("Floating NPU screen describer"),
                "{name}"
            );
        }
        assert_eq!(
            title_after("NPU describer, native").as_deref(),
            Some("NPU describer, native")
        );
        std::fs::remove_dir_all(&dir).expect("the scratch directory goes");
    }
}
