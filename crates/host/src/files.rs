//! What the daemon reads and watches for the windows: Claude Code's files (its sessions'
//! records, transcripts, projects) and edualc's settings. The browser demo can't read them, and
//! the native window needn't, so both ask the same way: they ask (`ipc::Read`), and are told
//! when what they watch changes
//! (`ipc::Target`), noticed by inotify, never by looking every few seconds (only a watch that
//! couldn't be placed is tried again so, until it is).

use std::collections::{HashMap, HashSet};
use std::ffi::{OsStr, OsString};
use std::mem::MaybeUninit;
use std::os::fd::OwnedFd;
use std::os::unix::ffi::OsStrExt as _;
use std::path::{Path, PathBuf};
use std::time::Duration;

use rustix::fs::inotify::{self, CreateFlags, WatchFlags};
use rustix::io::Errno;
use tokio::sync::mpsc;

use crate::acp::brain;
use crate::external::agy::{self, AgyTask};
use crate::external::claude_files::{ClaudeJsonWhole, ProjectWhole};
use crate::external::wire::TranscriptLine;
use crate::folder::Folder;
use crate::ipc::{self, ActId, Answer, FromDaemon, Place, ProjectOverview, Read, Target, Watched};
use crate::{sessions, settings, snapshots};

/// Where what is told to a window goes.
type Outbox = mpsc::Sender<FromDaemon>;

/// The most sessions listed at once.
const MOST_SESSIONS: usize = 1000;

/// Writes `name` in `folder` whole or not at all, readable by this user alone, as Claude Code
/// writes its own.
pub(crate) fn write_atomically(folder: &Path, name: &OsStr, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write as _;
    use std::os::unix::fs::OpenOptionsExt as _;
    let staged = folder.join(format!(".edualc-{}.tmp", uuid::Uuid::new_v4()));
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&staged)?;
    let written = file
        .write_all(bytes)
        .and_then(|()| std::fs::rename(&staged, folder.join(name)));
    if written.is_err() {
        remove(&staged);
    }
    written
}

/// Removes a file edualc wrote, if it is there; one that stays is logged.
pub(crate) fn remove(path: &Path) {
    if let Err(error) = std::fs::remove_file(path)
        && error.kind() != std::io::ErrorKind::NotFound
    {
        tracing::debug!(%error, path = %path.display(), "a file of edualc's stays");
    }
}

