//! What the daemon reads and watches for the windows: Claude Code's files (its sessions'
//! records, transcripts, projects) and edualc's settings. A page can't read them, and the native
//! window, confined, needn't: it asks (`ipc::Read`), and is told when what it watches changes
//! (`ipc::Target`), noticed by inotify, never by looking every few seconds.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::mem::MaybeUninit;
use std::os::fd::OwnedFd;
use std::path::{Path, PathBuf};

use rustix::fs::inotify::{self, CreateFlags, WatchFlags};
use rustix::io::Errno;
use tokio::sync::mpsc;

use crate::folder::Folder;
use crate::ipc::{ActId, Answer, FromDaemon, Place, Read, Target, Watched};
use crate::sessions::{self, ClaudeHome};
use crate::settings::{self, Settings};

/// The most sessions listed at once.
const MOST_SESSIONS: usize = 1000;
/// The most bytes of a transcript's lines sent in one message.
const PIECE_BYTES: usize = 16 * 1024 * 1024;

/// Where a transcript is.
pub fn resolve(place: &Place) -> PathBuf {
    match place {
        Place::Path(path) => path.clone(),
        Place::Session { cwd, id } => sessions::locate(cwd, id),
        Place::Subagent {
            sandboxed,
            cwd,
            id,
            task,
        } => {
            let home = if *sandboxed {
                ClaudeHome::Sandboxed
            } else {
                ClaudeHome::Users
            };
            sessions::transcript_of(&sessions::written_transcript(home, cwd, id), task)
        }
    }
}

