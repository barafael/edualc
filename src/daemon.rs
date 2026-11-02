//! The daemon (`edualc --daemon`): runs Claude Code's processes for the windows, and does for
//! them what their sandbox keeps them from (opening editors, notifying, stopping sessions that
//! run elsewhere). One per user, started by the first window, gone a while after the last window
//! and session are. Sessions outlive the window that started them: closing it leaves them
//! running, and a window opened later attaches to them.
//!
//! Its parts are actors in the manner of "More Actors with Tokio": each is its data, run by an
//! event loop that is given its channels and gives its data back, and spawned by its caller.
//! Their logic is sans-io (each says what is to be done; its loop does it), and their channels
//! make no cycle:
//!
//! ```text
//! socket ─▶ Connection (a window) ─▶ Hub (the sessions) ─▶ Session ─▶ a window's outbox ─▶ socket
//!                │                        └──────────── Hosted (the list) ─────────▶ outboxes
//!                └──────────────────── Done (what the desktop did) ────────────────▶ outbox
//! ```
//!
//! The hub learns that a session ended as its task does, not by a message back. A window
//! hanging up detaches its sessions, which run on; one left alone and quiet for [`QUIET`] ends,
//! its input closing. When no window and no session are left for [`IDLE`], the daemon ends.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::ChildStdin;
use tokio::sync::{mpsc, watch};
use tokio::task::JoinSet;
use tokio::time::Instant;

use crate::buses::{self, Buses};
use crate::claude::{Conversation, Launch};
use crate::decode::{self, Refusal};
use crate::desktop;
use crate::external::wire::{ControlPeek, PeekedKind, PeekedSubtype, SessionId, TaskId};
use crate::ipc::{
    self, Act, ActFailure, ActId, Deframer, FolderUse, FromDaemon, Generation, Hosted, HostedId,
    LineSplitter, PasteFailure, SandboxAccess, StartFailure, Stream, ToDaemon,
};
use crate::mirror::{self, Mirror};
use crate::relay::{self, FromProcess, Relay, Work};
use crate::{home, spawn, trust};

/// How long the daemon waits, with no window and no session, before it ends.
const IDLE: Duration = Duration::from_secs(5);
/// How long a window has to say hello.
const HELLO: Duration = Duration::from_secs(2);
/// How long a process whose input closed gets to end before it is killed.
const GRACE: Duration = Duration::from_secs(3);
/// How long a session no window shows may stay quiet before it is ended.
const QUIET: Duration = Duration::from_secs(60 * 60);
/// How often a sandboxed session's transcript is followed into the user's copy: a stat of it
/// when nothing changed.
const FOLLOW_EVERY: Duration = Duration::from_secs(2);
/// Messages queued for a window, requests from it, commands for a session.
const QUEUE: usize = 1024;
/// Permission requests kept for a window to come: more unanswered than this is a runaway.
const MAX_PENDING: usize = 64;
/// How long the clipboard's owner gets to hand over what it holds.
const CLIPBOARD: Duration = Duration::from_secs(5);
/// The most of the clipboard's list of types read: a few dozen names.
const MAX_CLIPBOARD_TYPES_BYTES: usize = 64 * 1024;

/// Runs the daemon; it returns as it ends.
pub(crate) fn run() -> std::process::ExitCode {
    // What the daemon and its children make (its socket, the proxies') is the user's alone.
    rustix::process::umask(rustix::fs::Mode::RWXG | rustix::fs::Mode::RWXO);
    // Closed to the user's other processes: its memory, and the descriptors it holds (the
    // claude processes' and the image decoder's pipes). Windows check it by user, not program.
    if let Err(error) =
        rustix::process::set_dumpable_behavior(rustix::process::DumpableBehavior::NotDumpable)
    {
        tracing::warn!(%error, "the daemon stays open to the user's other processes");
    }
    let folder = match ipc::Folder::open() {
        Ok(folder) => folder,
        Err(error) => {
            tracing::error!(%error, "no private place for the daemon");
            return std::process::ExitCode::FAILURE;
        }
    };
    let listener = match ipc::Listener::bind(&folder) {
        Ok(Some(listener)) => listener,
        Ok(None) => {
            tracing::info!("another daemon runs");
            return std::process::ExitCode::SUCCESS;
        }
        Err(error) => {
            tracing::error!(%error, "couldn't listen");
            return std::process::ExitCode::FAILURE;
        }
    };
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            tracing::error!(%error, "no runtime");
            return std::process::ExitCode::FAILURE;
        }
    };
    match runtime.block_on(serve(listener, folder.path())) {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(error) => {
            tracing::error!(%error, "the daemon failed");
            std::process::ExitCode::FAILURE
        }
    }
}

/// Takes windows as they connect, until no window and no session is left for [`IDLE`].
async fn serve(listener: ipc::Listener, folder: &std::path::Path) -> Result<(), ipc::Error> {
    let (listener, _lock) = listener.into_tokio()?;
    // Ended with the daemon.
    let (buses, _proxies) = buses::start(folder).await;
    let (hub, requests) = mpsc::channel(QUEUE);
    let (running, mut sessions) = watch::channel(0usize);
    let hub_task = tokio::spawn(Hub::default().event_loop(
        requests,
        running,
        tokio::runtime::Handle::current(),
    ));
    let mut windows = JoinSet::new();
    let mut last_window = WindowId::default();
    let mut quiet_since = Some(Instant::now());
    loop {
        let idle = async {
            match quiet_since {
                Some(since) => tokio::time::sleep_until(since + IDLE).await,
                None => std::future::pending().await,
            }
        };
        tokio::select! {
            accepted = ipc::AsyncChannel::accept(&listener) => match accepted {
                Ok(channel) => {
                    last_window = last_window.next();
                    windows.spawn(window(channel, last_window, buses.clone(), hub.clone()));
                    quiet_since = None;
                }
                Err(error) => tracing::warn!(%error, "refused a connection"),
            },
            Some(ended) = windows.join_next() => {
                if let Err(error) = ended {
                    tracing::error!(%error, "a window's connection failed");
                }
            },
            changed = sessions.changed() => {
                if changed.is_err() {
                    tracing::error!("the hub ended");
                    break;
                }
            },
            () = idle => {
                tracing::info!("no window and no session for a while: ending");
                break;
            },
        }
        let quiet = windows.is_empty() && *sessions.borrow() == 0;
        quiet_since = match (quiet, quiet_since) {
            (true, None) => Some(Instant::now()),
            (true, since @ Some(_)) => since,
            (false, _) => None,
        };
    }
    // The hub's requests close; it ends its sessions, and is awaited.
    drop(hub);
    if let Err(error) = hub_task.await {
        tracing::error!(%error, "the hub failed");
    }
    Ok(())
}

/// A window's connection: hello, then its requests to its [`Connection`], and what the hub,
/// sessions and the desktop say back, until it hangs up.
async fn window(channel: ipc::AsyncChannel, id: WindowId, buses: Buses, hub: mpsc::Sender<ToHub>) {
    let (mut reading, mut writing) = channel.split();
    let mut deframer = Deframer::default();
    let hello = tokio::time::timeout(HELLO, ipc::recv::<ToDaemon>(&mut reading, &mut deframer));
    match hello.await {
        Ok(Ok(ToDaemon::Hello { protocol })) => {
            let answer = FromDaemon::Hello {
                protocol: ipc::PROTOCOL,
                buses,
            };
            if let Err(error) = ipc::send(&mut writing, &answer).await {
                tracing::warn!(%error, "a window left as it came");
                return;
            }
            if protocol != ipc::PROTOCOL {
                tracing::warn!(protocol, "a window of another version");
                return;
            }
        }
        Ok(Ok(_)) | Ok(Err(_)) | Err(_) => {
            tracing::warn!("a connection that didn't say hello");
            return;
        }
    }
    let (requests, received) = mpsc::channel(QUEUE);
    let (outbox, answers) = mpsc::channel(QUEUE);
    // The socket's two directions: pumps, not actors.
    let reader = tokio::spawn(async move {
        loop {
            match ipc::recv::<ToDaemon>(&mut reading, &mut deframer).await {
                Ok(request) => {
                    if requests.send(request).await.is_err() {
                        return;
                    }
                }
                Err(error) => {
                    tracing::info!(%error, "a window hung up");
                    return;
                }
            }
        }
    });
    let writer = tokio::spawn(write(writing, answers));
    let connection = Connection::new(id).event_loop(received, outbox, hub).await;
    tracing::info!(window = ?connection.id, "a window's connection ended");
    reader.abort();
    if let Err(error) = writer.await {
        tracing::debug!(%error, "the writer ended");
    }
}

