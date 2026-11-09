//! The daemon: runs Claude Code's processes for the windows, and does for them what their
//! sandbox keeps them from (stopping sessions that run elsewhere). It runs inside edualcd's
//! server, which hands it each window's line (`Daemon::window`), from this machine or another
//! alike. Sessions outlive the window that started them: closing it leaves them running, and a
//! window opened later attaches to them.
//!
//! Its parts are actors in the manner of "More Actors with Tokio": each is its data, run by an
//! event loop that is given its channels and gives its data back, and spawned by its caller.
//! Their logic is sans-io (each says what is to be done; its loop does it), and their channels
//! make no cycle:
//!
//! ```text
//! line ─▶ Connection (a window) ─▶ Hub (the sessions) ─▶ Session ─▶ a window's outbox ─▶ line
//!                │                        └──────────── Hosted (the list) ─────────▶ outboxes
//!                └──────────────────── Done (what was done) ────────────────────────▶ outbox
//! ```
//!
//! The hub learns that a session ended as its task does, not by a message back. A window
//! hanging up detaches its sessions, which run on; one left alone and quiet for [`QUIET`] ends,
//! its input closing.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::ChildStdin;
use tokio::sync::mpsc;
use tokio::task::JoinSet;
use tokio::time::Instant;

use crate::claude::{Conversation, Launch};
use crate::desktop;
use crate::external::wire::{ControlPeek, PeekedKind, PeekedSubtype, SessionId, TaskId};
use crate::ipc::{
    self, Act, ActFailure, ActId, FromDaemon, Generation, Hosted, HostedId, LineSplitter,
    StartFailure, Stream, ToDaemon, WindowKey,
};
use crate::mirror::{self, Mirror};
use crate::relay::{self, FromProcess, Relay, Work};
use crate::{files, home, sessions, settings, spawn, trust};

/// How long a window has to say hello.
const HELLO: Duration = Duration::from_secs(10);
/// How long a process whose input closed gets to end before it is killed.
const GRACE: Duration = Duration::from_secs(3);
/// How long a session no window shows may stay quiet before it is ended.
const QUIET: Duration = Duration::from_secs(60 * 60);
/// How often a sandboxed session's transcript is followed into the user's copy: a stat of it
/// when nothing changed.
const FOLLOW_EVERY: Duration = Duration::from_secs(2);
/// Requests queued from a window, commands for a session.
pub const QUEUE: usize = 1024;

/// What goes to a window, as its line takes it. Unbounded, so that a window slow to read holds
/// up neither the hub nor a session's output (and with it the other windows): its line is
/// pinged, and one that stops reading stops answering and is let go of (see edualcd's
/// `serve::carry`), and what was queued for it with it.
pub type Outbox = mpsc::UnboundedSender<FromDaemon>;
/// Permission requests kept for a window to come: more unanswered than this is a runaway.
const MAX_PENDING: usize = 64;

/// What the process edualcd's server is must be, before its runtime starts: what it makes
/// (the sessions' homes, the mirrors' sockets) the user's alone, and its memory and descriptors
/// (the token, the sessions' pipes) closed to the user's other processes, Claude Code's among
/// them.
pub fn harden() {
    rustix::process::umask(rustix::fs::Mode::RWXG | rustix::fs::Mode::RWXO);
    if let Err(error) =
        rustix::process::set_dumpable_behavior(rustix::process::DumpableBehavior::NotDumpable)
    {
        tracing::warn!(%error, "edualcd stays open to the user's other processes");
    }
}

/// The daemon, running: the hub of its sessions, which windows join by [`Daemon::window`]. It
/// runs as long as one of these is held; as the last goes, its sessions end.
#[derive(Debug, Clone)]
pub struct Daemon {
    hub: mpsc::Sender<ToHub>,
}

impl Daemon {
    /// Starts the hub on the runtime this is called in.
    pub fn start() -> Self {
        let (hub, requests) = mpsc::channel(QUEUE);
        tokio::spawn(async move {
            Hub::default()
                .event_loop(requests, tokio::runtime::Handle::current())
                .await;
            tracing::info!("the hub ended, and its sessions with it");
        });
        Self { hub }
    }

