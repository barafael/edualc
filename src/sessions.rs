//! Claude Code's sessions on disk: saved ones as `~/.claude/projects/<project>/<id>.jsonl`,
//! one directory per working directory, named after its path with every character other than
//! letters and digits made a `-`; running ones as `~/.claude/sessions/<pid>.json`.

#[cfg(not(target_arch = "wasm32"))]
use std::collections::HashMap;
use std::fs::File;
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use serde::Deserialize;
use web_time::SystemTime;

use crate::chat::UserText;
#[cfg(not(target_arch = "wasm32"))]
use crate::external::claude_files::{AgentRecord, LiveRecord};
use crate::external::wire::{
    self, MessageOrigin, SessionId, TranscriptCwd, TranscriptLine, UserContent,
};
#[cfg(not(target_arch = "wasm32"))]
use crate::home::Home;
#[cfg(not(target_arch = "wasm32"))]
use crate::process;

/// A session's two words, an adjective and a noun picked by its id, so the same each time it
/// runs: edualc names a session's process by them (see `spawn`), and Claude Code keeps that name
/// as the session's custom title too, which is a name then, not a title.
#[derive(Debug, Clone, PartialEq, Eq, derive_more::Display)]
pub(crate) struct PetName(String);

impl PetName {
    pub(crate) fn of(id: &SessionId) -> Self {
        fn word<'list>(list: &[&'list str], number: usize) -> &'list str {
            list.get(number % list.len().max(1))
                .copied()
                .unwrap_or("session")
        }
        let words = petname::Petnames::small();
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

/// The name other Claude Code sessions message a session's process by (`--name`): an adjective
/// and a noun picked by the session's id, so the same each time it runs, then its project. The
/// CLI's own name comes from the process id, and changes as the session is started again.
#[derive(Debug, Clone, PartialEq, Eq, Hash, derive_more::Display)]
pub(crate) struct MessagingName(String);

impl MessagingName {
    /// The name of session `id` in the folder `cwd`: an adjective, a noun, the folder's name
    /// and two digits, all but the folder picked by the id.
    pub(crate) fn of(id: &SessionId, cwd: &Path) -> Self {
        Self(format!(
            "{}-{}-{:02}",
            PetName::of(id),
            project_part(cwd),
            id_number(id, 16) % 100
        ))
    }

    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }
}

/// The most of the folder's name a session's name takes.
const PROJECT_PART_CHARS: usize = 20;

/// The folder's name as a session's name takes it, as Claude Code writes its own names
/// (`claude_but_sane` as `claude-but-sane`): lowercase letters and digits, the rest a dash, at
/// most [`PROJECT_PART_CHARS`]. "project" for a folder with no such letters.
fn project_part(cwd: &Path) -> String {
    let folder = cwd
        .file_name()
        .map(|name| name.to_string_lossy().to_lowercase())
        .unwrap_or_default();
    let mut part = String::new();
    for character in folder.chars() {
        if character.is_ascii_alphanumeric() {
            part.push(character);
        } else if !part.is_empty() && !part.ends_with('-') {
            part.push('-');
        }
    }
    let part: String = part.chars().take(PROJECT_PART_CHARS).collect();
    match part.trim_end_matches('-') {
        "" => "project".to_owned(),
        trimmed => trimmed.to_owned(),
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
        .chain(sandboxed_projects(cwd))
        .map(|projects| projects.join(&name))
        .collect()
}

/// Every folder of transcripts' folders: the user's own home's and every sandboxed home's.
fn all_projects_dirs() -> Vec<PathBuf> {
    std::iter::once(projects_dir())
        .chain(all_sandboxed_projects())
        .collect()
}

/// The folder of transcripts' folders in the home of `cwd`'s sandboxed sessions.
#[cfg(not(target_arch = "wasm32"))]
fn sandboxed_projects(cwd: &Path) -> Option<PathBuf> {
    Home::of(cwd).map(|home| home.projects())
}

/// Every sandboxed home's folder of transcripts' folders.
#[cfg(not(target_arch = "wasm32"))]
fn all_sandboxed_projects() -> Vec<PathBuf> {
    Home::all().iter().map(Home::projects).collect()
}

/// The browser demo has no sandbox, and so no sandboxed homes.
#[cfg(target_arch = "wasm32")]
fn sandboxed_projects(_cwd: &Path) -> Option<PathBuf> {
    None
}

#[cfg(target_arch = "wasm32")]
fn all_sandboxed_projects() -> Vec<PathBuf> {
    Vec::new()
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
    newest(project_dirs(cwd).into_iter().map(|dir| dir.join(&name)))
        .or_else(|| transcript_in_any(&all_projects_dirs(), id))
}

/// Session `id`'s transcript in any project of the folders of projects `homes`; the newest,
/// if more than one has it.
fn transcript_in_any(homes: &[PathBuf], id: &SessionId) -> Option<PathBuf> {
    let name = format!("{}.jsonl", id.as_str());
    newest(
        homes
            .iter()
            .flat_map(|projects| std::fs::read_dir(projects).into_iter().flatten())
            .filter_map(Result::ok)
            .map(|entry| entry.path().join(&name)),
    )
}

/// What the transcripts say of a session to continue.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Saved {
    /// No home keeps a transcript of it.
    Nowhere,
    /// The folder it was started in: where it goes on, as the sidebar opens it there.
    StartedIn(PathBuf),
    /// Its transcript names no folder in its first lines.
    Unplaced,
}

