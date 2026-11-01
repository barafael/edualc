//! The daemon (`edualc --daemon`): runs Claude Code's processes for the windows, and does for
//! them what their sandbox keeps them from (opening editors, notifying, stopping sessions that
//! run elsewhere). One per user, started by the first window, gone a while after the last
//! window and its sessions are.
//!
//! Its parts are actors in the manner of "More Actors with Tokio": each is its data, run by an
//! event loop that is given its channels and gives its data back, and spawned by its caller.
//! Their logic is sans-io (`handle` says what to do; the loop does it), and their channels make
//! no cycle:
//!
//! ```text
//! socket ─▶ Connection (a window) ─▶ Session (a claude process) ─▶ outbox ─▶ socket
//!                       └───────────── Done (what the desktop did) ─────▶ outbox
//! ```
//!
//! A window hanging up ends its connection's loop, which drops its sessions' inputs; each
//! session then closes its process's input and ends it, and the connection ends once they all
//! have. When no connection is left for [`IDLE`], the daemon ends.

use std::collections::BTreeMap;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::ChildStdin;
use tokio::sync::mpsc;
use tokio::task::JoinSet;
use tokio::time::Instant;

use crate::buses::{self, Buses};
use crate::claude::Launch;
use crate::desktop;
use crate::ipc::{
    self, Act, ActFailure, Deframer, FromDaemon, LineSplitter, StartFailure, Stream, ToDaemon,
};
use crate::spawn;

/// How long the daemon waits, with no window, before it ends.
const IDLE: Duration = Duration::from_secs(5);
/// How long a window has to say hello.
const HELLO: Duration = Duration::from_secs(2);
/// How long a process whose input closed gets to end before it is killed.
const GRACE: Duration = Duration::from_secs(3);
/// Messages queued for a window, and requests from it; lines queued for a process.
const QUEUE: usize = 1024;

/// Runs the daemon; it returns as it ends.
pub(crate) fn run() -> std::process::ExitCode {
    // What the daemon and its children make (its socket, the proxies') is the user's alone.
    rustix::process::umask(rustix::fs::Mode::RWXG | rustix::fs::Mode::RWXO);
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

/// Takes windows as they connect, until none is left for [`IDLE`].
async fn serve(listener: ipc::Listener, folder: &std::path::Path) -> Result<(), ipc::Error> {
    let (listener, _lock) = listener.into_tokio()?;
    // Ended with the daemon.
    let (buses, _proxies) = buses::start(folder).await;
    let mut windows = JoinSet::new();
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
                    windows.spawn(window(channel, buses.clone()));
                    quiet_since = None;
                }
                Err(error) => tracing::warn!(%error, "refused a connection"),
            },
            Some(ended) = windows.join_next() => {
                if let Err(error) = ended {
                    tracing::error!(%error, "a window's connection failed");
                }
                if windows.is_empty() {
                    quiet_since = Some(Instant::now());
                }
            },
            () = idle => {
                tracing::info!("no window for a while: ending");
                return Ok(());
            },
        }
    }
}

/// A window's connection: hello, then its requests to its [`Connection`], and what its sessions
/// and the desktop say back, until it hangs up and its sessions have ended.
async fn window(channel: ipc::AsyncChannel, buses: Buses) {
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
    let connection = Connection::default()
        .event_loop(received, outbox, tokio::runtime::Handle::current())
        .await;
    tracing::info!(started = connection.started, "a window's connection ended");
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

/// What a window's requests come to, worked out without doing any of it.
#[derive(Debug, PartialEq, Eq)]
enum Effect {
    Start {
        key: u64,
        launch: Launch,
    },
    /// A whole line for a session's input.
    Write {
        key: u64,
        line: String,
    },
    End {
        key: u64,
    },
    Act {
        request: u64,
        act: Act,
    },
    /// What was asked can't be done; the window is told.
    Refuse {
        key: u64,
        failure: StartFailure,
    },
}

/// A window's sessions: which run, and the line each is being sent in pieces.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct Connection {
    /// Sessions started and not ended, by the window's key, with the piece of a line so far.
    sessions: BTreeMap<u64, String>,
    /// How many were started, for the log.
    started: u64,
}

