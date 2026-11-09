//! What the daemon reads and watches for the windows: Claude Code's files (its sessions'
//! records, transcripts, projects) and edualc's settings. A page can't read them, and the native
//! window, confined, needn't: it asks (`ipc::Read`), and is told when what it watches changes
//! (`ipc::Target`), noticed by inotify, never by looking every few seconds.

use std::collections::{HashMap, HashSet};
use std::ffi::{OsStr, OsString};
use std::mem::MaybeUninit;
use std::os::fd::OwnedFd;
use std::os::unix::ffi::OsStrExt as _;
use std::path::{Path, PathBuf};

use rustix::fs::inotify::{self, CreateFlags, WatchFlags};
use rustix::io::Errno;
use tokio::sync::mpsc;

use crate::daemon::Outbox;
use crate::folder::Folder;
use crate::ipc::{ActId, Answer, FromDaemon, Place, Read, Target, Watched};
use crate::sessions::{self, ClaudeHome};
use crate::settings::{self, Settings};

/// The most sessions listed at once.
const MOST_SESSIONS: usize = 1000;
/// The most bytes of a transcript's lines sent in one message: small, so that a slow network
/// carries each well within the time a line may go silent (a frame's pings wait behind it, and
/// a window reading a piece hears nothing else meanwhile).
const PIECE_BYTES: usize = 1024 * 1024;

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
    if outbox.send(FromDaemon::Answer { request, answer }).is_err() {
        tracing::debug!("a window gone before its answer");
    }
}