/// Writes what is queued for a window, until everything that could queue more is gone.
async fn write(mut writing: ipc::Writing, mut answers: mpsc::Receiver<FromDaemon>) {
    while let Some(answer) = answers.recv().await {
        if let Err(error) = ipc::send(&mut writing, &answer).await {
            tracing::info!(%error, "a window stopped listening");
            return;
        }
    }
}

/// What a window's requests come to.
#[derive(Debug, PartialEq, Eq)]
enum Effect {
    /// For the hub.
    Hub(Ask),
    /// For the desktop, answered to the window.
    Act { request: ActId, act: Act },
    /// What was asked can't be done; the window is told.
    Refuse {
        key: Generation,
        failure: StartFailure,
    },
}

/// What a window asks of the hub, by its keys.
#[derive(Debug, PartialEq, Eq)]
enum Ask {
    Start { key: Generation, launch: Launch },
    Line { key: Generation, line: String },
    End { key: Generation },
    Attach { key: Generation, hosted: HostedId },
    Detach { key: Generation },
    Identify { key: Generation, session: SessionId },
}

/// The daemon's id for a window's connection.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord)]
struct WindowId(u64);

impl WindowId {
    fn next(self) -> Self {
        Self(self.0 + 1)
    }
}

/// A window: the keys it shows sessions under, and the line it is sending each in pieces.
#[derive(Debug, PartialEq, Eq)]
struct Connection {
    id: WindowId,
    keys: BTreeMap<Generation, String>,
}

impl Connection {
    fn new(id: WindowId) -> Self {
        Self {
            id,
            keys: BTreeMap::new(),
        }
    }

    async fn event_loop(
        mut self,
        mut requests: mpsc::Receiver<ToDaemon>,
        outbox: mpsc::Sender<FromDaemon>,
        hub: mpsc::Sender<ToHub>,
    ) -> Self {
        let window = self.id;
        if hub
            .send(ToHub::Joined {
                window,
                outbox: outbox.clone(),
            })
            .await
            .is_err()
        {
            return self;
        }
        while let Some(request) = requests.recv().await {
            for effect in self.handle(request) {
                match effect {
                    Effect::Hub(ask) => {
                        let outbox = outbox.clone();
                        if hub
                            .send(ToHub::Asked {
                                window,
                                ask,
                                outbox,
                            })
                            .await
                            .is_err()
                        {
                            return self;
                        }
                    }
                    Effect::Act { request, act } => {
                        let outbox = outbox.clone();
                        tokio::spawn(act_for(request, act, outbox));
                    }
                    Effect::Refuse { key, failure } => {
                        tell(&outbox, FromDaemon::Failed { key, failure }).await;
                    }
                }
            }
        }
        // The window hung up: its sessions run on, shown nowhere.
        if hub.send(ToHub::Left { window }).await.is_err() {
            tracing::debug!("the hub was gone as a window left");
        }
        self
    }

    /// What a request comes to (sans-io).
    fn handle(&mut self, request: ToDaemon) -> Vec<Effect> {
        match request {
            ToDaemon::Start { key, .. } | ToDaemon::Attach { key, .. }
                if self.keys.contains_key(&key) =>
            {
                vec![Effect::Refuse {
                    key,
                    failure: StartFailure::KeyInUse,
                }]
            }
            ToDaemon::Start { key, launch } => {
                self.keys.insert(key, String::new());
                vec![Effect::Hub(Ask::Start { key, launch })]
            }
            ToDaemon::Attach { key, hosted } => {
                self.keys.insert(key, String::new());
                vec![Effect::Hub(Ask::Attach { key, hosted })]
            }
            ToDaemon::Input { key, text, last } => {
                let Some(line) = self.keys.get_mut(&key) else {
                    return Vec::new();
                };
                if line.len() + text.len() > ipc::MAX_LINE {
                    line.clear();
                    return vec![Effect::Refuse {
                        key,
                        failure: StartFailure::LineTooLong,
                    }];
                }
                line.push_str(&text);
                if last {
                    let line = std::mem::take(line);
                    vec![Effect::Hub(Ask::Line { key, line })]
                } else {
                    Vec::new()
                }
            }
            ToDaemon::End { key } => match self.keys.remove(&key) {
                Some(_) => vec![Effect::Hub(Ask::End { key })],
                None => Vec::new(),
            },
            ToDaemon::Detach { key } => match self.keys.remove(&key) {
                Some(_) => vec![Effect::Hub(Ask::Detach { key })],
                None => Vec::new(),
            },
            ToDaemon::Identify { key, session } if self.keys.contains_key(&key) => {
                vec![Effect::Hub(Ask::Identify { key, session })]
            }
            // A second hello, or a key not in use.
            ToDaemon::Hello { .. } | ToDaemon::Identify { .. } => Vec::new(),
            ToDaemon::Act { request, act } => vec![Effect::Act { request, act }],
        }
    }
}

/// Does what the desktop is asked, on a thread of its own (it may take a while), and tells the
/// window.
async fn act_for(request: ActId, act: Act, outbox: mpsc::Sender<FromDaemon>) {
    if let Act::PickFolder(folder_use) = act {
        let title = match folder_use {
            FolderUse::Project => "Open a folder",
            FolderUse::Sandbox(SandboxAccess::Read) => "A folder Claude may read in the sandbox",
            FolderUse::Sandbox(SandboxAccess::Write) => "A folder Claude may change in the sandbox",
        };
        let picked = tokio::task::spawn_blocking(move || {
            rfd::FileDialog::new().set_title(title).pick_folder()
        })
        .await;
        let folder = picked.unwrap_or_else(|error| {
            tracing::error!(%error, "the folder picker failed");
            None
        });
        tell(&outbox, FromDaemon::Picked { request, folder }).await;
        return;
    }
    if act == Act::PasteImage {
        let pasted = paste_image().await;
        tell_pasted(&outbox, request, &pasted).await;
        return;
    }
    if let Act::CheckTrust { folder } = act {
        // What the folder would run is read only for one that isn't trusted: it is asked about.
        let read = tokio::task::spawn_blocking(move || {
            let accepted = trust::accepted_in_terminal(&folder);
            let runs = if accepted {
                Vec::new()
            } else {
                trust::what_runs(&folder)
            };
            (accepted, runs)
        })
        .await;
        let (accepted, runs) = read.unwrap_or_else(|error| {
            tracing::error!(%error, "reading the folder's trust failed");
            (false, Vec::new())
        });
        tell(
            &outbox,
            FromDaemon::Trusted {
                request,
                accepted,
                runs,
            },
        )
        .await;
        return;
    }
    let done = tokio::task::spawn_blocking(move || perform(act)).await;
    let result = done.unwrap_or_else(|error| {
        tracing::error!(%error, "doing what the desktop was asked failed");
        Err(ActFailure::Crashed)
    });
    tell(&outbox, FromDaemon::Done { request, result }).await;
}

/// The clipboard's image, read with `wl-paste` (the window, confined, never holds its raw
/// bytes) and decoded in the confined helper (see `decode`).
async fn paste_image() -> ipc::Pasted {
    let listed = clipboard(&["--list-types"], MAX_CLIPBOARD_TYPES_BYTES).await?;
    let format = String::from_utf8_lossy(&listed)
        .lines()
        .find_map(|line| line.trim().parse::<decode::Format>().ok())
        .ok_or(PasteFailure::NoImage)?;
    let limit = decode::MAX_INPUT_BYTES + 1;
    let bytes = clipboard(&["--no-newline", "--type", format.as_ref()], limit).await?;
    if bytes.len() > decode::MAX_INPUT_BYTES {
        return Err(PasteFailure::Refused(Refusal::TooManyBytes));
    }
    decode::decode(&bytes).await.map_err(|error| match error {
        decode::Error::Refused(refusal) => PasteFailure::Refused(refusal),
        error @ (decode::Error::Start(_)
        | decode::Error::NoPipes
        | decode::Error::Hand(_)
        | decode::Error::Read(_)
        | decode::Error::Slow
        | decode::Error::Failed(_)
        | decode::Error::TooMuch
        | decode::Error::Unreadable(_)
        | decode::Error::Malformed) => PasteFailure::Decoder(error.to_string()),
    })
}

