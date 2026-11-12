//! An agy session: Google's Antigravity CLI (`agy`), spoken to over the Agent Client Protocol,
//! and shown in the window as a Claude Code session. The daemon starts this adapter
//! (`edualc --agy-adapter`) in place of `claude`; it speaks stream-json on its stdio as `claude`
//! does, so the daemon and the window need nothing of their own for it (see `translate`).
//!
//! agy runs in a Landlock sandbox: it writes only the project, caches, its own data and a
//! temporary folder; of socket files it reaches only the keyring, through a filtering proxy
//! that lets it read its own sign-in alone (see `buses::keyring`), and edualc's messaging tool.
//! agy doesn't ask before it edits files, so the sandbox is what keeps it to the project.
//!
//! The adapter itself runs outside the sandbox, as the daemon's child: it serves the session's
//! inbox and publishes its record (see `inbox`), so other sessions message it as they message
//! each other, and carries the messages agy sends with edualc's tool once the user allowed
//! each in the window (see `tool`, `relay`). It keeps the session's transcript, in Claude
//! Code's format, under edualc's state folder (see `sessions::agy_projects`), for the session
//! to be listed and continued: agy's own history is a database of its own.

mod inbox;
mod tool;
mod translate;

use std::collections::HashMap;
use std::io::Write as _;
use std::os::unix::fs::DirBuilderExt as _;
use std::path::{Path, PathBuf};
use std::process::Stdio;

use serde_json::Value;
use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _, BufReader};
use tokio::net::UnixListener;
use tokio::net::unix::OwnedWriteHalf;
use tokio::process::{Child, ChildStdin, Command};
use tokio::signal::unix::{Signal, SignalKind, signal};
use tokio::sync::mpsc;

use crate::external::mcp::SendArguments;
use crate::external::wire::SessionId;
use crate::relay::{self, Class, Job, Outcome, Places};
use crate::sandbox::{Enforcement, Policy};
use crate::sessions::{self, MessagingName};
use translate::{CallId, Effect, Start, ToolServer, Translator};

/// The first argument that makes this program an agy session's adapter.
pub(crate) const ADAPTER_FLAG: &str = "--agy-adapter";
/// The first argument that makes it the messaging tool agy starts.
pub(crate) const TOOL_FLAG: &str = "--agy-tool";
/// What a message line from a peer or the tool may hold, at most.
const QUEUE: usize = 256;
/// How long agy gets to end once its input closed, before it is killed.
const GRACE: std::time::Duration = std::time::Duration::from_secs(3);
/// The keyring item agy keeps its sign-in in (agy 1.5.6, through go-keyring).
const SIGN_IN: [(&str, &str); 2] = [("service", "gemini"), ("username", "antigravity")];

/// The adapter's command line, as `spawn` writes it.
#[derive(Debug, clap::Parser)]
#[command(name = "edualc --agy-adapter", disable_help_flag = true)]
struct Args {
    #[arg(long)]
    cwd: PathBuf,
    /// A new session, under this id.
    #[arg(long = "session-id", value_parser = session_id, conflicts_with = "resume")]
    session_id: Option<SessionId>,
    /// A session to continue.
    #[arg(long, value_parser = session_id)]
    resume: Option<SessionId>,
    #[arg(long)]
    model: Option<String>,
}

fn session_id(id: &str) -> Result<SessionId, String> {
    SessionId::try_from(id.to_owned()).map_err(|error| error.to_string())
}

