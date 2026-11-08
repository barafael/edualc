//! A sandboxed session's own Claude Code home (`CLAUDE_CONFIG_DIR`) and temporary folder. What
//! the sandbox writes there, nothing outside it reads as configuration: the user's own home
//! (`~/.claude`, `~/.claude.json`) stays read-only to the sandbox, and edualc copies into the
//! session's home, at each launch, what a session needs from it. Trust flows in, never out.
//! Only the transcript goes back as the session ends, checked to be one, so that a terminal
//! resumes the session where it stopped: what Claude reads again then, not what it runs.
//!
//! A project's sandboxed sessions share its home, so one may leave something there for the next
//! (a memory, say), which only sandboxed sessions read. What edualc seeds it with is seeded
//! afresh at every launch, so nothing planted in those copies outlives the next start.
//!
//! The sandbox may write the home while edualc writes into it. Every write is made in a staging
//! folder beside the homes, where the sandbox can't reach, then renamed into place through
//! folders opened one name at a time without following links, so nothing the sandbox planted (a
//! link to `~/.bashrc`, say) can turn edualc's write into one somewhere else.

use std::ffi::OsStr;
use std::io::{BufRead, BufReader, Write as _};
use std::os::fd::OwnedFd;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{DirBuilderExt, MetadataExt as _};
use std::path::{Path, PathBuf};

use rustix::fs::{AtFlags, CWD, FileType, Mode, OFlags};
use rustix::io::Errno;
use serde_json::{Map, Value};
use zeroize::Zeroizing;

use crate::claude::Conversation;
use crate::external::claude_files::Credentials;
use crate::external::wire::SessionId;
use crate::sessions;

/// A memory index longer than this isn't merged; the user's own is taken as it is.
const MEMORY_INDEX_BYTES: u64 = 1024 * 1024;
/// The sign-in file holds a few tokens.
const CREDENTIALS_BYTES: u64 = 64 * 1024;
/// A transcript longer than this isn't copied; Claude Code's run to a few hundred megabytes.
const TRANSCRIPT_BYTES: u64 = 1024 * 1024 * 1024;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("no folder for edualc's state: neither XDG_STATE_HOME nor HOME is set")]
    NoStateFolder,
    #[error("couldn't read or write {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("couldn't reach {path} without following links: {source}")]
    Os {
        path: PathBuf,
        #[source]
        source: rustix::io::Errno,
    },
    #[error("couldn't read {path}: {source}")]
    Json {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },
    #[error("{0} isn't a folder of this user's alone, so no session's temporary files go there")]
    SharedTemporaryFolder(PathBuf),
    #[error("{0} is a link, or a file with another name too, so it isn't copied")]
    NotOwnFile(PathBuf),
    #[error("{0} is longer than a transcript runs, so it isn't copied")]
    TooLong(PathBuf),
    #[error("{path} isn't this session's transcript: line {line} is none of its entries")]
    NotTheTranscript { path: PathBuf, line: usize },
}

fn io(path: &Path) -> impl FnOnce(std::io::Error) -> Error + '_ {
    move |source| Error::Io {
        path: path.to_owned(),
        source,
    }
}

fn os(path: &Path) -> impl FnOnce(rustix::io::Errno) -> Error + '_ {
    move |source| Error::Os {
        path: path.to_owned(),
        source,
    }
}

/// A project's Claude Code home for its sandboxed sessions:
/// `$XDG_STATE_HOME/edualc/claude/<the project's folder name, as Claude Code writes it>`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Home(PathBuf);

impl Home {
    /// Where the homes are.
    fn homes() -> Option<PathBuf> {
        dirs::state_dir().map(|state| state.join("edualc").join("claude"))
    }

    /// The home of sessions working in `cwd`.
    pub fn of(cwd: &Path) -> Option<Self> {
        Self::homes().map(|homes| Self(homes.join(sessions::project_name(cwd))))
    }

    /// Every project's home there is.
    pub fn all() -> Vec<Self> {
        let Some(homes) = Self::homes() else {
            return Vec::new();
        };
        std::fs::read_dir(homes)
            .into_iter()
            .flatten()
            .filter_map(Result::ok)
            .filter(|entry| !entry.file_name().to_string_lossy().starts_with('.'))
            .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_dir()))
            .map(|entry| Self(entry.path()))
            .collect()
    }

    pub fn path(&self) -> &Path {
        &self.0
    }

    /// Claude Code's folder of transcripts, one folder per project, in this home.
    pub fn projects(&self) -> PathBuf {
        self.0.join("projects")
    }
}

/// A session's own temporary folder, `/var/tmp/edualc-<uid>/<the id's first eight>`: short,
/// as Claude Code keeps sockets in it and a socket's path is short. On disk, as the user's
/// settings move Claude Code's temporary files there. Removed when dropped.
#[derive(Debug)]
pub struct Scratch(PathBuf);

impl Scratch {
    pub fn make(id: &SessionId) -> Result<Self, Error> {
        let uid = rustix::process::getuid();
        let base = PathBuf::from(format!("/var/tmp/edualc-{}", uid.as_raw()));
        create_private_folder(&base)?;
        // Anyone may make a folder in /var/tmp, so one of this name may not be ours.
        let stat = rustix::fs::lstat(&base).map_err(os(&base))?;
        let ours = FileType::from_raw_mode(stat.st_mode) == FileType::Directory
            && stat.st_uid == uid.as_raw()
            && stat.st_mode & 0o077 == 0;
        if !ours {
            return Err(Error::SharedTemporaryFolder(base));
        }
        let short: String = id.as_str().chars().take(8).collect();
        let dir = base.join(short);
        create_private_folder(&dir)?;
        Ok(Self(dir))
    }

    pub fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        if let Err(error) = std::fs::remove_dir_all(&self.0) {
            tracing::debug!(%error, path = %self.0.display(), "a session's temporary folder stays");
        }
    }
}

/// A folder only this user may enter, made unless it is there.
fn create_private_folder(path: &Path) -> Result<(), Error> {
    match std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(path)
    {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => Ok(()),
        Err(error) => Err(io(path)(error)),
    }
}

/// The user's Claude Code sign-in, for a session that can't read it: read here, outside the
/// sandbox, and handed over through a pipe.
pub struct AccessToken(Zeroizing<String>);

impl std::fmt::Debug for AccessToken {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("AccessToken(…)")
    }
}

impl AccessToken {
    /// The user's current sign-in, unless there is none (a session then starts signed out,
    /// and says so itself) or it has run out: Claude Code renews it as it runs outside the
    /// sandbox; the sandbox doesn't get what renews it.
    pub fn current() -> Option<Self> {
        let path = sessions::config_dir().join(".credentials.json");
        let small = std::fs::metadata(&path)
            .is_ok_and(|metadata| metadata.is_file() && metadata.len() <= CREDENTIALS_BYTES);
        if !small {
            tracing::info!(path = %path.display(), "no sign-in to hand a sandboxed session");
            return None;
        }
        let text = Zeroizing::new(std::fs::read_to_string(&path).ok()?);
        let credentials: Credentials = match serde_json::from_str(&text) {
            Ok(credentials) => credentials,
            Err(error) => {
                tracing::warn!(%error, "the sign-in doesn't parse");
                return None;
            }
        };
        let sign_in = credentials.sign_in?;
        let token = Zeroizing::new(sign_in.access_token);
        let now_ms = jiff::Timestamp::now().as_millisecond();
        if sign_in
            .expires_at_ms
            .is_some_and(|expires| expires <= now_ms)
        {
            tracing::warn!(
                "the sign-in has run out; a Claude Code session outside the sandbox renews it"
            );
            return None;
        }
        Some(Self(token))
    }

    pub fn as_bytes(&self) -> &[u8] {
        self.0.as_bytes()
    }
}

