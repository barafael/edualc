//! What a session changes in its folder. Before and after each turn the folder is looked at
//! (its files' sizes and times, and their contents' SHA-256, for those that changed), and each
//! file's contents are kept once, by that hash, for the diffs. `.gitignore`d files, `.git`, other
//! repositories and worktrees inside the folder, binary files and large ones are left out.
//!
//! Kept in `~/.local/state/edualc/changes/`: `blobs/` holds the contents, shared by every
//! session; `sessions/<id>/` a session's snapshots, each as what changed since the one before
//! it in the same folder, and its `index.json`. The [`keep`] task alone writes them, each file
//! whole or not at all, and windows' reads read them as they are.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::os::unix::fs::DirBuilderExt as _;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use serde::{Deserialize, Serialize};
use sha2::Digest as _;
use tokio::sync::mpsc;

use crate::changes::{
    ChangeSet, ContentId, FileChange, Hunk, Kind, LARGEST_FILE, LeftOut, MOST_FILES, Review, Scope,
    Summary, Who,
};
use crate::external::wire::{self, ContentBlock, SessionId, ToolKind};
use crate::tools;

/// A session whose changes nobody looked at for this long is forgotten.
const KEPT_FOR: Duration = Duration::from_secs(30 * 24 * 60 * 60);
/// Contents no snapshot names are kept this long all the same.
const FRESH: Duration = Duration::from_secs(24 * 60 * 60);
/// Read of a file to tell whether it is binary: one with a NUL byte in it is.
const SNIFFED: usize = 8 * 1024;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("couldn't read or write {path}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("{path} doesn't read as a session's changes")]
    Format {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },
    #[error("snapshot {0} isn't kept")]
    NoSnapshot(usize),
}

fn io(path: &Path) -> impl FnOnce(std::io::Error) -> Error + '_ {
    move |source| Error::Io {
        path: path.to_owned(),
        source,
    }
}

/// Where the changes are kept.
pub fn home() -> Option<PathBuf> {
    dirs::state_dir().map(|state| state.join("edualc").join("changes"))
}

/// A file as a snapshot saw it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Seen {
    size: u64,
    modified_ns: u64,
    id: ContentId,
}

/// A folder's files, by their paths in it.
type Manifest = BTreeMap<PathBuf, Seen>;