#[derive(Debug, thiserror::Error)]
enum Error {
    #[error("{0}")]
    Arguments(#[from] clap::Error),
    #[error("the adapter needs --session-id or --resume")]
    NoSession,
    #[error("no transcript of session {0} says which conversation of agy's it is")]
    NotAgys(SessionId),
    #[error("no runtime folder (XDG_RUNTIME_DIR)")]
    NoRuntime,
    #[error("couldn't make the session's folder: {0}")]
    Folder(#[source] std::io::Error),
    #[error("agy isn't on PATH; install it from antigravity.google")]
    NoAgy,
    #[error("couldn't start agy: {0}")]
    Start(#[source] std::io::Error),
    #[error(transparent)]
    Sandbox(#[from] crate::sandbox::Error),
    #[error(transparent)]
    Inbox(#[from] inbox::Error),
    #[error("couldn't run: {0}")]
    Runtime(#[source] std::io::Error),
}

/// Runs the adapter until the window's side closes or agy ends.
pub(crate) fn run_adapter() -> std::process::ExitCode {
    match adapter() {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(error) => {
            // The window shows what the adapter says on stderr.
            eprintln!("edualc: {error}");
            std::process::ExitCode::FAILURE
        }
    }
}

/// Runs the messaging tool agy starts (see `tool`).
pub(crate) fn run_tool() -> std::process::ExitCode {
    tool::run()
}

fn adapter() -> Result<(), Error> {
    let args = <Args as clap::Parser>::try_parse_from(std::env::args_os().skip(1))?;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(Error::Runtime)?;
    runtime.block_on(adapt(args, runtime.handle()))
}

/// The session's own folder in the runtime folder, for its sockets and temporary files;
/// removed as it ends.
#[derive(Debug)]
struct Own {
    folder: PathBuf,
}

impl Own {
    fn make() -> Result<Self, Error> {
        let runtime = std::env::var_os("XDG_RUNTIME_DIR")
            .map(PathBuf::from)
            .ok_or(Error::NoRuntime)?;
        let folder = runtime.join(format!("edualc-agy-{}", std::process::id()));
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(&folder)
            .and_then(|()| {
                std::fs::DirBuilder::new()
                    .mode(0o700)
                    .create(folder.join("tmp"))
            })
            .map_err(Error::Folder)?;
        Ok(Self { folder })
    }

    fn tool(&self) -> PathBuf {
        self.folder.join("tool")
    }

    fn bus(&self) -> PathBuf {
        self.folder.join("bus")
    }

    fn scratch(&self) -> PathBuf {
        self.folder.join("tmp")
    }
}

impl Drop for Own {
    fn drop(&mut self) {
        if let Err(error) = std::fs::remove_dir_all(&self.folder) {
            tracing::debug!(%error, "the session's folder stays");
        }
    }
}

/// Where session `id` working in `cwd` keeps its transcript.
fn transcript_path(cwd: &Path, id: &SessionId) -> Option<PathBuf> {
    sessions::agy_projects().map(|projects| {
        projects
            .join(sessions::project_name(cwd))
            .join(format!("{}.jsonl", id.as_str()))
    })
}

/// agy's id for the conversation session `id` continues, as its transcript's first line says.
fn agys_id(transcript: &Path) -> Option<String> {
    let text = std::fs::read_to_string(transcript).ok()?;
    text.lines().find_map(|line| {
        let line: Value = serde_json::from_str(line).ok()?;
        (line.get("type").and_then(Value::as_str) == Some("agy-session"))
            .then(|| {
                line.get("agySessionId")
                    .and_then(Value::as_str)
                    .map(str::to_owned)
            })
            .flatten()
    })
}

/// `program` as found on PATH.
fn on_path(program: &str) -> Option<PathBuf> {
    std::env::var_os("PATH").and_then(|path| {
        std::env::split_paths(&path)
            .map(|folder| folder.join(program))
            .find(|candidate| candidate.is_file())
    })
}

/// agy, started in its sandbox: `acp` on its stdio, the keyring through `bus`, and edualc's
/// tool through `tool`; and how far the kernel enforced the sandbox.
fn start_agy(
    cwd: &Path,
    own: &Own,
    with_bus: bool,
    runtime: &tokio::runtime::Handle,
) -> Result<(Child, Enforcement), Error> {
    let agy = on_path("agy").ok_or(Error::NoAgy)?;
    let program = std::env::current_exe().map_err(Error::Start)?;
    let home = dirs::home_dir().unwrap_or_default();
    let data = [home.join(".gemini"), home.join(".config/antigravity")];
    let mut sockets = vec![own.tool()];
    if with_bus {
        sockets.push(own.bus());
    }
    let policy = Policy::for_agy(
        cwd,
        &data,
        &own.scratch(),
        &sockets,
        &[agy.clone(), program],
    );
    let mut command = Command::new(&agy);
    command
        .args(["acp", "--disable-telemetry"])
        .current_dir(cwd)
        .env("TMPDIR", own.scratch())
        .env_remove("NIRI_SOCKET")
        .env_remove("DISPLAY")
        .env_remove("WAYLAND_DISPLAY")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    if with_bus {
        command.env(
            "DBUS_SESSION_BUS_ADDRESS",
            format!("unix:path={}", own.bus().display()),
        );
    } else {
        command.env_remove("DBUS_SESSION_BUS_ADDRESS");
    }
    // Restricted, the thread starts agy, which keeps the restriction, and ends.
    std::thread::scope(|scope| {
        scope
            .spawn(|| {
                let enforcement = policy.restrict_this_thread()?;
                tracing::info!(?enforcement, "agy is sandboxed");
                let _in_runtime = runtime.enter();
                let child = command.spawn().map_err(Error::Start)?;
                Ok((child, enforcement))
            })
            .join()
            .unwrap_or_else(|panic| std::panic::resume_unwind(panic))
    })
}

/// What the loop is told.
#[derive(Debug)]
enum Heard {
    Window(Option<String>),
    Agy(Option<String>),
    AgyLog(String),
    Peer(String),
    Tool(CallId, tool::Request, OwnedWriteHalf),
}

async fn adapt(args: Args, runtime: &tokio::runtime::Handle) -> Result<(), Error> {
    let Args {
        cwd,
        session_id,
        resume,
        model,
    } = args;
    let (id, start) = match (session_id, resume) {
        (Some(id), None) => (id, Start::New),
        (None, Some(id)) => {
            let agys = transcript_path(&cwd, &id)
                .as_deref()
                .and_then(agys_id)
                .ok_or_else(|| Error::NotAgys(id.clone()))?;
            (id, Start::Resume(agys))
        }
        (Some(_), Some(_)) | (None, None) => return Err(Error::NoSession),
    };
    // The name the daemon knows the session by, as it names Claude Code's (see `spawn`).
    let name = MessagingName::of(&id, &cwd);
    let own = Own::make()?;
    // The keyring, as agy may reach it: its own item alone.
    let item = crate::buses::keyring_item(&SIGN_IN).await;
    let proxies = match &item {
        Some(item) => crate::buses::keyring(&own.bus(), item).await,
        None => {
            tracing::warn!("no sign-in of agy's in the keyring: run `agy` once to sign in");
            None
        }
    };
    let tools = UnixListener::bind(own.tool()).map_err(Error::Folder)?;
    let inbox = inbox::Inbox::open(&id, &cwd, &name)?;
    tracing::info!(inbox = %inbox.socket().display(), %name, "the session's inbox is open");
    let (mut agy, enforcement) = start_agy(&cwd, &own, proxies.is_some(), runtime)?;
    let program = std::env::current_exe().map_err(Error::Start)?;
    let mut translator = Translator::new(
        id.clone(),
        cwd.clone(),
        start,
        model,
        ToolServer {
            program,
            socket: own.tool(),
        },
    );

    let (heard, mut hearing) = mpsc::channel::<Heard>(QUEUE);
    let mut agy_in = agy.stdin.take();
    if let Some(out) = agy.stdout.take() {
        let heard = heard.clone();
        tokio::spawn(async move {
            let mut lines = BufReader::new(out).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                if heard.send(Heard::Agy(Some(line))).await.is_err() {
                    return;
                }
            }
            let ended = heard.send(Heard::Agy(None)).await;
            drop(ended);
        });
    }
    if let Some(err) = agy.stderr.take() {
        let heard = heard.clone();
        tokio::spawn(async move {
            let mut lines = BufReader::new(err).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                if heard.send(Heard::AgyLog(line)).await.is_err() {
                    return;
                }
            }
        });
    }
    {
        let heard = heard.clone();
        tokio::spawn(async move {
            let mut lines = BufReader::new(tokio::io::stdin()).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                if heard.send(Heard::Window(Some(line))).await.is_err() {
                    return;
                }
            }
            let closed = heard.send(Heard::Window(None)).await;
            drop(closed);
        });
    }
    let (posted, mut posts) = mpsc::channel::<String>(QUEUE);
    {
        let heard = heard.clone();
        tokio::spawn(async move {
            while let Some(post) = posts.recv().await {
                if heard.send(Heard::Peer(post)).await.is_err() {
                    return;
                }
            }
        });
    }
    {
        let heard = heard.clone();
        tokio::spawn(async move {
            let mut next = 0;
            loop {
                let Ok((stream, _)) = tools.accept().await else {
                    continue;
                };
                next += 1;
                let call = CallId(next);
                let heard = heard.clone();
                tokio::spawn(async move {
                    let (read, write) = stream.into_split();
                    let mut line = String::new();
                    let read = BufReader::new(read).read_line(&mut line).await;
                    match read.ok().and_then(|_| serde_json::from_str(&line).ok()) {
                        Some(request) => {
                            let told = heard.send(Heard::Tool(call, request, write)).await;
                            drop(told);
                        }
                        None => tracing::debug!("a tool call that isn't one"),
                    }
                });
            }
        });
    }
    drop(heard);

    let mut out = Out {
        window: tokio::io::stdout(),
        agy: agy_in.take(),
        transcript: transcript_path(&cwd, &id),
        calls: HashMap::new(),
        name,
    };
    let mut started = vec![Effect::ToWindow(serde_json::json!({
        "type": "system",
        "subtype": "edualc_sandbox",
        "enforcement": enforcement,
    }))];
    started.extend(translator.start());
    out.carry_out(&mut translator, started).await;
    // Told to end, it ends as when its input closes, and takes its files away.
    let mut ending = Ending::listen().map_err(Error::Runtime)?;
    loop {
        let effects = tokio::select! {
            () = ending.told() => break,
            () = inbox.take(&posted) => continue,
            said = hearing.recv() => match said {
                Some(Heard::Window(Some(line))) => translator.window_said(&line),
                Some(Heard::Window(None)) => {
                    // The daemon closed the session's input: agy's goes too, and it ends.
                    out.agy = None;
                    continue;
                }
                Some(Heard::Agy(Some(line))) => translator.agy_said(&line),
                Some(Heard::Agy(None)) | None => break,
                Some(Heard::AgyLog(line)) => {
                    eprintln!("agy: {line}");
                    continue;
                }
                Some(Heard::Peer(post)) => translator.peer(post),
                Some(Heard::Tool(call, request, write)) => {
                    out.calls.insert(call, write);
                    match request {
                        tool::Request::Send { to, message } => {
                            translator.tool_called(call, to, message)
                        }
                        tool::Request::List => translator.listing_asked(call),
                    }
                }
            },
        };
        out.carry_out(&mut translator, effects).await;
    }
    // Its input closed, agy ends; one that doesn't within a while is killed.
    drop(out);
    let ended = match tokio::time::timeout(GRACE, agy.wait()).await {
        Ok(ended) => ended,
        Err(_late) => {
            if let Err(error) = agy.start_kill() {
                tracing::warn!(%error, "couldn't kill agy");
            }
            agy.wait().await
        }
    };
    tracing::info!(?ended, "agy ended");
    drop(inbox);
    drop(proxies);
    drop(own);
    Ok(())
}

/// The signals that end the adapter: the daemon's end, a terminal's.
#[derive(Debug)]
struct Ending {
    terminate: Signal,
    interrupt: Signal,
    hangup: Signal,
}

impl Ending {
    fn listen() -> std::io::Result<Self> {
        Ok(Self {
            terminate: signal(SignalKind::terminate())?,
            interrupt: signal(SignalKind::interrupt())?,
            hangup: signal(SignalKind::hangup())?,
        })
    }

    async fn told(&mut self) {
        tokio::select! {
            _ = self.terminate.recv() => {}
            _ = self.interrupt.recv() => {}
            _ = self.hangup.recv() => {}
        }
    }
}

/// Where the translator's effects go.
#[derive(Debug)]
struct Out {
    window: tokio::io::Stdout,
    agy: Option<ChildStdin>,
    transcript: Option<PathBuf>,
    /// The tool's calls waiting for their answer.
    calls: HashMap<CallId, OwnedWriteHalf>,
    name: MessagingName,
}

impl Out {
    /// Does what the translator said; what the tool is answered goes back to it, as agy tells
    /// nothing of an MCP tool's result.
    async fn carry_out(&mut self, translator: &mut Translator, effects: Vec<Effect>) {
        for effect in effects {
            match effect {
                Effect::ToAgy(line) => {
                    if let Some(agy) = self.agy.as_mut()
                        && write_line(agy, &line).await.is_err()
                    {
                        tracing::debug!("agy's input closed");
                    }
                }
                Effect::ToWindow(line) => {
                    if write_line(&mut self.window, &line).await.is_err() {
                        tracing::debug!("the window's side closed");
                    }
                }
                Effect::Transcript(line) => self.keep(&line),
                Effect::Send { call, to, message } => {
                    let job = Job::Send {
                        arguments: SendArguments { to, message },
                        class: Some(Class::Prompting),
                    };
                    let told = self.carried(job).await;
                    translator.tool_told(call, &told);
                    self.answer(call, told).await;
                }
                Effect::List(call) => {
                    let told = self.carried(Job::List).await;
                    translator.tool_told(call, &told);
                    self.answer(call, told).await;
                }
                Effect::AnswerTool { call, text, error } => {
                    let told = tool::Answer { text, error };
                    translator.tool_told(call, &told);
                    self.answer(call, told).await;
                }
            }
        }
    }

    /// A job of the relay's, carried out as for a sandboxed session (see `relay::carry_out`).
    async fn carried(&self, job: Job) -> tool::Answer {
        let sender = self.name.clone();
        let carried = tokio::task::spawn_blocking(move || {
            relay::carry_out(&Places::of_user(), &sender, &job)
        })
        .await
        .unwrap_or(Outcome::Refused(relay::Refusal::Failed));
        let (text, error) = carried.told();
        tool::Answer { text, error }
    }

    async fn answer(&mut self, call: CallId, answer: tool::Answer) {
        let Some(mut write) = self.calls.remove(&call) else {
            return;
        };
        let line = serde_json::to_string(&answer).unwrap_or_default();
        let written = write.write_all(format!("{line}\n").as_bytes()).await;
        if written.is_err() {
            tracing::debug!("the tool's call went away");
        }
    }

    /// A line for the transcript, appended.
    fn keep(&self, line: &Value) {
        let Some(path) = &self.transcript else {
            return;
        };
        let written = path
            .parent()
            .map_or(Ok(()), |folder| {
                std::fs::DirBuilder::new()
                    .recursive(true)
                    .mode(0o700)
                    .create(folder)
            })
            .and_then(|()| {
                std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(path)
            })
            .and_then(|mut file| writeln!(file, "{line}"));
        if let Err(error) = written {
            tracing::warn!(%error, "a transcript line wasn't kept");
        }
    }
}

async fn write_line(
    to: &mut (impl tokio::io::AsyncWrite + Unpin),
    line: &Value,
) -> std::io::Result<()> {
    to.write_all(format!("{line}\n").as_bytes()).await?;
    to.flush().await
}