impl Connection {
    pub(crate) async fn event_loop(
        mut self,
        mut requests: mpsc::Receiver<ToDaemon>,
        outbox: mpsc::Sender<FromDaemon>,
        runtime: tokio::runtime::Handle,
    ) -> Self {
        let mut inputs: BTreeMap<u64, mpsc::Sender<String>> = BTreeMap::new();
        let mut running = JoinSet::new();
        while let Some(request) = requests.recv().await {
            for effect in self.handle(request) {
                match effect {
                    Effect::Start { key, launch } => {
                        let starting = runtime.clone();
                        let spawned =
                            tokio::task::spawn_blocking(move || spawn::spawn(&launch, &starting))
                                .await;
                        match spawned {
                            Ok(Ok(spawned)) => {
                                let (input, lines) = mpsc::channel(QUEUE);
                                inputs.insert(key, input);
                                let session = Session::new(key);
                                running.spawn(session.event_loop(lines, spawned, outbox.clone()));
                            }
                            Ok(Err(error)) => {
                                self.sessions.remove(&key);
                                let failure = StartFailure::from(error);
                                tell(&outbox, FromDaemon::Failed { key, failure }).await;
                            }
                            Err(error) => {
                                self.sessions.remove(&key);
                                tracing::error!(%error, "starting a session failed");
                                let failure = StartFailure::Crashed;
                                tell(&outbox, FromDaemon::Failed { key, failure }).await;
                            }
                        }
                    }
                    Effect::Write { key, line } => {
                        if let Some(input) = inputs.get(&key)
                            && input.send(line).await.is_err()
                        {
                            tracing::debug!(key, "a line for a session that ended");
                        }
                    }
                    Effect::End { key } => {
                        // Its input closing ends it.
                        inputs.remove(&key);
                    }
                    Effect::Act {
                        request,
                        act: Act::PickFolder,
                    } => {
                        let outbox = outbox.clone();
                        tokio::spawn(async move {
                            let picked = tokio::task::spawn_blocking(|| {
                                rfd::FileDialog::new()
                                    .set_title("Open a folder")
                                    .pick_folder()
                            })
                            .await;
                            let folder = picked.unwrap_or_else(|error| {
                                tracing::error!(%error, "the folder picker failed");
                                None
                            });
                            tell(&outbox, FromDaemon::Picked { request, folder }).await;
                        });
                    }
                    Effect::Act { request, act } => {
                        let outbox = outbox.clone();
                        tokio::spawn(async move {
                            let done = tokio::task::spawn_blocking(move || perform(act)).await;
                            let result = done.unwrap_or_else(|error| {
                                tracing::error!(%error, "doing what the desktop was asked failed");
                                Err(ActFailure::Crashed)
                            });
                            tell(&outbox, FromDaemon::Done { request, result }).await;
                        });
                    }
                    Effect::Refuse { key, failure } => {
                        tell(&outbox, FromDaemon::Failed { key, failure }).await;
                    }
                }
            }
        }
        // The window hung up: its sessions' inputs close, and they end.
        inputs.clear();
        while let Some(ended) = running.join_next().await {
            if let Err(error) = ended {
                tracing::error!(%error, "a session failed");
            }
        }
        self
    }