/// What `wl-paste` gives with `args`, up to `limit` bytes (reading stops there).
async fn clipboard(args: &[&str], limit: usize) -> Result<Vec<u8>, PasteFailure> {
    let failed = |error: std::io::Error| PasteFailure::Clipboard(error.to_string());
    let mut child = tokio::process::Command::new("wl-paste")
        .args(args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .map_err(failed)?;
    let Some(output) = child.stdout.take() else {
        return Err(PasteFailure::Clipboard("wl-paste has no output".to_owned()));
    };
    let reading = async {
        let mut bytes = Vec::new();
        let cap = u64::try_from(limit).unwrap_or(u64::MAX);
        output
            .take(cap)
            .read_to_end(&mut bytes)
            .await
            .map_err(failed)?;
        if bytes.len() >= limit {
            // The caller refuses it; what is left isn't waited for (the child is killed as it
            // is dropped).
            return Ok(bytes);
        }
        let status = child.wait().await.map_err(failed)?;
        // It fails when the clipboard is empty, or holds nothing of the type asked.
        if status.success() {
            Ok(bytes)
        } else {
            Err(PasteFailure::NoImage)
        }
    };
    tokio::time::timeout(CLIPBOARD, reading)
        .await
        .map_err(|_elapsed| {
            PasteFailure::Clipboard("the clipboard's owner didn't hand it over in time".to_owned())
        })?
}

/// The pasted image to a window, in pieces that fit messages.
async fn tell_pasted(outbox: &mpsc::Sender<FromDaemon>, request: ActId, pasted: &ipc::Pasted) {
    let encoded = match postcard::to_stdvec(pasted) {
        Ok(encoded) => encoded,
        Err(error) => {
            tracing::error!(%error, "couldn't encode a pasted image");
            return;
        }
    };
    let mut pieces = encoded.chunks(ipc::MAX_CHUNK).peekable();
    while let Some(piece) = pieces.next() {
        let last = pieces.peek().is_none();
        let piece = piece.to_vec();
        tell(
            outbox,
            FromDaemon::Pasted {
                request,
                piece,
                last,
            },
        )
        .await;
    }
}

/// Tells a window something; a window gone isn't told.
async fn tell(outbox: &mpsc::Sender<FromDaemon>, message: FromDaemon) {
    if outbox.send(message).await.is_err() {
        tracing::debug!("a window gone");
    }
}

/// Does what the desktop is asked (it may take a while: a session stopping elsewhere).
fn perform(act: Act) -> Result<(), ActFailure> {
    match act {
        Act::OpenIn { opener, folder } => {
            if !spawn::is_folder(&folder) {
                return Err(ActFailure::NotAFolder(folder));
            }
            desktop::open_in(opener, &folder)?;
        }
        Act::Notify { summary, body } => desktop::notify(&summary, &body)?,
        Act::OpenUrl(link) => {
            // Claude writes the links: a file or another app's scheme could run what it wrote.
            let web = ["https://", "http://", "mailto:"].iter().any(|scheme| {
                link.get(..scheme.len())
                    .is_some_and(|start| start.eq_ignore_ascii_case(scheme))
            });
            if !web {
                return Err(ActFailure::NotAWebLink(link));
            }
            open::that_detached(&link).map_err(|error| ActFailure::Open {
                reason: error.to_string(),
                link,
            })?;
        }
        Act::FocusWindowOf { pid } => desktop::focus_window_of(pid)?,
        Act::StopSession(running) => desktop::stop_session(&running)?,
        // Answered as `Picked`, `Pasted` and `Trusted`, in `act_for`.
        Act::PickFolder(_) | Act::PasteImage | Act::CheckTrust { .. } => {}
    }
    Ok(())
}

/// What the hub is told.
#[derive(Debug)]
enum ToHub {
    /// A window connected: it hears the list of sessions from now on.
    Joined {
        window: WindowId,
        outbox: mpsc::Sender<FromDaemon>,
    },
    Asked {
        window: WindowId,
        ask: Ask,
        outbox: mpsc::Sender<FromDaemon>,
    },
    /// A window hung up: its sessions run on, shown nowhere.
    Left { window: WindowId },
}

/// A session as the hub keeps it.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Kept {
    cwd: PathBuf,
    session: Option<SessionId>,
    /// The window and key it is shown under.
    shown: Option<(WindowId, Generation)>,
}

/// The sessions the daemon runs, and which window shows each.
#[derive(Debug, Default, PartialEq, Eq)]
struct Hub {
    sessions: BTreeMap<HostedId, Kept>,
    last: HostedId,
}

/// What the hub's own decisions come to.
#[derive(Debug, PartialEq, Eq)]
enum Step {
    /// A new session, as `id`, shown under the window's key.
    Start { id: HostedId, launch: Launch },
    /// To a session.
    Tell { id: HostedId, command: Order },
    /// Something the window is told.
    Answer(FromDaemon),
    /// The list changed: every window hears it.
    Changed,
}

/// What the hub tells a session (`Attach` carries the outbox, added by the loop).
#[derive(Debug, PartialEq, Eq)]
enum Order {
    Line(String),
    Attach { key: Generation },
    Detach,
    End,
}

impl Hub {
    async fn event_loop(
        mut self,
        mut requests: mpsc::Receiver<ToHub>,
        running: watch::Sender<usize>,
        runtime: tokio::runtime::Handle,
    ) -> Self {
        let mut windows: BTreeMap<WindowId, mpsc::Sender<FromDaemon>> = BTreeMap::new();
        let mut orders: BTreeMap<HostedId, mpsc::Sender<Command>> = BTreeMap::new();
        let mut tasks = JoinSet::new();
        // Who wrote to whom, for sandboxed sessions to answer (see `relay::Replies`); it ends
        // as the sessions holding its sender do.
        let (replies, replying) = mpsc::channel(QUEUE);
        tokio::spawn(keep_replies(replying));
        loop {
            let steps = tokio::select! {
                request = requests.recv() => match request {
                    Some(ToHub::Joined { window, outbox }) => {
                        let list = FromDaemon::Hosted(self.list());
                        tell(&outbox, list).await;
                        windows.insert(window, outbox);
                        Vec::new()
                    }
                    Some(ToHub::Asked { window, ask, outbox }) => {
                        let steps = self.handle(window, ask);
                        let mut rest = Vec::new();
                        for step in steps {
                            match step {
                                Step::Start { id, launch } => {
                                    let (order, commands) = mpsc::channel(QUEUE);
                                    orders.insert(id, order);
                                    let key = self.key_of(id);
                                    let shown = key.map(|key| (key, outbox.clone()));
                                    let replies = replies.clone();
                                    tasks.spawn(host(id, launch, runtime.clone(), commands, shown, replies));
                                }
                                Step::Tell { id, command: Order::Attach { key } } => {
                                    send(&orders, id, Command::Attach { key, outbox: outbox.clone() }).await;
                                }
                                Step::Answer(answer) => tell(&outbox, answer).await,
                                other @ (Step::Tell { .. } | Step::Changed) => rest.push(other),
                            }
                        }
                        rest
                    }
                    Some(ToHub::Left { window }) => {
                        windows.remove(&window);
                        self.left(window)
                    }
                    // The daemon ends: the sessions' orders close, and each ends.
                    None => break,
                },
                Some(ended) = tasks.join_next() => {
                    match ended {
                        Ok(session) => {
                            orders.remove(&session.id);
                            self.ended(session.id)
                        }
                        Err(error) => {
                            tracing::error!(%error, "a session failed");
                            Vec::new()
                        }
                    }
                },
            };
            for step in steps {
                match step {
                    Step::Tell { id, command } => {
                        let command = match command {
                            Order::Line(line) => Command::Line(line),
                            Order::Detach => Command::Detach,
                            Order::End => {
                                // Its orders closing ends it.
                                orders.remove(&id);
                                continue;
                            }
                            Order::Attach { .. } => continue,
                        };
                        send(&orders, id, command).await;
                    }
                    Step::Changed => {
                        let list = self.list();
                        for outbox in windows.values() {
                            tell(outbox, FromDaemon::Hosted(list.clone())).await;
                        }
                    }
                    Step::Start { .. } | Step::Answer(_) => {}
                }
            }
            running.send_replace(self.sessions.len());
        }
        orders.clear();
        while tasks.join_next().await.is_some() {}
        self
    }

