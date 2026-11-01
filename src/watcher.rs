//! Noticing when files other processes write change: the records of the sessions running, and
//! the transcript of one followed here. The kernel says when (inotify); nothing looks at them
//! every few seconds, so a quiet window does no work.

use std::mem::MaybeUninit;
use std::os::fd::OwnedFd;
use std::path::{Path, PathBuf};

use async_channel::Sender;
use rustix::fs::inotify::{self, CreateFlags, WatchFlags};
use rustix::io::Errno;

use crate::app::Event;

/// A file or folder watched, as its watch is named in what is noticed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Watched(i32);

/// The app's end: it adds and drops watches; a thread of its own reads what they notice and
/// sends it on as `Event::Changed`.
#[derive(Debug)]
pub(crate) struct Watcher {
    inotify: OwnedFd,
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum Error {
    #[error("couldn't watch files for changes: {0}")]
    Start(#[source] Errno),
    #[error("couldn't watch {path}: {source}")]
    Add {
        path: PathBuf,
        #[source]
        source: Errno,
    },
}

impl Watcher {
    pub(crate) fn start(events: Sender<Event>) -> Result<Self, Error> {
        let inotify = inotify::init(CreateFlags::CLOEXEC).map_err(Error::Start)?;
        // The reading thread has a descriptor of its own for the same watches; closed on exec
        // like the first (a `dup` isn't), so no program the window starts holds it.
        let reading = rustix::io::fcntl_dupfd_cloexec(&inotify, 0).map_err(Error::Start)?;
        std::thread::spawn(move || read(&reading, &events));
        Ok(Self { inotify })
    }

    /// Notices files made, written, moved or removed in `dir`.
    pub(crate) fn folder(&self, dir: &Path) -> Result<Watched, Error> {
        let flags = WatchFlags::CREATE
            | WatchFlags::MODIFY
            | WatchFlags::DELETE
            | WatchFlags::CLOSE_WRITE
            | WatchFlags::MOVED_TO
            | WatchFlags::MOVED_FROM;
        self.add(dir, flags)
    }

    /// Notices `file` being written to.
    pub(crate) fn file(&self, file: &Path) -> Result<Watched, Error> {
        self.add(file, WatchFlags::MODIFY)
    }

    pub(crate) fn forget(&self, watched: Watched) {
        if let Err(error) = inotify::remove_watch(&self.inotify, watched.0) {
            tracing::debug!(%error, "a watch that was gone already");
        }
    }

    fn add(&self, path: &Path, flags: WatchFlags) -> Result<Watched, Error> {
        inotify::add_watch(&self.inotify, path, flags)
            .map(Watched)
            .map_err(|source| Error::Add {
                path: path.to_owned(),
                source,
            })
    }
}

/// Sends on what the watches notice, until the window closes.
fn read(inotify: &OwnedFd, events: &Sender<Event>) {
    let mut buffer = [MaybeUninit::<u8>::uninit(); 4096];
    let mut reader = inotify::Reader::new(inotify, &mut buffer);
    loop {
        match reader.next() {
            Ok(noticed) => {
                if events
                    .send_blocking(Event::Changed(Watched(noticed.wd())))
                    .is_err()
                {
                    break;
                }
            }
            Err(Errno::INTR) => {}
            Err(error) => {
                tracing::warn!(%error, "stopped noticing changes");
                break;
            }
        }
    }
}