/// A snapshot as it is kept: what differs from `base`, the one before it in the same folder, or
/// the whole folder if there is none.
#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Delta {
    base: Option<usize>,
    set: Manifest,
    gone: BTreeSet<PathBuf>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Taken {
    root: PathBuf,
    left_out: LeftOut,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Turn {
    before: usize,
    after: usize,
    /// The files Claude's editing tools named in it, as they named them.
    touched: BTreeSet<PathBuf>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Index {
    snapshots: Vec<Taken>,
    turns: Vec<Turn>,
    /// The turn under way, by its snapshot from before.
    open: Option<usize>,
    /// The latest snapshot when you last looked at the changes.
    looked: Option<usize>,
    /// Files you marked reviewed, by their whole paths, as they were then (none: deleted).
    reviewed: BTreeMap<PathBuf, Option<ContentId>>,
}

/// A session's changes as kept.
#[derive(Debug)]
pub struct Store {
    home: PathBuf,
    dir: PathBuf,
    index: Index,
}

impl Store {
    /// The changes of `session` kept under `home`; none yet if it has none.
    pub fn open(home: &Path, session: &SessionId) -> Result<Self, Error> {
        let dir = home.join("sessions").join(session.as_str());
        let index = read_json(&dir.join("index.json"))?.unwrap_or_default();
        Ok(Self {
            home: home.to_owned(),
            dir,
            index,
        })
    }

    /// A turn starts in `root`: the folder as it is before it.
    pub fn turn_started(&mut self, root: &Path) -> Result<(), Error> {
        let before = self.snapshot(root)?;
        self.index.open = Some(before);
        self.save_index()
    }

    /// The turn under way ended, Claude's editing tools having named `touched`: the folder as it
    /// is after it, and the turn's changes in short. None if no turn was under way.
    pub fn turn_ended(&mut self, touched: BTreeSet<PathBuf>) -> Result<Option<Summary>, Error> {
        let Some(before) = self.index.open.take() else {
            return Ok(None);
        };
        let root = self.taken(before)?.root.clone();
        let after = self.snapshot(&root)?;
        self.index.turns.push(Turn {
            before,
            after,
            touched,
        });
        self.save_index()?;
        let changes = self.changes(Scope::Turn)?;
        Ok(Some(Summary::from(&changes)))
    }

    /// You looked at the changes: "since you last looked" starts here.
    pub fn looked(&mut self) -> Result<(), Error> {
        self.index.looked = self.index.snapshots.len().checked_sub(1);
        self.save_index()
    }

    /// You marked the file at `path` reviewed as it holds `now`.
    pub fn mark_reviewed(&mut self, path: PathBuf, now: Option<ContentId>) -> Result<(), Error> {
        self.index.reviewed.insert(path, now);
        self.save_index()
    }

    /// The changes in `scope`.
    pub fn changes(&self, scope: Scope) -> Result<ChangeSet, Error> {
        let mut manifests = Manifests::default();
        let mut changes = ChangeSet::default();
        for (from, to) in self.pairs(scope) {
            let root = self.taken(to)?.root.clone();
            let left_out = self.taken(to)?.left_out;
            changes.left_out.large = changes.left_out.large.saturating_add(left_out.large);
            changes.left_out.binary = changes.left_out.binary.saturating_add(left_out.binary);
            changes.left_out.beyond_limit |= left_out.beyond_limit;
            let earlier = self.manifest(from, &mut manifests)?.clone();
            let later = self.manifest(to, &mut manifests)?.clone();
            for (path, before, after) in differing(&earlier, &later) {
                let old = self.text(before.as_ref())?;
                let new = self.text(after.as_ref())?;
                let (added, removed) = crate::changes::counts(&old, &new);
                let now = after.map(|seen| seen.id);
                let review = match self.index.reviewed.get(&root.join(&path)) {
                    None => Review::NotReviewed,
                    Some(marked) if *marked == now => Review::Reviewed,
                    Some(_) => Review::ChangedSinceReviewed,
                };
                let who = self.who(&root, &path, from, to, &mut manifests)?;
                changes.files.push(FileChange {
                    root: root.clone(),
                    kind: match (before, after) {
                        (None, _) => Kind::Added,
                        (Some(_), None) => Kind::Deleted,
                        (Some(_), Some(_)) => Kind::Changed,
                    },
                    path,
                    who,
                    added,
                    removed,
                    review,
                    now,
                });
            }
        }
        Ok(changes)
    }

    /// The hunks of the file at `path` in `root`, in `scope`.
    pub fn hunks(&self, scope: Scope, root: &Path, path: &Path) -> Result<Vec<Hunk>, Error> {
        let mut manifests = Manifests::default();
        for (from, to) in self.pairs(scope) {
            if self.taken(to)?.root != root {
                continue;
            }
            let before = self.manifest(from, &mut manifests)?.get(path).copied();
            let after = self.manifest(to, &mut manifests)?.get(path).copied();
            let old = self.text(before.as_ref())?;
            let new = self.text(after.as_ref())?;
            return Ok(crate::changes::hunks(&old, &new));
        }
        Ok(Vec::new())
    }

    /// The snapshots `scope` compares, one pair per folder: from the earliest to the latest.
    fn pairs(&self, scope: Scope) -> Vec<(usize, usize)> {
        let snapshots = &self.index.snapshots;
        match scope {
            Scope::Turn => self
                .index
                .turns
                .last()
                .map(|turn| (turn.before, turn.after))
                .into_iter()
                .collect(),
            Scope::Session | Scope::SinceLooked => {
                let looked = match scope {
                    Scope::SinceLooked => self.index.looked,
                    Scope::Session | Scope::Turn => None,
                };
                let mut roots: Vec<&Path> = Vec::new();
                for taken in snapshots {
                    if !roots.contains(&taken.root.as_path()) {
                        roots.push(&taken.root);
                    }
                }
                roots
                    .into_iter()
                    .filter_map(|root| {
                        let in_root = |index: &usize| {
                            snapshots
                                .get(*index)
                                .is_some_and(|taken| taken.root == root)
                        };
                        let first = (0..snapshots.len()).find(in_root)?;
                        let last = (0..snapshots.len()).rev().find(in_root)?;
                        let from = looked
                            .and_then(|looked| (0..=looked).rev().find(in_root))
                            .unwrap_or(first);
                        Some((from, last))
                    })
                    .collect()
            }
        }
    }

    /// Who changed `path` in `root` between snapshots `from` and `to`, by the turns between.
    fn who(
        &self,
        root: &Path,
        path: &Path,
        from: usize,
        to: usize,
        manifests: &mut Manifests,
    ) -> Result<Who, Error> {
        let (mut claude, mut command) = (false, false);
        for turn in &self.index.turns {
            if turn.before < from || turn.after > to || self.taken(turn.before)?.root != root {
                continue;
            }
            let before = self
                .manifest(turn.before, manifests)?
                .get(path)
                .map(|seen| seen.id);
            let after = self
                .manifest(turn.after, manifests)?
                .get(path)
                .map(|seen| seen.id);
            if before != after {
                let whole = root.join(path);
                if turn.touched.contains(&whole) || turn.touched.contains(path) {
                    claude = true;
                } else {
                    command = true;
                }
            }
        }
        Ok(match (claude, command) {
            (true, true) => Who::Both,
            (true, false) => Who::Claude,
            (false, true) => Who::Command,
            (false, false) => Who::BetweenTurns,
        })
    }

    fn taken(&self, index: usize) -> Result<&Taken, Error> {
        self.index
            .snapshots
            .get(index)
            .ok_or(Error::NoSnapshot(index))
    }

    /// Snapshot `index` whole, from its deltas.
    fn manifest<'a>(
        &self,
        index: usize,
        manifests: &'a mut Manifests,
    ) -> Result<&'a Manifest, Error> {
        if !manifests.0.contains_key(&index) {
            let delta: Delta =
                read_json(&self.snapshot_path(index))?.ok_or(Error::NoSnapshot(index))?;
            let mut manifest = match delta.base {
                Some(base) => self.manifest(base, manifests)?.clone(),
                None => Manifest::new(),
            };
            for gone in &delta.gone {
                manifest.remove(gone);
            }
            manifest.extend(delta.set);
            manifests.0.insert(index, manifest);
        }
        manifests.0.get(&index).ok_or(Error::NoSnapshot(index))
    }

    /// What a file holds as text, or nothing for one that isn't there.
    fn text(&self, seen: Option<&Seen>) -> Result<String, Error> {
        let Some(seen) = seen else {
            return Ok(String::new());
        };
        let path = blob_path(&self.home, &seen.id);
        let bytes = std::fs::read(&path).map_err(io(&path))?;
        Ok(String::from_utf8_lossy(&bytes).into_owned())
    }

    fn snapshot_path(&self, index: usize) -> PathBuf {
        self.dir.join(format!("snapshot-{index}.json"))
    }

    /// The folder `root` as it is now, kept as snapshot n, which is returned.
    fn snapshot(&mut self, root: &Path) -> Result<usize, Error> {
        let base = (0..self.index.snapshots.len()).rev().find(|index| {
            self.index
                .snapshots
                .get(*index)
                .is_some_and(|taken| taken.root == root)
        });
        let mut manifests = Manifests::default();
        let earlier = match base {
            Some(base) => self.manifest(base, &mut manifests)?.clone(),
            None => Manifest::new(),
        };
        let (now, left_out) = look(root, &earlier, &self.home)?;
        let delta = Delta {
            base,
            set: now
                .iter()
                .filter(|(path, seen)| earlier.get(*path) != Some(seen))
                .map(|(path, seen)| (path.clone(), *seen))
                .collect(),
            gone: earlier
                .keys()
                .filter(|path| !now.contains_key(*path))
                .cloned()
                .collect(),
        };
        let index = self.index.snapshots.len();
        private_folder(&self.dir)?;
        write_json(&self.snapshot_path(index), &delta)?;
        self.index.snapshots.push(Taken {
            root: root.to_owned(),
            left_out,
        });
        Ok(index)
    }

    fn save_index(&self) -> Result<(), Error> {
        private_folder(&self.dir)?;
        write_json(&self.dir.join("index.json"), &self.index)
    }
}