/// Session `id` as its transcript says, whichever home and project keep it.
pub(crate) fn saved(id: &SessionId) -> Saved {
    saved_in(&all_projects_dirs(), id)
}

fn saved_in(homes: &[PathBuf], id: &SessionId) -> Saved {
    match transcript_in_any(homes, id) {
        None => Saved::Nowhere,
        Some(transcript) => recorded_cwd(&transcript).map_or(Saved::Unplaced, Saved::StartedIn),
    }
}

/// The Claude Code home a session's process writes its transcripts in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ClaudeHome {
    /// The user's own.
    Users,
    /// Its folder's sandboxed home (see `home`). The user's own follows the session's transcript
    /// there, not its subagents'.
    Sandboxed,
}

/// Where session `id` working in `cwd` writes its transcript in `home`, or will once it writes
/// (see [`transcript_in`]).
pub(crate) fn written_transcript(home: ClaudeHome, cwd: &Path, id: &SessionId) -> PathBuf {
    let projects = match home {
        ClaudeHome::Users => None,
        ClaudeHome::Sandboxed => sandboxed_projects(cwd),
    }
    .unwrap_or_else(projects_dir);
    transcript_in(&projects, cwd, id)
}

/// Where session `id` working in `cwd` has its transcript in the folder of projects' folders
/// `projects`: under its folder's project, or another's when it moved there (into a worktree);
/// under its folder's project if it is not written yet.
fn transcript_in(projects: &Path, cwd: &Path, id: &SessionId) -> PathBuf {
    let name = format!("{}.jsonl", id.as_str());
    let own = projects.join(project_name(cwd)).join(&name);
    if own.is_file() {
        return own;
    }
    newest(
        std::fs::read_dir(projects)
            .into_iter()
            .flatten()
            .filter_map(Result::ok)
            .map(|entry| entry.path().join(&name)),
    )
    .unwrap_or(own)
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
#[cfg(not(target_arch = "wasm32"))]
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
#[cfg(not(target_arch = "wasm32"))]
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
#[cfg(not(target_arch = "wasm32"))]
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
#[cfg(not(target_arch = "wasm32"))]
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
#[cfg(not(target_arch = "wasm32"))]
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

/// The demo project's sessions, in the browser demo (see `web::demo`), newest first.
#[cfg(target_arch = "wasm32")]
pub(crate) fn list(cwd: &Path, limit: usize, _known: Vec<SessionInfo>) -> Vec<SessionInfo> {
    let mut sessions = crate::web::demo::saved(cwd);
    sessions.truncate(limit);
    sessions
}

/// The demo's folders, in the browser demo.
#[cfg(target_arch = "wasm32")]
pub(crate) fn projects() -> Vec<ProjectInfo> {
    crate::web::demo::projects()
}

/// The demo's sessions "running in other terminals", in the browser demo.
#[cfg(target_arch = "wasm32")]
pub(crate) fn live() -> Vec<LiveSession> {
    crate::web::demo::running()
}

/// The whole lines of one of the demo's transcripts from `offset` on, in the browser demo,
/// and the offset after the last; they are built into the page whole.
#[cfg(target_arch = "wasm32")]
pub(crate) fn read_from(path: &Path, offset: u64) -> std::io::Result<(Vec<TranscriptLine>, u64)> {
    let text = crate::web::demo::transcript(path)
        .ok_or_else(|| std::io::Error::from(std::io::ErrorKind::NotFound))?;
    let rest = usize::try_from(offset)
        .ok()
        .and_then(|at| text.get(at..))
        .unwrap_or_default();
    let lines = rest.lines().filter_map(parse_line).collect();
    Ok((lines, u64::try_from(text.len()).unwrap_or(u64::MAX)))
}

/// The whole lines of a transcript from `offset` on, and the offset after the last.
#[cfg(not(target_arch = "wasm32"))]
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
        return Some((title_line(&title).to_owned(), worktree));
    }
    // The first prompt; or, for a session another one started by writing to it, that message,
    // whose turn can fill more than the head before the user writes anything.
    file.seek(SeekFrom::Start(0)).ok()?;
    head_lines(file)
        .find_map(|line| match serde_json::from_str(&line) {
            Ok(TranscriptLine::User {
                is_sidechain: false,
                origin: Some(MessageOrigin::Peer { name, body }),
                ..
            }) => Some(peer_title(name.as_deref(), body.as_deref())),
            Ok(TranscriptLine::User {
                message,
                is_meta: false,
                is_sidechain: false,
                ..
            }) => prompt_text(&message.content),
            Ok(_) | Err(_) => None,
        })
        .map(|prompt| {
            (
                crate::tools::truncate_chars(title_line(&prompt), 80),
                worktree,
            )
        })
}

