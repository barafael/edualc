//! An Agent Client Protocol session: an agent (`agy`, Google's Antigravity CLI, or `opencode`)
//! spoken to over ACP and shown in the window as a Claude Code session. The daemon starts this
//! adapter (`edualcd --agy-adapter`) in place of `claude`; it speaks stream-json on its stdio as
//! `claude` does, so the daemon and the window need nothing of their own for it (see
//! [`translate`]).
//!
//! The agent runs as it would in a terminal. The adapter, the daemon's child, serves the
//! session's inbox and publishes its record (see [`inbox`]), so other sessions message it as
//! they message each other, and carries the messages the agent sends with edualc's tool (see
//! [`tool`] and [`outbox`]). For agy it keeps the session's transcript, in Claude Code's format,
//! under edualc's state folder (see [`crate::sessions::agy_projects`]), for the session to be
//! listed and continued: agy's own history is a database of its own. opencode keeps its sessions
//! itself, so no transcript is kept for it, and they can't be continued yet.

pub(crate) mod brain;
mod inbox;
mod outbox;
mod tool;
mod translate;

use std::collections::HashMap;
use std::io::Write as _;
use std::os::unix::fs::{DirBuilderExt as _, OpenOptionsExt as _};
use std::path::{Path, PathBuf};
use std::process::Stdio;

use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _, BufReader};
use tokio::net::UnixListener;
use tokio::net::unix::OwnedWriteHalf;
use tokio::process::{Child, ChildStdin, Command};
use tokio::signal::unix::{Signal, SignalKind, signal};
use tokio::sync::mpsc;

use crate::external::acp::{AgyArguments, OpencodeArguments, RpcError, ToolCall};
use crate::external::tool_input::{
    BashInput, EditInput, KnownCall, ListSessionsInput, ReadInput, SendMessageInput, WriteInput,
};
use crate::external::wire::{
    BackgroundTask, Message, SessionId, System, TranscriptEntry, TranscriptLine,
};
use crate::sessions::{self, MessagingName};
use outbox::{Job, Outcome, Places, SendArguments};
use translate::{CallId, Effect, Start, ToolServer, Translator};

/// The first argument that makes this program an ACP session's adapter.
pub const ADAPTER_FLAG: &str = "--agy-adapter";
/// The first argument that makes it the messaging tool an agent starts.
pub const TOOL_FLAG: &str = "--agy-tool";
/// What a message line from a peer or the tool may hold, at most.
const QUEUE: usize = 256;
/// How long the agent gets to end once its input closed, before it is killed.
const GRACE: std::time::Duration = std::time::Duration::from_secs(3);

/// The tool agy sees edualc's messaging tool as, by the MCP server's name and the tool's.
const AGY_SEND: &str = "mcp_edualc_send_message";
/// edualc's tool listing the sessions, as agy names it.
const AGY_LIST: &str = "mcp_edualc_list_sessions";

/// The agents edualc runs through the Agent Client Protocol, and what each does its own way.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum, strum::Display)]
#[strum(serialize_all = "snake_case")]
enum Agent {
    Agy,
    Opencode,
}