/// Snapshots read whole while answering one question.
#[derive(Debug, Default)]
struct Manifests(HashMap<usize, Manifest>);

/// The files that differ between two snapshots of a folder: each with what it was and is.
fn differing(before: &Manifest, after: &Manifest) -> Vec<(PathBuf, Option<Seen>, Option<Seen>)> {
    let paths: BTreeSet<&PathBuf> = before.keys().chain(after.keys()).collect();
    paths
        .into_iter()
        .filter_map(|path| {
            let was = before.get(path).copied();
            let is = after.get(path).copied();
            (was.map(|seen| seen.id) != is.map(|seen| seen.id)).then(|| (path.clone(), was, is))
        })
        .collect()
}

/// Looks at the files in `root`: those unchanged in size and time since `earlier` are taken as
/// they were, the others read, and their contents kept under `home`.
fn look(root: &Path, earlier: &Manifest, home: &Path) -> Result<(Manifest, LeftOut), Error> {
    let mut manifest = Manifest::new();
    let mut left_out = LeftOut::default();
    let walker = ignore::WalkBuilder::new(root)
        .hidden(false)
        .require_git(false)
        .follow_links(false)
        .sort_by_file_name(std::ffi::OsStr::cmp)
        .filter_entry(|entry| {
            let is_dir = entry.file_type().is_some_and(|kind| kind.is_dir());
            // `.git`, and another repository or a worktree inside the folder.
            entry.file_name() != ".git"
                && !(is_dir && entry.depth() > 0 && entry.path().join(".git").exists())
        })
        .build();
    for entry in walker {
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) => {
                tracing::debug!(%error, "a file the snapshot leaves out");
                continue;
            }
        };
        if !entry.file_type().is_some_and(|kind| kind.is_file()) {
            continue;
        }
        if manifest.len() >= MOST_FILES {
            left_out.beyond_limit = true;
            break;
        }
        let Ok(path) = entry.path().strip_prefix(root).map(Path::to_path_buf) else {
            continue;
        };
        if path.to_str().is_none() {
            continue;
        }
        let Ok(metadata) = entry.metadata() else {
            continue;
        };
        let size = metadata.len();
        if size > LARGEST_FILE {
            left_out.large = left_out.large.saturating_add(1);
            continue;
        }
        let modified_ns = metadata
            .modified()
            .ok()
            .and_then(|modified| modified.duration_since(SystemTime::UNIX_EPOCH).ok())
            .and_then(|since| u64::try_from(since.as_nanos()).ok())
            .unwrap_or_default();
        if let Some(seen) = earlier
            .get(&path)
            .filter(|seen| seen.size == size && seen.modified_ns == modified_ns)
        {
            manifest.insert(path, *seen);
            continue;
        }
        let Ok(bytes) = std::fs::read(entry.path()) else {
            continue;
        };
        if bytes.iter().take(SNIFFED).any(|byte| *byte == 0) {
            left_out.binary = left_out.binary.saturating_add(1);
            continue;
        }
        let id = ContentId(sha2::Sha256::digest(&bytes).into());
        keep_blob(home, &id, &bytes)?;
        manifest.insert(
            path,
            Seen {
                size,
                modified_ns,
                id,
            },
        );
    }
    Ok((manifest, left_out))
}