/// Why a folder's trust wasn't recorded for Claude Code.
#[derive(Debug, thiserror::Error)]
pub(crate) enum ClaudeTrustError {
    #[error("couldn't read {path}")]
    Read {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("{0} is too large to be Claude Code's settings")]
    TooLarge(PathBuf),
    #[error("{path} doesn't read as Claude Code's settings")]
    Parse {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },
    #[error("couldn't write Claude Code's settings")]
    Encode(#[source] serde_json::Error),
    #[error("couldn't write {path}")]
    Write {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("Claude Code kept {0} locked")]
    Busy(PathBuf),
    #[error("couldn't lock {path}")]
    Lock {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

/// How long edualc waits for Claude Code to finish writing `~/.claude.json` before it records
/// the trust: a few short tries, as Claude Code holds its lock for moments.
const CLAUDE_LOCK_WAITS: [std::time::Duration; 4] = [
    std::time::Duration::from_millis(100),
    std::time::Duration::from_millis(200),
    std::time::Duration::from_millis(400),
    std::time::Duration::from_millis(800),
];
/// When Claude Code takes a lock left behind as stale (proper-lockfile's default, which Claude
/// Code 2.1.323 keeps, refreshing a lock it holds every 5 s).
const CLAUDE_LOCK_STALE: std::time::Duration = std::time::Duration::from_secs(10);

/// What recording a folder's trust for Claude Code came to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ClaudeTrust {
    /// Its record says it is trusted now.
    Recorded,
    /// Its record said so already: nothing was written.
    AlreadyTrusted,
}

/// Whether Claude Code applies `folder`'s own settings: its record, by Claude Code's key, says
/// the folder is trusted.
pub(crate) fn trusted_for_claude_code(folder: &Path) -> Result<bool, ClaudeTrustError> {
    let config = sessions::global_config_path();
    let path = std::fs::canonicalize(&config).unwrap_or(config);
    Ok(read_claude_json(&path)?
        .projects
        .get(&claude_project_key(folder))
        .is_some_and(|project| project.accepted == Some(true)))
}

/// Records `folder` as trusted for Claude Code too, as its trust dialog would
/// (`hasTrustDialogAccepted` in `~/.claude.json`): Claude Code applies a folder's own permission
/// rules only then. Written as Claude Code writes the file itself: under its lock, read afresh,
/// that one record changed and everything else kept; not at all if the record says so already.
pub(crate) fn trust_for_claude_code(folder: &Path) -> Result<ClaudeTrust, ClaudeTrustError> {
    trust_in(
        &sessions::global_config_path(),
        &claude_project_key(folder),
        &CLAUDE_LOCK_WAITS,
    )
}

/// `~/.claude.json` at `path` (its link followed) whole; an empty one where there is none.
fn read_claude_json(path: &Path) -> Result<ClaudeJsonWhole, ClaudeTrustError> {
    match std::fs::metadata(path) {
        Ok(metadata) if metadata.len() > sessions::GLOBAL_CONFIG_BYTES => {
            Err(ClaudeTrustError::TooLarge(path.to_owned()))
        }
        Ok(_) => {
            let text = std::fs::read_to_string(path).map_err(|source| ClaudeTrustError::Read {
                path: path.to_owned(),
                source,
            })?;
            serde_json::from_str(&text).map_err(|source| ClaudeTrustError::Parse {
                path: path.to_owned(),
                source,
            })
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            Ok(ClaudeJsonWhole::default())
        }
        Err(source) => Err(ClaudeTrustError::Read {
            path: path.to_owned(),
            source,
        }),
    }
}

/// The key Claude Code keeps a folder's record under (2.1.323): the real path of the git
/// repository it is in, or of the folder itself outside one.
fn claude_project_key(folder: &Path) -> PathBuf {
    let real = std::fs::canonicalize(folder).unwrap_or_else(|_| folder.to_owned());
    real.ancestors()
        .find(|place| place.join(".git").exists())
        .map_or_else(|| real.clone(), Path::to_path_buf)
}

/// Claude Code's lock on `config`, as it takes it: a folder beside the file, there while held.
struct ClaudeLock(PathBuf);

impl ClaudeLock {
    /// Takes the lock, waiting `waits` in turn while Claude Code holds it; one left behind
    /// longer than [`CLAUDE_LOCK_STALE`] is taken over, as Claude Code takes it over.
    fn take(config: &Path, waits: &[std::time::Duration]) -> Result<Self, ClaudeTrustError> {
        let mut path = config.as_os_str().to_owned();
        path.push(".lock");
        let path = PathBuf::from(path);
        let mut waits = waits.iter();
        loop {
            match std::fs::create_dir(&path) {
                Ok(()) => return Ok(Self(path)),
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                    let stale = std::fs::metadata(&path)
                        .and_then(|metadata| metadata.modified())
                        .ok()
                        .and_then(|modified| modified.elapsed().ok())
                        .is_some_and(|age| age > CLAUDE_LOCK_STALE);
                    if stale {
                        tracing::info!(lock = %path.display(), "a stale lock of Claude Code's taken over");
                        if let Err(error) = std::fs::remove_dir(&path)
                            && error.kind() != std::io::ErrorKind::NotFound
                        {
                            return Err(ClaudeTrustError::Lock {
                                path,
                                source: error,
                            });
                        }
                        continue;
                    }
                    let Some(wait) = waits.next() else {
                        return Err(ClaudeTrustError::Busy(path));
                    };
                    std::thread::sleep(*wait);
                }
                Err(source) => return Err(ClaudeTrustError::Lock { path, source }),
            }
        }
    }
}

impl Drop for ClaudeLock {
    fn drop(&mut self) {
        if let Err(error) = std::fs::remove_dir(&self.0) {
            tracing::warn!(%error, lock = %self.0.display(), "Claude Code's lock wasn't let go");
        }
    }
}

fn trust_in(
    config: &Path,
    key: &Path,
    waits: &[std::time::Duration],
) -> Result<ClaudeTrust, ClaudeTrustError> {
    let _lock = ClaudeLock::take(config, waits)?;
    // A link (a dotfiles repository's, say) is followed, and the file it names written.
    let path = std::fs::canonicalize(config).unwrap_or_else(|_| config.to_owned());
    let mut whole = read_claude_json(&path)?;
    // Claude Code reads a folder's settings by its own record alone: trust on a folder above it
    // doesn't count.
    match whole.projects.get_mut(key) {
        Some(project) if project.accepted == Some(true) => return Ok(ClaudeTrust::AlreadyTrusted),
        Some(project) => project.accepted = Some(true),
        None => {
            let project = ProjectWhole::trusted().map_err(ClaudeTrustError::Encode)?;
            whole.projects.insert(key.to_owned(), project);
        }
    }
    let text = serde_json::to_vec_pretty(&whole).map_err(ClaudeTrustError::Encode)?;
    let (Some(parent), Some(name)) = (path.parent(), path.file_name()) else {
        return Err(ClaudeTrustError::Write {
            path,
            source: std::io::Error::from(std::io::ErrorKind::InvalidInput),
        });
    };
    write_atomically(parent, name, &text).map_err(|source| ClaudeTrustError::Write {
        path: path.clone(),
        source,
    })?;
    Ok(ClaudeTrust::Recorded)
}

/// A transcript, by where it is and how it is written.
#[derive(Debug)]
pub enum Source {
    /// In Claude Code's format, which the ACP adapter writes too.
    Transcript(PathBuf),
    /// An agy conversation's steps, read as its transcript (see [`brain::step_line`]).
    AgySteps(PathBuf),
    /// An agy swarm's board, read as what its members said (see [`brain::post_line`]).
    AgyBoard(PathBuf),
}

impl Source {
    pub fn path(&self) -> &Path {
        match self {
            Self::Transcript(path) | Self::AgySteps(path) | Self::AgyBoard(path) => path,
        }
    }