/// Makes `home` ready for a session working in `cwd` on `conversation`, its temporary files
/// in `scratch`: seeds it afresh from the user's own home. A resumed session's transcript that
/// went on separately in the user's home is named.
pub fn prepare(
    home: &Home,
    cwd: &Path,
    scratch: &Scratch,
    conversation: &Conversation,
) -> Result<Option<Diverged>, Error> {
    let homes = home.path().parent().unwrap_or(home.path());
    create_private_folder(home.path())?;
    let staging = Staging::make(homes)?;
    let home_fd = open_folder(CWD, home.path())?;
    let real = sessions::config_dir();

    let settings = settings(&real.join("settings.json"), scratch.path())?;
    staging.write("settings.json", settings.as_bytes())?;
    staging.move_into(&home_fd, home.path(), "settings.json", "settings.json")?;

    for name in crate::sandbox::CLAUDE_CODE_RUNS {
        if *name == "settings.json" {
            continue;
        }
        remove_at(&home_fd, home.path(), name.as_ref())?;
        let source = real.join(name);
        if std::fs::symlink_metadata(&source).is_ok() {
            copy_tree(&source, &staging.path().join(name))?;
            staging.move_into(&home_fd, home.path(), name, name)?;
        }
    }

    let global = global_config(&sessions::global_config_path(), cwd)?;
    staging.write(".claude.json", global.as_bytes())?;
    staging.move_into(&home_fd, home.path(), ".claude.json", ".claude.json")?;

    merge_memory(&staging, &home_fd, home, &real, cwd)?;

    match conversation {
        Conversation::Continue(id) | Conversation::Fork { of: id, .. } => {
            copy_in_transcript(&staging, &home_fd, home, id)
        }
        Conversation::New(_) => Ok(None),
    }
}

/// The user's settings, with Claude Code's temporary files in `scratch`. A setting's `env`
/// outranks the process's own variables, and the user's may move them elsewhere.
fn settings(real: &Path, scratch: &Path) -> Result<String, Error> {
    let parsed = match std::fs::read_to_string(real) {
        Ok(text) => serde_json::from_str(&text).map_err(|source| Error::Json {
            path: real.to_owned(),
            source,
        })?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Value::Null,
        Err(error) => return Err(io(real)(error)),
    };
    let mut settings = object(parsed);
    let mut env = object(settings.remove("env").unwrap_or(Value::Null));
    let scratch = Value::String(scratch.to_string_lossy().into_owned());
    env.insert("CLAUDE_CODE_TMPDIR".to_owned(), scratch.clone());
    env.insert("TMPDIR".to_owned(), scratch);
    settings.insert("env".to_owned(), Value::Object(env));
    Ok(Value::Object(settings).to_string())
}

/// A JSON object's fields; anything else, an empty object's.
fn object(value: Value) -> Map<String, Value> {
    match value {
        Value::Object(fields) => fields,
        Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_) | Value::Array(_) => {
            Map::new()
        }
    }
}

/// `~/.claude.json` as the session's own: everything but the other projects, this one
/// trusted, as edualc decided before starting it.
fn global_config(real: &Path, cwd: &Path) -> Result<String, Error> {
    let small = std::fs::metadata(real).is_ok_and(|metadata| {
        metadata.is_file() && metadata.len() <= sessions::GLOBAL_CONFIG_BYTES
    });
    let parsed = if small {
        let text = std::fs::read_to_string(real).map_err(io(real))?;
        serde_json::from_str(&text).map_err(|source| Error::Json {
            path: real.to_owned(),
            source,
        })?
    } else {
        Value::Null
    };
    let mut global = object(parsed);
    let key = cwd.to_string_lossy().into_owned();
    let mut project = object(
        object(global.remove("projects").unwrap_or(Value::Null))
            .remove(&key)
            .unwrap_or(Value::Null),
    );
    project.insert("hasTrustDialogAccepted".to_owned(), Value::Bool(true));
    let mut projects = Map::new();
    projects.insert(key, Value::Object(project));
    global.insert("projects".to_owned(), Value::Object(projects));
    Ok(Value::Object(global).to_string())
}

/// The project's memory as the user's sessions left it, over what sandboxed sessions wrote. A
/// user's file the user changed since it was last seeded is copied in; one the user hasn't
/// changed leaves the session's version, which the session may have edited. The index takes the
/// session's entry where the user's is as seeded (see `merged_index`).
fn merge_memory(
    staging: &Staging,
    home_fd: &OwnedFd,
    home: &Home,
    real: &Path,
    cwd: &Path,
) -> Result<(), Error> {
    let project = sessions::project_name(cwd);
    let relative = Path::new("projects").join(&project).join("memory");
    let source = real.join(&relative);
    let Ok(entries) = std::fs::read_dir(&source) else {
        return Ok(());
    };
    let homes = home.path().parent().unwrap_or(home.path());
    let manifest_name = format!(".{project}.seeded.json");
    let seeded = Seeded::read(&homes.join(&manifest_name));
    let mut now = Seeded::default();
    let target_path = home.path().join(&relative);
    let target = open_beneath(home_fd, &target_path, &relative)?;
    for entry in entries.filter_map(Result::ok) {
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        let Ok(metadata) = entry.metadata() else {
            continue;
        };
        if !metadata.is_file() {
            continue;
        }
        let staged = format!("memory-{name}");
        if name == "MEMORY.md" {
            let theirs = std::fs::read_to_string(entry.path()).map_err(io(&entry.path()))?;
            let index = merged_index(
                &theirs,
                read_beneath(&target, name).as_deref(),
                seeded.index.as_deref(),
            );
            staging.write(&staged, index.as_bytes())?;
            staging.move_into(&target, &target_path, &staged, name)?;
            now.index = Some(theirs);
            continue;
        }
        let stamp = Stamp::of(&metadata);
        let sessions_own = rustix::fs::statat(&target, name, AtFlags::SYMLINK_NOFOLLOW)
            .ok()
            .filter(|stat| FileType::from_raw_mode(stat.st_mode) == FileType::RegularFile);
        let keep_the_sessions = match (sessions_own, seeded.files.get(name)) {
            (None, _) => false,
            (Some(_), Some(was)) => *was == stamp,
            // Seeded before stamps were kept: the newer of the two stays.
            (Some(ours), None) => {
                let users_ns = u64::try_from(metadata.mtime_nsec()).unwrap_or_default();
                (ours.st_mtime, ours.st_mtime_nsec) > (metadata.mtime(), users_ns)
            }
        };
        if !keep_the_sessions {
            copy_tree(&entry.path(), &staging.path().join(&staged))?;
            staging.move_into(&target, &target_path, &staged, name)?;
        }
        now.files.insert(name.to_owned(), stamp);
    }
    let manifest = serde_json::to_vec(&now).map_err(|source| Error::Json {
        path: homes.join(&manifest_name),
        source,
    })?;
    staging.write(&manifest_name, &manifest)?;
    staging.move_into(
        &open_folder(CWD, homes)?,
        homes,
        &manifest_name,
        &manifest_name,
    )
}

/// What a project's memory was in the user's home when it was last seeded into its sandboxed
/// home. Kept beside the homes, out of the sandboxes' reach, so that a file the user hasn't
/// changed since isn't seeded over what a sandboxed session made of it.
#[derive(Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct Seeded {
    /// By file name.
    files: std::collections::BTreeMap<String, Stamp>,
    /// The user's index.
    index: Option<String>,
}

impl Seeded {
    /// The manifest at `path`; an empty one where there is none, or none that reads.
    fn read(path: &Path) -> Self {
        let small =
            std::fs::metadata(path).is_ok_and(|metadata| metadata.len() <= MEMORY_INDEX_BYTES * 4);
        if !small {
            return Self::default();
        }
        std::fs::read_to_string(path)
            .ok()
            .and_then(|text| serde_json::from_str(&text).ok())
            .unwrap_or_default()
    }
}