fn blob_path(home: &Path, id: &ContentId) -> PathBuf {
    let hex = id.to_string();
    let (folder, name) = hex.split_at(2);
    home.join("blobs").join(folder).join(name)
}

fn keep_blob(home: &Path, id: &ContentId, bytes: &[u8]) -> Result<(), Error> {
    let path = blob_path(home, id);
    if path.exists() {
        return Ok(());
    }
    let (Some(folder), Some(name)) = (path.parent(), path.file_name()) else {
        return Ok(());
    };
    private_folder(folder)?;
    crate::files::write_atomically(folder, name, bytes).map_err(io(&path))
}

fn private_folder(folder: &Path) -> Result<(), Error> {
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(folder)
        .map_err(io(folder))
}

fn read_json<T: serde::de::DeserializeOwned>(path: &Path) -> Result<Option<T>, Error> {
    match std::fs::read(path) {
        Ok(bytes) => serde_json::from_slice(&bytes)
            .map(Some)
            .map_err(|source| Error::Format {
                path: path.to_owned(),
                source,
            }),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(source) => Err(Error::Io {
            path: path.to_owned(),
            source,
        }),
    }
}

fn write_json<T: Serialize>(path: &Path, value: &T) -> Result<(), Error> {
    let bytes = serde_json::to_vec(value).map_err(|source| Error::Format {
        path: path.to_owned(),
        source,
    })?;
    let (Some(folder), Some(name)) = (path.parent(), path.file_name()) else {
        return Ok(());
    };
    crate::files::write_atomically(folder, name, &bytes).map_err(io(path))
}