impl Agent {
    /// The program that speaks ACP.
    fn program(self) -> &'static str {
        match self {
            Self::Agy => "agy",
            Self::Opencode => "opencode",
        }
    }

    /// Its arguments, to speak ACP on stdio.
    fn arguments(self) -> &'static [&'static str] {
        match self {
            Self::Agy => &["acp", "--disable-telemetry"],
            Self::Opencode => &["acp"],
        }
    }

    /// How to get it, told when it isn't on PATH.
    fn install_hint(self) -> &'static str {
        match self {
            Self::Agy => "install it from antigravity.google",
            Self::Opencode => "install it from opencode.ai",
        }
    }

    /// Whether the adapter keeps the session's transcript for edualc to list and continue it.
    fn keeps_transcript(self) -> bool {
        match self {
            Self::Agy => true,
            Self::Opencode => false,
        }
    }

    /// Whether it keeps records of the subagents and swarms it starts, which edualc shows as
    /// the session's background tasks (see [`brain`]).
    fn keeps_subagents(self) -> bool {
        match self {
            Self::Agy => true,
            Self::Opencode => false,
        }
    }

    /// The arguments it sends with a tool call before the call's own: opencode sends the working
    /// directory first.
    fn early_arguments(self) -> &'static [&'static str] {
        match self {
            Self::Agy => &[],
            Self::Opencode => &["cwd"],
        }
    }

    /// The agent, started in `cwd` with ACP on its stdio.
    fn start(self, cwd: &Path) -> Result<Child, Error> {
        let program = on_path(self.program()).ok_or(Error::NotOnPath(self))?;
        Command::new(&program)
            .args(self.arguments())
            .current_dir(cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .map_err(|source| Error::Start(self, source))
    }

    /// A tool call this agent named `name`, as the window knows Claude Code's: its file and
    /// command tools as Read, Write, Edit and Bash, with their arguments by Claude Code's names.
    /// None for a name it doesn't know.
    fn tool(self, name: &str, call: &ToolCall) -> Option<KnownCall> {
        let title = || call.title.clone().unwrap_or_default();
        match self {
            Self::Agy => {
                let given: AgyArguments = arguments(name, call);
                let path = given
                    .path
                    .or_else(|| first_location(call))
                    .unwrap_or_else(|| PathBuf::from(title()));
                let content = given.content.unwrap_or_default();
                match name {
                    AGY_SEND => Some(KnownCall::SendMessage(SendMessageInput {
                        to: given.to.unwrap_or_default(),
                        message: given.message.unwrap_or_default(),
                    })),
                    AGY_LIST => Some(KnownCall::ListSessions(ListSessionsInput {})),
                    "run_command" => Some(KnownCall::Bash(BashInput::command(
                        given.command_line.unwrap_or_else(title),
                        given.tool_summary,
                    ))),
                    "read_file" | "view_file" => Some(KnownCall::Read(ReadInput::whole(path))),
                    // Appending adds to the file: as an edit that adds the lines, not a Write,
                    // which would read as the whole file.
                    "write_file" if given.append == Some(true) => Some(KnownCall::Edit(
                        EditInput::once(path, String::new(), content),
                    )),
                    "write_file" => Some(KnownCall::Write(WriteInput {
                        file_path: path,
                        content,
                    })),
                    "edit_file" => Some(KnownCall::Edit(EditInput::once(
                        path,
                        given.target.unwrap_or_default(),
                        given.replacement.unwrap_or_default(),
                    ))),
                    _ => None,
                }
            }
            Self::Opencode => {
                let given: OpencodeArguments = arguments(name, call);
                // Its paths are relative in the arguments and absolute in the locations.
                let path = first_location(call).or(given.path).unwrap_or_default();
                match name {
                    "shell" => Some(KnownCall::Bash(BashInput::command(
                        given.command.unwrap_or_default(),
                        None,
                    ))),
                    "read" => Some(KnownCall::Read(ReadInput::whole(path))),
                    "write" => Some(KnownCall::Write(WriteInput {
                        file_path: path,
                        content: given.content.unwrap_or_default(),
                    })),
                    "edit" => Some(KnownCall::Edit(EditInput::once(
                        path,
                        given.old_string.unwrap_or_default(),
                        given.new_string.unwrap_or_default(),
                    ))),
                    _ => None,
                }
            }
        }
    }

    /// Whether the agent refused a prompt for want of a valid sign-in: ACP's code, or its own
    /// words for a token Google's API refused (`UNAUTHENTICATED`, HTTP 401) or for no sign-in at
    /// all.
    fn sign_in_refused(self, error: &RpcError) -> bool {
        error.sign_in_refused()
            || match self {
                Self::Agy => {
                    error.message.contains("UNAUTHENTICATED")
                        || error.message.contains("not logged into Antigravity")
                }
                Self::Opencode => false,
            }
    }
}