    /// The transcript's whole lines after byte `from`, in Claude Code's format, and where the
    /// next read starts.
    fn read_from(&self, from: u64) -> std::io::Result<(Vec<String>, u64)> {
        let (lines, offset) = sessions::read_lines_from(self.path(), from)?;
        let lines = match self {
            Self::Transcript(_) => lines,
            Self::AgySteps(_) => translated(&lines, brain::step_line),
            Self::AgyBoard(_) => translated(&lines, brain::post_line),
        };
        Ok((lines, offset))
    }
}

/// Lines another program wrote, as transcript lines; a line giving none is left out.
fn translated(lines: &[String], line: fn(&str) -> Option<TranscriptLine>) -> Vec<String> {
    lines
        .iter()
        .filter_map(|written| line(written))
        .filter_map(|transcribed| {
            serde_json::to_string(&transcribed)
                .inspect_err(|error| tracing::warn!(%error, "a transcript line not written"))
                .ok()
        })
        .collect()
}

/// Where a transcript is: an agy session's subagents and board are in agy's own records, by the
/// conversation the task names.
pub fn resolve(place: &Place) -> Source {
    match place {
        Place::Path(path) => Source::Transcript(path.clone()),
        Place::Session { cwd, id } => Source::Transcript(sessions::locate(cwd, id)),
        Place::Subagent { cwd, id, task } => match (AgyTask::of(task), agy::brain()) {
            (Some(AgyTask::Conversation(conversation)), Some(brain)) => Source::AgySteps(
                agy::generated(&brain, &conversation).join("logs/transcript.jsonl"),
            ),
            (Some(AgyTask::Board(conversation)), Some(brain)) => {
                Source::AgyBoard(agy::generated(&brain, &conversation).join("swarm/board.jsonl"))
            }
            (Some(_), None) | (None, _) => Source::Transcript(sessions::transcript_of(
                &sessions::written_transcript(cwd, id),
                task,
            )),
        },
    }
}

/// Reads what a window asked for, off the runtime's thread, and tells it.
pub async fn answer(request: ActId, read: Read, outbox: Outbox) {
    let pieces = outbox.clone();
    let answered = tokio::task::spawn_blocking(move || read_for(request, read, &pieces)).await;
    let answer = match answered {
        Ok(answer) => answer,
        Err(error) => {
            tracing::error!(%error, "reading for a window failed");
            return;
        }
    };
    if outbox
        .send(FromDaemon::Answer { request, answer })
        .await
        .is_err()
    {
        tracing::debug!("a window gone before its answer");
    }
}

/// The most sessions read per project for the start page.
const OVERVIEW_SESSIONS: usize = 5;

/// What `read` comes to; a transcript's lines go to `pieces` as they are read, before.
fn read_for(request: ActId, read: Read, pieces: &Outbox) -> Answer {
    match read {
        Read::Live => Answer::Live(sessions::live()),
        Read::Projects => Answer::Projects(sessions::projects()),
        Read::Overview => {
            let overviews: Vec<ProjectOverview> = sessions::projects()
                .into_iter()
                .map(|project| {
                    let sessions = sessions::list(&project.path, OVERVIEW_SESSIONS, Vec::new());
                    ProjectOverview {
                        path: project.path,
                        modified: project.modified,
                        sessions,
                    }
                })
                .collect();
            Answer::Overview(chunk_overview(request, overviews, pieces))
        }
        Read::Sessions { cwd, limit, known } => {
            Answer::Sessions(sessions::list(&cwd, limit.min(MOST_SESSIONS), known))
        }
        Read::Transcript { place, from } => {
            let source = resolve(&place);
            // Only a transcript is read, by no link (`sessions::open_transcript`): a place a
            // window names could otherwise lead the daemon to any file of the user's.
            let read = match source.read_from(from) {
                Ok((lines, offset)) => {
                    send_lines(request, &lines, pieces);
                    Ok(offset)
                }
                Err(error) => Err(ipc::Unread::from(error)),
            };
            Answer::Transcript {
                path: source.path().to_owned(),
                read,
            }
        }
        Read::WorktreeOf { cwd, id } => Answer::Worktree(
            sessions::find_transcript(&cwd, &id)
                .and_then(|transcript| sessions::worktree_of(&transcript)),
        ),
        Read::LinkedWorktree { cwd } => Answer::Worktree(sessions::linked_worktree(&cwd)),
        Read::TypedFolder(typed) => Answer::Folder(
            Folder::typed(&typed, dirs::home_dir().as_deref())
                .map(Folder::into_path)
                .map_err(|problem| problem.to_string()),
        ),
        Read::Folder(path) => Answer::Folder(
            Folder::at(path)
                .map(Folder::into_path)
                .map_err(|problem| problem.to_string()),
        ),
        Read::Changes { session, scope } => Answer::Changes(
            kept_changes(&session)
                .and_then(|store| store.changes(scope).map_err(|error| unkept(&error))),
        ),
        Read::Diff {
            session,
            scope,
            root,
            path,
        } => Answer::Diff(kept_changes(&session).and_then(|store| {
            store
                .hunks(scope, &root, &path)
                .map(crate::changes::Diff::fitted)
                .map_err(|error| unkept(&error))
        })),
    }
}

/// A session's changes as the keeper wrote them.
fn kept_changes(
    session: &crate::external::wire::SessionId,
) -> Result<snapshots::Store, ipc::Unkept> {
    let home =
        snapshots::home().ok_or_else(|| ipc::Unkept("there is no state folder".to_owned()))?;
    snapshots::Store::open(&home, session).map_err(|error| unkept(&error))
}

/// A store's error in words, with what caused it, for the window.
fn unkept(error: &snapshots::Error) -> ipc::Unkept {
    let mut words = error.to_string();
    let mut source = std::error::Error::source(error);
    while let Some(cause) = source {
        words.push_str(": ");
        words.push_str(&cause.to_string());
        source = cause.source();
    }
    ipc::Unkept(words)
}

/// A transcript's lines to the window, as text in pieces that fit messages (a line may be longer
/// than one); a window gone isn't sent the rest.
fn send_lines(request: ActId, lines: &[String], pieces: &mpsc::Sender<FromDaemon>) {
    let mut text = String::with_capacity(lines.iter().map(|line| line.len() + 1).sum());
    for line in lines {
        text.push_str(line);
        text.push('\n');
    }
    for piece in ipc::chunks(&text).filter(|piece| !piece.is_empty()) {
        let lines = FromDaemon::Lines {
            request,
            text: piece.to_owned(),
        };
        if pieces.blocking_send(lines).is_err() {
            tracing::debug!("a window gone as its transcript was read");
            return;
        }
    }
}

/// Splits a large overview list across `FromDaemon::OverviewChunk` messages so each frame stays
/// within `ipc::MAX_MESSAGE`, returning the trailing projects for `Answer::Overview`.
fn chunk_overview(
    request: ActId,
    overviews: Vec<ProjectOverview>,
    pieces: &Outbox,
) -> Vec<ProjectOverview> {
    let mut batch: Vec<ProjectOverview> = Vec::new();
    for project in overviews {
        batch.push(project);
        let fits = ipc::frame(&FromDaemon::OverviewChunk {
            request,
            projects: batch.clone(),
        })
        .is_ok();
        if !fits && batch.len() > 1 {
            let overflowed = batch.pop();
            let chunk = FromDaemon::OverviewChunk {
                request,
                projects: std::mem::take(&mut batch),
            };
            if pieces.blocking_send(chunk).is_err() {
                tracing::debug!("a window gone as its overview was read");
                return Vec::new();
            }
            if let Some(overflowed) = overflowed {
                batch.push(overflowed);
            }
        }
    }
    batch
}

/// A window's change to the settings, and where to say if it wasn't kept.
#[derive(Debug)]
pub struct SettingsChange {
    pub change: settings::Change,
    pub outbox: Outbox,
}

/// The settings file's one owner: applies each window's change in turn to what the file holds,
/// off the runtime's thread, and tells the window that made a change that couldn't be kept.
pub async fn keep_settings(mut changes: mpsc::Receiver<SettingsChange>) {
    while let Some(SettingsChange { change, outbox }) = changes.recv().await {
        let changed = tokio::task::spawn_blocking(move || settings::change(change)).await;
        let why = match changed {
            Ok(Ok(_)) => continue,
            Ok(Err(error)) => {
                tracing::warn!(%error, "couldn't keep the settings");
                error.to_string()
            }
            Err(error) => {
                tracing::error!(%error, "keeping the settings failed");
                error.to_string()
            }
        };
        if outbox.send(FromDaemon::SettingsNotKept(why)).await.is_err() {
            tracing::debug!("the window that changed the settings is gone");
        }
    }
}

/// What a window's watches are to do.
#[derive(Debug, PartialEq, Eq)]
pub enum Order {
    Watch(Watched, Target),
    Unwatch(Watched),
}

/// A window's watch: a folder, watched by inotify; for a transcript, the folder it is in. A
/// transcript is watched by its folder, not itself: a file watched follows the file, so one
/// written anew by a rename over it would go unseen, and its folder sees that too. Before the folder is there, the nearest one above it is watched (at most
/// [`MOST_ABOVE`] up), until the next one down comes.
#[derive(Debug)]
struct Watch {
    /// The folder it watches once that is there.
    goal: PathBuf,
    /// The transcript in it; none for the records' folder, any change in which is told.
    file: Option<PathBuf>,
    /// Where it is watched now, and its descriptor: the goal, or a folder above it.
    at: Option<(PathBuf, i32)>,
    /// It couldn't be watched where it is to be, and is tried again every [`RETRY`].
    misplaced: bool,
}

impl Watch {
    fn new(goal: PathBuf, file: Option<PathBuf>) -> Self {
        Self {
            goal,
            file,
            at: None,
            misplaced: true,
        }
    }