/// The files Claude's editing tools name in a line of a session's output.
pub fn edited(line: &str) -> Vec<PathBuf> {
    if !line.contains("\"tool_use\"") {
        return Vec::new();
    }
    let Ok(wire::Message::Assistant { message, .. }) = serde_json::from_str(line) else {
        return Vec::new();
    };
    message
        .content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::ToolUse { name, input, .. } => match name.kind() {
                ToolKind::Edit | ToolKind::Write => tools::file_of(name, input),
                ToolKind::Read
                | ToolKind::Command
                | ToolKind::Search
                | ToolKind::Web
                | ToolKind::Agent
                | ToolKind::Other => None,
            },
            ContentBlock::Text { .. }
            | ContentBlock::Thinking { .. }
            | ContentBlock::ServerToolUse { .. }
            | ContentBlock::ToolResult { .. }
            | ContentBlock::Image { .. }
            | ContentBlock::Other => None,
        })
        .collect()
}

/// What the keeper is asked to do.
#[derive(Debug)]
pub enum Job {
    /// In the folder `cwd`, or the worktree the session's transcript says it works in.
    TurnStarted {
        session: SessionId,
        cwd: PathBuf,
    },
    /// Its changes in short go to `done`, unless it changed nothing.
    TurnEnded {
        session: SessionId,
        touched: BTreeSet<PathBuf>,
        done: mpsc::Sender<Summary>,
    },
    Looked {
        session: SessionId,
    },
    Reviewed {
        session: SessionId,
        path: PathBuf,
        now: Option<ContentId>,
    },
}

/// Writes the changes for every session, a job at a time, until the jobs end. First it forgets
/// sessions nobody looked at for long, and contents no session keeps.
pub async fn keep(home: PathBuf, mut jobs: mpsc::Receiver<Job>) {
    let pruning = home.clone();
    if let Err(error) =
        tokio::task::spawn_blocking(move || prune(&pruning, SystemTime::now())).await
    {
        tracing::error!(%error, "pruning the changes failed");
    }
    while let Some(job) = jobs.recv().await {
        let home = home.clone();
        if let Err(error) = tokio::task::spawn_blocking(move || apply(&home, job)).await {
            tracing::error!(%error, "keeping a session's changes failed");
        }
    }
}

fn apply(home: &Path, job: Job) {
    let session = match &job {
        Job::TurnStarted { session, .. }
        | Job::TurnEnded { session, .. }
        | Job::Looked { session }
        | Job::Reviewed { session, .. } => session.clone(),
    };
    let done = match Store::open(home, &session) {
        Ok(mut store) => match job {
            Job::TurnStarted { cwd, .. } => {
                let worktree =
                    crate::sessions::worktree_of(&crate::sessions::locate(&cwd, &session));
                store.turn_started(&worktree.map_or(cwd, |worktree| worktree.path))
            }
            Job::TurnEnded { touched, done, .. } => store.turn_ended(touched).map(|summary| {
                if let Some(summary) = summary.filter(|summary| summary.files > 0)
                    && done.try_send(summary).is_err()
                {
                    tracing::debug!("the session ended before its turn's changes were known");
                }
            }),
            Job::Looked { .. } => store.looked(),
            Job::Reviewed { path, now, .. } => store.mark_reviewed(path, now),
        },
        Err(error) => Err(error),
    };
    if let Err(error) = done {
        tracing::warn!(%error, session = %session, "a session's changes weren't kept");
    }
}

