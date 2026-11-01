//! A sandboxed session's own Claude Code home (`CLAUDE_CONFIG_DIR`) and temporary folder. What
//! the sandbox writes there, nothing outside it reads as configuration: the user's own home
//! (`~/.claude`, `~/.claude.json`) stays read-only to the sandbox, and edualc copies into the
//! session's home, at each launch, what a session needs from it. Trust flows in, never out.
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
use std::io::Write as _;
use std::os::fd::OwnedFd;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::DirBuilderExt;
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

#[derive(Debug, thiserror::Error)]
pub(crate) enum Error {
    #[error("no folder for edualc's state: neither XDG_STATE_HOME nor HOME is set")]
    NoStateFolder,
    #[error("couldn't prepare {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("couldn't prepare {path} without following links: {source}")]
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
pub(crate) struct Home(PathBuf);

impl Home {
    /// Where the homes are.
    fn homes() -> Option<PathBuf> {
        dirs::state_dir().map(|state| state.join("edualc").join("claude"))
    }

    /// The home of sessions working in `cwd`.
    pub(crate) fn of(cwd: &Path) -> Option<Self> {
        Self::homes().map(|homes| Self(homes.join(sessions::project_name(cwd))))
    }

    /// Every project's home there is.
    pub(crate) fn all() -> Vec<Self> {
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

    pub(crate) fn path(&self) -> &Path {
        &self.0
    }

    /// Claude Code's folder of transcripts, one folder per project, in this home.
    pub(crate) fn projects(&self) -> PathBuf {
        self.0.join("projects")
    }
}

/// A session's own temporary folder, `/var/tmp/edualc-<uid>/<the id's first eight>`: short,
/// as Claude Code keeps sockets in it and a socket's path is short. On disk, as the user's
/// settings move Claude Code's temporary files there. Removed when dropped.
#[derive(Debug)]
pub(crate) struct Scratch(PathBuf);

impl Scratch {
    pub(crate) fn make(id: &SessionId) -> Result<Self, Error> {
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

    pub(crate) fn path(&self) -> &Path {
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
pub(crate) struct AccessToken(Zeroizing<String>);

impl std::fmt::Debug for AccessToken {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("AccessToken(…)")
    }
}

impl AccessToken {
    /// The user's current sign-in, unless there is none (a session then starts signed out,
    /// and says so itself) or it has run out: Claude Code renews it as it runs outside the
    /// sandbox; the sandbox doesn't get what renews it.
    pub(crate) fn current() -> Option<Self> {
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

    pub(crate) fn as_bytes(&self) -> &[u8] {
        self.0.as_bytes()
    }
}

/// Makes `home` ready for a session working in `cwd` on `conversation`, its temporary files
/// in `scratch`: seeds it afresh from the user's own home.
pub(crate) fn prepare(
    home: &Home,
    cwd: &Path,
    scratch: &Scratch,
    conversation: &Conversation,
) -> Result<(), Error> {
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

    if let Conversation::Continue(id) | Conversation::Fork { of: id, .. } = conversation {
        copy_in_transcript(&staging, &home_fd, home, id)?;
    }
    Ok(())
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

/// The project's memory as the user's sessions left it, over what sandboxed sessions wrote:
/// the user's files win by name, the index lists both.
fn merge_memory(
    staging: &Staging,
    home_fd: &OwnedFd,
    home: &Home,
    real: &Path,
    cwd: &Path,
) -> Result<(), Error> {
    let relative = Path::new("projects")
        .join(sessions::project_name(cwd))
        .join("memory");
    let source = real.join(&relative);
    let Ok(entries) = std::fs::read_dir(&source) else {
        return Ok(());
    };
    let target_path = home.path().join(&relative);
    let target = open_beneath(home_fd, &target_path, &relative)?;
    for entry in entries.filter_map(Result::ok) {
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        if !entry.file_type().is_ok_and(|kind| kind.is_file()) {
            continue;
        }
        let staged = format!("memory-{name}");
        if name == "MEMORY.md" {
            let theirs = std::fs::read_to_string(entry.path()).map_err(io(&entry.path()))?;
            let index = merged_index(&theirs, read_beneath(&target, name).as_deref());
            staging.write(&staged, index.as_bytes())?;
        } else {
            copy_tree(&entry.path(), &staging.path().join(&staged))?;
        }
        staging.move_into(&target, &target_path, &staged, name)?;
    }
    Ok(())
}

/// The user's memory index, then the lines only the sandboxed sessions' has.
fn merged_index(users: &str, sandboxed: Option<&str>) -> String {
    let mut index = users.trim_end().to_owned();
    for line in sandboxed.unwrap_or("").lines() {
        if !line.trim().is_empty() && !users.lines().any(|known| known == line) {
            index.push('\n');
            index.push_str(line);
        }
    }
    index.push('\n');
    index
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
/// unless the home has it already.
fn copy_in_transcript(
    staging: &Staging,
    home_fd: &OwnedFd,
    home: &Home,
    id: &SessionId,
) -> Result<(), Error> {
    let name = format!("{}.jsonl", id.as_str());
    let in_home = std::fs::read_dir(home.projects())
        .into_iter()
        .flatten()
        .filter_map(Result::ok)
        .any(|project| project.path().join(&name).is_file());
    if in_home {
        return Ok(());
    }
    let Some(source) = sessions::find_user_transcript(id) else {
        return Ok(());
    };
    let Some(project) = source
        .parent()
        .and_then(Path::file_name)
        .and_then(|project| project.to_str())
    else {
        return Ok(());
    };
    let relative = Path::new("projects").join(project);
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
    Ok(())
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
            merged_index(users, Some(sandboxed)),
            "- [A](a.md) — a\n- [B](b.md) — b\n- [C](c.md) — c\n"
        );
        assert_eq!(merged_index(users, None), users);
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
}
