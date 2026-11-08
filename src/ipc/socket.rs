//! The socket the windows and the daemon speak over (see the parent module): its private
//! folder, the lock deciding who is the daemon, and the checks of each peer. What goes over it
//! is `ws`'s. Linux only.

use std::os::fd::{AsFd, OwnedFd};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};

use rustix::fs::{AtFlags, FlockOperation, Mode, OFlags};
use rustix::io::Errno;
use rustix::net::{AddressFamily, SocketAddrUnix, SocketFlags, SocketType};

use super::PROTOCOL;

/// Connections waiting to be taken.
const BACKLOG: i32 = 8;

const FOLDER: &str = "edualc";
const SOCKET: &str = "edualcd.sock";
const LOCK: &str = "edualcd.lock";

#[derive(Debug, thiserror::Error)]
pub(crate) enum Error {
    #[error("XDG_RUNTIME_DIR isn't set: there is no private place for the daemon's socket")]
    NoRuntimeDir,
    #[error("{0} isn't the user's alone (it must be a folder they own, closed to others)")]
    NotPrivate(PathBuf),
    #[error("couldn't open {path}: {source}")]
    Open {
        path: PathBuf,
        #[source]
        source: Errno,
    },
    #[error("the daemon's socket: {0}")]
    Socket(#[source] Errno),
    #[error("the daemon's socket: {0}")]
    Io(#[source] std::io::Error),
    #[error(transparent)]
    WebSocket(#[from] tokio_tungstenite::tungstenite::Error),
    #[error(transparent)]
    Encode(#[from] super::EncodeError),
    #[error(transparent)]
    Unreadable(#[from] super::DecodeError),
    #[error("the other side hung up")]
    Closed,
    #[error("a peer that isn't this user's edualc (uid {uid}, pid {pid})")]
    Stranger { uid: u32, pid: i32 },
}

/// The private folder, open: what is done in it is done through this descriptor.
#[derive(Debug)]
pub(crate) struct Folder {
    fd: OwnedFd,
    path: PathBuf,
}

impl Folder {
    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    /// `$XDG_RUNTIME_DIR/edualc/protocol-<PROTOCOL>`, made if it isn't there, and checked to be
    /// the user's alone. Each protocol has a folder of its own, so a daemon of an earlier build
    /// runs on beside this build's until its sessions end, instead of keeping its windows out.
    pub(crate) fn open() -> Result<Self, Error> {
        let runtime =
            PathBuf::from(std::env::var_os("XDG_RUNTIME_DIR").ok_or(Error::NoRuntimeDir)?);
        let runtime_fd = open_folder(rustix::fs::CWD, &runtime, &runtime)?;
        let edualc = runtime.join(FOLDER);
        let edualc_fd = made_folder(&runtime_fd, FOLDER, &edualc)?;
        let name = format!("protocol-{PROTOCOL}");
        let path = edualc.join(&name);
        let fd = made_folder(&edualc_fd, &name, &path)?;
        Ok(Self { fd, path })
    }

    /// The daemon's log, begun anew, the user's alone.
    pub(crate) fn log_file(&self) -> Result<std::fs::File, Error> {
        let flags =
            OFlags::CREATE | OFlags::TRUNC | OFlags::WRONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC;
        let fd = rustix::fs::openat(&self.fd, "edualcd.log", flags, Mode::RUSR | Mode::WUSR)
            .map_err(|source| Error::Open {
                path: self.path.join("edualcd.log"),
                source,
            })?;
        Ok(std::fs::File::from(fd))
    }

    /// A name in the folder as a socket address that resolves through the folder's descriptor,
    /// not through the path it was found at.
    fn address(&self, name: &str) -> Result<SocketAddrUnix, Error> {
        use std::os::fd::AsRawFd;
        let through = format!("/proc/self/fd/{}/{name}", self.fd.as_raw_fd());
        SocketAddrUnix::new(through).map_err(Error::Socket)
    }
}

/// The folder `name` in `at` (found at `path`), made if it isn't there, then opened as
/// [`open_folder`] opens one.
fn made_folder(at: impl AsFd, name: &str, path: &Path) -> Result<OwnedFd, Error> {
    match rustix::fs::mkdirat(&at, name, Mode::RWXU) {
        Ok(()) | Err(Errno::EXIST) => {}
        Err(source) => {
            return Err(Error::Open {
                path: path.to_owned(),
                source,
            });
        }
    }
    open_folder(at, Path::new(name), path)
}

/// Opens a folder without following a link to it, and checks it is the user's alone; one of
/// theirs with looser permissions is closed to others first.
fn open_folder(at: impl AsFd, name: &Path, path: &Path) -> Result<OwnedFd, Error> {
    let flags = OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC | OFlags::RDONLY;
    let fd = rustix::fs::openat(at, name, flags, Mode::empty()).map_err(|source| Error::Open {
        path: path.to_owned(),
        source,
    })?;
    let stat = rustix::fs::fstat(&fd).map_err(|source| Error::Open {
        path: path.to_owned(),
        source,
    })?;
    if stat.st_uid != rustix::process::getuid().as_raw() {
        return Err(Error::NotPrivate(path.to_owned()));
    }
    if Mode::from_raw_mode(stat.st_mode).intersects(Mode::RWXG | Mode::RWXO) {
        rustix::fs::fchmod(&fd, Mode::RWXU).map_err(|source| Error::Open {
            path: path.to_owned(),
            source,
        })?;
    }
    Ok(fd)
}

/// The daemon's end: the lock it holds, and the socket it listens on.
#[derive(Debug)]
pub(crate) struct Listener {
    socket: UnixListener,
    /// Held as long as the daemon runs; the kernel lets go of it when the process ends.
    lock: OwnedFd,
}

impl Listener {
    /// Becomes the daemon: `None` if another one is.
    pub(crate) fn bind(folder: &Folder) -> Result<Option<Self>, Error> {
        let flags = OFlags::CREATE | OFlags::RDWR | OFlags::NOFOLLOW | OFlags::CLOEXEC;
        let lock = rustix::fs::openat(&folder.fd, LOCK, flags, Mode::RUSR | Mode::WUSR).map_err(
            |source| Error::Open {
                path: folder.path.join(LOCK),
                source,
            },
        )?;
        match rustix::fs::flock(&lock, FlockOperation::NonBlockingLockExclusive) {
            Ok(()) => {}
            Err(Errno::WOULDBLOCK) => return Ok(None),
            Err(source) => {
                return Err(Error::Open {
                    path: folder.path.join(LOCK),
                    source,
                });
            }
        }
        // Holding the lock: whatever socket is there is a dead daemon's.
        match rustix::fs::unlinkat(&folder.fd, SOCKET, AtFlags::empty()) {
            Ok(()) | Err(Errno::NOENT) => {}
            Err(source) => return Err(Error::Socket(source)),
        }
        let socket = rustix::net::socket_with(
            AddressFamily::UNIX,
            SocketType::STREAM,
            SocketFlags::CLOEXEC,
            None,
        )
        .map_err(Error::Socket)?;
        rustix::net::bind(&socket, &folder.address(SOCKET)?).map_err(Error::Socket)?;
        rustix::fs::chmodat(
            &folder.fd,
            SOCKET,
            Mode::RUSR | Mode::WUSR,
            AtFlags::empty(),
        )
        .map_err(Error::Socket)?;
        rustix::net::listen(&socket, BACKLOG).map_err(Error::Socket)?;
        Ok(Some(Self {
            socket: UnixListener::from(socket),
            lock,
        }))
    }

    /// For tokio: the socket, and the lock to hold as long as it is listened on.
    pub(crate) fn into_tokio(self) -> Result<(tokio::net::UnixListener, OwnedFd), Error> {
        self.socket.set_nonblocking(true).map_err(Error::Io)?;
        let socket = tokio::net::UnixListener::from_std(self.socket).map_err(Error::Io)?;
        Ok((socket, self.lock))
    }
}

/// The daemon a window reached is this user's. Its program isn't compared: the daemon makes
/// itself non-dumpable, which closes its `/proc` entries to the window; that it is the daemon
/// the socket's place says, a folder of the user's alone that only the lock's holder binds in.
fn check_daemon(fd: impl AsFd) -> Result<(), Error> {
    let (uid, pid) = peer(fd)?;
    if uid == rustix::process::getuid().as_raw() {
        Ok(())
    } else {
        Err(Error::Stranger { uid, pid })
    }
}

/// A window is this user's, running edualc: this program, or another of edualc's beside it (the
/// window's and the daemon's binaries are installed together), a build that replaced one on
/// disk counts too, and the hello's protocol says whether the two agree. A process of the
/// user's that isn't edualc, Claude Code's in the sandbox above all, may not have the daemon
/// run what it likes.
fn check_window(fd: impl AsFd) -> Result<(), Error> {
    let (uid, pid) = peer(fd)?;
    let stranger = Error::Stranger { uid, pid };
    if uid != rustix::process::getuid().as_raw() {
        return Err(stranger);
    }
    let theirs = std::fs::read_link(format!("/proc/{pid}/exe"));
    let ours = std::fs::read_link("/proc/self/exe");
    match (theirs, ours) {
        (Ok(theirs), Ok(ours)) if same_program(&theirs, &ours) || edualcs(&theirs, &ours) => Ok(()),
        _ => Err(stranger),
    }
}

/// The programs edualc is made of, by their names on disk.
const PROGRAMS: [&str; 2] = ["edualc", "edualcd"];

fn peer(fd: impl AsFd) -> Result<(u32, i32), Error> {
    let peer = rustix::net::sockopt::socket_peercred(fd).map_err(Error::Socket)?;
    Ok((peer.uid.as_raw(), peer.pid.as_raw_nonzero().get()))
}

/// A program's path without the kernel's " (deleted)", which names a program so once a new
/// build replaced it on disk.
fn undeleted(program: &Path) -> PathBuf {
    use std::os::unix::ffi::OsStrExt;
    let bytes = program.as_os_str().as_bytes();
    PathBuf::from(std::ffi::OsStr::from_bytes(
        bytes.strip_suffix(b" (deleted)").unwrap_or(bytes),
    ))
}

/// Two programs' paths name the same program.
fn same_program(theirs: &Path, ours: &Path) -> bool {
    undeleted(theirs) == undeleted(ours)
}

/// Two programs' paths name two of edualc's, in the same folder.
fn edualcs(theirs: &Path, ours: &Path) -> bool {
    let (theirs, ours) = (undeleted(theirs), undeleted(ours));
    let named = |path: &Path| {
        path.file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| PROGRAMS.contains(&name))
    };
    named(&theirs) && named(&ours) && theirs.parent() == ours.parent()
}

/// Connects to the daemon, if one is listening, checked to be this user's: the window's end,
/// which it waits on.
pub(crate) fn connect(folder: &Folder) -> Result<UnixStream, Error> {
    let fd = rustix::net::socket_with(
        AddressFamily::UNIX,
        SocketType::STREAM,
        SocketFlags::CLOEXEC,
        None,
    )
    .map_err(Error::Socket)?;
    rustix::net::connect(&fd, &folder.address(SOCKET)?).map_err(Error::Socket)?;
    check_daemon(&fd)?;
    Ok(UnixStream::from(fd))
}

/// The next peer of `listener`, once it is checked to be this user's edualc: the daemon's end.
pub(crate) async fn accept(
    listener: &tokio::net::UnixListener,
) -> Result<tokio::net::UnixStream, Error> {
    let (stream, _) = listener.accept().await.map_err(Error::Io)?;
    check_window(&stream)?;
    Ok(stream)
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use super::*;

    /// A private folder of the test's own, standing in for `$XDG_RUNTIME_DIR/edualc`.
    fn scratch(name: &str) -> Folder {
        let path = std::env::temp_dir().join(format!("edualc-ipc-{name}-{}", std::process::id()));
        std::fs::create_dir_all(&path).expect("a scratch folder");
        let fd = open_folder(rustix::fs::CWD, &path, &path).expect("our own folder");
        Folder { fd, path }
    }

    /// A new build replacing the daemon's program on disk is still this program; another
    /// program, or another place, isn't.
    #[test]
    fn a_rebuilt_program_is_the_same_program() {
        let ours = Path::new("/home/someone/edualc/target/release/edualc");
        let replaced = Path::new("/home/someone/edualc/target/release/edualc (deleted)");
        assert!(same_program(ours, ours));
        assert!(same_program(ours, replaced));
        assert!(same_program(replaced, ours));
        assert!(!same_program(Path::new("/usr/bin/python3"), ours));
        assert!(!same_program(
            Path::new("/home/someone/edualc/target/debug/edualc"),
            ours
        ));
    }

    #[test]
    fn one_daemon_binds_and_a_second_stands_back() {
        let folder = scratch("lock");
        let first = Listener::bind(&folder)
            .expect("binds")
            .expect("is the daemon");
        assert!(
            Listener::bind(&folder).expect("asks").is_none(),
            "the lock is held"
        );
        let mode = std::fs::metadata(folder.path.join(SOCKET)).expect("a socket");
        assert_eq!(mode.permissions().mode() & 0o777, 0o600);
        // The first one's socket is untouched: a client still reaches it.
        connect(&folder).expect("reaches the first");
        drop(first);
        std::fs::remove_dir_all(&folder.path).expect("the scratch folder goes");
    }

    /// The window's binary and the daemon's, installed side by side, take each other; one
    /// of them elsewhere, or another program beside them, doesn't.
    #[test]
    fn edualcs_programs_beside_each_other_take_each_other() {
        let window = Path::new("/usr/bin/edualc");
        let daemon = Path::new("/usr/bin/edualcd");
        assert!(edualcs(window, daemon));
        assert!(edualcs(Path::new("/usr/bin/edualcd (deleted)"), window));
        assert!(!edualcs(Path::new("/usr/local/bin/edualcd"), window));
        assert!(!edualcs(Path::new("/usr/bin/claude"), daemon));
    }

    #[test]
    fn a_folder_open_to_others_is_closed_and_a_link_refused() {
        let path = std::env::temp_dir().join(format!("edualc-ipc-mode-{}", std::process::id()));
        std::fs::create_dir_all(&path).expect("a folder");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o777)).expect("opened");
        open_folder(rustix::fs::CWD, &path, &path).expect("ours, closed again");
        let mode = std::fs::metadata(&path)
            .expect("there")
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o700);
        let link = path.with_extension("link");
        std::os::unix::fs::symlink(&path, &link).expect("a link");
        assert!(
            open_folder(rustix::fs::CWD, &link, &link).is_err(),
            "not followed"
        );
        std::fs::remove_file(&link).expect("the link goes");
        std::fs::remove_dir_all(&path).expect("the folder goes");
    }
}