/// A file as its length, inode and change time (which only the kernel sets) tell it apart.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct Stamp {
    length: u64,
    inode: u64,
    changed_s: i64,
    changed_ns: i64,
}

impl Stamp {
    fn of(metadata: &std::fs::Metadata) -> Self {
        Self {
            length: metadata.len(),
            inode: metadata.ino(),
            changed_s: metadata.ctime(),
            changed_ns: metadata.ctime_nsec(),
        }
    }
}

/// The memory index for the sandboxed home, entries told apart by the file each links to: the
/// user's, but the session's own version of an entry the user hasn't changed since it was seeded
/// (`seeded`, the user's index then), none of an entry the session removed, then the entries only
/// the session has. Without `seeded`, the user's entry wins.
fn merged_index(users: &str, sandboxed: Option<&str>, seeded: Option<&str>) -> String {
    let Some(sandboxed) = sandboxed else {
        // Nothing of the session's to keep: the user's, as it is.
        return format!("{}\n", users.trim_end());
    };
    let sessions: Vec<&str> = sandboxed
        .lines()
        .filter(|line| !line.trim().is_empty())
        .collect();
    let as_seeded = |line: &str| seeded.is_some_and(|seeded| seeded.lines().any(|was| was == line));
    let mut index: Vec<&str> = Vec::new();
    for line in users.trim_end().lines() {
        let Some(target) = link_target(line) else {
            index.push(line);
            continue;
        };
        let session_s = sessions
            .iter()
            .copied()
            .find(|line| link_target(line) == Some(target));
        match (session_s, as_seeded(line)) {
            (Some(session_s), true) => index.push(session_s),
            (None, true) => {}
            (Some(_) | None, false) => index.push(line),
        }
    }
    for line in sessions {
        let known = link_target(line).is_some_and(|target| {
            users
                .lines()
                .any(|user_s| link_target(user_s) == Some(target))
        });
        if !known && !index.contains(&line) {
            index.push(line);
        }
    }
    let mut merged = index.join("\n");
    merged.push('\n');
    merged
}

/// The file an index line links to, as in `- [Title](file.md) — what it holds`.
fn link_target(line: &str) -> Option<&str> {
    let start = line.find("](")?.checked_add(2)?;
    let rest = line.get(start..)?;
    rest.get(..rest.find(')')?)
}

/// A file in an opened folder, unless it is missing, a link, or too long.
fn read_beneath(folder: &OwnedFd, name: &str) -> Option<String> {
    let file = rustix::fs::openat(
        folder,
        name,
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .ok()?;
    let mut text = String::new();
    let file = std::fs::File::from(file);
    std::io::Read::read_to_string(
        &mut std::io::Read::take(file, MEMORY_INDEX_BYTES),
        &mut text,
    )
    .ok()?;
    Some(text)
}

/// A session resumed from the user's own home: its transcript and its subagents' copied in,
/// unless the home has it already, as long as the user's copy or longer. Where the two went on
/// separately, the home's is kept, and the user's is named as left behind.
fn copy_in_transcript(
    staging: &Staging,
    home_fd: &OwnedFd,
    home: &Home,
    id: &SessionId,
) -> Result<Option<Diverged>, Error> {
    let name = format!("{}.jsonl", id.as_str());
    let Some(source) = sessions::find_user_transcript(id) else {
        return Ok(None);
    };
    let Some(project) = source
        .parent()
        .and_then(Path::file_name)
        .and_then(|project| project.to_str())
    else {
        return Ok(None);
    };
    let relative = Path::new("projects").join(project);
    match copy_in(&source, home, &relative, &name)? {
        CopyIn::Take => {}
        CopyIn::Keep => return Ok(None),
        CopyIn::Diverged => return Ok(Some(Diverged { left: source })),
    }
    let target_path = home.path().join(&relative);
    let target = open_beneath(home_fd, &target_path, &relative)?;
    copy_tree(&source, &staging.path().join(&name))?;
    staging.move_into(&target, &target_path, &name, &name)?;
    let companion = source.with_extension("");
    if companion.is_dir() {
        let folder = id.as_str();
        copy_tree(&companion, &staging.path().join(folder))?;
        staging.move_into(&target, &target_path, folder, folder)?;
    }
    Ok(None)
}

/// Whether a resumed session's transcript is copied in from the user's own home.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CopyIn {
    /// The home has none, or the user's goes on from it (the session was resumed in a terminal
    /// since), or the home's is no plain file, which is replaced, never read.
    Take,
    /// The home's is the same, or goes on from the user's.
    Keep,
    /// Each went on without the other.
    Diverged,
}

/// How the user's transcript at `users` stands to the home's copy at `relative`/`name`.
fn copy_in(users: &Path, home: &Home, relative: &Path, name: &str) -> Result<CopyIn, Error> {
    let own = match open_existing(home.path(), relative, name) {
        Ok(Some(own)) => own,
        Ok(None) | Err(Error::NotOwnFile(_)) => return Ok(CopyIn::Take),
        Err(Error::TooLong(_)) => return Ok(CopyIn::Keep),
        Err(error) => return Err(error),
    };
    let theirs = std::fs::File::open(users).map_err(io(users))?;
    let related = relation(BufReader::new(theirs), BufReader::new(own)).map_err(io(users))?;
    Ok(match related {
        Relation::Extends => CopyIn::Take,
        Relation::Same | Relation::Extended => CopyIn::Keep,
        Relation::Apart => CopyIn::Diverged,
    })
}

/// Two copies of a session's transcript that went on separately: it was resumed in one home
/// while the other held what it wrote last. It goes on from one; the other stays where it is,
/// with what only it has.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Diverged {
    /// The copy the session doesn't go on from.
    pub left: PathBuf,
}

/// What became of a sandboxed session's transcript, copied back.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CopyBack {
    /// The user's own home has it now.
    Copied,
    /// The user's own home had it as it is.
    Unchanged,
    /// The session wrote none.
    NoneWritten,
    /// The user's copy goes on from it (the session was resumed in a terminal since), so it
    /// stays as it is.
    UsersGoesOn,
    /// Each went on without the other: the user's copy stays, and the home's is left behind.
    Diverged(Diverged),
}

/// Before a session working in `cwd` is resumed outside the sandbox: what it last wrote in its
/// sandboxed home there, copied back, so that it goes on from that.
pub fn bring_back(cwd: &Path, id: &SessionId) -> Result<CopyBack, Error> {
    let Some(home) = Home::of(cwd) else {
        return Ok(CopyBack::NoneWritten);
    };
    copy_back_transcript(&home, cwd, id)
}

/// Copies the transcript a sandboxed session wrote in `home` into the user's own home, so that
/// a terminal resumes the session where it ended. For a session whose process has ended, so
/// that the transcript no longer grows. Only a plain file of the session's own entries is
/// copied, and only where the user has no copy or one it goes on from.
pub fn copy_back_transcript(home: &Home, cwd: &Path, id: &SessionId) -> Result<CopyBack, Error> {
    let relative = Path::new("projects").join(sessions::project_name(cwd));
    copy_back(home, &relative, &sessions::config_dir().join(&relative), id)
}