/// The adapter's command line, as `spawn` writes it.
#[derive(Debug, clap::Parser)]
#[command(name = "edualcd --agy-adapter", disable_help_flag = true)]
struct Args {
    /// The agent this adapter serves.
    #[arg(long, value_enum, default_value_t = Agent::Agy)]
    agent: Agent,
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
    #[error("no transcript of session {1} says which conversation of {0}'s it is")]
    NoConversation(Agent, SessionId),
    #[error("continuing {0} sessions isn't supported yet")]
    ResumeNotSupported(Agent),
    #[error("no runtime folder (XDG_RUNTIME_DIR)")]
    NoRuntime,
    #[error("couldn't make the session's folder: {0}")]
    Folder(#[source] std::io::Error),
    #[error("{} isn't on PATH; {}", .0, .0.install_hint())]
    NotOnPath(Agent),
    #[error("couldn't start {0}: {1}")]
    Start(Agent, #[source] std::io::Error),
    #[error("couldn't find this program: {0}")]
    Program(#[source] std::io::Error),
    #[error(transparent)]
    Inbox(#[from] inbox::Error),
    #[error("couldn't run: {0}")]
    Runtime(#[source] std::io::Error),
}

/// Runs the adapter until the window's side closes or the agent ends.
pub fn run_adapter() -> std::process::ExitCode {
    match adapter() {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(error) => {
            // The window shows what the adapter says on stderr.
            eprintln!("edualc: {error}");
            std::process::ExitCode::FAILURE
        }
    }
}

/// Runs the messaging tool the agent starts (see [`tool`]).
pub fn run_tool() -> std::process::ExitCode {
    tool::run()
}

fn adapter() -> Result<(), Error> {
    let args = <Args as clap::Parser>::try_parse_from(std::env::args_os().skip(1))?;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(Error::Runtime)?;
    runtime.block_on(adapt(args))
}

/// The session's own folder in the runtime folder, for the messaging tool's socket; removed as
/// it ends.
#[derive(Debug)]
struct Own {
    folder: PathBuf,
}

impl Own {
    fn make(agent: Agent) -> Result<Self, Error> {
        let runtime = std::env::var_os("XDG_RUNTIME_DIR")
            .map(PathBuf::from)
            .ok_or(Error::NoRuntime)?;
        let folder = runtime.join(format!("edualc-{agent}-{}", std::process::id()));
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(&folder)
            .map_err(Error::Folder)?;
        Ok(Self { folder })
    }

    fn tool(&self) -> PathBuf {
        self.folder.join("tool")
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

/// The agent's id for the conversation session `id` continues, as its transcript's first line
/// keeps it.
fn acp_session_id(transcript: &Path) -> Option<String> {
    let text = std::fs::read_to_string(transcript).ok()?;
    text.lines()
        .find_map(|line| match serde_json::from_str(line).ok()? {
            TranscriptLine::AgySession { agent_session_id } => Some(agent_session_id),
            TranscriptLine::User { .. }
            | TranscriptLine::Assistant { .. }
            | TranscriptLine::System(_)
            | TranscriptLine::CustomTitle { .. }
            | TranscriptLine::AiTitle { .. }
            | TranscriptLine::Summary { .. }
            | TranscriptLine::WorktreeState { .. }
            | TranscriptLine::Other => None,
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

/// The arguments of tool call `name`, as far as the agent has given them: none yet, or ones
/// edualc can't read (logged), show as none.
fn arguments<Arguments>(name: &str, call: &ToolCall) -> Arguments
where
    Arguments: serde::de::DeserializeOwned + Default,
{
    let Some(input) = call.raw_input.clone() else {
        return Arguments::default();
    };
    match serde_json::from_value::<Option<Arguments>>(input) {
        Ok(given) => given.unwrap_or_default(),
        Err(error) => {
            tracing::warn!(%error, tool = name, "a tool's arguments edualc can't read");
            Arguments::default()
        }
    }
}

/// The path of a tool call's first location, for a call whose input doesn't name one.
fn first_location(call: &ToolCall) -> Option<PathBuf> {
    call.locations
        .as_ref()
        .and_then(|locations| locations.first())
        .map(|location| location.path.clone())
}

/// What the loop is told.
#[derive(Debug)]
enum Heard {
    Window(Option<String>),
    Agent(Option<String>),
    AgentLog(String),
    Peer(String),
    Tool(CallId, tool::Request, OwnedWriteHalf),
}

async fn adapt(args: Args) -> Result<(), Error> {
    let Args {
        agent,
        cwd,
        session_id,
        resume,
        model,
    } = args;
    let (id, start) = match (session_id, resume) {
        (Some(id), None) => (id, Start::New),
        (None, Some(id)) => {
            if !agent.keeps_transcript() {
                return Err(Error::ResumeNotSupported(agent));
            }
            let conversation = transcript_path(&cwd, &id)
                .as_deref()
                .and_then(acp_session_id)
                .ok_or_else(|| Error::NoConversation(agent, id.clone()))?;
            (id, Start::Resume(conversation))
        }
        (Some(_), Some(_)) | (None, None) => return Err(Error::NoSession),
    };
    let transcript = if agent.keeps_transcript() {
        transcript_path(&cwd, &id)
    } else {
        None
    };
    // The name the daemon knows the session by, as it names Claude Code's (see `spawn`).
    let name = MessagingName::of(&id, &cwd);
    let own = Own::make(agent)?;
    let tools = UnixListener::bind(own.tool()).map_err(Error::Folder)?;
    let inbox = inbox::Inbox::open(&id, &cwd, &name)?;
    tracing::info!(inbox = %inbox.socket().display(), %name, "the session's inbox is open");
    let mut process = agent.start(&cwd)?;
    let program = std::env::current_exe().map_err(Error::Program)?;
    let mut translator = Translator::new(
        agent,
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
    let mut agent_in = process.stdin.take();
    if let Some(out) = process.stdout.take() {
        let heard = heard.clone();
        tokio::spawn(async move {
            let mut lines = BufReader::new(out).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                if heard.send(Heard::Agent(Some(line))).await.is_err() {
                    return;
                }
            }
            if heard.send(Heard::Agent(None)).await.is_err() {
                tracing::debug!("the adapter's loop ended before the agent did");
            }
        });
    }
    if let Some(err) = process.stderr.take() {
        let heard = heard.clone();
        tokio::spawn(async move {
            let mut lines = BufReader::new(err).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                if heard.send(Heard::AgentLog(line)).await.is_err() {
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
            if heard.send(Heard::Window(None)).await.is_err() {
                tracing::debug!("the adapter's loop ended before the window's side closed");
            }
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
                            if heard.send(Heard::Tool(call, request, write)).await.is_err() {
                                tracing::debug!("a tool call after the adapter's loop ended");
                            }
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
        agent: agent_in.take(),
        transcript,
        id,
        cwd,
        calls: HashMap::new(),
        name,
        tasks: Vec::new(),
    };
    let started = translator.start();
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
                    // The daemon closed the session's input: the agent's goes too, and it ends.
                    out.agent = None;
                    continue;
                }
                Some(Heard::Agent(Some(line))) => translator.agent_said(&line),
                Some(Heard::Agent(None)) | None => break,
                Some(Heard::AgentLog(line)) => {
                    eprintln!("{agent}: {line}");
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
    // Its input closed, the agent ends; one that doesn't within a while is killed.
    drop(out);
    let ended = match tokio::time::timeout(GRACE, process.wait()).await {
        Ok(ended) => ended,
        Err(_late) => {
            if let Err(error) = process.start_kill() {
                tracing::warn!(%error, %agent, "couldn't kill the agent");
            }
            process.wait().await
        }
    };
    tracing::info!(?ended, %agent, "the agent ended");
    drop(inbox);
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
    agent: Option<ChildStdin>,
    transcript: Option<PathBuf>,
    /// The session and the folder it works in, which each transcript line is kept with.
    id: SessionId,
    cwd: PathBuf,
    /// The tool's calls waiting for their answer.
    calls: HashMap<CallId, OwnedWriteHalf>,
    name: MessagingName,
    /// The subagents last told to the window.
    tasks: Vec<BackgroundTask>,
}

impl Out {
    /// Does what the translator said; what the tool is answered goes back to it, as the agent
    /// tells nothing of an MCP tool's result.
    async fn carry_out(&mut self, translator: &mut Translator, effects: Vec<Effect>) {
        for effect in effects {
            match effect {
                Effect::ToAgent(message) => match message.to_line() {
                    Ok(line) => {
                        if let Some(agent) = self.agent.as_mut()
                            && write_line(agent, &line).await.is_err()
                        {
                            tracing::debug!("the agent's input closed");
                        }
                    }
                    Err(error) => {
                        tracing::warn!(%error, "a line for the agent that can't be written: not sent");
                    }
                },
                Effect::ToWindow(message) => self.tell(&message).await,
                Effect::Transcript(line) => self.keep(&line),
                Effect::Send { call, to, message } => {
                    let told = self.carried(Job::Send(SendArguments { to, message })).await;
                    translator.tool_told(call, &told);
                    self.answer(call, told).await;
                }
                Effect::List(call) => {
                    let told = self.carried(Job::List).await;
                    translator.tool_told(call, &told);
                    self.answer(call, told).await;
                }
                Effect::Subagents { conversation } => self.subagents(conversation).await,
            }
        }
    }

    /// A line to the window.
    async fn tell(&mut self, message: &Message) {
        match serde_json::to_string(message) {
            Ok(line) => {
                if write_line(&mut self.window, &line).await.is_err() {
                    tracing::debug!("the window's side closed");
                }
            }
            Err(error) => {
                tracing::warn!(%error, "a line for the window that can't be written: not sent");
            }
        }
    }

    /// The window told the subagents the agent keeps for `conversation`, if they changed.
    async fn subagents(&mut self, conversation: String) {
        let Ok(conversation) = SessionId::try_from(conversation) else {
            tracing::debug!("a conversation id that isn't agy's: no subagents read");
            return;
        };
        let tasks = brain::tasks(&conversation);
        if tasks != self.tasks {
            self.tasks.clone_from(&tasks);
            self.tell(&Message::System(System::BackgroundTasksChanged { tasks }))
                .await;
        }
    }

    /// A call of edualc's tool, carried out (see [`outbox::carry_out`]).
    async fn carried(&self, job: Job) -> tool::Answer {
        let sender = self.name.clone();
        let carried = tokio::task::spawn_blocking(move || {
            outbox::carry_out(&Places::of_user(), &sender, &job)
        })
        .await
        .unwrap_or(Outcome::Refused(outbox::Refusal::Failed));
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
    fn keep(&self, line: &TranscriptLine) {
        let Some(path) = &self.transcript else {
            return;
        };
        let entry = TranscriptEntry {
            line,
            session_id: &self.id,
            cwd: &self.cwd,
            timestamp: jiff::Timestamp::now(),
        };
        let line = match serde_json::to_string(&entry) {
            Ok(line) => line,
            Err(error) => {
                tracing::warn!(%error, "a transcript line that can't be written: not kept");
                return;
            }
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
                    .mode(0o600)
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
    line: &impl std::fmt::Display,
) -> std::io::Result<()> {
    to.write_all(format!("{line}\n").as_bytes()).await?;
    to.flush().await
}