    /// A window's line: its hello, then its requests to its [`Connection`], and what the hub,
    /// sessions and the files say back, until it hangs up (`requests` closes) or stops
    /// listening (`outbox` closes).
    pub async fn window(&self, mut requests: mpsc::Receiver<ToDaemon>, outbox: Outbox) {
        let key = match tokio::time::timeout(HELLO, requests.recv()).await {
            Ok(Some(ToDaemon::Hello { protocol, window })) if protocol == ipc::PROTOCOL => window,
            Ok(Some(ToDaemon::Hello { protocol, .. })) => {
                tracing::warn!(protocol, "a window of another version");
                tell(&outbox, hello().await).await;
                return;
            }
            Ok(Some(_) | None) | Err(_) => {
                tracing::warn!("a window that didn't say hello");
                return;
            }
        };
        tell(&outbox, hello().await).await;
        let connection = Connection::new(WindowId::new(), key)
            .event_loop(requests, outbox, self.hub.clone())
            .await;
        tracing::info!(window = ?connection.id, "a window's connection ended");
    }
}

/// What a window is told as it comes: the settings, and the sessions running now.
async fn hello() -> FromDaemon {
    let (settings, live, claude_codes_sandbox) = tokio::task::spawn_blocking(|| {
        (
            settings::load().map_err(|error| error.to_string()),
            sessions::live(),
            crate::sandbox::claude_codes_own_is_on(),
        )
    })
    .await
    .unwrap_or_else(|error| (Err(error.to_string()), Vec::new(), false));
    FromDaemon::Hello {
        protocol: ipc::PROTOCOL,
        settings,
        live,
        claude_codes_sandbox,
        home: dirs::home_dir(),
    }
}

/// What a window's requests come to.
#[derive(Debug, PartialEq, Eq)]
enum Effect {
    /// For the hub.
    Hub(Ask),
    /// For the desktop, answered to the window.
    Act {
        request: ActId,
        act: Act,
    },
    /// What was asked can't be done; the window is told.
    Refuse {
        key: Generation,
        failure: StartFailure,
    },
    /// For the files, answered to the window.
    Read {
        request: ActId,
        read: ipc::Read,
    },
    Save(crate::settings::Settings),
    /// For the window's watches.
    Watch(files::Order),
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
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct WindowId(u64);

impl WindowId {
    /// A new one: random, as windows join from any task the server runs.
    fn new() -> Self {
        Self(uuid::Uuid::new_v4().as_u64_pair().0)
    }
}

/// A window: the keys it shows sessions under.
#[derive(Debug, PartialEq, Eq)]
struct Connection {
    id: WindowId,
    /// Which window it is, as it names itself.
    key: WindowKey,
    keys: std::collections::BTreeSet<Generation>,
}

impl Connection {
    fn new(id: WindowId, key: WindowKey) -> Self {
        Self {
            id,
            key,
            keys: std::collections::BTreeSet::new(),
        }
    }