/// Copies session `id`'s transcript from the folder `relative` in `home` into the folder
/// `target_path` (see `copy_back_transcript`).
fn copy_back(
    home: &Home,
    relative: &Path,
    target_path: &Path,
    id: &SessionId,
) -> Result<CopyBack, Error> {
    let name = format!("{}.jsonl", id.as_str());
    let source_path = home.path().join(relative).join(&name);
    let Some(source) = open_existing(home.path(), relative, &name)? else {
        return Ok(CopyBack::NoneWritten);
    };
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(target_path)
        .map_err(io(target_path))?;
    let target = open_folder(CWD, target_path)?;
    let mut copy = Unplaced::make(&target, target_path, &name)?;
    copy_entries(source, &source_path, &copy.file, &copy.path(), id)?;
    let placed = target_path.join(&name);
    let users = match rustix::fs::openat(
        &target,
        name.as_str(),
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    ) {
        Ok(users) => std::fs::File::from(users),
        Err(Errno::NOENT) => {
            copy.place(&name)?;
            return Ok(CopyBack::Copied);
        }
        Err(error) => return Err(os(&placed)(error)),
    };
    let ours = copy.reread()?;
    match relation(ours, BufReader::new(users)).map_err(io(&placed))? {
        Relation::Same => Ok(CopyBack::Unchanged),
        Relation::Extends => {
            copy.place(&name)?;
            Ok(CopyBack::Copied)
        }
        Relation::Extended => Ok(CopyBack::UsersGoesOn),
        Relation::Apart => Ok(CopyBack::Diverged(Diverged { left: source_path })),
    }
}

/// Keeps the user's copy of a running sandboxed session's transcript in step with the one it
/// writes in its home, its ended lines appended as they come, so that what it wrote is in
/// `~/.claude` even if edualc ends before the session does (a logout ended the daemon before its
/// copy-back once, and a resume outside the sandbox lost what was only in the home). The copy
/// as the session ends still follows (see `copy_back_transcript`).
#[derive(Debug)]
pub struct Follower {
    home: Home,
    relative: PathBuf,
    target_path: PathBuf,
    name: String,
    id: SessionId,
    state: Following,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Following {
    /// Not begun: the user's copy is brought in line with the home's first.
    Starting,
    /// The user's copy holds the home's first this many bytes, as written here.
    At(u64),
    /// Given up: the user's copy went on otherwise (resumed in a terminal meanwhile), or the
    /// home's isn't a plain file of this session's.
    Stopped,
}

/// What a step of following came to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Followed {
    /// Nothing new, or nothing written yet.
    Unchanged,
    /// This many bytes of ended lines went into the user's copy.
    Copied(u64),
    Stopped,
}

impl Follower {
    /// For session `id` working in `cwd`, which writes its transcript in `home`.
    pub fn of(home: Home, cwd: &Path, id: SessionId) -> Self {
        let relative = Path::new("projects").join(sessions::project_name(cwd));
        let target_path = sessions::config_dir().join(&relative);
        Self::between(home, relative, target_path, id)
    }

    fn between(home: Home, relative: PathBuf, target_path: PathBuf, id: SessionId) -> Self {
        Self {
            home,
            relative,
            target_path,
            name: format!("{}.jsonl", id.as_str()),
            id,
            state: Following::Starting,
        }
    }

    /// Brings what the session wrote since into the user's copy.
    pub fn step(&mut self) -> Followed {
        let followed = match self.state {
            Following::Stopped => return Followed::Stopped,
            Following::Starting => self.start(),
            Following::At(copied) => self.go_on(copied),
        };
        match followed {
            Ok((state, followed)) => {
                self.state = state;
                followed
            }
            Err(error) => {
                tracing::warn!(%error, "the transcript isn't followed into ~/.claude any more");
                self.state = Following::Stopped;
                Followed::Stopped
            }
        }
    }

    /// The first step: the user's copy made the home's, where it has none or the home's goes
    /// on from it.
    fn start(&self) -> Result<(Following, Followed), Error> {
        match copy_back(&self.home, &self.relative, &self.target_path, &self.id)? {
            CopyBack::NoneWritten => Ok((Following::Starting, Followed::Unchanged)),
            CopyBack::Copied | CopyBack::Unchanged => {
                let users = self.target_path.join(&self.name);
                let length = std::fs::symlink_metadata(&users).map_err(io(&users))?.len();
                Ok((Following::At(length), Followed::Copied(length)))
            }
            CopyBack::UsersGoesOn | CopyBack::Diverged(_) => {
                Ok((Following::Stopped, Followed::Stopped))
            }
        }
    }

    /// A later step: the ended lines after the first `copied` bytes appended to the user's copy,
    /// as long as it is still what was written here.
    fn go_on(&self, copied: u64) -> Result<(Following, Followed), Error> {
        let source_path = self.home.path().join(&self.relative).join(&self.name);
        let Some(mut source) = open_existing(self.home.path(), &self.relative, &self.name)? else {
            return Ok((Following::Stopped, Followed::Stopped));
        };
        let length = source.metadata().map_err(io(&source_path))?.len();
        if length == copied {
            return Ok((Following::At(copied), Followed::Unchanged));
        }
        if length < copied {
            // Replaced or cut short: not the transcript this followed.
            return Ok((Following::Stopped, Followed::Stopped));
        }
        let users_path = self.target_path.join(&self.name);
        let target = open_folder(CWD, &self.target_path)?;
        let users = rustix::fs::openat(
            &target,
            self.name.as_str(),
            OFlags::WRONLY | OFlags::APPEND | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .map_err(os(&users_path))?;
        let mut users = std::fs::File::from(users);
        if users.metadata().map_err(io(&users_path))?.len() != copied {
            return Ok((Following::Stopped, Followed::Stopped));
        }
        std::io::Seek::seek(&mut source, std::io::SeekFrom::Start(copied))
            .map_err(io(&source_path))?;
        let mut lines = BufReader::new(std::io::Read::take(
            source,
            TRANSCRIPT_BYTES.saturating_sub(copied),
        ));
        let mut ended = Vec::new();
        let mut line = Vec::new();
        loop {
            line.clear();
            lines
                .read_until(b'\n', &mut line)
                .map_err(io(&source_path))?;
            let Some(entry) = line.strip_suffix(b"\n") else {
                break;
            };
            if !is_entry_of(entry, &self.id) {
                return Ok((Following::Stopped, Followed::Stopped));
            }
            ended.extend_from_slice(&line);
        }
        if ended.is_empty() {
            return Ok((Following::At(copied), Followed::Unchanged));
        }
        users.write_all(&ended).map_err(io(&users_path))?;
        let added = u64::try_from(ended.len()).unwrap_or(u64::MAX);
        Ok((
            Following::At(copied.saturating_add(added)),
            Followed::Copied(added),
        ))
    }
}

/// Opens the file `name` in the folder `relative` under `root`, one name at a time and
/// following no link; none where something on the way is missing. Only a plain file with no
/// other name, as a sandbox could otherwise hand in, by a link, a file it can't read itself.
pub(crate) fn open_existing(
    root: &Path,
    relative: &Path,
    name: &str,
) -> Result<Option<std::fs::File>, Error> {
    let full = root.join(relative).join(name);
    let mut folder = open_folder(CWD, root)?;
    for part in relative.components() {
        let std::path::Component::Normal(part) = part else {
            continue;
        };
        folder = match open_folder(&folder, Path::new(part)) {
            Ok(opened) => opened,
            Err(Error::Os {
                source: Errno::NOENT,
                ..
            }) => return Ok(None),
            Err(error) => return Err(error),
        };
    }
    // Not blocking: a pipe put there would otherwise hold the open until something writes.
    let file = match rustix::fs::openat(
        &folder,
        name,
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
        Mode::empty(),
    ) {
        Ok(file) => file,
        Err(Errno::NOENT) => return Ok(None),
        Err(Errno::LOOP) => return Err(Error::NotOwnFile(full)),
        Err(error) => return Err(os(&full)(error)),
    };
    let stat = rustix::fs::fstat(&file).map_err(os(&full))?;
    if FileType::from_raw_mode(stat.st_mode) != FileType::RegularFile || stat.st_nlink != 1 {
        return Err(Error::NotOwnFile(full));
    }
    if u64::try_from(stat.st_size)
        .ok()
        .is_none_or(|length| length > TRANSCRIPT_BYTES)
    {
        return Err(Error::TooLong(full));
    }
    Ok(Some(std::fs::File::from(file)))
}

/// Copies the ended lines of `source`, a transcript of session `id`, to `to`, each checked to
/// be one of its entries. A last line not ended (the process stopped as it wrote it) is left
/// out.
fn copy_entries(
    source: std::fs::File,
    source_path: &Path,
    to: &std::fs::File,
    to_path: &Path,
    id: &SessionId,
) -> Result<(), Error> {
    let mut lines = BufReader::new(std::io::Read::take(source, TRANSCRIPT_BYTES));
    let mut out = std::io::BufWriter::new(to);
    let mut line = Vec::new();
    for number in 1.. {
        line.clear();
        lines
            .read_until(b'\n', &mut line)
            .map_err(io(source_path))?;
        let Some(entry) = line.strip_suffix(b"\n") else {
            break;
        };
        if !is_entry_of(entry, id) {
            return Err(Error::NotTheTranscript {
                path: source_path.to_owned(),
                line: number,
            });
        }
        out.write_all(&line).map_err(io(to_path))?;
    }
    out.flush().map_err(io(to_path))
}

/// Whether `line` is an entry of session `id`'s transcript: a JSON object whose `sessionId`,
/// where it has one (file-history entries have none), is `id`.
fn is_entry_of(line: &[u8], id: &SessionId) -> bool {
    serde_json::from_slice::<Map<String, Value>>(line).is_ok_and(|entry| {
        match entry.get("sessionId") {
            None => true,
            Some(Value::String(owner)) => owner == id.as_str(),
            Some(
                Value::Null
                | Value::Bool(_)
                | Value::Number(_)
                | Value::Array(_)
                | Value::Object(_),
            ) => false,
        }
    })
}

/// How one copy of a transcript stands to another, which Claude Code only ever appends to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Relation {
    Same,
    /// The first goes on where the second ends.
    Extends,
    /// The second goes on where the first ends.
    Extended,
    /// They part somewhere.
    Apart,
}