    fn descriptor(&self) -> Option<i32> {
        self.at.as_ref().map(|&(_, descriptor)| descriptor)
    }

    /// Whether what inotify noticed in the folder watched (in `name` there, or in the folder
    /// itself) is about what it is for: its file, anything in the records' folder, or a folder
    /// on the way to its goal.
    fn concerns(&self, name: Option<&OsStr>) -> bool {
        let Some((folder, _)) = &self.at else {
            return false;
        };
        let Some(name) = name else {
            return true;
        };
        let named = folder.join(name);
        if *folder == self.goal {
            self.file.as_ref().is_none_or(|file| *file == named)
        } else {
            self.goal.starts_with(&named)
        }
    }

    /// Whether it watches a folder above its goal, which a folder nearer may have come to.
    fn above_its_goal(&self) -> bool {
        self.at
            .as_ref()
            .is_some_and(|(folder, _)| *folder != self.goal)
    }

    /// Watches the nearest folder there is to its goal, down as far as folders are there (one
    /// made as it is placed too); the descriptors it let go of. Whether it moved, in `moved`.
    fn place(&mut self, inotify: &OwnedFd, moved: &mut bool) -> Vec<i32> {
        let mut left = Vec::new();
        self.misplaced = false;
        // Each step down nearer: as many as there are folders it may be above its goal.
        for _ in 0..=MOST_ABOVE {
            let Some(nearest) = nearest_folder(&self.goal) else {
                left.extend(self.at.take().map(|(_, descriptor)| descriptor));
                self.misplaced = true;
                break;
            };
            if self
                .at
                .as_ref()
                .is_some_and(|(folder, _)| *folder == nearest)
            {
                break;
            }
            match add(inotify, &nearest, FOLDER_FLAGS) {
                Ok(descriptor) => {
                    if let Some((_, old)) = self.at.replace((nearest, descriptor))
                        && old != descriptor
                    {
                        left.push(old);
                    }
                    *moved = true;
                }
                Err(error) => {
                    tracing::debug!(%error, folder = ?nearest, "a watch stays where it was");
                    self.misplaced = true;
                    break;
                }
            }
        }
        left
    }
}

/// What inotify noticed, once read off its queue.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct Noticed {
    descriptor: i32,
    /// In the folder watched; none for the folder itself.
    name: Option<OsString>,
    /// The folder watched itself went.
    left: Option<Left>,
}

/// How a folder watched went.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum Left {
    /// Removed, or off its file system: inotify let go of its watch.
    Gone,
    /// Moved: its watch goes with it, and is to be let go of.
    Moved,
}

/// A window's watches (an actor): one inotify instance, its watches named as the window named
/// them, until the window's orders end. What inotify notices is read on a thread of its own,
/// which ends as the actor does (the pipe it waits on beside inotify closes).
pub async fn watch(mut orders: mpsc::Receiver<Order>, outbox: Outbox) {
    // Not blocking, so the thread reads all there is each time `poll` wakes it.
    let started = inotify::init(CreateFlags::CLOEXEC | CreateFlags::NONBLOCK).and_then(|inotify| {
        let reading = rustix::io::fcntl_dupfd_cloexec(&inotify, 0)?;
        let (waits, _held) = rustix::pipe::pipe_with(rustix::pipe::PipeFlags::CLOEXEC)?;
        Ok((inotify, reading, waits, _held))
    });
    // `_held` is the pipe's other end, closed as this returns: the thread wakes, and ends.
    let (inotify, reading, ending, _held) = match started {
        Ok(started) => started,
        Err(error) => {
            tracing::warn!(%error, "a window's watches go unnoticed");
            while orders.recv().await.is_some() {}
            return;
        }
    };
    let (noticing, mut noticed) = mpsc::channel(QUEUE);
    std::thread::spawn(move || read(&reading, &ending, &noticing));
    let mut watches: HashMap<Watched, Watch> = HashMap::new();
    let mut retrying = tokio::time::interval(RETRY);
    retrying.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        let misplaced = watches.values().any(|watch| watch.misplaced);
        let changed: HashSet<Watched> = tokio::select! {
            order = orders.recv() => match order {
                Some(Order::Watch(watched, target)) => {
                    let mut watch = start(&target);
                    let mut left = watch.place(&inotify, &mut false);
                    left.extend(watches.insert(watched, watch).and_then(|old| old.descriptor()));
                    forget_unused(&inotify, &watches, left);
                    HashSet::new()
                }
                Some(Order::Unwatch(watched)) => {
                    let left = watches.remove(&watched).and_then(|old| old.descriptor());
                    forget_unused(&inotify, &watches, left);
                    HashSet::new()
                }
                None => return,
            },
            batch = noticed.recv() => {
                let Some(batch) = batch else {
                    tracing::warn!("stopped noticing changes for a window");
                    return;
                };
                if batch.iter().any(|noticed| noticed.descriptor == OVERFLOW) {
                    overflowed(&inotify, &mut watches)
                } else {
                    batch
                        .iter()
                        .flat_map(|noticed| touched(&inotify, &mut watches, noticed))
                        .collect()
                }
            },
            _ = retrying.tick(), if misplaced => retried(&inotify, &mut watches),
        };
        for watched in changed {
            if outbox.send(FromDaemon::Changed(watched)).await.is_err() {
                return;
            }
        }
    }
}