/// Forgets the sessions whose changes nobody looked at since `now` less [`KEPT_FOR`], then the
/// contents no session left keeps.
fn prune(home: &Path, now: SystemTime) {
    let sessions = home.join("sessions");
    let Ok(entries) = std::fs::read_dir(&sessions) else {
        return;
    };
    let mut kept: BTreeSet<String> = BTreeSet::new();
    for entry in entries.filter_map(Result::ok) {
        let dir = entry.path();
        let index = dir.join("index.json");
        let stale = std::fs::metadata(&index)
            .and_then(|metadata| metadata.modified())
            .is_ok_and(|modified| now.duration_since(modified).is_ok_and(|age| age > KEPT_FOR));
        if stale {
            if let Err(error) = std::fs::remove_dir_all(&dir) {
                tracing::debug!(%error, dir = %dir.display(), "an old session's changes stay");
            }
            continue;
        }
        let Ok(snapshots) = std::fs::read_dir(&dir) else {
            continue;
        };
        for snapshot in snapshots.filter_map(Result::ok) {
            if !snapshot
                .file_name()
                .to_string_lossy()
                .starts_with("snapshot-")
            {
                continue;
            }
            // One that doesn't read may name anything: then no contents are forgotten.
            match read_json::<Delta>(&snapshot.path()) {
                Ok(Some(delta)) => kept.extend(delta.set.values().map(|seen| seen.id.to_string())),
                Ok(None) => {}
                Err(error) => {
                    tracing::warn!(%error, "a snapshot doesn't read: no contents are forgotten");
                    return;
                }
            }
        }
    }
    let Ok(folders) = std::fs::read_dir(home.join("blobs")) else {
        return;
    };
    for folder in folders.filter_map(Result::ok) {
        let Ok(blobs) = std::fs::read_dir(folder.path()) else {
            continue;
        };
        for blob in blobs.filter_map(Result::ok) {
            let name = format!(
                "{}{}",
                folder.file_name().to_string_lossy(),
                blob.file_name().to_string_lossy()
            );
            // A new one may be another daemon's (another protocol's), its snapshot not written
            // yet.
            let old = blob
                .metadata()
                .and_then(|metadata| metadata.modified())
                .is_ok_and(|modified| now.duration_since(modified).is_ok_and(|age| age > FRESH));
            if old && !kept.contains(&name) {
                crate::files::remove(&blob.path());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A scratch folder for one test: the session's folder and the store's home in it.
    struct Bench {
        top: PathBuf,
        root: PathBuf,
        home: PathBuf,
        session: SessionId,
    }

    impl Bench {
        fn new(name: &str) -> Self {
            let top = std::env::temp_dir()
                .join(format!("edualc-snapshots-{name}-{}", std::process::id()));
            if top.exists() {
                std::fs::remove_dir_all(&top).expect("a stale scratch folder goes");
            }
            let root = top.join("project");
            std::fs::create_dir_all(&root).expect("a scratch project");
            Self {
                home: top.join("home"),
                root,
                top,
                session: SessionId::try_from("11111111-2222-4333-8444-555555555555".to_owned())
                    .expect("a session id"),
            }
        }

        fn write(&self, path: &str, text: &str) {
            let path = self.root.join(path);
            if let Some(folder) = path.parent() {
                std::fs::create_dir_all(folder).expect("a folder");
            }
            std::fs::write(path, text).expect("a file");
        }

        fn store(&self) -> Store {
            Store::open(&self.home, &self.session).expect("the store")
        }

        /// A turn in which `change` happens, Claude's tools naming `touched`.
        fn turn(&self, touched: &[&str], change: impl FnOnce(&Self)) -> Option<Summary> {
            let mut store = self.store();
            store.turn_started(&self.root).expect("before the turn");
            change(self);
            let touched = touched.iter().map(|path| self.root.join(path)).collect();
            let mut store = self.store();
            store.turn_ended(touched).expect("after the turn")
        }

        fn files(&self, scope: Scope) -> Vec<(String, Kind, Who, Review)> {
            self.store()
                .changes(scope)
                .expect("the changes")
                .files
                .into_iter()
                .map(|file| {
                    (
                        file.path.display().to_string(),
                        file.kind,
                        file.who,
                        file.review,
                    )
                })
                .collect()
        }
    }

    impl Drop for Bench {
        fn drop(&mut self) {
            if let Err(error) = std::fs::remove_dir_all(&self.top) {
                tracing::debug!(%error, "a scratch folder stays");
            }
        }
    }

    /// What the plan asks of the view: Claude's edit and a command's are told apart, and what
    /// `.gitignore` names, `.git`, and binary and large files are left out.
    #[test]
    fn a_turns_changes_say_who_made_them() {
        let bench = Bench::new("who");
        bench.write(".gitignore", "target/\n");
        bench.write("src/lib.rs", "fn a() {}\n");
        bench.write("Cargo.lock", "old\n");
        let summary = bench.turn(&["src/lib.rs"], |bench| {
            bench.write("src/lib.rs", "fn a() {}\nfn b() {}\n");
            bench.write("Cargo.lock", "new\n");
            bench.write("target/debug/build.log", "ignored\n");
            bench.write(".git/HEAD", "ref: refs/heads/main\n");
            std::fs::write(bench.root.join("logo.png"), [0x89, b'P', 0, 0]).expect("binary");
            std::fs::write(bench.root.join("big.txt"), "x".repeat(2 * 1024 * 1024)).expect("large");
        });
        assert_eq!(
            bench.files(Scope::Turn),
            [
                (
                    "Cargo.lock".to_owned(),
                    Kind::Changed,
                    Who::Command,
                    Review::NotReviewed
                ),
                (
                    "src/lib.rs".to_owned(),
                    Kind::Changed,
                    Who::Claude,
                    Review::NotReviewed
                ),
            ]
        );
        assert_eq!(
            summary,
            Some(Summary {
                files: 2,
                added: 2,
                removed: 1
            })
        );
        let left_out = bench
            .store()
            .changes(Scope::Turn)
            .expect("the changes")
            .left_out;
        assert_eq!((left_out.large, left_out.binary), (1, 1));
    }

    /// A file whose size and time are as before isn't read again: here its contents changed
    /// under the same size and time, and the snapshot still takes it as it was.
    #[test]
    fn an_unchanged_file_is_not_read_again() {
        let bench = Bench::new("unread");
        bench.write("a.txt", "one\n");
        let path = bench.root.join("a.txt");
        let modified = std::fs::metadata(&path)
            .and_then(|metadata| metadata.modified())
            .expect("its time");
        bench.turn(&[], |bench| {
            bench.write("a.txt", "two\n");
            std::fs::File::options()
                .write(true)
                .open(&path)
                .and_then(|file| file.set_modified(modified))
                .expect("its time put back");
        });
        assert!(bench.files(Scope::Turn).is_empty());
    }

    /// Marked reviewed, a file stays so until it changes again.
    #[test]
    fn a_review_mark_comes_off_when_the_file_changes() {
        let bench = Bench::new("review");
        bench.write("a.txt", "one\n");
        bench.turn(&["a.txt"], |bench| bench.write("a.txt", "two\n"));
        let review = || bench.files(Scope::Session).first().map(|file| file.3);
        let now = bench
            .store()
            .changes(Scope::Session)
            .expect("the changes")
            .files
            .first()
            .and_then(|file| file.now);
        bench
            .store()
            .mark_reviewed(bench.root.join("a.txt"), now)
            .expect("marked");
        assert_eq!(review(), Some(Review::Reviewed));
        bench.turn(&["a.txt"], |bench| bench.write("a.txt", "three\n"));
        assert_eq!(review(), Some(Review::ChangedSinceReviewed));
    }

    /// The whole session spans its turns; "since you last looked" only what came after; a
    /// change between turns is told as such.
    #[test]
    fn scopes_span_turns_and_the_last_look() {
        let bench = Bench::new("scopes");
        bench.write("a.txt", "a\n");
        bench.write("b.txt", "b\n");
        bench.turn(&["a.txt"], |bench| bench.write("a.txt", "a2\n"));
        bench.store().looked().expect("looked");
        bench.write("notes.txt", "mine\n");
        bench.turn(&["b.txt"], |bench| bench.write("b.txt", "b2\n"));
        let paths = |scope| -> Vec<(String, Who)> {
            bench
                .files(scope)
                .into_iter()
                .map(|(path, _, who, _)| (path, who))
                .collect()
        };
        assert_eq!(paths(Scope::Turn), [("b.txt".to_owned(), Who::Claude)]);
        assert_eq!(
            paths(Scope::SinceLooked),
            [
                ("b.txt".to_owned(), Who::Claude),
                ("notes.txt".to_owned(), Who::BetweenTurns)
            ]
        );
        assert_eq!(
            paths(Scope::Session),
            [
                ("a.txt".to_owned(), Who::Claude),
                ("b.txt".to_owned(), Who::Claude),
                ("notes.txt".to_owned(), Who::BetweenTurns)
            ]
        );
        let hunks = bench
            .store()
            .hunks(Scope::Session, &bench.root, Path::new("a.txt"))
            .expect("the hunks");
        let texts: Vec<&str> = hunks
            .iter()
            .flat_map(|hunk| &hunk.lines)
            .map(|line| line.text.as_str())
            .collect();
        assert_eq!(texts, ["a", "a2"]);
    }

    #[test]
    fn claudes_editing_tools_name_the_files_they_change() {
        let line = r#"{"type":"assistant","parent_tool_use_id":null,"message":{"content":[
            {"type":"tool_use","id":"t1","name":"Edit","input":{"file_path":"/p/a.rs","old_string":"a","new_string":"b"}},
            {"type":"tool_use","id":"t2","name":"Bash","input":{"command":"cargo fmt"}},
            {"type":"tool_use","id":"t3","name":"NotebookEdit","input":{"notebook_path":"/p/n.ipynb","new_source":"x"}}
        ]}}"#
            .replace('\n', "");
        assert_eq!(
            edited(&line),
            [PathBuf::from("/p/a.rs"), PathBuf::from("/p/n.ipynb")]
        );
        assert!(edited(r#"{"type":"result","subtype":"success"}"#).is_empty());
    }

    /// Pruning forgets a session nobody looked at for long, and old contents no snapshot
    /// names; contents a snapshot names, or new ones, stay.
    #[test]
    fn pruning_keeps_what_is_named_or_new() {
        let bench = Bench::new("prune");
        bench.write("a.txt", "kept\n");
        bench.turn(&[], |_| {});
        let named = bench.store().changes(Scope::Session).expect("changes");
        assert!(named.files.is_empty());
        let unnamed = ContentId([7; 32]);
        keep_blob(&bench.home, &unnamed, b"loose").expect("a loose blob");
        let kept_id = ContentId(sha2::Sha256::digest(b"kept\n").into());

        let later = SystemTime::now() + FRESH + Duration::from_secs(60);
        prune(&bench.home, SystemTime::now());
        assert!(blob_path(&bench.home, &unnamed).exists(), "new, so kept");
        prune(&bench.home, later);
        assert!(
            !blob_path(&bench.home, &unnamed).exists(),
            "old and unnamed"
        );
        assert!(
            blob_path(&bench.home, &kept_id).exists(),
            "named by a snapshot"
        );

        prune(
            &bench.home,
            SystemTime::now() + KEPT_FOR + FRESH + Duration::from_secs(60),
        );
        assert!(
            !bench
                .home
                .join("sessions")
                .join(bench.session.as_str())
                .exists()
        );
        assert!(!blob_path(&bench.home, &kept_id).exists());
    }
}