    fn key_of(&self, id: HostedId) -> Option<Generation> {
        self.sessions
            .get(&id)
            .and_then(|kept| kept.shown)
            .map(|(_, key)| key)
    }

    /// The session a window shows under `key`.
    fn shown_as(&self, window: WindowId, key: Generation) -> Option<HostedId> {
        self.sessions
            .iter()
            .find(|(_, kept)| kept.shown == Some((window, key)))
            .map(|(id, _)| *id)
    }

    /// What a window's ask comes to (sans-io).
    fn handle(&mut self, window: WindowId, ask: Ask) -> Vec<Step> {
        match ask {
            Ask::Start { key, launch } => {
                self.last = self.last.next();
                let id = self.last;
                self.sessions.insert(
                    id,
                    Kept {
                        cwd: launch.cwd.clone(),
                        // A new session or a copy is listed once it has a transcript to
                        // open, as its window identifies it (`Identify`).
                        session: match &launch.conversation {
                            Conversation::Continue(session) => Some(session.clone()),
                            Conversation::New(_) | Conversation::Fork { .. } => None,
                        },
                        shown: Some((window, key)),
                    },
                );
                vec![Step::Start { id, launch }, Step::Changed]
            }
            Ask::Line { key, line } => match self.shown_as(window, key) {
                Some(id) => vec![Step::Tell {
                    id,
                    command: Order::Line(line),
                }],
                None => Vec::new(),
            },
            Ask::End { key } => match self.shown_as(window, key) {
                Some(id) => vec![Step::Tell {
                    id,
                    command: Order::End,
                }],
                None => Vec::new(),
            },
            Ask::Detach { key } => match self.shown_as(window, key) {
                Some(id) => {
                    if let Some(kept) = self.sessions.get_mut(&id) {
                        kept.shown = None;
                    }
                    vec![
                        Step::Tell {
                            id,
                            command: Order::Detach,
                        },
                        Step::Changed,
                    ]
                }
                None => Vec::new(),
            },
            Ask::Attach { key, hosted } => match self.sessions.get_mut(&hosted) {
                Some(kept) if kept.shown.is_none() => {
                    kept.shown = Some((window, key));
                    vec![
                        Step::Tell {
                            id: hosted,
                            command: Order::Attach { key },
                        },
                        Step::Changed,
                    ]
                }
                Some(_) | None => vec![Step::Answer(FromDaemon::Failed {
                    key,
                    failure: StartFailure::NotHosted,
                })],
            },
            Ask::Identify { key, session } => match self.shown_as(window, key) {
                Some(id) => {
                    if let Some(kept) = self.sessions.get_mut(&id) {
                        kept.session = Some(session);
                    }
                    vec![Step::Changed]
                }
                None => Vec::new(),
            },
        }
    }

    /// A window hung up: what it showed is shown nowhere, and runs on.
    fn left(&mut self, window: WindowId) -> Vec<Step> {
        let mut steps = Vec::new();
        for (id, kept) in &mut self.sessions {
            if kept.shown.is_some_and(|(shown_in, _)| shown_in == window) {
                kept.shown = None;
                steps.push(Step::Tell {
                    id: *id,
                    command: Order::Detach,
                });
            }
        }
        if !steps.is_empty() {
            steps.push(Step::Changed);
        }
        steps
    }

    fn ended(&mut self, id: HostedId) -> Vec<Step> {
        if self.sessions.remove(&id).is_some() {
            vec![Step::Changed]
        } else {
            Vec::new()
        }
    }

    fn list(&self) -> Vec<Hosted> {
        self.sessions
            .iter()
            .map(|(id, kept)| Hosted {
                id: *id,
                session: kept.session.clone(),
                cwd: kept.cwd.clone(),
                attached: kept.shown.is_some(),
            })
            .collect()
    }
}

async fn send(orders: &BTreeMap<HostedId, mpsc::Sender<Command>>, id: HostedId, command: Command) {
    if let Some(order) = orders.get(&id)
        && order.send(command).await.is_err()
    {
        tracing::debug!(?id, "an order for a session that ended");
    }
}

/// Starts a session's process and runs it, telling the window it is shown in.
async fn host(
    id: HostedId,
    launch: Launch,
    runtime: tokio::runtime::Handle,
    commands: mpsc::Receiver<Command>,
    shown: Option<(Generation, mpsc::Sender<FromDaemon>)>,
    replies: mpsc::Sender<ToReplies>,
) -> Session {
    let session = Session::new(id);
    let spawned = tokio::task::spawn_blocking(move || spawn::spawn(&launch, &runtime)).await;
    let failure = match spawned {
        Ok(Ok(spawned)) => return session.event_loop(spawned, commands, shown, replies).await,
        Ok(Err(error)) => StartFailure::from(error),
        Err(error) => {
            tracing::error!(%error, "starting a session failed");
            StartFailure::Crashed
        }
    };
    if let Some((key, outbox)) = shown {
        tell(&outbox, FromDaemon::Failed { key, failure }).await;
    }
    session
}

/// What a session's loop is told.
#[derive(Debug)]
enum Command {
    Line(String),
    /// Shown in a window from now on, under its key.
    Attach {
        key: Generation,
        outbox: mpsc::Sender<FromDaemon>,
    },
    Detach,
}

/// Where a session's process is in its life (sans-io: the event loop says what happened and
/// when, and is told what to do).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    Running,
    /// Its input closed at this time; it is killed if it hasn't ended a while after.
    Closing(Instant),
    Killed,
}

/// A session's process, as the daemon runs it.
#[derive(Debug, PartialEq, Eq)]
struct Session {
    id: HostedId,
    phase: Phase,
    /// What it asked the user and wasn't answered: asked again of a window that attaches.
    pending: Pending,
    lines_in: u64,
    /// Since when no window shows it.
    alone_since: Option<Instant>,
    /// When it last said or was told something.
    active: Instant,
}

impl Session {
    fn new(id: HostedId) -> Self {
        Self {
            id,
            phase: Phase::Running,
            pending: Pending::default(),
            lines_in: 0,
            alone_since: None,
            active: Instant::now(),
        }
    }

    /// The input closed: the process is to end.
    fn input_closed(&mut self, now: Instant) {
        if self.phase == Phase::Running {
            self.phase = Phase::Closing(now);
        }
    }

    /// When the process is to be killed, if it is to be.
    fn kill_at(&self) -> Option<Instant> {
        match self.phase {
            Phase::Closing(since) => Some(since + GRACE),
            Phase::Running | Phase::Killed => None,
        }
    }

    /// When, alone and quiet, it is to be ended.
    fn quiet_end_at(&self) -> Option<Instant> {
        let alone = self.alone_since?;
        (self.phase == Phase::Running).then(|| alone.max(self.active) + QUIET)
    }