/// Batches of watches noticed and waiting for the actor.
const QUEUE: usize = 64;
/// The watch descriptor inotify tells its queue's overflow on (`IN_Q_OVERFLOW`).
const OVERFLOW: i32 = -1;
/// How far above its goal a watch may be: no further, lest it watch the home folder, or the
/// root, whose many writes would each wake it.
const MOST_ABOVE: usize = 2;
/// How often a watch that couldn't be placed is tried again.
const RETRY: Duration = Duration::from_secs(5);

/// Sends on what inotify notices, all it has to tell at a time, until the actor ends: its end
/// of `ending` closes.
fn read(inotify: &OwnedFd, ending: &OwnedFd, noticing: &mpsc::Sender<HashSet<Noticed>>) {
    use rustix::event::{PollFd, PollFlags, poll};
    let mut buffer = [MaybeUninit::<u8>::uninit(); 4096];
    loop {
        let mut ready = [
            PollFd::new(inotify, PollFlags::IN),
            PollFd::new(ending, PollFlags::IN),
        ];
        match poll(&mut ready, None) {
            Ok(_) | Err(Errno::INTR) => {}
            Err(error) => {
                tracing::warn!(%error, "stopped noticing changes for a window");
                return;
            }
        }
        let [noticed, ended] = ready;
        if !ended.revents().is_empty() {
            return;
        }
        if noticed.revents().is_empty() {
            continue;
        }
        let mut reader = inotify::Reader::new(inotify, &mut buffer);
        let mut batch = HashSet::new();
        loop {
            match reader.next() {
                Ok(event) => {
                    let flags = event.events();
                    let left = if flags.contains(inotify::ReadFlags::IGNORED) {
                        Some(Left::Gone)
                    } else if flags.contains(inotify::ReadFlags::MOVE_SELF) {
                        Some(Left::Moved)
                    } else {
                        None
                    };
                    batch.insert(Noticed {
                        descriptor: event.wd(),
                        name: event
                            .file_name()
                            .map(|name| OsStr::from_bytes(name.to_bytes()).to_owned()),
                        left,
                    });
                }
                Err(Errno::INTR) => {}
                // All read, for now.
                Err(Errno::WOULDBLOCK) => {
                    if !batch.is_empty() && noticing.blocking_send(batch).is_err() {
                        return;
                    }
                    break;
                }
                Err(error) => {
                    tracing::warn!(%error, "stopped noticing changes for a window");
                    return;
                }
            }
        }
    }
}

/// The window's watches `noticed` is about. One whose folder went is placed again; one above
/// its goal moves down as folders on the way come.
fn touched(
    inotify: &OwnedFd,
    watches: &mut HashMap<Watched, Watch>,
    noticed: &Noticed,
) -> Vec<Watched> {
    let named: Vec<Watched> = watches
        .iter()
        .filter(|(_, watch)| {
            watch.descriptor() == Some(noticed.descriptor)
                && (noticed.left.is_some() || watch.concerns(noticed.name.as_deref()))
        })
        .map(|(watched, _)| *watched)
        .collect();
    let mut left = Vec::new();
    if noticed.left == Some(Left::Moved) {
        left.push(noticed.descriptor);
    }
    for watched in &named {
        let Some(watch) = watches.get_mut(watched) else {
            continue;
        };
        if noticed.left.is_some() {
            // Gone, or watching a folder elsewhere now: placed afresh.
            watch.at = None;
        } else if !watch.above_its_goal() {
            continue;
        }
        left.extend(watch.place(inotify, &mut false));
    }
    forget_unused(inotify, watches, left);
    named
}

/// inotify's queue overflowed: what was lost may have been any watch's, a folder come or gone
/// among it. Each is placed afresh, and told.
fn overflowed(inotify: &OwnedFd, watches: &mut HashMap<Watched, Watch>) -> HashSet<Watched> {
    let mut left = Vec::new();
    for watch in watches.values_mut() {
        left.extend(watch.at.take().map(|(_, descriptor)| descriptor));
        left.extend(watch.place(inotify, &mut false));
    }
    forget_unused(inotify, watches, left);
    watches.keys().copied().collect()
}

/// Tries again to place the watches that couldn't be; those that moved are told.
fn retried(inotify: &OwnedFd, watches: &mut HashMap<Watched, Watch>) -> HashSet<Watched> {
    let mut changed = HashSet::new();
    let mut left = Vec::new();
    for (watched, watch) in watches.iter_mut().filter(|(_, watch)| watch.misplaced) {
        let mut moved = false;
        left.extend(watch.place(inotify, &mut moved));
        if moved {
            changed.insert(*watched);
        }
    }
    forget_unused(inotify, watches, left);
    changed
}

/// What a folder watched is watched for: anything written, made, removed or moved in it, and
/// its own moving away.
const FOLDER_FLAGS: WatchFlags = WatchFlags::CREATE
    .union(WatchFlags::MODIFY)
    .union(WatchFlags::DELETE)
    .union(WatchFlags::CLOSE_WRITE)
    .union(WatchFlags::MOVED_TO)
    .union(WatchFlags::MOVED_FROM)
    .union(WatchFlags::MOVE_SELF);

/// The nearest folder to `goal` that is there: itself, or one at most [`MOST_ABOVE`] above it.
fn nearest_folder(goal: &Path) -> Option<PathBuf> {
    goal.ancestors()
        .take(MOST_ABOVE + 1)
        .find(|folder| folder.is_dir())
        .map(Path::to_path_buf)
}