/// The first line with text in it: a title is shown on one line, in a row of its height.
fn title_line(text: &str) -> &str {
    text.lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or_default()
}

/// A title from another session's message: who sent it and what it says, as far as known.
fn peer_title(name: Option<&str>, body: Option<&str>) -> String {
    match (name, body) {
        (Some(name), Some(body)) => format!("{name}: {}", body.trim()),
        (None, Some(body)) => body.to_owned(),
        (Some(name), None) => format!("Message from {name}"),
        (None, None) => "Message from another session".to_owned(),
    }
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

    /// A session keeps its name however often it is started: an adjective and a noun from its
    /// id, its project, then two digits from its id; other ids get other names, but for rare
    /// collisions.
    #[test]
    fn a_session_is_named_by_its_project_and_id() {
        let id = |text: &str| SessionId::try_from(text.to_owned()).expect("a session id");
        let project = Path::new("/home/someone/code/claude_but_sane");
        let session = "d1ba2743-c47a-4446-aefc-d4594dbc045d";
        let name = MessagingName::of(&id(session), project);
        assert_eq!(
            name,
            MessagingName::of(&id(session), project),
            "started again"
        );
        let mut parts = name.0.splitn(3, '-');
        let (Some(adjective), Some(noun), Some(rest)) = (parts.next(), parts.next(), parts.next())
        else {
            panic!("two words first in {name}");
        };
        let words = petname::Petnames::small();
        assert!(words.adjectives.contains(&adjective), "{name}");
        assert!(words.nouns.contains(&noun), "{name}");
        let number = rest
            .strip_prefix("claude-but-sane-")
            .expect("named after its project");
        assert!(
            number.len() == 2 && number.chars().all(|digit| digit.is_ascii_digit()),
            "{name}"
        );
        let names: std::collections::HashSet<MessagingName> = (0..200)
            .map(|_| MessagingName::of(&SessionId::new_random(), project))
            .collect();
        assert!(names.len() > 195, "only {} names of 200", names.len());
    }

    /// A project's part of the name is its folder's, as Claude Code writes its own names, and
    /// short; a folder with no Latin letters or digits is "project".
    #[test]
    fn a_project_is_named_after_its_folder() {
        assert_eq!(
            project_part(Path::new("/code/claude_but_sane")),
            "claude-but-sane"
        );
        assert_eq!(
            project_part(Path::new("/code/My Project.v2")),
            "my-project-v2"
        );
        assert_eq!(
            project_part(Path::new("/code/a-very-long-project-name-indeed")),
            "a-very-long-project"
        );
        assert_eq!(project_part(Path::new("/home/someone/文档")), "project");
        assert_eq!(project_part(Path::new("/")), "project");
    }

    /// The browser demo's transcripts (`web/demo`) read as Claude Code's: every line parses, and
    /// each has the title the demo's sidebar lists it by.
    #[test]
    fn the_browser_demos_transcripts_read_as_transcripts() {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("web/demo");
        let transcripts: Vec<PathBuf> = std::fs::read_dir(&dir)
            .expect("the demo's folder")
            .filter_map(|entry| entry.ok().map(|entry| entry.path()))
            .filter(|path| {
                path.extension()
                    .is_some_and(|extension| extension == "jsonl")
            })
            .collect();
        assert_eq!(transcripts.len(), 11, "{transcripts:?}");
        for path in transcripts {
            let text = std::fs::read_to_string(&path).expect("a transcript");
            let lines: Vec<TranscriptLine> = text.lines().filter_map(parse_line).collect();
            assert_eq!(lines.len(), text.lines().count(), "{}", path.display());
            assert!(
                lines
                    .iter()
                    .any(|line| matches!(line, TranscriptLine::AiTitle { .. })),
                "{}",
                path.display()
            );
        }
    }

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

    /// A session's transcript in a home is under its folder's project, or under the project it
    /// moved to; under its folder's while not written.
    #[test]
    fn a_transcript_in_a_home_is_found_where_it_went() {
        let projects = std::env::temp_dir().join(format!("edualc-home-{}", std::process::id()));
        let cwd = Path::new("/w/app");
        let id =
            SessionId::try_from("0b8f1c2d-3e4f-4a5b-8c6d-7e8f9a0b1c2d".to_owned()).expect("an id");
        let own = projects.join("-w-app").join(format!("{id}.jsonl"));
        assert_eq!(transcript_in(&projects, cwd, &id), own, "not written yet");
        let moved = projects.join("-w-app-wt").join(format!("{id}.jsonl"));
        std::fs::create_dir_all(moved.parent().expect("a folder")).expect("a project folder");
        std::fs::write(&moved, "").expect("a transcript");
        assert_eq!(
            transcript_in(&projects, cwd, &id),
            moved,
            "moved to a worktree"
        );
        std::fs::create_dir_all(own.parent().expect("a folder")).expect("a project folder");
        std::fs::write(&own, "").expect("a transcript");
        let later = SystemTime::now() + std::time::Duration::from_secs(60);
        File::options()
            .write(true)
            .open(&moved)
            .and_then(|file| file.set_modified(later))
            .expect("the moved one newer");
        assert_eq!(
            transcript_in(&projects, cwd, &id),
            own,
            "its folder's first, if newer elsewhere"
        );
        std::fs::remove_dir_all(&projects).expect("the scratch folder goes");
    }

    /// A session to continue is found in whichever home keeps it, and goes on in the folder it
    /// was started in, not one its later turns ran in; one no home has, isn't.
    #[test]
    fn a_session_goes_on_where_it_was_started() {
        let scratch = std::env::temp_dir().join(format!("edualc-started-{}", std::process::id()));
        let (users, sandboxed) = (scratch.join("users"), scratch.join("sandboxed"));
        let id =
            SessionId::try_from("5d6e7f80-91a2-4b3c-8d4e-5f60718293a4".to_owned()).expect("an id");
        let transcript = sandboxed.join("-w-app").join(format!("{id}.jsonl"));
        std::fs::create_dir_all(transcript.parent().expect("a folder")).expect("a project");
        std::fs::create_dir_all(&users).expect("a home");
        let line = |cwd: &str| json!({ "type": "user", "cwd": cwd, "sessionId": id.as_str() });
        std::fs::write(
            &transcript,
            format!("{}\n{}\n", line("/w/app"), line("/somewhere/else")),
        )
        .expect("a transcript");
        let homes = [users.clone(), sandboxed];
        assert_eq!(
            saved_in(&homes, &id),
            Saved::StartedIn(PathBuf::from("/w/app"))
        );
        let unknown =
            SessionId::try_from("00000000-1111-4222-8333-444444444444".to_owned()).expect("an id");
        assert_eq!(saved_in(&homes, &unknown), Saved::Nowhere);
        let unplaced = users.join("-w-lib").join(format!("{unknown}.jsonl"));
        std::fs::create_dir_all(unplaced.parent().expect("a folder")).expect("a project");
        std::fs::write(&unplaced, format!("{}\n", json!({ "type": "summary" })))
            .expect("a transcript");
        assert_eq!(saved_in(&homes, &unknown), Saved::Unplaced);
        std::fs::remove_dir_all(&scratch).expect("the scratch folder goes");
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

    /// A session another session started by writing to it is titled by that message, and so
    /// listed, though the user's first prompt comes later than the head that is read; a
    /// session the user started keeps its prompt, whatever reached it after.
    #[test]
    fn a_title_is_one_line() {
        let dir = std::env::temp_dir().join(format!("edualc-one-line-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("a scratch directory");
        let path = dir.join("80bdeda4-8427-4be3-b815-ea82e7490483.jsonl");
        let title_of = |line: serde_json::Value| {
            std::fs::write(&path, format!("{line}\n")).expect("a transcript");
            summary(&path).map(|(title, _)| title)
        };
        let prompt = json!({
            "type": "user",
            "message": { "role": "user", "content": "\n  Check the kitchen\nThen move on to the hall" },
        });
        assert_eq!(title_of(prompt).as_deref(), Some("Check the kitchen"));
        let titled = json!({ "type": "custom-title", "customTitle": "Two\nlines" });
        assert_eq!(title_of(titled).as_deref(), Some("Two"));
        std::fs::remove_dir_all(&dir).expect("cleaned up");
    }

    #[test]
    fn a_session_another_started_is_titled_by_its_message() {
        let dir = std::env::temp_dir().join(format!("edualc-peer-titles-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("a scratch directory");
        let path = dir.join("80bdeda4-8427-4be3-b815-ea82e7490482.jsonl");
        let title_of = |lines: &[serde_json::Value]| {
            let text: String = lines.iter().map(|line| format!("{line}\n")).collect();
            std::fs::write(&path, text).expect("a transcript");
            summary(&path).map(|(title, _)| title)
        };
        let peer = json!({
            "type": "user",
            "isMeta": true,
            "message": { "role": "user", "content": "Another Claude session sent a message: …" },
            "origin": { "kind": "peer", "from": "unknown", "name": "sterling-gibbon-94",
                        "body": "Please reply with one sentence." },
        });
        let filler = json!({ "type": "attachment", "attachment": {
            "type": "skill_listing", "content": "x".repeat(usize::try_from(PEEK_BYTES).expect("small")) } });
        let prompt = json!({ "type": "user", "message": { "role": "user", "content": "Send a second note" } });
        assert_eq!(
            title_of(&[peer.clone(), filler, prompt.clone()]).as_deref(),
            Some("sterling-gibbon-94: Please reply with one sentence.")
        );
        assert_eq!(
            title_of(&[prompt, peer]).as_deref(),
            Some("Send a second note")
        );
        std::fs::remove_dir_all(&dir).expect("the scratch directory goes");
    }
}