/// How `first` stands to `second`, read through to where they part or end.
fn relation(mut first: impl BufRead, mut second: impl BufRead) -> std::io::Result<Relation> {
    loop {
        let step = {
            let mine = first.fill_buf()?;
            let other = second.fill_buf()?;
            match (mine.is_empty(), other.is_empty()) {
                (true, true) => return Ok(Relation::Same),
                (false, true) => return Ok(Relation::Extends),
                (true, false) => return Ok(Relation::Extended),
                (false, false) => {}
            }
            if mine.iter().zip(other).any(|(mine, other)| mine != other) {
                return Ok(Relation::Apart);
            }
            mine.len().min(other.len())
        };
        first.consume(step);
        second.consume(step);
    }
}

/// A file being written into a folder of the user's own home, under a name of its own until it
/// is placed; removed if it never is.
struct Unplaced<'folder> {
    folder: &'folder OwnedFd,
    folder_path: &'folder Path,
    /// While it isn't placed.
    name: Option<String>,
    file: std::fs::File,
}

impl<'folder> Unplaced<'folder> {
    fn make(
        folder: &'folder OwnedFd,
        folder_path: &'folder Path,
        for_name: &str,
    ) -> Result<Self, Error> {
        let name = format!(".{for_name}.edualc-{}", uuid::Uuid::new_v4());
        let file = rustix::fs::openat(
            folder,
            name.as_str(),
            OFlags::RDWR | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::from_raw_mode(0o600),
        )
        .map_err(os(&folder_path.join(&name)))?;
        Ok(Self {
            folder,
            folder_path,
            name: Some(name),
            file: std::fs::File::from(file),
        })
    }

    fn path(&self) -> PathBuf {
        self.folder_path
            .join(self.name.as_deref().unwrap_or_default())
    }

    /// What was written, from its start.
    fn reread(&mut self) -> Result<BufReader<&std::fs::File>, Error> {
        std::io::Seek::rewind(&mut self.file).map_err(io(&self.path()))?;
        Ok(BufReader::new(&self.file))
    }

    /// Renames it to `name`, over what is there.
    fn place(mut self, name: &str) -> Result<(), Error> {
        if let Some(unplaced) = &self.name {
            rustix::fs::renameat(self.folder, unplaced.as_str(), self.folder, name)
                .map_err(os(&self.folder_path.join(name)))?;
        }
        self.name = None;
        Ok(())
    }
}

impl Drop for Unplaced<'_> {
    fn drop(&mut self) {
        if let Some(name) = self.name.take()
            && let Err(error) = rustix::fs::unlinkat(self.folder, name.as_str(), AtFlags::empty())
        {
            tracing::warn!(%error, %name, "a copy that wasn't placed stays");
        }
    }
}

/// A folder beside the homes, out of the sandbox's reach, where what goes into a home is
/// made. Removed when dropped.
struct Staging {
    path: PathBuf,
    fd: OwnedFd,
}

impl Staging {
    fn make(homes: &Path) -> Result<Self, Error> {
        let path = homes.join(format!(".staging-{}", uuid::Uuid::new_v4()));
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(&path)
            .map_err(io(&path))?;
        let fd = open_folder(CWD, &path)?;
        Ok(Self { path, fd })
    }

    fn path(&self) -> &Path {
        &self.path
    }

    fn write(&self, name: &str, bytes: &[u8]) -> Result<(), Error> {
        let path = self.path.join(name);
        let mut file = std::fs::File::create_new(&path).map_err(io(&path))?;
        file.write_all(bytes).map_err(io(&path))
    }

    /// Renames `staged` into the opened folder `target` as `name`, over what is there: a
    /// link there is replaced, never followed.
    fn move_into(
        &self,
        target: &OwnedFd,
        target_path: &Path,
        staged: &str,
        name: &str,
    ) -> Result<(), Error> {
        let placed = target_path.join(name);
        match rustix::fs::renameat(&self.fd, staged, target, name) {
            Ok(()) => Ok(()),
            // A folder over a folder: the old one goes first.
            Err(Errno::NOTEMPTY | Errno::EXIST | Errno::ISDIR) => {
                remove_at(target, target_path, name.as_ref())?;
                rustix::fs::renameat(&self.fd, staged, target, name).map_err(os(&placed))
            }
            Err(error) => Err(os(&placed)(error)),
        }
    }
}

impl Drop for Staging {
    fn drop(&mut self) {
        if let Err(error) = std::fs::remove_dir_all(&self.path) {
            tracing::debug!(%error, path = %self.path.display(), "a staging folder stays");
        }
    }
}

/// Removes `name` from the opened `folder`, a folder with all in it, a link as a link; a
/// missing one is fine. By descriptor, so no link swapped in on the way is followed.
fn remove_at(folder: &OwnedFd, folder_path: &Path, name: &OsStr) -> Result<(), Error> {
    let path = folder_path.join(name);
    let stat = match rustix::fs::statat(folder, name, AtFlags::SYMLINK_NOFOLLOW) {
        Ok(stat) => stat,
        Err(Errno::NOENT) => return Ok(()),
        Err(error) => return Err(os(&path)(error)),
    };
    if FileType::from_raw_mode(stat.st_mode) == FileType::Directory {
        let inner = open_folder(folder, Path::new(name))?;
        let mut names = Vec::new();
        for entry in rustix::fs::Dir::read_from(&inner).map_err(os(&path))? {
            let entry = entry.map_err(os(&path))?;
            let child = OsStr::from_bytes(entry.file_name().to_bytes()).to_owned();
            if child != "." && child != ".." {
                names.push(child);
            }
        }
        for child in names {
            remove_at(&inner, &path, &child)?;
        }
        rustix::fs::unlinkat(folder, name, AtFlags::REMOVEDIR).map_err(os(&path))
    } else {
        rustix::fs::unlinkat(folder, name, AtFlags::empty()).map_err(os(&path))
    }
}