/// What `target` is watched for, not placed yet.
fn start(target: &Target) -> Watch {
    match target {
        Target::Records => Watch::new(sessions::config_dir().join("sessions"), None),
        Target::Transcript(place) => {
            let file = resolve(place).path().to_owned();
            let goal = file.parent().map(Path::to_path_buf).unwrap_or_default();
            Watch::new(goal, Some(file))
        }
    }
}

fn add(inotify: &OwnedFd, path: &Path, flags: WatchFlags) -> Result<i32, Errno> {
    // Added to what is watched there already for another of the window's watches.
    inotify::add_watch(inotify, path, flags | WatchFlags::MASK_ADD)
}

/// Lets go of each of `descriptors` that none of the window's watches is on.
fn forget_unused(
    inotify: &OwnedFd,
    watches: &HashMap<Watched, Watch>,
    descriptors: impl IntoIterator<Item = i32>,
) {
    let mut forgotten = HashSet::new();
    for descriptor in descriptors {
        if forgotten.insert(descriptor)
            && watches
                .values()
                .all(|watch| watch.descriptor() != Some(descriptor))
            && let Err(error) = inotify::remove_watch(inotify, descriptor)
        {
            tracing::debug!(%error, "a watch that was gone already");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Found with a Haiku turn: Claude Code ignored a folder's allow rules, as only edualc had
    /// trusted it. Recording its trust adds that folder's record, with Claude Code's defaults,
    /// and changes nothing else in the file; a folder whose record says so already leaves the
    /// file as it is, while one inside a trusted folder gets its own, as Claude Code reads only
    /// the folder's own record.
    #[test]
    fn trust_is_recorded_for_claude_code_keeping_the_rest() {
        let dir = std::env::temp_dir().join(format!("edualc-claude-json-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("a scratch folder");
        let config = dir.join(".claude.json");
        let before = serde_json::json!({
            "numStartups": 7,
            "mcpServers": {"tool": {"command": "x", "env": {"TOKEN": "secret"}}},
            "projects": {
                "/code/other": {"allowedTools": [], "lastCost": 0.5},
                "/code/trusted": {"hasTrustDialogAccepted": true}
            }
        });
        std::fs::write(&config, before.to_string()).expect("a config");

        assert!(matches!(
            trust_in(&config, Path::new("/code/app"), &[]),
            Ok(ClaudeTrust::Recorded)
        ));
        let after: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&config).expect("read")).expect("json");
        let project = |json: &serde_json::Value, folder: &str| {
            json.get("projects")
                .and_then(|projects| projects.get(folder))
                .cloned()
        };
        let app = project(&after, "/code/app").expect("the record");
        assert_eq!(
            app.get("hasTrustDialogAccepted"),
            Some(&serde_json::Value::Bool(true))
        );
        assert_eq!(app.get("allowedTools"), Some(&serde_json::json!([])));
        assert_eq!(
            project(&after, "/code/other"),
            project(&before, "/code/other")
        );
        assert_eq!(after.get("mcpServers"), before.get("mcpServers"));
        assert_eq!(after.get("numStartups"), before.get("numStartups"));

        let written = std::fs::read(&config).expect("read");
        assert!(matches!(
            trust_in(&config, Path::new("/code/app"), &[]),
            Ok(ClaudeTrust::AlreadyTrusted)
        ));
        assert_eq!(
            std::fs::read(&config).expect("read"),
            written,
            "not written again"
        );
        assert!(matches!(
            trust_in(&config, Path::new("/code/trusted/sub"), &[]),
            Ok(ClaudeTrust::Recorded)
        ));

        std::fs::write(&config, "{ not json").expect("broken");
        assert!(matches!(
            trust_in(&config, Path::new("/code/app"), &[]),
            Err(ClaudeTrustError::Parse { .. })
        ));
        assert_eq!(
            std::fs::read_to_string(&config).expect("read"),
            "{ not json"
        );
        std::fs::remove_dir_all(&dir).expect("the scratch folder goes");
    }

    /// The file is written under Claude Code's own lock: while Claude Code holds it, nothing is
    /// written; one left behind longer than Claude Code waits is taken over; and the lock is
    /// let go once the record is in.
    #[test]
    fn trust_is_recorded_under_claude_codes_lock() {
        let dir = std::env::temp_dir().join(format!("edualc-claude-lock-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("a scratch folder");
        let config = dir.join(".claude.json");
        std::fs::write(&config, "{}").expect("a config");
        let lock = dir.join(".claude.json.lock");

        std::fs::create_dir(&lock).expect("Claude Code's lock");
        assert!(matches!(
            trust_in(&config, Path::new("/code/app"), &[]),
            Err(ClaudeTrustError::Busy(_))
        ));
        assert_eq!(std::fs::read_to_string(&config).expect("read"), "{}");
        assert!(lock.exists(), "Claude Code's lock is left as it is");

        let long_ago = std::time::SystemTime::now() - CLAUDE_LOCK_STALE * 2;
        std::fs::File::open(&lock)
            .and_then(|held| held.set_modified(long_ago))
            .expect("the lock made old");
        assert!(matches!(
            trust_in(&config, Path::new("/code/app"), &[]),
            Ok(ClaudeTrust::Recorded)
        ));
        assert!(!lock.exists(), "the lock is let go");
        std::fs::remove_dir_all(&dir).expect("the scratch folder goes");
    }

    /// A folder is known to Claude Code by the real path of the git repository it is in, or by
    /// its own outside one.
    #[test]
    fn a_folder_is_keyed_as_claude_code_keys_it() {
        let dir = std::env::temp_dir().join(format!("edualc-claude-key-{}", std::process::id()));
        let repository = dir.join("repo");
        let inside = repository.join("crates/app");
        let outside = dir.join("loose");
        for folder in [&inside, &repository.join(".git"), &outside] {
            std::fs::create_dir_all(folder).expect("a scratch folder");
        }
        let real = |path: &Path| std::fs::canonicalize(path).expect("real");
        assert_eq!(claude_project_key(&inside), real(&repository));
        assert_eq!(claude_project_key(&outside), real(&outside));
        std::fs::remove_dir_all(&dir).expect("the scratch folder goes");
    }

    /// The next watch told of as changed, if one is within a while.
    async fn changed(outbox: &mut mpsc::Receiver<FromDaemon>) -> Option<Watched> {
        match tokio::time::timeout(Duration::from_secs(10), outbox.recv()).await {
            Ok(Some(FromDaemon::Changed(watched))) => Some(watched),
            Ok(_) | Err(_) => None,
        }
    }

    /// Lets what is still being told of the last change be told.
    async fn settle(outbox: &mut mpsc::Receiver<FromDaemon>) {
        while let Ok(Some(_)) =
            tokio::time::timeout(Duration::from_millis(200), outbox.recv()).await
        {}
    }

    /// Nothing is told within a while.
    async fn quiet(outbox: &mut mpsc::Receiver<FromDaemon>) -> bool {
        tokio::time::timeout(Duration::from_millis(300), outbox.recv())
            .await
            .is_err()
    }

    /// A scratch folder for a test, and a window's watches of a transcript in it.
    struct Scene {
        dir: PathBuf,
        orders: mpsc::Sender<Order>,
        told: mpsc::Receiver<FromDaemon>,
        actor: tokio::task::JoinHandle<()>,
        watched: Watched,
    }

    impl Scene {
        async fn watching(name: &str, transcript: &str) -> Self {
            let dir = std::env::temp_dir().join(format!("edualc-{name}-{}", std::process::id()));
            std::fs::create_dir_all(&dir).expect("a scratch folder");
            let (orders, received) = mpsc::channel(8);
            let (outbox, told) = mpsc::channel(8);
            let actor = tokio::spawn(watch(received, outbox));
            let watched = Watched::new();
            let target = Target::Transcript(Place::Path(dir.join(transcript)));
            orders
                .send(Order::Watch(watched, target))
                .await
                .expect("ordered");
            // The order is taken before the test goes on.
            tokio::time::sleep(Duration::from_millis(50)).await;
            Self {
                dir,
                orders,
                told,
                actor,
                watched,
            }
        }

        async fn end(self) {
            drop(self.orders);
            self.actor.await.expect("the actor ended");
            std::fs::remove_dir_all(&self.dir).expect("the scratch folder goes");
        }
    }

    /// A transcript goes to the window as text in pieces that each fit a message, a line longer
    /// than one among them; put together, the pieces are the lines.
    #[test]
    fn a_transcripts_lines_come_in_pieces_that_fit_messages() {
        let lines = vec![
            "short".to_owned(),
            "é".repeat(3 * ipc::MAX_CHUNK),
            "also short".to_owned(),
        ];
        let (pieces, mut received) = mpsc::channel(64);
        send_lines(ActId::default(), &lines, &pieces);
        drop(pieces);
        let mut text = String::new();
        while let Ok(FromDaemon::Lines { text: piece, .. }) = received.try_recv() {
            let sent = crate::ipc::frame(&FromDaemon::Lines {
                request: ActId::default(),
                text: piece.clone(),
            });
            assert!(sent.is_ok(), "a piece of {} bytes didn't fit", piece.len());
            text.push_str(&piece);
        }
        let mut splitter = ipc::LineSplitter::default();
        assert_eq!(splitter.push(text.as_bytes()), lines);
    }

    /// What the daemon reads in one message, at the most it reads, fits one.
    #[test]
    fn a_thousand_sessions_fit_a_message() {
        let session = |at: usize| sessions::SessionInfo {
            backend: crate::claude::Backend::Claude,
            id: crate::external::wire::SessionId::from(uuid::Uuid::from_u128(at as u128)),
            title: "x".repeat(300),
            modified: std::time::SystemTime::now(),
            worktree: None,
            last_said: None,
        };
        let answer = FromDaemon::Answer {
            request: ActId::default(),
            answer: Answer::Sessions((0..MOST_SESSIONS).map(session).collect()),
        };
        assert!(crate::ipc::frame(&answer).is_ok());
    }

    #[test]
    fn large_overviews_are_chunked_so_each_message_fits() {
        let now = std::time::SystemTime::now();
        let make_project = |idx: usize| ProjectOverview {
            path: PathBuf::from(format!("/code/project-{idx}-{}", "x".repeat(400))),
            modified: now,
            sessions: (0..OVERVIEW_SESSIONS)
                .map(|sub| sessions::SessionInfo {
                    backend: crate::claude::Backend::Claude,
                    id: crate::external::wire::SessionId::from(uuid::Uuid::from_u128(
                        u128::try_from(idx * 10 + sub).unwrap_or(0),
                    )),
                    title: "title ".repeat(200),
                    modified: now,
                    worktree: None,
                    last_said: Some("sentence ".repeat(200)),
                })
                .collect(),
        };
        let total = 150;
        let all: Vec<ProjectOverview> = (0..total).map(make_project).collect();
        let (pieces, mut received) = mpsc::channel(64);
        let tail = chunk_overview(ActId::default(), all, &pieces);
        drop(pieces);
        let mut collected = Vec::new();
        while let Ok(FromDaemon::OverviewChunk { projects, .. }) = received.try_recv() {
            let framed = FromDaemon::OverviewChunk {
                request: ActId::default(),
                projects: projects.clone(),
            };
            assert!(ipc::frame(&framed).is_ok());
            collected.extend(projects);
        }
        assert!(!collected.is_empty(), "expected chunking across messages");
        let answer = FromDaemon::Answer {
            request: ActId::default(),
            answer: Answer::Overview(tail.clone()),
        };
        assert!(ipc::frame(&answer).is_ok());
        collected.extend(tail);
        assert_eq!(collected.len(), total);
    }

    /// A transcript is watched through its folder: its being made and written to are told,
    /// and nothing is after it is unwatched.
    #[tokio::test]
    async fn a_transcript_is_watched_before_and_after_it_is_written() {
        let mut scene = Scene::watching("watch", "session.jsonl").await;
        let path = scene.dir.join("session.jsonl");
        std::fs::write(&path, "{}\n").expect("written");
        assert_eq!(changed(&mut scene.told).await, Some(scene.watched), "made");
        settle(&mut scene.told).await;
        std::fs::write(&path, "{}\n{}\n").expect("written again");
        assert_eq!(
            changed(&mut scene.told).await,
            Some(scene.watched),
            "written to"
        );
        settle(&mut scene.told).await;
        let watched = scene.watched;
        scene
            .orders
            .send(Order::Unwatch(watched))
            .await
            .expect("ordered");
        tokio::time::sleep(Duration::from_millis(50)).await;
        std::fs::write(&path, "{}\n{}\n{}\n").expect("written after");
        assert!(quiet(&mut scene.told).await, "unwatched");
        scene.end().await;
    }

    /// A transcript whose folder isn't there yet is watched from above it, and moves down as the
    /// folder comes; written anew by a rename over it, it is told; another file in its folder
    /// isn't.
    #[tokio::test]
    async fn a_transcript_is_followed_through_its_folder() {
        let mut scene = Scene::watching("follow", "project/session.jsonl").await;
        let project = scene.dir.join("project");
        let path = project.join("session.jsonl");
        std::fs::create_dir(&project).expect("its folder made");
        assert_eq!(
            changed(&mut scene.told).await,
            Some(scene.watched),
            "its folder came"
        );
        settle(&mut scene.told).await;
        std::fs::write(&path, "{}\n").expect("written");
        assert_eq!(
            changed(&mut scene.told).await,
            Some(scene.watched),
            "made in its folder"
        );
        settle(&mut scene.told).await;
        std::fs::write(project.join("another.jsonl"), "{}\n").expect("another written");
        assert!(quiet(&mut scene.told).await, "another file isn't it");
        let staged = scene.dir.join("staged.jsonl");
        std::fs::write(&staged, "{}\n{}\n").expect("staged");
        std::fs::rename(&staged, &path).expect("renamed over it");
        assert_eq!(
            changed(&mut scene.told).await,
            Some(scene.watched),
            "renamed over"
        );
        settle(&mut scene.told).await;
        std::fs::write(&path, "{}\n{}\n{}\n").expect("written to after");
        assert_eq!(
            changed(&mut scene.told).await,
            Some(scene.watched),
            "written to after"
        );
        scene.end().await;
    }

    /// Folders made at once, as `mkdir -p` makes them, are followed all the way down.
    #[tokio::test]
    async fn a_watch_moves_down_through_folders_made_at_once() {
        let mut scene = Scene::watching("deep", "p/s/subagents/agent.jsonl").await;
        let folder = scene.dir.join("p/s/subagents");
        std::fs::create_dir_all(&folder).expect("its folders made");
        assert_eq!(
            changed(&mut scene.told).await,
            Some(scene.watched),
            "its folders came"
        );
        settle(&mut scene.told).await;
        std::fs::write(folder.join("agent.jsonl"), "{}\n").expect("written");
        assert_eq!(
            changed(&mut scene.told).await,
            Some(scene.watched),
            "made in its folder"
        );
        scene.end().await;
    }

    /// A folder watched that is moved away isn't followed: the one made in its place is.
    #[tokio::test]
    async fn a_folder_moved_away_is_let_go_of() {
        let mut scene = Scene::watching("moved", "project/session.jsonl").await;
        let project = scene.dir.join("project");
        std::fs::create_dir(&project).expect("its folder made");
        assert_eq!(
            changed(&mut scene.told).await,
            Some(scene.watched),
            "its folder came"
        );
        settle(&mut scene.told).await;
        let old = scene.dir.join("project.old");
        std::fs::rename(&project, &old).expect("moved away");
        assert_eq!(
            changed(&mut scene.told).await,
            Some(scene.watched),
            "moved away"
        );
        settle(&mut scene.told).await;
        std::fs::write(old.join("session.jsonl"), "{}\n").expect("written where it went");
        assert!(
            quiet(&mut scene.told).await,
            "the folder moved away isn't it"
        );
        std::fs::create_dir(&project).expect("made again");
        assert_eq!(
            changed(&mut scene.told).await,
            Some(scene.watched),
            "made again"
        );
        settle(&mut scene.told).await;
        std::fs::write(project.join("session.jsonl"), "{}\n").expect("written");
        assert_eq!(
            changed(&mut scene.told).await,
            Some(scene.watched),
            "written there"
        );
        scene.end().await;
    }

    /// A transcript with no folder near it to watch from is tried again until there is one.
    #[tokio::test]
    async fn a_watch_with_nowhere_to_be_is_tried_again() {
        let mut scene = Scene::watching("retry", "a/b/c/d/session.jsonl").await;
        std::fs::create_dir_all(scene.dir.join("a/b/c/d")).expect("its folders made");
        assert_eq!(
            changed(&mut scene.told).await,
            Some(scene.watched),
            "placed at last"
        );
        settle(&mut scene.told).await;
        std::fs::write(scene.dir.join("a/b/c/d/session.jsonl"), "{}\n").expect("written");
        assert_eq!(
            changed(&mut scene.told).await,
            Some(scene.watched),
            "made in its folder"
        );
        scene.end().await;
    }

    /// What is noticed in a folder is about a transcript if it names the transcript, or a folder
    /// on the way to it; in the records' folder, anything is.
    #[test]
    fn what_is_noticed_concerns_a_transcript_on_its_way() {
        let file = PathBuf::from("/h/projects/p/s.jsonl");
        let mut watch = Watch::new(PathBuf::from("/h/projects/p"), Some(file));
        assert!(!watch.concerns(None), "nowhere yet");
        watch.at = Some((PathBuf::from("/h/projects"), 1));
        assert!(watch.concerns(Some(OsStr::new("p"))));
        assert!(!watch.concerns(Some(OsStr::new("q"))));
        assert!(!watch.concerns(Some(OsStr::new("p2"))));
        assert!(watch.concerns(None));
        assert!(watch.above_its_goal());
        watch.at = Some((PathBuf::from("/h/projects/p"), 2));
        assert!(watch.concerns(Some(OsStr::new("s.jsonl"))));
        assert!(!watch.concerns(Some(OsStr::new("t.jsonl"))));
        assert!(!watch.above_its_goal());
        let mut records = Watch::new(PathBuf::from("/h/sessions"), None);
        records.at = Some((PathBuf::from("/h/sessions"), 3));
        assert!(records.concerns(Some(OsStr::new("anything"))));
        records.at = Some((PathBuf::from("/h"), 4));
        assert!(records.concerns(Some(OsStr::new("sessions"))));
        assert!(!records.concerns(Some(OsStr::new("projects"))));
    }
}