    /// Lines to the process's input, its output to the window that shows it, and its end, until
    /// it ends.
    async fn event_loop(
        mut self,
        spawned: spawn::Spawned,
        mut commands: mpsc::Receiver<Command>,
        shown: Option<(Generation, mpsc::Sender<FromDaemon>)>,
        replies: mpsc::Sender<ToReplies>,
    ) -> Self {
        // The session's temporary folder goes when this ends.
        let spawn::Spawned {
            mut child,
            enforcement,
            scratch: _kept_while_running,
            record,
            diverged,
            name,
        } = spawned;
        let Some(pid) = child.id() else {
            return self;
        };
        if let Some(diverged) = diverged {
            tell_diverged(diverged, shown.as_ref()).await;
        }
        // A sandboxed session's record, mirrored for others to message it by, until this ends.
        let mut mirror = record
            .as_ref()
            .and_then(|record| match Mirror::start(record, pid) {
                Ok(mirror) => Some(mirror),
                Err(error) => {
                    tracing::warn!(%error, id = ?self.id, "other sessions can't message this one");
                    None
                }
            });
        // A sandboxed session's messages to other sessions, carried as the user allows them.
        let mut relaying = record
            .as_ref()
            .map(|record| Relay::new(record.name.clone(), record.consent));
        // A sandboxed session's transcript, followed into the user's copy as it is written.
        let mut follower = record
            .as_ref()
            .map(|record| home::Follower::of(record.home.clone(), &record.cwd, record.id.clone()));
        let mut following = tokio::time::interval(FOLLOW_EVERY);
        following.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let mut shown = shown;
        if shown.is_none() {
            self.alone_since = Some(Instant::now());
        }
        if let Some((key, outbox)) = &shown {
            tell(
                outbox,
                FromDaemon::Started {
                    key: *key,
                    pid,
                    enforcement,
                    turns: 0,
                },
            )
            .await;
        }
        let mut stdin = child.stdin.take();
        let (lines, mut output) = mpsc::channel(QUEUE);
        let pumps = [
            child
                .stdout
                .take()
                .map(|out| tokio::spawn(pump(out, Stream::Out, lines.clone()))),
            child
                .stderr
                .take()
                .map(|err| tokio::spawn(pump(err, Stream::Err, lines.clone()))),
        ];
        drop(lines);
        let mut commands_open = true;
        let mut output_open = true;
        let code = loop {
            let deadline = self
                .kill_at()
                .or_else(|| self.quiet_end_at())
                .map(tokio::time::sleep_until);
            let deadline = async {
                match deadline {
                    Some(sleep) => sleep.await,
                    None => std::future::pending().await,
                }
            };
            tokio::select! {
                command = commands.recv(), if commands_open && self.phase == Phase::Running => match command {
                    Some(Command::Line(line)) => {
                        self.lines_in += 1;
                        self.active = Instant::now();
                        self.pending.answered(&line);
                        if let Some(relaying) = relaying.as_mut() {
                            relaying.window_said(&line);
                        }
                        let line = initialize_with(&line, relaying.is_some()).unwrap_or(line);
                        if let Some(writing) = stdin.as_mut()
                            && write_line(writing, &line).await.is_err()
                        {
                            tracing::debug!(id = ?self.id, "claude's input closed");
                        }
                    }
                    Some(Command::Attach { key, outbox }) => {
                        self.alone_since = None;
                        let turns = self.pending.turns;
                        tell(&outbox, FromDaemon::Started { key, pid, enforcement, turns }).await;
                        // What it asked meanwhile, asked again.
                        for line in self.pending.lines() {
                            send_line(&outbox, key, Stream::Out, line).await;
                        }
                        shown = Some((key, outbox));
                    }
                    Some(Command::Detach) => {
                        shown = None;
                        self.alone_since = Some(Instant::now());
                    }
                    None => {
                        commands_open = false;
                        stdin = None;
                        self.input_closed(Instant::now());
                    }
                },
                line = output.recv(), if output_open => match line {
                    Some((stream, line)) => {
                        self.active = Instant::now();
                        let mut relayed = match (stream, relaying.as_mut()) {
                            (Stream::Out, Some(relaying)) => relaying.process_said(&line),
                            (Stream::Out, None) => {
                                // Outside the sandbox: whom it writes to, for them to answer.
                                for to in relay::sent_to(&line) {
                                    let to = named(&to).await;
                                    let wrote = ToReplies::Wrote { from: name.clone(), to };
                                    if let Err(error) = replies.try_send(wrote) {
                                        tracing::debug!(%error, "a message written isn't kept for answering");
                                    }
                                }
                                FromProcess::PassOn
                            }
                            (Stream::Err, _) => FromProcess::PassOn,
                        };
                        if let FromProcess::Consult(consult) = relayed {
                            relayed = match relaying.as_mut() {
                                Some(relaying) => {
                                    let sender = relaying.sender().clone();
                                    let to = named(consult.to()).await;
                                    let may = may_answer(&replies, sender, to).await;
                                    relaying.consulted(consult, may)
                                }
                                None => FromProcess::PassOn,
                            };
                        }
                        let answer = match relayed {
                            // Decided above: a message consulted about is passed on, answered
                            // or carried out by now.
                            FromProcess::PassOn | FromProcess::Consult(_) => None,
                            FromProcess::Answer(answer) => Some(answer),
                            FromProcess::Work(Work { job, reply }) => {
                                let places = relay::Places::of_user();
                                let sender = relaying.as_ref().map(|relaying| relaying.sender().clone());
                                let failed = || relay::Outcome::Refused(relay::Refusal::Failed);
                                let carried = tokio::task::spawn_blocking(move || match sender {
                                    Some(sender) => relay::carry_out(&places, &sender, &job),
                                    None => failed(),
                                })
                                .await;
                                Some(reply.answer(&carried.unwrap_or_else(|_| failed())))
                            }
                        };
                        if let Some(answer) = answer {
                            // The relay's own: answered to the process, shown to no window.
                            if let Some(writing) = stdin.as_mut()
                                && write_line(writing, &answer).await.is_err()
                            {
                                tracing::debug!(id = ?self.id, "claude's input closed");
                            }
                            continue;
                        }
                        if stream == Stream::Out {
                            self.pending.said(&line);
                        }
                        if let Some((key, outbox)) = &shown {
                            send_line(outbox, *key, stream, &line).await;
                        }
                    }
                    None => output_open = false,
                },
                () = follow(mirror.as_mut()) => {}
                _ = following.tick(), if follower.is_some() => {
                    if let Some(mut stepping) = follower.take() {
                        let stepped = tokio::task::spawn_blocking(move || {
                            let followed = stepping.step();
                            (stepping, followed)
                        })
                        .await;
                        follower = match stepped {
                            Ok((_, home::Followed::Stopped)) => None,
                            Ok((stepping, home::Followed::Unchanged | home::Followed::Copied(_))) => {
                                Some(stepping)
                            }
                            Err(error) => {
                                tracing::error!(%error, "following a transcript failed");
                                None
                            }
                        };
                    }
                }
                ended = child.wait() => break ended.ok().and_then(|status| status.code()),
                () = deadline => {
                    if self.phase == Phase::Running {
                        // Alone and quiet for long: ended as a closed input ends it.
                        stdin = None;
                        self.input_closed(Instant::now());
                    } else {
                        self.phase = Phase::Killed;
                        if let Err(error) = child.start_kill() {
                            tracing::warn!(%error, id = ?self.id, "couldn't kill claude");
                        }
                    }
                }
            }
        };
        // What it wrote goes out before its end is told.
        for pump in pumps.into_iter().flatten() {
            if let Err(error) = pump.await {
                tracing::debug!(%error, "an output pump failed");
            }
        }
        while let Some((stream, line)) = output.recv().await {
            if let Some((key, outbox)) = &shown {
                send_line(outbox, *key, stream, &line).await;
            }
        }
        if let Some((key, outbox)) = &shown {
            tell(outbox, FromDaemon::Ended { key: *key, code }).await;
        }
        drop(mirror);
        if let Some(mirror::Source { home, id, cwd, .. }) = record {
            let copied =
                tokio::task::spawn_blocking(move || home::copy_back_transcript(&home, &cwd, &id))
                    .await;
            match copied {
                Ok(Ok(home::CopyBack::Diverged(diverged))) => {
                    tell_diverged(diverged, shown.as_ref()).await;
                }
                Ok(Ok(outcome)) => tracing::info!(?outcome, id = ?self.id, "transcript back home"),
                Ok(Err(error)) => {
                    tracing::warn!(%error, id = ?self.id, "the transcript stays in the session's home");
                }
                Err(error) => tracing::error!(%error, "copying a transcript back failed"),
            }
        }
        self
    }
}

/// Tells the user of a conversation whose transcript went two ways in and out of the sandbox
/// (see `home::Diverged`), in the window that shows it and on the desktop: the window shows
/// only the copy the session goes on from, and what only the other has would otherwise go
/// unnoticed.
/// What the sessions tell or ask the keeper of who wrote to whom (see `relay::Replies`).
#[derive(Debug)]
enum ToReplies {
    /// Session `from`, outside the sandbox, sent a message to `to`.
    Wrote {
        from: spawn::MessagingName,
        to: String,
    },
    /// Whether sandboxed session `sender` may answer `to` without the user being asked.
    MayAnswer {
        sender: spawn::MessagingName,
        to: String,
        answer: tokio::sync::oneshot::Sender<bool>,
    },
}

/// Keeps who wrote to whom for the daemon's sessions, until none of them can tell it more.
async fn keep_replies(mut requests: mpsc::Receiver<ToReplies>) {
    let mut replies = relay::Replies::default();
    while let Some(request) = requests.recv().await {
        match request {
            ToReplies::Wrote { from, to } => {
                replies.wrote(from, &to, std::time::Instant::now());
            }
            ToReplies::MayAnswer { sender, to, answer } => {
                let may = replies.may_answer(&sender, &to, std::time::Instant::now());
                if answer.send(may).is_err() {
                    tracing::debug!("a session stopped waiting to hear whether it may answer");
                }
            }
        }
    }
}