    /// What a request comes to (sans-io).
    fn handle(&mut self, request: ToDaemon) -> Vec<Effect> {
        match request {
            ToDaemon::Hello { .. } => Vec::new(),
            ToDaemon::Start { key, launch } => {
                if self.sessions.contains_key(&key) {
                    return vec![Effect::Refuse {
                        key,
                        failure: StartFailure::KeyInUse,
                    }];
                }
                self.sessions.insert(key, String::new());
                self.started += 1;
                vec![Effect::Start { key, launch }]
            }
            ToDaemon::Input { key, text, last } => {
                let Some(line) = self.sessions.get_mut(&key) else {
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
                    vec![Effect::Write {
                        key,
                        line: std::mem::take(line),
                    }]
                } else {
                    Vec::new()
                }
            }
            ToDaemon::End { key } => {
                if self.sessions.remove(&key).is_some() {
                    vec![Effect::End { key }]
                } else {
                    Vec::new()
                }
            }
            ToDaemon::Act { request, act } => vec![Effect::Act { request, act }],
        }
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
        // Answered as `Picked`, by the connection.
        Act::PickFolder => {}
        Act::FocusWindowOf { pid } => desktop::focus_window_of(pid)?,
        Act::StopSession(running) => desktop::stop_session(&running)?,
    }
    Ok(())
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
pub(crate) struct Session {
    key: u64,
    phase: Phase,
    lines_in: u64,
}

impl Session {
    fn new(key: u64) -> Self {
        Self {
            key,
            phase: Phase::Running,
            lines_in: 0,
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

    /// Lines to the process's input, its output to the window, and its end, until it ends.
    pub(crate) async fn event_loop(
        mut self,
        mut lines: mpsc::Receiver<String>,
        spawned: spawn::Spawned,
        outbox: mpsc::Sender<FromDaemon>,
    ) -> Self {
        let spawn::Spawned {
            mut child,
            enforcement,
        } = spawned;
        let key = self.key;
        let Some(pid) = child.id() else {
            tell(&outbox, FromDaemon::Ended { key, code: None }).await;
            return self;
        };
        tell(
            &outbox,
            FromDaemon::Started {
                key,
                pid,
                enforcement,
            },
        )
        .await;
        let mut stdin = child.stdin.take();
        let pumps = [
            child
                .stdout
                .take()
                .map(|out| tokio::spawn(pump(out, key, Stream::Out, outbox.clone()))),
            child
                .stderr
                .take()
                .map(|err| tokio::spawn(pump(err, key, Stream::Err, outbox.clone()))),
        ];
        let code = loop {
            let kill_at = self.kill_at();
            let deadline = async {
                match kill_at {
                    Some(at) => tokio::time::sleep_until(at).await,
                    None => std::future::pending().await,
                }
            };
            tokio::select! {
                line = lines.recv(), if self.phase == Phase::Running => match line {
                    Some(line) => {
                        self.lines_in += 1;
                        if let Some(writing) = stdin.as_mut()
                            && write_line(writing, &line).await.is_err()
                        {
                            tracing::debug!(key, "claude's input closed");
                        }
                    }
                    None => {
                        stdin = None;
                        self.input_closed(Instant::now());
                    }
                },
                ended = child.wait() => break ended.ok().and_then(|status| status.code()),
                () = deadline => {
                    self.phase = Phase::Killed;
                    if let Err(error) = child.start_kill() {
                        tracing::warn!(%error, key, "couldn't kill claude");
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
        tell(&outbox, FromDaemon::Ended { key, code }).await;
        self
    }
}

async fn write_line(stdin: &mut ChildStdin, line: &str) -> std::io::Result<()> {
    stdin.write_all(line.as_bytes()).await?;
    stdin.write_all(b"\n").await?;
    stdin.flush().await
}

/// A process's output to its window, line by line (split sans-io), in pieces that fit messages.
async fn pump(
    mut output: impl tokio::io::AsyncRead + Unpin,
    key: u64,
    stream: Stream,
    outbox: mpsc::Sender<FromDaemon>,
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
            if send_line(&outbox, key, stream, &line).await.is_err() {
                return;
            }
        }
    }
    if let Some(line) = splitter.finish()
        && send_line(&outbox, key, stream, &line).await.is_err()
    {
        tracing::debug!("a window gone");
    }
}

async fn send_line(
    outbox: &mpsc::Sender<FromDaemon>,
    key: u64,
    stream: Stream,
    line: &str,
) -> Result<(), mpsc::error::SendError<FromDaemon>> {
    let mut pieces = ipc::chunks(line).peekable();
    while let Some(text) = pieces.next() {
        let last = pieces.peek().is_none();
        outbox
            .send(FromDaemon::Output {
                key,
                stream,
                text: text.to_owned(),
                last,
            })
            .await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn launch() -> Launch {
        Launch {
            cwd: std::path::PathBuf::from("/tmp"),
            resume: None,
            model: None,
            sandbox: None,
        }
    }

    /// Pieces of a line make one line, for a session that runs; a key used twice is refused.
    #[test]
    fn a_connection_works_out_what_requests_come_to() {
        let mut connection = Connection::default();
        assert_eq!(
            connection.handle(ToDaemon::Start {
                key: 1,
                launch: launch()
            }),
            vec![Effect::Start {
                key: 1,
                launch: launch()
            }]
        );
        assert!(matches!(
            connection
                .handle(ToDaemon::Start {
                    key: 1,
                    launch: launch()
                })
                .as_slice(),
            [Effect::Refuse {
                key: 1,
                failure: StartFailure::KeyInUse
            }]
        ));
        let piece = |text: &str, last| ToDaemon::Input {
            key: 1,
            text: text.to_owned(),
            last,
        };
        assert!(connection.handle(piece("{\"a\":", false)).is_empty());
        assert_eq!(
            connection.handle(piece("1}", true)),
            vec![Effect::Write {
                key: 1,
                line: "{\"a\":1}".to_owned()
            }]
        );
        assert!(
            connection
                .handle(ToDaemon::Input {
                    key: 9,
                    text: "x".to_owned(),
                    last: true
                })
                .is_empty(),
            "no session, no line"
        );
        assert_eq!(
            connection.handle(ToDaemon::End { key: 1 }),
            vec![Effect::End { key: 1 }]
        );
        assert!(
            connection.handle(ToDaemon::End { key: 1 }).is_empty(),
            "ended once"
        );
        assert_eq!(connection.started, 1);
    }

    #[test]
    fn a_session_is_killed_only_a_grace_after_its_input_closed() {
        let mut session = Session::new(1);
        assert_eq!(session.kill_at(), None);
        let now = Instant::now();
        session.input_closed(now);
        assert_eq!(session.kill_at(), Some(now + GRACE));
        session.input_closed(now + GRACE);
        assert_eq!(
            session.kill_at(),
            Some(now + GRACE),
            "the first close counts"
        );
    }

    /// A real process, through the actor: its output comes as lines, and its end after them.
    #[tokio::test(flavor = "current_thread")]
    async fn a_session_relays_a_process_and_ends_with_its_input() {
        let mut command = tokio::process::Command::new("cat");
        command
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);
        let child = command.spawn().expect("cat runs");
        let spawned = spawn::Spawned {
            child,
            enforcement: None,
        };
        let (lines, received) = mpsc::channel(4);
        let (outbox, mut answers) = mpsc::channel(16);
        lines.send("hello".to_owned()).await.expect("queued");
        drop(lines);
        let session = Session::new(7).event_loop(received, spawned, outbox).await;
        assert_eq!(session.lines_in, 1);
        let mut told = Vec::new();
        while let Some(answer) = answers.recv().await {
            told.push(answer);
        }
        assert!(matches!(
            told.first(),
            Some(FromDaemon::Started { key: 7, .. })
        ));
        assert!(told.iter().any(|answer| matches!(
            answer,
            FromDaemon::Output { key: 7, stream: Stream::Out, text, last: true } if text == "hello"
        )));
        assert!(matches!(
            told.last(),
            Some(FromDaemon::Ended {
                key: 7,
                code: Some(0)
            })
        ));
    }
}