    async fn event_loop(
        mut self,
        mut requests: mpsc::Receiver<ToDaemon>,
        outbox: Outbox,
        hub: mpsc::Sender<ToHub>,
    ) -> Self {
        let window = self.id;
        if hub
            .send(ToHub::Joined {
                window,
                key: self.key,
                outbox: outbox.clone(),
            })
            .await
            .is_err()
        {
            return self;
        }
        let (watching, watch_orders) = mpsc::channel(QUEUE);
        tokio::spawn(files::watch(watch_orders, outbox.clone()));
        while let Some(request) = requests.recv().await {
            for effect in self.handle(request) {
                match effect {
                    Effect::Read { request, read } => {
                        tokio::spawn(files::answer(request, read, outbox.clone()));
                    }
                    // Kept one after the other, in the order asked: saves at once would write
                    // over each other, the last asked not always the last kept.
                    Effect::Save(settings) => files::save(settings).await,
                    Effect::Watch(order) => {
                        if watching.send(order).await.is_err() {
                            tracing::debug!("a window's watches ended");
                        }
                    }
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
                if self.keys.contains(&key) =>
            {
                vec![Effect::Refuse {
                    key,
                    failure: StartFailure::KeyInUse,
                }]
            }
            ToDaemon::Start { key, launch } => {
                self.keys.insert(key);
                vec![Effect::Hub(Ask::Start { key, launch })]
            }
            ToDaemon::Attach { key, hosted } => {
                self.keys.insert(key);
                vec![Effect::Hub(Ask::Attach { key, hosted })]
            }
            ToDaemon::Input { key, .. } if !self.keys.contains(&key) => Vec::new(),
            ToDaemon::Input { key, line } if line.len() > ipc::MAX_LINE => {
                vec![Effect::Refuse {
                    key,
                    failure: StartFailure::LineTooLong,
                }]
            }
            ToDaemon::Input { key, line } => vec![Effect::Hub(Ask::Line { key, line })],
            ToDaemon::End { key } if self.keys.remove(&key) => {
                vec![Effect::Hub(Ask::End { key })]
            }
            ToDaemon::Detach { key } if self.keys.remove(&key) => {
                vec![Effect::Hub(Ask::Detach { key })]
            }
            ToDaemon::Identify { key, session } if self.keys.contains(&key) => {
                vec![Effect::Hub(Ask::Identify { key, session })]
            }
            // A second hello, or a key not in use.
            ToDaemon::Hello { .. }
            | ToDaemon::Identify { .. }
            | ToDaemon::End { .. }
            | ToDaemon::Detach { .. } => Vec::new(),
            ToDaemon::Act { request, act } => vec![Effect::Act { request, act }],
            ToDaemon::Read { request, read } => vec![Effect::Read { request, read }],
            ToDaemon::SaveSettings(settings) => vec![Effect::Save(settings)],
            ToDaemon::Watch { watched, target } => {
                vec![Effect::Watch(files::Order::Watch(watched, target))]
            }
            ToDaemon::Unwatch(watched) => vec![Effect::Watch(files::Order::Unwatch(watched))],
        }
    }
}

/// Does what a window asked of this machine, on a thread of its own (it may take a while), and
/// tells the window.
async fn act_for(request: ActId, act: Act, outbox: Outbox) {
    match act {
        Act::CheckTrust { folder } => {
            // What the folder would run is read only for one that isn't trusted: it is asked
            // about.
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
        }
        Act::StopSession(running) => {
            let stopped =
                tokio::task::spawn_blocking(move || desktop::stop_session(&running)).await;
            let result = match stopped {
                Ok(stopped) => stopped.map_err(ActFailure::from),
                Err(error) => {
                    tracing::error!(%error, "stopping a session elsewhere failed");
                    Err(ActFailure::Crashed)
                }
            };
            tell(&outbox, FromDaemon::Done { request, result }).await;
        }
    }
}

/// Tells a window something; a window gone isn't told.
async fn tell(outbox: &Outbox, message: FromDaemon) {
    if outbox.send(message).is_err() {
        tracing::debug!("a window gone");
    }
}

/// What the hub is told.
#[derive(Debug)]
enum ToHub {
    /// A window connected: it hears the list of sessions from now on.
    Joined {
        window: WindowId,
        key: WindowKey,
        outbox: Outbox,
    },
    Asked {
        window: WindowId,
        ask: Ask,
        outbox: Outbox,
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
    /// The line each window is on, by the key it names itself with.
    lines: BTreeMap<WindowKey, WindowId>,
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
        runtime: tokio::runtime::Handle,
    ) -> Self {
        let mut windows: BTreeMap<WindowId, Outbox> = BTreeMap::new();
        let mut orders: BTreeMap<HostedId, mpsc::Sender<Command>> = BTreeMap::new();
        let mut tasks = JoinSet::new();
        // Which session each task runs: one that panics gives back no `Session` to say.
        let mut running: BTreeMap<tokio::task::Id, HostedId> = BTreeMap::new();
        // Who wrote to whom, for sandboxed sessions to answer (see `relay::Replies`); it ends
        // as the sessions holding its sender do.
        let (replies, replying) = mpsc::channel(QUEUE);
        tokio::spawn(keep_replies(replying));
        loop {
            let steps = tokio::select! {
                request = requests.recv() => match request {
                    Some(ToHub::Joined { window, key, outbox }) => {
                        // Before the list: it says the sessions let go of as no window's.
                        let (earlier, steps) = self.joined(window, key);
                        if let Some(earlier) = earlier {
                            windows.remove(&earlier);
                        }
                        let list = FromDaemon::Hosted(self.list());
                        tell(&outbox, list).await;
                        windows.insert(window, outbox);
                        steps
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
                                    let task = tasks.spawn(host(id, launch, runtime.clone(), commands, shown, replies));
                                    running.insert(task.id(), id);
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
                Some(ended) = tasks.join_next_with_id() => {
                    let task = match &ended {
                        Ok((task, _)) => *task,
                        Err(error) => error.id(),
                    };
                    match (running.remove(&task), ended) {
                        (Some(_) | None, Ok((_, session))) => {
                            orders.remove(&session.id);
                            self.ended(session.id)
                        }
                        // Its process is killed as the task drops it; it goes from the list, and
                        // the window showing it is told it ended, rather than waiting on it.
                        (Some(id), Err(error)) => {
                            tracing::error!(%error, ?id, "a session failed");
                            orders.remove(&id);
                            let shown = self.sessions.get(&id).and_then(|kept| kept.shown);
                            if let Some((window, key)) = shown
                                && let Some(outbox) = windows.get(&window)
                            {
                                tell(outbox, FromDaemon::Ended { key, code: None }).await;
                            }
                            self.ended(id)
                        }
                        (None, Err(error)) => {
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
    /// A window joined, on line `window`; if the same window was on another line, which its
    /// network lost and which may not be found dead for a while, that line's sessions are let
    /// go of now, for the window to take back: that line, and the steps.
    fn joined(&mut self, window: WindowId, key: WindowKey) -> (Option<WindowId>, Vec<Step>) {
        match self.lines.insert(key, window) {
            Some(earlier) if earlier != window => (Some(earlier), self.left(earlier)),
            Some(_) | None => (None, Vec::new()),
        }
    }

    fn left(&mut self, window: WindowId) -> Vec<Step> {
        self.lines.retain(|_, line| *line != window);
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
    shown: Option<(Generation, Outbox)>,
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
        outbox: Outbox,
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

    /// A line to the process's input, never waited on here: one that isn't reading holds up
    /// none of the rest. One that has left [`QUEUE`] lines unread has hung, and is ended as a
    /// closed input ends it.
    fn write(&mut self, stdin: &mut Option<mpsc::Sender<String>>, line: String) {
        let Some(input) = stdin.as_ref() else {
            return;
        };
        match input.try_send(line) {
            Ok(()) => {}
            Err(mpsc::error::TrySendError::Closed(_)) => {
                tracing::debug!(id = ?self.id, "claude's input closed");
            }
            Err(mpsc::error::TrySendError::Full(_)) => {
                tracing::warn!(id = ?self.id, "claude isn't reading its input: ending it");
                *stdin = None;
                self.input_closed(Instant::now());
            }
        }
    }

    /// Lines to the process's input, its output to the window that shows it, and its end, until
    /// it ends.
    async fn event_loop(
        mut self,
        spawned: spawn::Spawned,
        mut commands: mpsc::Receiver<Command>,
        shown: Option<(Generation, Outbox)>,
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
        let mut stdin = child.stdin.take().map(input);
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
                        self.write(&mut stdin, line);
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
                            self.write(&mut stdin, answer);
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

/// What the sessions tell or ask the keeper of who wrote to whom (see `relay::Replies`).
#[derive(Debug)]
enum ToReplies {
    /// Session `from`, outside the sandbox, sent a message to `to`.
    Wrote {
        from: sessions::MessagingName,
        to: String,
    },
    /// Whether sandboxed session `sender` may answer `to` without the user being asked.
    MayAnswer {
        sender: sessions::MessagingName,
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
    sender: sessions::MessagingName,
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

/// Tells the window that shows a conversation whose transcript went two ways in and out of the
/// sandbox (see `home::Diverged`): it shows only the copy the session goes on from, and what
/// only the other has would otherwise go unnoticed. The window tells its desktop.
async fn tell_diverged(diverged: home::Diverged, shown: Option<&(Generation, Outbox)>) {
    tracing::warn!(
        left = %diverged.left.display(),
        "a conversation went two ways: what only one copy has is kept there"
    );
    if let Some((key, outbox)) = shown {
        let left = diverged.left.clone();
        tell(outbox, FromDaemon::Diverged { key: *key, left }).await;
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

/// A process's input, written on a task of its own, line by line as they are queued. Dropped,
/// the input closes once the lines queued are written; one the process doesn't read holds it
/// open only until the process is killed, a grace after its input closed.
fn input(mut stdin: ChildStdin) -> mpsc::Sender<String> {
    let (lines, mut writing) = mpsc::channel::<String>(QUEUE);
    tokio::spawn(async move {
        while let Some(line) = writing.recv().await {
            if let Err(error) = write_line(&mut stdin, &line).await {
                tracing::debug!(%error, "claude's input closed");
                return;
            }
        }
    });
    lines
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

/// A line to a window.
async fn send_line(outbox: &Outbox, key: Generation, stream: Stream, line: &str) {
    let output = FromDaemon::Output {
        key,
        stream,
        line: line.to_owned(),
    };
    if outbox.send(output).is_err() {
        tracing::debug!("a window gone");
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

    fn input(key: Generation, line: &str) -> ToDaemon {
        ToDaemon::Input {
            key,
            line: line.to_owned(),
        }
    }

    /// A line goes to the session of a key in use; a key used twice is refused.
    #[test]
    fn a_connection_works_out_what_requests_come_to() {
        let key = Generation::default().next();
        let unused_key = key.next();
        let mut connection = Connection::new(WindowId(1), WindowKey::new());
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
        assert_eq!(
            connection.handle(input(key, "{\"a\":1}")),
            vec![Effect::Hub(Ask::Line {
                key,
                line: "{\"a\":1}".to_owned()
            })]
        );
        assert!(
            connection.handle(input(unused_key, "x")).is_empty(),
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

    /// A window that comes again on a new line, its network having lost the old one, takes
    /// its sessions back at once: they are let go of from the old line, which is forgotten, and
    /// listed as no window's. Another window, or the first time, lets go of nothing.
    #[test]
    fn a_window_coming_again_takes_its_sessions_back() {
        let key = WindowKey::new();
        let (old_line, new_line) = (WindowId(1), WindowId(2));
        let session = HostedId::default().next();
        let mut hub = Hub {
            sessions: BTreeMap::from([(
                session,
                Kept {
                    cwd: PathBuf::from("/p"),
                    session: None,
                    shown: Some((old_line, Generation::default().next())),
                },
            )]),
            last: session,
            lines: BTreeMap::new(),
        };
        assert_eq!(hub.joined(old_line, key), (None, Vec::new()));
        assert_eq!(
            hub.joined(WindowId(3), WindowKey::new()),
            (None, Vec::new())
        );
        let (earlier, steps) = hub.joined(new_line, key);
        assert_eq!(earlier, Some(old_line));
        assert_eq!(
            steps,
            vec![
                Step::Tell {
                    id: session,
                    command: Order::Detach,
                },
                Step::Changed,
            ]
        );
        assert!(hub.list().iter().all(|hosted| !hosted.attached));
        // The old line, found dead later, lets go of nothing more.
        assert_eq!(hub.left(old_line), Vec::new());
        assert_eq!(hub.lines.get(&key), Some(&new_line));
    }

    /// A window joining hears the list of sessions before anything else: its handshake reads
    /// the list right after the hello, to attach to a session the daemon runs rather than start
    /// it a second time.
    #[tokio::test(flavor = "current_thread")]
    async fn a_window_joining_hears_the_list_first() {
        let (hub, requests) = mpsc::channel(8);
        let hub_task =
            tokio::spawn(Hub::default().event_loop(requests, tokio::runtime::Handle::current()));
        let (outbox, mut told) = mpsc::unbounded_channel();
        hub.send(ToHub::Joined {
            window: WindowId(1),
            key: WindowKey::new(),
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
        running("cat", &[])
    }

    fn running(program: &str, args: &[&str]) -> spawn::Spawned {
        let mut command = tokio::process::Command::new(program);
        command
            .args(args)
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
            name: sessions::MessagingName::of(
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
        let (outbox, mut answers) = mpsc::unbounded_channel();
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
            FromDaemon::Output { key: shown, stream: Stream::Out, line }
                if *shown == key && line == asked
        )));
        assert!(matches!(
            told.last(),
            Some(FromDaemon::Ended { key: ended, code: Some(0) }) if *ended == key
        ));
    }

    /// A process that doesn't read its input holds up nothing else: a window attaching is
    /// answered, and closing it ends it, killed a grace after.
    #[tokio::test(flavor = "current_thread")]
    async fn a_session_not_reading_its_input_goes_on() {
        let (orders, commands) = mpsc::channel(8);
        let (outbox, mut answers) = mpsc::unbounded_channel();
        let key = Generation::default().next();
        let (replies, _unheard) = mpsc::channel(QUEUE);
        let session = tokio::spawn(Session::new(HostedId::default().next()).event_loop(
            running("sleep", &["30"]),
            commands,
            None,
            replies,
        ));
        // More than a pipe holds: its writing waits on the process for good.
        let unread = "x".repeat(4 * 1024 * 1024);
        orders.send(Command::Line(unread)).await.expect("queued");
        orders
            .send(Command::Attach { key, outbox })
            .await
            .expect("queued");
        let started = tokio::time::timeout(Duration::from_secs(5), answers.recv()).await;
        assert!(
            matches!(started, Ok(Some(FromDaemon::Started { key: shown, .. })) if shown == key)
        );
        drop(orders);
        let ended = tokio::time::timeout(GRACE * 3, session).await;
        assert!(matches!(ended, Ok(Ok(Session { lines_in: 1, .. }))));
        let mut told = Vec::new();
        while let Some(answer) = answers.recv().await {
            told.push(answer);
        }
        assert!(matches!(
            told.last(),
            Some(FromDaemon::Ended { key: ended, code: None }) if *ended == key
        ));
    }
}