/// Whether sandboxed session `sender` may answer `to` without the user being asked; not, where
/// the keeper can't tell.
async fn may_answer(
    replies: &mpsc::Sender<ToReplies>,
    sender: spawn::MessagingName,
    to: String,
) -> bool {
    let (answer, answered) = tokio::sync::oneshot::channel();
    if replies
        .send(ToReplies::MayAnswer { sender, to, answer })
        .await
        .is_err()
    {
        return false;
    }
    answered.await.unwrap_or(false)
}

/// The name of the session `to` addresses, read off the running sessions' records (see
/// `relay::named`); as it is, where they can't be read.
async fn named(to: &str) -> String {
    let places = relay::Places::of_user();
    let address = to.to_owned();
    tokio::task::spawn_blocking(move || relay::named(&places, &address))
        .await
        .unwrap_or_else(|_| to.to_owned())
}

async fn tell_diverged(
    diverged: home::Diverged,
    shown: Option<&(Generation, mpsc::Sender<FromDaemon>)>,
) {
    tracing::warn!(left = %diverged.left.display(), "a conversation went two ways");
    if let Some((key, outbox)) = shown {
        let left = diverged.left.clone();
        tell(outbox, FromDaemon::Diverged { key: *key, left }).await;
    }
    let body = format!(
        "Two copies of a session's transcript went on separately, in and out of edualc's \
         sandbox. What only one of them has is kept in {}",
        diverged.left.display()
    );
    let told =
        tokio::task::spawn_blocking(move || desktop::notify("A conversation went two ways", &body))
            .await;
    match told {
        Ok(Ok(())) => {}
        Ok(Err(error)) => tracing::warn!(%error, "couldn't tell of a conversation gone two ways"),
        Err(error) => tracing::error!(%error, "telling of a conversation gone two ways failed"),
    }
}

/// The window's `initialize` request as the daemon passes it on, the system prompt rendered
/// afresh on every request: Claude Code otherwise records a session's first and reuses it on
/// every resume, so that the note appended for the sandbox would follow the session's first
/// launch, not this one. With `relaying`, edualc's messaging tool registered too (see
/// `relay`). Any other line, none.
fn initialize_with(line: &str, relaying: bool) -> Option<String> {
    // Most lines are prompts and answers, some with pasted images: not parsed for this.
    if !line.contains("\"initialize\"") {
        return None;
    }
    let mut message: serde_json::Map<String, serde_json::Value> =
        serde_json::from_str(line).ok()?;
    let control_request =
        message.get("type").and_then(serde_json::Value::as_str) == Some("control_request");
    let request = message
        .get_mut("request")
        .and_then(serde_json::Value::as_object_mut)?;
    let initialize =
        request.get("subtype").and_then(serde_json::Value::as_str) == Some("initialize");
    if !(control_request && initialize) {
        return None;
    }
    request.insert(
        "systemPromptSnapshot".to_owned(),
        serde_json::Value::Bool(false),
    );
    if relaying {
        for (field, value) in relay::registration() {
            request.insert(field.to_owned(), value);
        }
    }
    serde_json::to_string(&message).ok()
}

/// What a window attaching is told again (sans-io): how many turns are under way, the
/// process's latest `init` (its session, model and mode), what runs in the background, then the
/// permission requests it asked and nothing answered yet. Only the messages' kind and request
/// id are read, the rest is passed on as it is.
#[derive(Debug, Default, PartialEq, Eq)]
struct Pending {
    init: Option<String>,
    /// The latest list of background tasks: each one names them all.
    tasks: Option<String>,
    /// What was said about each task begun in this turn and not yet ended: a foreground one is
    /// in no list of background tasks.
    open_tasks: BTreeMap<TaskId, Vec<String>>,
    /// How far each running task got, as last said; a background one runs on after its turn.
    progress: BTreeMap<TaskId, String>,
    asked: BTreeMap<String, String>,
    /// Turns under way: prompts written, and not yet ended by a result. Counted as the window
    /// counts them, for one that attaches meanwhile.
    turns: usize,
}

impl Pending {
    /// A line the process wrote.
    fn said(&mut self, line: &str) {
        let Ok(peek) = serde_json::from_str::<ControlPeek>(line) else {
            return;
        };
        match (peek.kind, peek.subtype, peek.request_id) {
            (PeekedKind::System, Some(PeekedSubtype::Init), _) => {
                self.init = Some(line.to_owned());
            }
            (PeekedKind::System, Some(PeekedSubtype::BackgroundTasksChanged), _) => {
                self.tasks = Some(line.to_owned());
            }
            (PeekedKind::System, Some(PeekedSubtype::TaskStarted), _) => {
                if let Some(task) = peek.task_id
                    && self.open_tasks.len() < MAX_PENDING
                {
                    self.open_tasks.insert(task, vec![line.to_owned()]);
                }
            }
            (PeekedKind::System, Some(PeekedSubtype::TaskUpdated), _) => {
                if let Some(said) = peek.task_id.and_then(|task| self.open_tasks.get_mut(&task))
                    && said.len() < MAX_PENDING
                {
                    said.push(line.to_owned());
                }
            }
            (PeekedKind::System, Some(PeekedSubtype::TaskNotification), _) => {
                if let Some(task) = peek.task_id {
                    self.open_tasks.remove(&task);
                    self.progress.remove(&task);
                }
            }
            (PeekedKind::System, Some(PeekedSubtype::TaskProgress), _) => {
                if let Some(task) = peek.task_id
                    && (self.progress.len() < MAX_PENDING || self.progress.contains_key(&task))
                {
                    self.progress.insert(task, line.to_owned());
                }
            }
            (PeekedKind::ControlRequest, _, Some(id)) if self.asked.len() < MAX_PENDING => {
                self.asked.insert(id, line.to_owned());
            }
            (PeekedKind::ControlCancelRequest, _, Some(id)) => {
                self.asked.remove(&id);
            }
            (PeekedKind::Result, _, _) => {
                self.turns = self.turns.saturating_sub(1);
                // No foreground task outlives its turn, and the list names those that run on.
                self.open_tasks.clear();
            }
            _ => {}
        }
    }

    /// A line the window wrote to the process.
    fn answered(&mut self, line: &str) {
        let Ok(peek) = serde_json::from_str::<ControlPeek>(line) else {
            return;
        };
        match peek.kind {
            PeekedKind::ControlResponse => {
                if let Some(id) = peek.response.and_then(|response| response.request_id) {
                    self.asked.remove(&id);
                }
            }
            PeekedKind::User => self.turns = self.turns.saturating_add(1),
            PeekedKind::System
            | PeekedKind::ControlRequest
            | PeekedKind::ControlCancelRequest
            | PeekedKind::Result
            | PeekedKind::Other => {}
        }
    }

    fn lines(&self) -> impl Iterator<Item = &String> {
        self.init
            .iter()
            .chain(self.tasks.iter())
            .chain(self.open_tasks.values().flatten())
            .chain(self.progress.values())
            .chain(self.asked.values())
    }
}

async fn write_line(stdin: &mut ChildStdin, line: &str) -> std::io::Result<()> {
    stdin.write_all(line.as_bytes()).await?;
    stdin.write_all(b"\n").await?;
    stdin.flush().await
}

/// Follows a sandboxed session's record, if there is one to follow; else waits forever.
async fn follow(mirror: Option<&mut Mirror>) {
    match mirror {
        Some(mirror) => mirror.follow().await,
        None => std::future::pending().await,
    }
}

/// A process's output, line by line (split sans-io), to its session.
async fn pump(
    mut output: impl tokio::io::AsyncRead + Unpin,
    stream: Stream,
    lines: mpsc::Sender<(Stream, String)>,
) {
    let mut splitter = LineSplitter::default();
    let mut bytes = vec![0u8; 64 * 1024];
    loop {
        let read = match output.read(&mut bytes).await {
            Ok(0) | Err(_) => break,
            Ok(read) => read,
        };
        let Some(read) = bytes.get(..read) else {
            break;
        };
        for line in splitter.push(read) {
            if lines.send((stream, line)).await.is_err() {
                return;
            }
        }
    }
    if let Some(line) = splitter.finish()
        && lines.send((stream, line)).await.is_err()
    {
        tracing::debug!("a session gone");
    }
}