/// Reads what a window asked for, off the runtime's thread, and tells it.
pub async fn answer(request: ActId, read: Read, outbox: mpsc::Sender<FromDaemon>) {
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

/// What `read` comes to; a transcript's lines go to `pieces` as they are read, before.
fn read_for(request: ActId, read: Read, pieces: &mpsc::Sender<FromDaemon>) -> Answer {
    match read {
        Read::Live => Answer::Live(sessions::live()),
        Read::Projects => Answer::Projects(sessions::projects()),
        Read::Sessions { cwd, limit, known } => {
            Answer::Sessions(sessions::list(&cwd, limit.min(MOST_SESSIONS), known))
        }
        Read::Transcript { place, from } => {
            let path = resolve(&place);
            let read = match sessions::read_lines_from(&path, from) {
                Ok((lines, offset)) => {
                    send_lines(request, lines, pieces);
                    Ok(offset)
                }
                Err(error) => Err(error.to_string()),
            };
            Answer::Transcript { path, read }
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
    }
}

/// A transcript's lines to the window, in pieces that fit messages; a window gone isn't sent
/// the rest.
fn send_lines(request: ActId, lines: Vec<String>, pieces: &mpsc::Sender<FromDaemon>) {
    let mut piece = Vec::new();
    let mut bytes = 0;
    for line in lines {
        if bytes + line.len() > PIECE_BYTES && !piece.is_empty() {
            let lines = std::mem::take(&mut piece);
            if pieces
                .blocking_send(FromDaemon::Lines { request, lines })
                .is_err()
            {
                return;
            }
            bytes = 0;
        }
        bytes += line.len();
        piece.push(line);
    }
    if !piece.is_empty()
        && pieces
            .blocking_send(FromDaemon::Lines {
                request,
                lines: piece,
            })
            .is_err()
    {
        tracing::debug!("a window gone as its transcript was read");
    }
}

/// Keeps the settings, off the runtime's thread.
pub async fn save(settings: Settings) {
    let saved = tokio::task::spawn_blocking(move || settings::save(&settings)).await;
    match saved {
        Ok(Ok(())) => {}
        Ok(Err(error)) => tracing::warn!(%error, "couldn't keep the settings"),
        Err(error) => tracing::error!(%error, "keeping the settings failed"),
    }
}

/// What a window's watches are to do.
#[derive(Debug, PartialEq, Eq)]
pub enum Order {
    Watch(Watched, Target),
    Unwatch(Watched),
}

/// A window's watch: the file or folder watched for it now.
#[derive(Debug)]
struct Watch {
    /// The file it names, if it names one.
    file: Option<PathBuf>,
    descriptor: i32,
    /// Its file is watched, not a folder it will be in.
    on_file: bool,
}

/// A window's watches (an actor): one inotify instance, its watches named as the window named
/// them, until the window's orders end. What inotify notices is read on a thread of its own,
/// which ends as the actor does (the pipe it waits on beside inotify closes).
pub async fn watch(mut orders: mpsc::Receiver<Order>, outbox: mpsc::Sender<FromDaemon>) {
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
    loop {
        tokio::select! {
            order = orders.recv() => match order {
                Some(Order::Watch(watched, target)) => {
                    if let Some(watch) = start(&inotify, &target)
                        && let Some(old) = watches.insert(watched, watch)
                    {
                        forget_unused(&inotify, &watches, old.descriptor);
                    }
                }
                Some(Order::Unwatch(watched)) => {
                    if let Some(old) = watches.remove(&watched) {
                        forget_unused(&inotify, &watches, old.descriptor);
                    }
                }
                None => return,
            },
            descriptors = noticed.recv() => {
                let Some(descriptors) = descriptors else {
                    tracing::warn!("stopped noticing changes for a window");
                    return;
                };
                // A burst of writes tells each watch once.
                let changed: HashSet<Watched> = descriptors
                    .into_iter()
                    .flat_map(|descriptor| touched(&inotify, &mut watches, descriptor))
                    .collect();
                for watched in changed {
                    if outbox.send(FromDaemon::Changed(watched)).await.is_err() {
                        return;
                    }
                }
            },
        }
    }
}

/// Batches of watches noticed and waiting for the actor.
const QUEUE: usize = 64;

/// Sends on the watches inotify names, each once for all it has to tell at a time, until the
/// actor ends: its end of `ending` closes.
fn read(inotify: &OwnedFd, ending: &OwnedFd, noticing: &mpsc::Sender<BTreeSet<i32>>) {
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
        let mut descriptors = BTreeSet::new();
        loop {
            match reader.next() {
                Ok(event) => {
                    descriptors.insert(event.wd());
                }
                Err(Errno::INTR) => {}
                // All read, for now.
                Err(Errno::WOULDBLOCK) => {
                    if !descriptors.is_empty() && noticing.blocking_send(descriptors).is_err() {
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

/// The window's watches on `descriptor`; one on a folder its file has come to is moved to the
/// file.
fn touched(
    inotify: &OwnedFd,
    watches: &mut HashMap<Watched, Watch>,
    descriptor: i32,
) -> Vec<Watched> {
    let mut moved = Vec::new();
    let named: Vec<Watched> = watches
        .iter()
        .filter(|(_, watch)| watch.descriptor == descriptor)
        .map(|(watched, _)| *watched)
        .collect();
    for watched in &named {
        let Some(watch) = watches.get_mut(watched) else {
            continue;
        };
        if !watch.on_file
            && let Some(file) = watch.file.as_ref().filter(|file| file.is_file())
        {
            match add(inotify, file, WatchFlags::MODIFY) {
                Ok(on_file) => {
                    moved.push(watch.descriptor);
                    watch.descriptor = on_file;
                    watch.on_file = true;
                }
                Err(error) => tracing::debug!(%error, "a transcript stays watched by its folder"),
            }
        }
    }
    for descriptor in moved {
        forget_unused(inotify, watches, descriptor);
    }
    named
}

/// Starts watching what `target` names.
fn start(inotify: &OwnedFd, target: &Target) -> Option<Watch> {
    let folder_flags = WatchFlags::CREATE
        | WatchFlags::MODIFY
        | WatchFlags::DELETE
        | WatchFlags::CLOSE_WRITE
        | WatchFlags::MOVED_TO
        | WatchFlags::MOVED_FROM;
    let started = match target {
        Target::Records => add(inotify, &sessions::records_dir(), folder_flags)
            .map(|descriptor| (None, descriptor, false)),
        Target::Transcript(place) => {
            let file = resolve(place);
            if file.is_file() {
                add(inotify, &file, WatchFlags::MODIFY)
                    .map(|descriptor| (Some(file), descriptor, true))
            } else {
                // The nearest folder that will hold it.
                match file.ancestors().skip(1).take(2).find(|dir| dir.is_dir()) {
                    Some(folder) => add(inotify, folder, folder_flags),
                    None => Err(Errno::NOENT),
                }
                .map(|descriptor| (Some(file), descriptor, false))
            }
        }
    };
    match started {
        Ok((file, descriptor, on_file)) => Some(Watch {
            file,
            descriptor,
            on_file,
        }),
        Err(error) => {
            tracing::debug!(%error, ?target, "couldn't watch");
            None
        }
    }
}

fn add(inotify: &OwnedFd, path: &Path, flags: WatchFlags) -> Result<i32, Errno> {
    // Added to what is watched there already for another of the window's watches.
    inotify::add_watch(inotify, path, flags | WatchFlags::MASK_ADD)
}

/// Lets go of `descriptor` once none of the window's watches is on it.
fn forget_unused(inotify: &OwnedFd, watches: &HashMap<Watched, Watch>, descriptor: i32) {
    if watches.values().all(|watch| watch.descriptor != descriptor)
        && let Err(error) = inotify::remove_watch(inotify, descriptor)
    {
        tracing::debug!(%error, "a watch that was gone already");
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    /// The next watch told of as changed, if one is within a while.
    async fn changed(outbox: &mut mpsc::Receiver<FromDaemon>) -> Option<Watched> {
        match tokio::time::timeout(Duration::from_secs(5), outbox.recv()).await {
            Ok(Some(FromDaemon::Changed(watched))) => Some(watched),
            Ok(_) | Err(_) => None,
        }
    }

    /// A transcript not written yet is watched by its folder; once there, by itself: its being
    /// written to is told, and nothing is after it is unwatched.
    #[tokio::test]
    async fn a_transcript_is_watched_before_and_after_it_is_written() {
        let dir = std::env::temp_dir().join(format!("edualc-watch-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("a scratch folder");
        let path = dir.join("session.jsonl");
        let (orders, received) = mpsc::channel(8);
        let (outbox, mut told) = mpsc::channel(8);
        let actor = tokio::spawn(watch(received, outbox));
        let watched = Watched::new();
        let target = Target::Transcript(Place::Path(path.clone()));
        orders
            .send(Order::Watch(watched, target))
            .await
            .expect("ordered");
        // Ordered before the file comes: the order is taken first.
        tokio::time::sleep(Duration::from_millis(50)).await;
        std::fs::write(&path, "{}\n").expect("written");
        assert_eq!(
            changed(&mut told).await,
            Some(watched),
            "made in its folder"
        );
        while tokio::time::timeout(Duration::from_millis(100), told.recv())
            .await
            .is_ok()
        {}
        std::fs::write(&path, "{}\n{}\n").expect("written again");
        assert_eq!(changed(&mut told).await, Some(watched), "written to");
        orders.send(Order::Unwatch(watched)).await.expect("ordered");
        drop(orders);
        actor.await.expect("the actor ended");
        std::fs::remove_dir_all(&dir).expect("the scratch folder goes");
    }
}