/// What `read` comes to; a transcript's lines go to `pieces` as they are read, before.
fn read_for(request: ActId, read: Read, pieces: &Outbox) -> Answer {
    match read {
        Read::Live => Answer::Live(sessions::live()),
        Read::Projects => Answer::Projects(sessions::projects()),
        Read::Sessions { cwd, limit, known } => {
            Answer::Sessions(sessions::list(&cwd, limit.min(MOST_SESSIONS), known))
        }
        Read::Transcript { place, from } => {
            let path = resolve(&place);
            // Only a transcript is read, by no link (`sessions::open_transcript`): the window's
            // own reading is confined, and the daemon's isn't.
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
        Read::Saved(id) => Answer::Saved(sessions::saved(&id)),
    }
}

/// A transcript's lines to the window, in pieces that fit messages; a window gone isn't sent
/// the rest.
fn send_lines(request: ActId, lines: Vec<String>, pieces: &Outbox) {
    let mut piece = Vec::new();
    let mut bytes = 0;
    for line in lines {
        if bytes + line.len() > PIECE_BYTES && !piece.is_empty() {
            let lines = std::mem::take(&mut piece);
            if pieces.send(FromDaemon::Lines { request, lines }).is_err() {
                return;
            }
            bytes = 0;
        }
        bytes += line.len();
        piece.push(line);
    }
    if !piece.is_empty()
        && pieces
            .send(FromDaemon::Lines {
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

/// A window's watch: the folder watched for it now. A transcript is watched by its folder, not
/// itself: a file watched follows the file, and the transcript is written anew by a rename over
/// it (`home::copy_back_transcript`), which its folder sees. Before the folder is there, the
/// nearest one above it is watched, until the next one down comes.
#[derive(Debug)]
struct Watch {
    /// The transcript it is for; none for the records' folder, any change in which is told.
    file: Option<PathBuf>,
    folder: PathBuf,
    descriptor: i32,
}

impl Watch {
    /// Whether what inotify noticed in the watch's folder (in `name` there, or in the folder
    /// itself) is about its file, or a folder on the way to it.
    fn concerns(&self, name: Option<&OsStr>) -> bool {
        match (&self.file, name) {
            (None, _) | (_, None) => true,
            (Some(file), Some(name)) => file.starts_with(self.folder.join(name)),
        }
    }

    /// Whether a folder nearer its file than the one watched may have come.
    fn above_its_folder(&self) -> bool {
        self.file
            .as_ref()
            .is_some_and(|file| file.parent() != Some(self.folder.as_path()))
    }
}

/// What inotify noticed, once read off its queue.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Noticed {
    descriptor: i32,
    /// In the folder watched; none for the folder itself.
    name: Option<OsString>,
    /// The watch is gone: its folder was removed, or moved off its file system.
    gone: bool,
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
    loop {
        tokio::select! {
            order = orders.recv() => match order {
                Some(Order::Watch(watched, target)) => {
                    let watch = start(&inotify, &target);
                    let old = match watch {
                        Some(watch) => watches.insert(watched, watch),
                        None => watches.remove(&watched),
                    };
                    if let Some(old) = old {
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
            batch = noticed.recv() => {
                let Some(batch) = batch else {
                    tracing::warn!("stopped noticing changes for a window");
                    return;
                };
                // A burst of writes tells each watch once. Events lost to a full queue (told as
                // its overflow, on no watch) may have been any watch's: each is told.
                let changed: HashSet<Watched> =
                    if batch.iter().any(|noticed| noticed.descriptor == OVERFLOW) {
                        watches.keys().copied().collect()
                    } else {
                        batch
                            .iter()
                            .flat_map(|noticed| touched(&inotify, &mut watches, noticed))
                            .collect()
                    };
                for watched in changed {
                    if outbox.send(FromDaemon::Changed(watched)).is_err() {
                        return;
                    }
                }
            },
        }
    }
}

/// Batches of watches noticed and waiting for the actor.
const QUEUE: usize = 64;
/// The watch descriptor inotify tells its queue's overflow on (`IN_Q_OVERFLOW`).
const OVERFLOW: i32 = -1;

/// Sends on what inotify notices, all it has to tell at a time, until the actor ends: its end
/// of `ending` closes.
fn read(inotify: &OwnedFd, ending: &OwnedFd, noticing: &mpsc::Sender<Vec<Noticed>>) {
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
        let mut batch = Vec::new();
        loop {
            match reader.next() {
                Ok(event) => {
                    let noticed = Noticed {
                        descriptor: event.wd(),
                        name: event
                            .file_name()
                            .map(|name| OsStr::from_bytes(name.to_bytes()).to_owned()),
                        gone: event.events().contains(inotify::ReadFlags::IGNORED),
                    };
                    if !batch.contains(&noticed) {
                        batch.push(noticed);
                    }
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

/// The window's watches `noticed` is about. One whose folder went is watched again from the
/// nearest one there; one above its file's folder moves down as folders on the way come.
fn touched(
    inotify: &OwnedFd,
    watches: &mut HashMap<Watched, Watch>,
    noticed: &Noticed,
) -> Vec<Watched> {
    let named: Vec<Watched> = watches
        .iter()
        .filter(|(_, watch)| {
            watch.descriptor == noticed.descriptor
                && (noticed.gone || watch.concerns(noticed.name.as_deref()))
        })
        .map(|(watched, _)| *watched)
        .collect();
    let mut left = Vec::new();
    for watched in &named {
        let Some(watch) = watches.get_mut(watched) else {
            continue;
        };
        if !noticed.gone && !watch.above_its_folder() {
            continue;
        }
        let Some(file) = watch.file.as_ref() else {
            // The records' folder went: there is no other to watch for it.
            continue;
        };
        match nearest_folder(file) {
            Some(folder) if noticed.gone || folder != watch.folder => {
                match add(inotify, &folder, FOLDER_FLAGS) {
                    Ok(descriptor) => {
                        if !noticed.gone {
                            left.push(watch.descriptor);
                        }
                        watch.descriptor = descriptor;
                        watch.folder = folder;
                    }
                    Err(error) => tracing::debug!(%error, ?folder, "a watch stays where it was"),
                }
            }
            Some(_) => {}
            None => tracing::debug!(?file, "nowhere left to watch a transcript from"),
        }
    }
    for descriptor in left {
        forget_unused(inotify, watches, descriptor);
    }
    named
}

/// What a folder watched is watched for: anything written, made, removed or moved in it.
const FOLDER_FLAGS: WatchFlags = WatchFlags::CREATE
    .union(WatchFlags::MODIFY)
    .union(WatchFlags::DELETE)
    .union(WatchFlags::CLOSE_WRITE)
    .union(WatchFlags::MOVED_TO)
    .union(WatchFlags::MOVED_FROM);

/// The nearest folder above `file` that is there.
fn nearest_folder(file: &Path) -> Option<PathBuf> {
    file.ancestors()
        .skip(1)
        .find(|folder| folder.is_dir())
        .map(Path::to_path_buf)
}

/// Starts watching what `target` names.
fn start(inotify: &OwnedFd, target: &Target) -> Option<Watch> {
    let (file, folder) = match target {
        Target::Records => (None, sessions::records_dir()),
        Target::Transcript(place) => {
            let file = resolve(place);
            let Some(folder) = nearest_folder(&file) else {
                tracing::debug!(?target, "nowhere to watch from");
                return None;
            };
            (Some(file), folder)
        }
    };
    match add(inotify, &folder, FOLDER_FLAGS) {
        Ok(descriptor) => Some(Watch {
            file,
            folder,
            descriptor,
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
    async fn changed(outbox: &mut mpsc::UnboundedReceiver<FromDaemon>) -> Option<Watched> {
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
        let (outbox, mut told) = mpsc::unbounded_channel();
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

    /// Nothing more told within a while.
    async fn quiet(outbox: &mut mpsc::UnboundedReceiver<FromDaemon>) -> bool {
        while let Ok(told) = tokio::time::timeout(Duration::from_millis(200), outbox.recv()).await {
            if told.is_none() {
                break;
            }
        }
        tokio::time::timeout(Duration::from_millis(200), outbox.recv())
            .await
            .is_err()
    }

    /// A transcript whose folder isn't there yet is watched from above it, and moves down as the
    /// folder comes; written anew by a rename over it, it is told; another file in its folder
    /// isn't.
    #[tokio::test]
    async fn a_transcript_is_followed_through_its_folder() {
        let dir = std::env::temp_dir().join(format!("edualc-follow-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("a scratch folder");
        let project = dir.join("project");
        let path = project.join("session.jsonl");
        let (orders, received) = mpsc::channel(8);
        let (outbox, mut told) = mpsc::unbounded_channel();
        let actor = tokio::spawn(watch(received, outbox));
        let watched = Watched::new();
        let target = Target::Transcript(Place::Path(path.clone()));
        orders
            .send(Order::Watch(watched, target))
            .await
            .expect("ordered");
        tokio::time::sleep(Duration::from_millis(50)).await;
        std::fs::create_dir(&project).expect("its folder made");
        assert_eq!(changed(&mut told).await, Some(watched), "its folder came");
        assert!(quiet(&mut told).await);
        std::fs::write(&path, "{}\n").expect("written");
        assert_eq!(
            changed(&mut told).await,
            Some(watched),
            "made in its folder"
        );
        assert!(quiet(&mut told).await);
        std::fs::write(project.join("another.jsonl"), "{}\n").expect("another written");
        assert!(quiet(&mut told).await, "another file isn't it");
        let staged = dir.join("staged.jsonl");
        std::fs::write(&staged, "{}\n{}\n").expect("staged");
        std::fs::rename(&staged, &path).expect("renamed over it");
        assert_eq!(changed(&mut told).await, Some(watched), "renamed over");
        assert!(quiet(&mut told).await);
        std::fs::write(&path, "{}\n{}\n{}\n").expect("written to after");
        assert_eq!(changed(&mut told).await, Some(watched), "written to after");
        drop(orders);
        actor.await.expect("the actor ended");
        std::fs::remove_dir_all(&dir).expect("the scratch folder goes");
    }

    /// What is noticed in a folder is about a transcript if it names the transcript, or a folder
    /// on the way to it.
    #[test]
    fn what_is_noticed_concerns_a_transcript_on_its_way() {
        let watch = Watch {
            file: Some(PathBuf::from("/h/projects/p/s.jsonl")),
            folder: PathBuf::from("/h/projects"),
            descriptor: 1,
        };
        assert!(watch.concerns(Some(OsStr::new("p"))));
        assert!(!watch.concerns(Some(OsStr::new("q"))));
        assert!(!watch.concerns(Some(OsStr::new("p2"))));
        assert!(watch.concerns(None));
        assert!(watch.above_its_folder());
        let records = Watch {
            file: None,
            folder: PathBuf::from("/h/sessions"),
            descriptor: 2,
        };
        assert!(records.concerns(Some(OsStr::new("anything"))));
        assert!(!records.above_its_folder());
    }
}