/// Opens a folder that is no link.
fn open_folder(at: impl rustix::fd::AsFd, path: &Path) -> Result<OwnedFd, Error> {
    rustix::fs::openat(
        at,
        path,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map_err(os(path))
}

/// Opens `relative` under the opened `folder` one name at a time, making what is missing, and
/// following no link on the way.
fn open_beneath(folder: &OwnedFd, full: &Path, relative: &Path) -> Result<OwnedFd, Error> {
    let mut current = rustix::io::fcntl_dupfd_cloexec(folder, 0).map_err(os(full))?;
    for part in relative.components() {
        let std::path::Component::Normal(name) = part else {
            continue;
        };
        match rustix::fs::mkdirat(&current, name, Mode::from_raw_mode(0o700)) {
            Ok(()) | Err(rustix::io::Errno::EXIST) => {}
            Err(error) => return Err(os(full)(error)),
        }
        current = open_folder(&current, Path::new(name))?;
    }
    Ok(current)
}

/// Copies `from`, out of the user's own home, to `to`, in a staging folder: folders, files,
/// links as links; anything else (a socket, say) is left out.
fn copy_tree(from: &Path, to: &Path) -> Result<(), Error> {
    let metadata = std::fs::symlink_metadata(from).map_err(io(from))?;
    let kind = metadata.file_type();
    if kind.is_symlink() {
        let target = std::fs::read_link(from).map_err(io(from))?;
        std::os::unix::fs::symlink(target, to).map_err(io(to))
    } else if kind.is_dir() {
        std::fs::create_dir(to).map_err(io(to))?;
        for entry in std::fs::read_dir(from).map_err(io(from))? {
            let entry = entry.map_err(io(from))?;
            copy_tree(&entry.path(), &to.join(entry.file_name()))?;
        }
        Ok(())
    } else if kind.is_file() {
        std::fs::copy(from, to).map(|_| ()).map_err(io(from))
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The value at a path of keys, if there is one.
    fn at<'a>(value: &'a Value, keys: &[&str]) -> Option<&'a Value> {
        keys.iter().try_fold(value, |inner, key| inner.get(key))
    }

    #[test]
    fn the_index_keeps_the_users_lines_first_and_adds_the_others() {
        let users = "- [A](a.md) — a\n- [B](b.md) — b\n";
        let sandboxed = "- [B](b.md) — b\n- [C](c.md) — c\n\n";
        assert_eq!(
            merged_index(users, Some(sandboxed), None),
            "- [A](a.md) — a\n- [B](b.md) — b\n- [C](c.md) — c\n"
        );
        assert_eq!(merged_index(users, None, Some(users)), users);
    }

    /// An entry the session edited stays edited, once, where the user's is as it was seeded; an
    /// entry the user changed since wins; one the session removed stays removed.
    #[test]
    fn the_sessions_entries_stay_where_the_user_didnt_change_them() {
        let seeded = "# Memory\n- [A](a.md) — a\n- [B](b.md) — b\n- [D](d.md) — d\n";
        let users =
            "# Memory\n- [A](a.md) — a\n- [B](b.md) — b, as the user put it\n- [D](d.md) — d\n";
        let sandboxed = "# Memory\n- [A](a.md) — a, as the session put it\n- [B](b.md) — b, \
                         as the session put it\n- [C](c.md) — c\n";
        assert_eq!(
            merged_index(users, Some(sandboxed), Some(seeded)),
            "# Memory\n- [A](a.md) — a, as the session put it\n- [B](b.md) — b, as the user put \
             it\n- [C](c.md) — c\n"
        );
    }

    /// A memory file the user hasn't changed since it was seeded keeps what the session made of
    /// it; one the user changed since is seeded again.
    #[test]
    fn a_memory_file_is_seeded_again_only_where_the_user_changed_it() {
        let scratch = std::env::temp_dir().join(format!("edualc-memory-{}", std::process::id()));
        let cwd = Path::new("/work/project");
        let relative = Path::new("projects")
            .join(sessions::project_name(cwd))
            .join("memory");
        let real = scratch.join("real");
        std::fs::create_dir_all(real.join(&relative)).expect("the user's memory");
        let users = real.join(&relative).join("note.md");
        std::fs::write(&users, "v1").expect("a memory file");
        let homes = scratch.join("homes");
        let home = Home(homes.join("project"));
        create_private_folder(home.path()).expect("a home");
        let seed = || {
            let staging = Staging::make(&homes).expect("staging");
            let home_fd = open_folder(CWD, home.path()).expect("the home");
            merge_memory(&staging, &home_fd, &home, &real, cwd).expect("seeded");
        };
        let sessions = home.path().join(&relative).join("note.md");
        seed();
        assert_eq!(std::fs::read_to_string(&sessions).expect("seeded"), "v1");
        std::fs::write(&sessions, "v1, as the session put it").expect("edited");
        seed();
        assert_eq!(
            std::fs::read_to_string(&sessions).expect("kept"),
            "v1, as the session put it"
        );
        std::fs::write(&users, "v2, as the user put it").expect("changed");
        seed();
        assert_eq!(
            std::fs::read_to_string(&sessions).expect("seeded again"),
            "v2, as the user put it"
        );
        std::fs::remove_dir_all(&scratch).expect("the scratch folder goes");
    }

    #[test]
    fn the_sessions_own_config_trusts_its_project_and_knows_no_other() {
        let scratch = std::env::temp_dir().join(format!("edualc-home-{}", std::process::id()));
        std::fs::create_dir_all(&scratch).expect("a folder");
        let real = scratch.join(".claude.json");
        std::fs::write(
            &real,
            r#"{"numStartups":3,"projects":{"/work/a":{"allowedTools":["Bash(ls)"],"mcpServers":{"m":{}}},"/work/b":{"hasTrustDialogAccepted":true}}}"#,
        )
        .expect("a config");
        let own: Value =
            serde_json::from_str(&global_config(&real, Path::new("/work/a")).expect("built"))
                .expect("json");
        assert_eq!(at(&own, &["numStartups"]), Some(&Value::from(3)));
        assert_eq!(
            at(&own, &["projects", "/work/a", "allowedTools"]),
            Some(&serde_json::json!(["Bash(ls)"]))
        );
        assert_eq!(
            at(&own, &["projects", "/work/a", "hasTrustDialogAccepted"]),
            Some(&Value::Bool(true))
        );
        assert_eq!(at(&own, &["projects", "/work/b"]), None, "{own}");
        std::fs::remove_dir_all(&scratch).expect("the scratch folder goes");
    }

    #[test]
    fn settings_send_temporary_files_to_the_sessions_folder() {
        let scratch = std::env::temp_dir().join(format!("edualc-settings-{}", std::process::id()));
        std::fs::create_dir_all(&scratch).expect("a folder");
        let real = scratch.join("settings.json");
        std::fs::write(
            &real,
            r#"{"model":"opus","env":{"TMPDIR":"/var/tmp","OTHER":"x"}}"#,
        )
        .expect("settings");
        let own: Value = serde_json::from_str(
            &settings(&real, Path::new("/var/tmp/edualc-1/abcd")).expect("built"),
        )
        .expect("json");
        let text = |keys: &[&str]| at(&own, keys).and_then(Value::as_str).map(str::to_owned);
        assert_eq!(text(&["model"]).as_deref(), Some("opus"));
        assert_eq!(
            text(&["env", "TMPDIR"]).as_deref(),
            Some("/var/tmp/edualc-1/abcd")
        );
        assert_eq!(
            text(&["env", "CLAUDE_CODE_TMPDIR"]).as_deref(),
            Some("/var/tmp/edualc-1/abcd")
        );
        assert_eq!(text(&["env", "OTHER"]).as_deref(), Some("x"));
        let missing: Value = serde_json::from_str(
            &settings(&scratch.join("none.json"), Path::new("/t")).expect("built"),
        )
        .expect("json");
        assert_eq!(
            at(&missing, &["env", "TMPDIR"]).and_then(Value::as_str),
            Some("/t")
        );
        std::fs::remove_dir_all(&scratch).expect("the scratch folder goes");
    }

    /// A link the sandbox planted where edualc writes is replaced, never written through.
    #[test]
    fn a_planted_link_is_replaced_not_followed() {
        let scratch = std::env::temp_dir().join(format!("edualc-planted-{}", std::process::id()));
        let homes = scratch.join("homes");
        let home = homes.join("project");
        let outside = scratch.join("outside");
        std::fs::create_dir_all(&home).expect("a home");
        std::fs::create_dir_all(&outside).expect("a folder outside");
        // Planted: the memory folder's parent is a link out of the home.
        std::fs::create_dir_all(home.join("projects")).expect("projects");
        std::os::unix::fs::symlink(&outside, home.join("projects").join("p")).expect("a link");
        std::os::unix::fs::symlink(outside.join("settings.json"), home.join("settings.json"))
            .expect("a link");
        let staging = Staging::make(&homes).expect("staging");
        let home_fd = open_folder(CWD, &home).expect("the home");
        staging.write("settings.json", b"{}").expect("staged");
        staging
            .move_into(&home_fd, &home, "settings.json", "settings.json")
            .expect("moved");
        assert!(
            !outside.join("settings.json").exists(),
            "nothing was written through the link"
        );
        let relative = Path::new("projects").join("p").join("memory");
        assert!(
            open_beneath(&home_fd, &home.join(&relative), &relative).is_err(),
            "a link on the way is refused"
        );
        assert!(
            !outside.join("memory").exists(),
            "nor was a folder made through it"
        );
        drop(staging);
        std::fs::remove_dir_all(&scratch).expect("the scratch folder goes");
    }

    /// Clearing a seeded folder for its fresh copy takes a link planted in it, never what it
    /// leads to.
    #[test]
    fn clearing_a_folder_leaves_what_a_link_in_it_leads_to() {
        let scratch = std::env::temp_dir().join(format!("edualc-clear-{}", std::process::id()));
        let home = scratch.join("home");
        let outside = scratch.join("outside");
        std::fs::create_dir_all(home.join("skills").join("inner")).expect("a seeded folder");
        std::fs::create_dir_all(&outside).expect("a folder outside");
        std::fs::write(outside.join("keep.txt"), "keep").expect("a file outside");
        std::os::unix::fs::symlink(&outside, home.join("skills").join("inner").join("away"))
            .expect("a planted link");
        let home_fd = open_folder(CWD, &home).expect("the home");
        remove_at(&home_fd, &home, OsStr::new("skills")).expect("cleared");
        assert!(!home.join("skills").exists(), "the seeded folder went");
        assert!(
            outside.join("keep.txt").exists(),
            "what the link led to stays"
        );
        remove_at(&home_fd, &home, OsStr::new("skills")).expect("a missing one is fine");
        std::fs::remove_dir_all(&scratch).expect("the scratch folder goes");
    }

    const SESSION: &str = "11111111-2222-4333-8444-555555555555";
    const SNAPSHOT: &str = "{\"type\":\"file-history-snapshot\",\"messageId\":\"m\"}\n";

    /// A line of the session's transcript.
    fn entry(text: &str) -> String {
        format!("{{\"type\":\"user\",\"sessionId\":\"{SESSION}\",\"text\":\"{text}\"}}\n")
    }

    /// A sandboxed session's home with its transcript folder, and the folder of the user's own
    /// home its transcript goes back to.
    struct Copying {
        scratch: PathBuf,
        home: Home,
        relative: PathBuf,
        users: PathBuf,
        id: SessionId,
    }

    impl Copying {
        fn new(test: &str) -> Self {
            let scratch =
                std::env::temp_dir().join(format!("edualc-{test}-{}", std::process::id()));
            let home = Home(scratch.join("home"));
            let relative = Path::new("projects").join("-work");
            std::fs::create_dir_all(home.path().join(&relative)).expect("a transcript folder");
            Self {
                users: scratch.join("users").join(&relative),
                scratch,
                home,
                relative,
                id: SessionId::try_from(SESSION.to_owned()).expect("a session id"),
            }
        }

        fn name() -> String {
            format!("{SESSION}.jsonl")
        }

        fn sandboxed(&self) -> PathBuf {
            self.home.path().join(&self.relative).join(Self::name())
        }

        fn users_copy(&self) -> PathBuf {
            self.users.join(Self::name())
        }

        fn write_users(&self, text: &str) {
            std::fs::create_dir_all(&self.users).expect("the user's folder");
            std::fs::write(self.users_copy(), text).expect("the user's copy");
        }

        fn copy_back(&self) -> Result<CopyBack, Error> {
            copy_back(&self.home, &self.relative, &self.users, &self.id)
        }

        fn users_text(&self) -> String {
            std::fs::read_to_string(self.users_copy()).expect("the user's copy")
        }

        /// What the user's folder holds: no copy is ever left there unplaced.
        fn users_folder(&self) -> Vec<String> {
            let mut names: Vec<String> = std::fs::read_dir(&self.users)
                .into_iter()
                .flatten()
                .filter_map(Result::ok)
                .map(|entry| entry.file_name().to_string_lossy().into_owned())
                .collect();
            names.sort();
            names
        }

        fn done(self) {
            std::fs::remove_dir_all(&self.scratch).expect("the scratch folder goes");
        }
    }

    #[test]
    fn a_transcript_goes_back_where_the_user_has_none() {
        let copying = Copying::new("back-new");
        let transcript = entry("a") + SNAPSHOT + &entry("b");
        std::fs::write(copying.sandboxed(), &transcript).expect("a transcript");
        assert_eq!(copying.copy_back().expect("copied"), CopyBack::Copied);
        assert_eq!(copying.users_text(), transcript);
        assert_eq!(copying.users_folder(), [Copying::name()]);
        let mode = std::fs::metadata(copying.users_copy())
            .expect("the copy")
            .permissions();
        assert_eq!(
            std::os::unix::fs::PermissionsExt::mode(&mode) & 0o777,
            0o600
        );
        copying.done();
    }

    #[test]
    fn a_transcript_goes_back_over_the_copy_it_continues() {
        let copying = Copying::new("back-over");
        copying.write_users(&entry("a"));
        std::fs::write(copying.sandboxed(), entry("a") + &entry("b")).expect("a transcript");
        assert_eq!(copying.copy_back().expect("copied"), CopyBack::Copied);
        assert_eq!(copying.users_text(), entry("a") + &entry("b"));
        assert_eq!(copying.copy_back().expect("compared"), CopyBack::Unchanged);
        assert_eq!(copying.users_folder(), [Copying::name()]);
        copying.done();
    }

    /// Resumed in a terminal meanwhile, the user's copy went further, or its own way: it stays
    /// as it is, and a copy that went its own way is told apart, naming the one left behind.
    #[test]
    fn the_users_copy_that_went_on_stays() {
        let copying = Copying::new("back-kept");
        std::fs::write(copying.sandboxed(), entry("a") + &entry("b")).expect("a transcript");
        let left = Diverged {
            left: copying.sandboxed(),
        };
        for (users, outcome) in [
            (entry("a") + &entry("c"), CopyBack::Diverged(left)),
            (
                entry("a") + &entry("b") + &entry("c"),
                CopyBack::UsersGoesOn,
            ),
        ] {
            copying.write_users(&users);
            assert_eq!(copying.copy_back().expect("compared"), outcome);
            assert_eq!(copying.users_text(), users);
            assert_eq!(copying.users_folder(), [Copying::name()]);
        }
        copying.done();
    }

    /// The process stopped as it wrote its last line.
    #[test]
    fn an_unended_last_line_stays_behind() {
        let copying = Copying::new("back-unended");
        std::fs::write(copying.sandboxed(), entry("a") + "{\"type\":\"us").expect("a transcript");
        assert_eq!(copying.copy_back().expect("copied"), CopyBack::Copied);
        assert_eq!(copying.users_text(), entry("a"));
        copying.done();
    }

    #[test]
    fn what_isnt_the_sessions_transcript_isnt_copied() {
        let other = "{\"type\":\"user\",\"sessionId\":\"99999999-2222-4333-8444-555555555555\"}\n";
        for (test, second) in [
            ("back-other", other),
            ("back-text", "export PATH=/planted:$PATH\n"),
            ("back-array", "[]\n"),
            ("back-blank", "\n"),
        ] {
            let copying = Copying::new(test);
            std::fs::write(copying.sandboxed(), entry("a") + second).expect("a transcript");
            let refused = copying.copy_back();
            assert!(
                matches!(refused, Err(Error::NotTheTranscript { line: 2, .. })),
                "{test}: {refused:?}"
            );
            assert!(
                copying.users_folder().is_empty(),
                "{test}: nothing left behind"
            );
            copying.done();
        }
    }

    /// A sandbox could otherwise have a file it can't read, linked from its home, copied out to
    /// where a later resume copies it in.
    #[test]
    fn a_link_or_a_second_name_isnt_copied() {
        let copying = Copying::new("back-link");
        let outside = copying.scratch.join("secret.jsonl");
        std::fs::write(&outside, entry("secret")).expect("a file outside");
        std::os::unix::fs::symlink(&outside, copying.sandboxed()).expect("a planted link");
        assert!(matches!(copying.copy_back(), Err(Error::NotOwnFile(_))));
        std::fs::remove_file(copying.sandboxed()).expect("the link goes");
        std::fs::hard_link(&outside, copying.sandboxed()).expect("a second name");
        assert!(matches!(copying.copy_back(), Err(Error::NotOwnFile(_))));
        std::fs::remove_file(copying.sandboxed()).expect("the second name goes");
        // A pipe would hold the copy at its opening until something wrote to it.
        rustix::fs::mknodat(
            CWD,
            copying.sandboxed(),
            FileType::Fifo,
            Mode::from_raw_mode(0o600),
            0,
        )
        .expect("a planted pipe");
        assert!(matches!(copying.copy_back(), Err(Error::NotOwnFile(_))));
        assert!(copying.users_folder().is_empty());
        copying.done();
    }

    #[test]
    fn a_session_that_wrote_nothing_leaves_the_users_home_alone() {
        let copying = Copying::new("back-none");
        assert_eq!(copying.copy_back().expect("nothing"), CopyBack::NoneWritten);
        assert!(!copying.users.exists(), "no folder was made for it");
        copying.done();
    }

    /// Resuming, the user's copy is taken where it goes on from the home's (the session went on
    /// in a terminal), and where the home's is no plain file; two copies that went their own
    /// ways are told apart, and the home's is kept.
    #[test]
    fn copying_in_takes_the_users_copy_only_where_it_goes_on() {
        let copying = Copying::new("in-goes-on");
        let name = Copying::name();
        let copy_in_over = |users: &str| {
            copying.write_users(users);
            copy_in(
                &copying.users_copy(),
                &copying.home,
                &copying.relative,
                &name,
            )
            .expect("compared")
        };
        assert_eq!(copy_in_over(&entry("a")), CopyIn::Take, "the home has none");
        std::fs::write(copying.sandboxed(), entry("a") + &entry("b")).expect("the home's copy");
        let further = entry("a") + &entry("b") + &entry("c");
        assert_eq!(copy_in_over(&further), CopyIn::Take);
        let same = entry("a") + &entry("b");
        assert_eq!(copy_in_over(&same), CopyIn::Keep);
        assert_eq!(copy_in_over(&entry("a")), CopyIn::Keep, "shorter");
        let apart = entry("a") + &entry("c");
        assert_eq!(copy_in_over(&apart), CopyIn::Diverged);
        let apart_and_longer = entry("a") + &entry("c") + &entry("d");
        assert_eq!(copy_in_over(&apart_and_longer), CopyIn::Diverged);
        std::fs::remove_file(copying.sandboxed()).expect("the copy goes");
        std::os::unix::fs::symlink(copying.users_copy(), copying.sandboxed()).expect("a link");
        assert_eq!(
            copy_in_over(&entry("a")),
            CopyIn::Take,
            "a link is replaced"
        );
        copying.done();
    }

    fn append(path: &Path, text: &str) {
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(path)
            .expect("a file to append to");
        file.write_all(text.as_bytes()).expect("appended");
    }

    fn follower_of(copying: &Copying) -> Follower {
        Follower::between(
            copying.home.clone(),
            copying.relative.clone(),
            copying.users.clone(),
            copying.id.clone(),
        )
    }

    /// While the session runs, its ended lines reach the user's copy as they come: from none,
    /// then line by line, a line still being written left for its end. The following stops
    /// where the user's copy went on otherwise, and leaves it as it is.
    #[test]
    fn the_users_copy_follows_the_running_session() {
        let copying = Copying::new("follow");
        let mut follower = follower_of(&copying);
        assert_eq!(follower.step(), Followed::Unchanged, "nothing written yet");
        std::fs::write(copying.sandboxed(), entry("a")).expect("a transcript");
        assert!(matches!(follower.step(), Followed::Copied(_)));
        assert_eq!(copying.users_text(), entry("a"));
        append(&copying.sandboxed(), &(entry("b") + "{\"type\":\"us"));
        assert!(matches!(follower.step(), Followed::Copied(_)));
        assert_eq!(copying.users_text(), entry("a") + &entry("b"));
        assert_eq!(
            follower.step(),
            Followed::Unchanged,
            "the unended line waits"
        );
        let rest = format!("er\",\"sessionId\":\"{SESSION}\"}}\n");
        append(&copying.sandboxed(), &rest);
        assert!(matches!(follower.step(), Followed::Copied(_)));
        let ended = format!("{{\"type\":\"user\",\"sessionId\":\"{SESSION}\"}}\n");
        assert_eq!(copying.users_text(), entry("a") + &entry("b") + &ended);
        append(&copying.users_copy(), &entry("from a terminal"));
        append(&copying.sandboxed(), &entry("c"));
        assert_eq!(follower.step(), Followed::Stopped);
        assert_eq!(follower.step(), Followed::Stopped, "for good");
        assert!(copying.users_text().ends_with(&entry("from a terminal")));
        assert_eq!(copying.users_folder(), [Copying::name()]);
        copying.done();
    }

    /// A line that isn't the session's, or a transcript replaced by a link, stops the following
    /// before anything of it reaches the user's copy.
    #[test]
    fn what_isnt_the_sessions_stops_the_following() {
        let copying = Copying::new("follow-stop");
        let mut follower = follower_of(&copying);
        std::fs::write(copying.sandboxed(), entry("a")).expect("a transcript");
        assert!(matches!(follower.step(), Followed::Copied(_)));
        append(&copying.sandboxed(), "export PATH=/planted:$PATH\n");
        assert_eq!(follower.step(), Followed::Stopped);
        assert_eq!(copying.users_text(), entry("a"));
        let copying_link = Copying::new("follow-link");
        let mut follower = follower_of(&copying_link);
        std::fs::write(copying_link.sandboxed(), entry("a")).expect("a transcript");
        assert!(matches!(follower.step(), Followed::Copied(_)));
        let outside = copying_link.scratch.join("outside.jsonl");
        std::fs::write(&outside, entry("a") + &entry("secret")).expect("a file outside");
        std::fs::remove_file(copying_link.sandboxed()).expect("the transcript goes");
        std::os::unix::fs::symlink(&outside, copying_link.sandboxed()).expect("a planted link");
        assert_eq!(follower.step(), Followed::Stopped);
        assert_eq!(copying_link.users_text(), entry("a"));
        copying.done();
        copying_link.done();
    }
}