/// A line to a window, in pieces that fit messages.
async fn send_line(outbox: &mpsc::Sender<FromDaemon>, key: Generation, stream: Stream, line: &str) {
    let mut pieces = ipc::chunks(line).peekable();
    while let Some(text) = pieces.next() {
        let last = pieces.peek().is_none();
        let piece = FromDaemon::Output {
            key,
            stream,
            text: text.to_owned(),
            last,
        };
        if outbox.send(piece).await.is_err() {
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::claude::FolderConfiguration;

    /// The window's `initialize`, as it writes it, goes on with the system prompt rendered
    /// afresh and the rest as it was, and for a sandboxed session with edualc's messaging tool
    /// registered, `send_message` asked about each time; no other line is touched.
    #[test]
    fn initialize_renders_the_system_prompt_afresh() {
        use crate::external::wire::{ControlRequest, Input, RequestId};
        let id = RequestId::from("gui_1".to_owned());
        let windows = serde_json::to_string(&Input::ControlRequest {
            request_id: &id,
            request: &ControlRequest::Initialize {
                per_task_stop_affordance: true,
            },
        })
        .expect("a line");
        let expected = |fields: &[(&str, serde_json::Value)]| {
            let mut expected: serde_json::Value = serde_json::from_str(&windows).expect("JSON");
            if let Some(request) = expected
                .get_mut("request")
                .and_then(serde_json::Value::as_object_mut)
            {
                for (field, value) in fields {
                    request.insert((*field).to_owned(), value.clone());
                }
            }
            expected
        };
        let passed_on = |relaying| -> serde_json::Value {
            let line = initialize_with(&windows, relaying).expect("an initialize");
            serde_json::from_str(&line).expect("JSON")
        };
        let afresh = ("systemPromptSnapshot", serde_json::Value::Bool(false));
        assert_eq!(passed_on(false), expected(std::slice::from_ref(&afresh)));
        let registered = serde_json::json!({
            "sdkMcpServers": ["edualc"],
            "sdkMcpServerConfigs": {"edualc": {"tools": [
                {"name": "send_message", "permission_policy": "always_ask"}
            ]}}
        });
        let mut fields = vec![afresh];
        if let Some(registered) = registered.as_object() {
            fields.extend(
                registered
                    .iter()
                    .map(|(key, value)| (key.as_str(), value.clone())),
            );
        }
        assert_eq!(passed_on(true), expected(&fields));
        let prompt = r#"{"type":"user","message":{"role":"user","content":"\"initialize\" it"}}"#;
        assert_eq!(initialize_with(prompt, true), None);
        let other =
            r#"{"type":"control_request","request_id":"r","request":{"subtype":"interrupt"}}"#;
        assert_eq!(initialize_with(other, true), None);
    }

    fn launch() -> Launch {
        Launch {
            cwd: PathBuf::from("/tmp"),
            conversation: Conversation::New(
                SessionId::try_from("11111111-2222-4333-8444-555555555555".to_owned())
                    .expect("a session id"),
            ),
            model: None,
            sandbox: None,
            folder_configuration: FolderConfiguration::Read,
        }
    }

    fn piece(key: Generation, text: &str, last: bool) -> ToDaemon {
        ToDaemon::Input {
            key,
            text: text.to_owned(),
            last,
        }
    }

    /// Pieces of a line make one line, for a key in use; a key used twice is refused.
    #[test]
    fn a_connection_works_out_what_requests_come_to() {
        let key = Generation::default().next();
        let unused_key = key.next();
        let mut connection = Connection::new(WindowId(1));
        assert_eq!(
            connection.handle(ToDaemon::Start {
                key,
                launch: launch()
            }),
            vec![Effect::Hub(Ask::Start {
                key,
                launch: launch()
            })]
        );
        assert_eq!(
            connection.handle(ToDaemon::Attach {
                key,
                hosted: HostedId::default().next()
            }),
            vec![Effect::Refuse {
                key,
                failure: StartFailure::KeyInUse
            }]
        );
        assert!(connection.handle(piece(key, "{\"a\":", false)).is_empty());
        assert_eq!(
            connection.handle(piece(key, "1}", true)),
            vec![Effect::Hub(Ask::Line {
                key,
                line: "{\"a\":1}".to_owned()
            })]
        );
        assert!(
            connection.handle(piece(unused_key, "x", true)).is_empty(),
            "no key, no line"
        );
        assert_eq!(
            connection.handle(ToDaemon::Detach { key }),
            vec![Effect::Hub(Ask::Detach { key })]
        );
        assert!(
            connection.handle(ToDaemon::End { key }).is_empty(),
            "let go once"
        );
    }

    /// A window leaving lets its sessions run on, shown nowhere; another window attaches to
    /// one, but not to one shown already.
    #[test]
    fn the_hub_keeps_sessions_past_their_window() {
        let (first_window, second_window) = (WindowId(1), WindowId(2));
        let first_key = Generation::default().next();
        let second_key = first_key.next();
        let mut hub = Hub::default();
        let started = hub.handle(
            first_window,
            Ask::Start {
                key: first_key,
                launch: launch(),
            },
        );
        let Some(&Step::Start { id: hosted, .. }) = started.first() else {
            panic!("no session started: {started:?}");
        };
        assert_eq!(hub.shown_as(first_window, first_key), Some(hosted));
        assert_eq!(
            hub.handle(
                second_window,
                Ask::Attach {
                    key: second_key,
                    hosted
                }
            ),
            vec![Step::Answer(FromDaemon::Failed {
                key: second_key,
                failure: StartFailure::NotHosted
            })],
            "shown in the first window"
        );
        assert_eq!(
            hub.left(first_window),
            vec![
                Step::Tell {
                    id: hosted,
                    command: Order::Detach
                },
                Step::Changed
            ]
        );
        assert!(hub.list().iter().all(|listed| !listed.attached));
        assert_eq!(
            hub.handle(
                second_window,
                Ask::Attach {
                    key: second_key,
                    hosted
                }
            ),
            vec![
                Step::Tell {
                    id: hosted,
                    command: Order::Attach { key: second_key }
                },
                Step::Changed
            ]
        );
        let session = SessionId::try_from("ee79c043-bfa7-44c0-82b6-509839a05c22".to_owned())
            .expect("a session id");
        assert_eq!(
            hub.handle(
                second_window,
                Ask::Identify {
                    key: second_key,
                    session: session.clone()
                }
            ),
            vec![Step::Changed]
        );
        assert_eq!(
            hub.list().first().and_then(|listed| listed.session.clone()),
            Some(session)
        );
        assert_eq!(
            hub.handle(second_window, Ask::End { key: second_key }),
            vec![Step::Tell {
                id: hosted,
                command: Order::End
            }]
        );
        assert_eq!(hub.ended(hosted), vec![Step::Changed]);
        assert!(hub.list().is_empty());
    }

    #[test]
    fn the_latest_init_is_told_first() {
        let mut pending = Pending::default();
        let first = r#"{"type":"system","subtype":"init","session_id":"a","model":"m1"}"#;
        let later = r#"{"type":"system","subtype":"init","session_id":"a","model":"m2"}"#;
        let asked = r#"{"type":"control_request","request_id":"r1","request":{}}"#;
        pending.said(first);
        pending.said(asked);
        pending.said(later);
        pending.said(r#"{"type":"system","subtype":"status","status":"requesting"}"#);
        assert_eq!(pending.lines().collect::<Vec<_>>(), [later, asked]);
    }

    /// A window attaching meanwhile learns what runs in the background from the latest list,
    /// which names all of it; an emptied list is told too, as nothing running.
    #[test]
    fn the_latest_background_tasks_are_told_after_the_init() {
        let mut pending = Pending::default();
        let init = r#"{"type":"system","subtype":"init","session_id":"a","model":"m"}"#;
        let two = r#"{"type":"system","subtype":"background_tasks_changed","tasks":[{"task_type":"local_agent","description":"a"},{"task_type":"local_bash","description":"b"}]}"#;
        let one = r#"{"type":"system","subtype":"background_tasks_changed","tasks":[{"task_type":"local_bash","description":"b"}]}"#;
        let none = r#"{"type":"system","subtype":"background_tasks_changed","tasks":[]}"#;
        let asked = r#"{"type":"control_request","request_id":"r1","request":{}}"#;
        pending.said(two);
        pending.said(init);
        pending.said(asked);
        pending.said(one);
        assert_eq!(pending.lines().collect::<Vec<_>>(), [init, one, asked]);
        pending.said(none);
        assert_eq!(pending.lines().collect::<Vec<_>>(), [init, none, asked]);
    }

    /// A task begun and not yet ended is told again, all that was said about it, in order and
    /// after the background list: a foreground one is in no list. Its end, or the turn's, drops
    /// it.
    #[test]
    fn open_tasks_are_told_until_they_end() {
        let mut pending = Pending::default();
        let init = r#"{"type":"system","subtype":"init","session_id":"a","model":"m"}"#;
        let list = r#"{"type":"system","subtype":"background_tasks_changed","tasks":[]}"#;
        let started = |id: &str| {
            format!(
                r#"{{"type":"system","subtype":"task_started","task_id":"{id}","is_backgrounded":false,"task_type":"local_agent","description":"d"}}"#
            )
        };
        let (agent, other) = (started("a1"), started("a2"));
        let moved = r#"{"type":"system","subtype":"task_updated","task_id":"a1","patch":{"is_backgrounded":true}}"#;
        let ended = r#"{"type":"system","subtype":"task_notification","task_id":"a2","status":"completed"}"#;
        let asked = r#"{"type":"control_request","request_id":"r1","request":{}}"#;
        for line in [init, list, &agent, &other, asked, moved] {
            pending.said(line);
        }
        assert_eq!(
            pending.lines().collect::<Vec<_>>(),
            [init, list, &agent, moved, &other, asked]
        );
        pending.said(ended);
        assert_eq!(
            pending.lines().collect::<Vec<_>>(),
            [init, list, &agent, moved, asked]
        );
        pending.said(r#"{"type":"result","subtype":"success","is_error":false}"#);
        assert_eq!(pending.lines().collect::<Vec<_>>(), [init, list, asked]);
    }

    /// Only a task's latest progress is told again, past the turn's end (a background task runs
    /// on), until the task ends.
    #[test]
    fn a_tasks_latest_progress_is_told_until_it_ends() {
        let mut pending = Pending::default();
        let progress = |tokens: u32| {
            format!(
                r#"{{"type":"system","subtype":"task_progress","task_id":"a1","description":"d","usage":{{"total_tokens":{tokens},"tool_uses":1,"duration_ms":5}}}}"#
            )
        };
        let (early, late) = (progress(10), progress(20));
        pending.said(&early);
        pending.said(&late);
        pending.said(r#"{"type":"result","subtype":"success","is_error":false}"#);
        assert_eq!(pending.lines().collect::<Vec<_>>(), [&late]);
        pending.said(r#"{"type":"system","subtype":"task_notification","task_id":"a1","status":"completed"}"#);
        assert_eq!(pending.lines().count(), 0);
    }

    /// Turns are counted as the window counts them: one for each prompt written, one fewer for
    /// each result; a `!` command, a control request or a replayed row starts none.
    #[test]
    fn turns_under_way_are_counted_for_a_window_attaching() {
        let mut pending = Pending::default();
        let prompt = r#"{"type":"user","message":{"role":"user","content":"Hi"},"parent_tool_use_id":null,"session_id":"default"}"#;
        let result = r#"{"type":"result","subtype":"success","is_error":false}"#;
        pending.answered(prompt);
        pending.answered(prompt);
        pending.answered(r#"{"type":"bash_command","command":"ls","uuid":"x"}"#);
        pending.answered(
            r#"{"type":"control_request","request_id":"gui_1","request":{"subtype":"interrupt"}}"#,
        );
        pending.said(r#"{"type":"user","message":{"role":"user","content":"<bash-input>ls</bash-input>"},"isReplay":true}"#);
        assert_eq!(pending.turns, 2);
        pending.said(result);
        assert_eq!(pending.turns, 1);
        pending.said(result);
        pending.said(result);
        assert_eq!(pending.turns, 0, "never below none");
    }

    #[test]
    fn what_was_asked_and_not_answered_is_kept() {
        let mut pending = Pending::default();
        let asked =
            r#"{"type":"control_request","request_id":"r1","request":{"subtype":"can_use_tool"}}"#;
        pending.said(asked);
        pending.said(r#"{"type":"control_request","request_id":"r2","request":{}}"#);
        pending.said(r#"{"type":"control_cancel_request","request_id":"r2"}"#);
        pending.said("not json");
        assert_eq!(pending.lines().collect::<Vec<_>>(), [asked]);
        pending.answered(
            r#"{"type":"control_response","response":{"subtype":"success","request_id":"r1"}}"#,
        );
        assert_eq!(pending.lines().count(), 0);
    }

    /// A window joining hears the list of sessions before anything else: its handshake reads
    /// the list right after the hello, to attach to a session the daemon runs rather than start
    /// it a second time.
    #[tokio::test(flavor = "current_thread")]
    async fn a_window_joining_hears_the_list_first() {
        let (hub, requests) = mpsc::channel(8);
        let (running, _sessions) = watch::channel(0usize);
        let hub_task = tokio::spawn(Hub::default().event_loop(
            requests,
            running,
            tokio::runtime::Handle::current(),
        ));
        let (outbox, mut told) = mpsc::channel(8);
        hub.send(ToHub::Joined {
            window: WindowId(1),
            outbox,
        })
        .await
        .expect("the hub listens");
        assert_eq!(told.recv().await, Some(FromDaemon::Hosted(Vec::new())));
        drop(hub);
        hub_task.await.expect("the hub ends");
    }

    #[test]
    fn a_session_ends_a_grace_after_its_input_closed_or_long_alone() {
        let mut session = Session::new(HostedId::default().next());
        assert_eq!(session.kill_at(), None);
        assert_eq!(
            session.quiet_end_at(),
            None,
            "shown, it is never ended for quiet"
        );
        let now = Instant::now();
        session.alone_since = Some(now);
        session.active = now;
        assert_eq!(session.quiet_end_at(), Some(now + QUIET));
        session.input_closed(now);
        assert_eq!(session.kill_at(), Some(now + GRACE));
        session.input_closed(now + GRACE);
        assert_eq!(
            session.kill_at(),
            Some(now + GRACE),
            "the first close counts"
        );
    }

    fn cat() -> spawn::Spawned {
        let mut command = tokio::process::Command::new("cat");
        command
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);
        spawn::Spawned {
            child: command.spawn().expect("cat runs"),
            enforcement: None,
            scratch: None,
            record: None,
            diverged: None,
            name: spawn::MessagingName::of(
                &SessionId::try_from("11111111-2222-4333-8444-555555555555".to_owned())
                    .expect("a session id"),
                std::path::Path::new("/tmp"),
            ),
        }
    }

    /// A process through the actor, shown nowhere at first: what it asked is asked again of the
    /// window that attaches, and its end is told there.
    #[tokio::test(flavor = "current_thread")]
    async fn a_session_asks_again_what_a_window_missed() {
        let (orders, commands) = mpsc::channel(8);
        let (outbox, mut answers) = mpsc::channel(32);
        let asked = r#"{"type":"control_request","request_id":"r1","request":{}}"#;
        let key = Generation::default().next();
        orders
            .send(Command::Line(asked.to_owned()))
            .await
            .expect("queued");
        // `cat` says it back, as claude would ask; then a window attaches.
        let attach = tokio::spawn(async move {
            tokio::task::yield_now().await;
            orders
                .send(Command::Attach { key, outbox })
                .await
                .expect("queued");
            orders
                .send(Command::Line("bye".to_owned()))
                .await
                .expect("queued");
        });
        let (replies, _unheard) = mpsc::channel(QUEUE);
        let session = Session::new(HostedId::default().next())
            .event_loop(cat(), commands, None, replies)
            .await;
        attach.await.expect("the attaching task");
        assert_eq!(session.lines_in, 2);
        let mut told = Vec::new();
        while let Some(answer) = answers.recv().await {
            told.push(answer);
        }
        assert!(
            matches!(told.first(), Some(FromDaemon::Started { key: started, .. }) if *started == key)
        );
        assert!(told.iter().any(|answer| matches!(
            answer,
            FromDaemon::Output { key: shown, stream: Stream::Out, text, last: true }
                if *shown == key && text == asked
        )));
        assert!(matches!(
            told.last(),
            Some(FromDaemon::Ended { key: ended, code: Some(0) }) if *ended == key
        ));
    }
}
