//! The one owner of everything the window shows: the conversation, the process holding it,
//! what it waits for. UI callbacks, the process's output, a timer and background work reach it
//! as [`Event`]s on one channel, and it draws what changed once per batch.

use std::collections::{HashMap, HashSet, VecDeque};
use std::ops::ControlFlow;
use std::path::{Path, PathBuf};
use std::time::Duration;

use async_channel::{Receiver, Sender};
use serde::Deserialize;
use serde_json::Value;

use crate::chat::{Body, Change, Chat, ImageKey, ToolState};
use crate::claude::{self, Claude, FolderConfiguration, Launch, Output, Resume, SandboxWish};
use crate::decode::{Picture, Pixels};
use crate::desktop;
use crate::external::wire::{
    self, ControlRequest, Destination, PermissionResult, PermissionUpdate, RequestId, SessionId,
    ToolInput, ToolName, ToolUseId, TranscriptLine,
};
use crate::folder::Folder;
use crate::generated::AppWindow;
use crate::ipc::{
    Act, ActFailure, ActId, FolderUse, Generation, Hosted, PasteFailure, Pasted, Runs,
    SandboxAccess, ToDaemon,
};
use crate::link::{self, Link};
use crate::plan;
use crate::sandbox;
use crate::sessions::{
    self, LiveSession, LiveStatus, MessagingName, ProjectInfo, SessionInfo, Worktree,
};
use crate::settings::{self, Settings, Zoom};
use crate::sidebar;
use crate::trust;
use crate::ui::View;
use crate::watcher::{Watched, Watcher};

#[cfg_attr(target_arch = "wasm32", path = "web/images.rs")]
mod images;
mod peek;
mod protocol;
mod side;
mod tasks;
mod usage;
mod worktree;

pub(crate) use peek::{CommandOutput, Conversation, Followed, Peek, PeekView, Step, StepKind};
pub(crate) use side::{Answer, SideChat, SideView};
pub(crate) use tasks::{Place, Running, TaskProgress, Tasks};

/// The permission modes the menu offers, as the CLI names them.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    serde::Serialize,
    serde::Deserialize,
    strum::EnumIter,
    strum::EnumString,
    strum::IntoStaticStr,
)]
#[strum(serialize_all = "camelCase")]
#[serde(rename_all = "camelCase")]
pub(crate) enum PermissionMode {
    Default,
    AcceptEdits,
    Plan,
    Auto,
    BypassPermissions,
}

impl PermissionMode {
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Default => "Ask before edits",
            Self::AcceptEdits => "Accept edits",
            Self::Plan => "Plan mode",
            Self::Auto => "Auto",
            Self::BypassPermissions => "Bypass permissions",
        }
    }

    /// What the mode lets through, for its menu.
    pub(crate) fn description(self) -> &'static str {
        match self {
            Self::Default => "Claude asks before it edits files or runs commands",
            Self::AcceptEdits => "File edits go through; commands still ask",
            Self::Plan => "Claude only reads and plans, then asks you to approve the plan",
            Self::Auto => "A classifier lets the safe through and asks about the rest",
            Self::BypassPermissions => "Nothing asks. Only for sandboxes you can throw away",
        }
    }
}

/// The mode the CLI reports: one the menu knows, or one named as the CLI names it.
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize)]
#[serde(untagged)]
pub(crate) enum Mode {
    Known(PermissionMode),
    Unknown(String),
}

#[derive(Debug)]
pub(crate) enum Event {
    Action(Action),
    Claude {
        generation: Generation,
        output: Output,
    },
    SessionsListed {
        cwd: PathBuf,
        sessions: Vec<SessionInfo>,
    },
    ProjectsListed(Vec<ProjectInfo>),
    /// A saved session read from its start, as a conversation (built off the UI thread); where
    /// its transcript was found, and how far into it the conversation reaches.
    SessionLoaded {
        id: SessionId,
        path: PathBuf,
        result: std::io::Result<(Chat, u64)>,
    },
    /// What a watched session's transcript says after what is shown.
    TranscriptGrew {
        id: SessionId,
        result: std::io::Result<(Vec<TranscriptLine>, u64)>,
    },
    /// What the transcript of the subagent peeked into says after what is shown.
    PeekGrew {
        task: wire::TaskId,
        result: std::io::Result<(Vec<TranscriptLine>, u64)>,
    },
    LiveListed(Vec<LiveSession>),
    /// The daemon did what the desktop was asked, or says why not.
    Acted {
        request: ActId,
        result: Result<(), ActFailure>,
    },
    /// The folder the daemon's picker came back with.
    Picked {
        request: ActId,
        folder: Option<PathBuf>,
    },
    /// Whether the terminal's trust dialog was accepted for the folder asked about.
    Trusted {
        request: ActId,
        accepted: bool,
        /// What the folder would run, if it isn't trusted.
        runs: Vec<Runs>,
    },
    /// The clipboard's image, decoded in the confined helper, or why there is none.
    Pasted {
        request: ActId,
        pasted: Pasted,
    },
    /// An image of the conversation, decoded in the confined helper to show, or why it isn't.
    ImageShown {
        key: ImageKey,
        shown: Result<Pixels, images::ShowError>,
    },
    /// A code block of the conversation, coloured off the UI thread (see `colouring`).
    CodeColoured(crate::highlight::Coloured),
    /// The daemon is gone, and the sessions it ran for this window.
    DaemonLost,
    /// A window asked the daemon to shut down: the sessions end, and then it.
    DaemonShuttingDown,
    /// The sessions the daemon runs, anew.
    Hosted(Vec<Hosted>),
    /// The worktree a session's transcript says it works in.
    WorktreeRead {
        id: SessionId,
        worktree: Option<Worktree>,
    },

    /// Something watched changed: the running sessions' records, or a followed transcript.
    Changed(Watched),
    /// Now and then: sessions whose process ended without saying so are let go.
    Tick,
    /// Every little while during a turn: whether Claude Code has gone quiet.
    Listen,
    /// Every second while a command is followed: what it printed since.
    PollOutput,
}

/// A prompt as sent, to send again once the sign-in works.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SentPrompt {
    text: String,
    images: Vec<Picture>,
}

/// What the window knows of its daemon's life.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Daemon {
    Running,
    /// A window asked it to shut down: its end that follows is no loss to tell of.
    ShuttingDown,
}

/// Where the composer is among the session's prompts, as Up and Down step through them.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
enum Recall {
    /// The draft is the user's own.
    #[default]
    Writing,
    /// The draft is prompt `index` of the session's, oldest first; `draft` is what was
    /// written before, given back once Down steps past the newest.
    Recalled { index: usize, draft: String },
}

/// What Down puts in the draft.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Recalled {
    /// The session's prompt at this index.
    Prompt(usize),
    /// What was being written before Up.
    Draft(String),
}

impl Recall {
    /// Up, among `count` prompts, with `draft` in the composer: the prompt to show, if there
    /// is an older one.
    fn older(&mut self, count: usize, draft: &str) -> Option<usize> {
        let (index, kept) = match self {
            Self::Writing => (count.checked_sub(1)?, draft.to_owned()),
            Self::Recalled { index, draft } => (index.checked_sub(1)?, draft.clone()),
        };
        *self = Self::Recalled { index, draft: kept };
        Some(index)
    }

    /// Down, among `count` prompts: the next prompt, or past the newest the draft from before,
    /// the user writing again; nothing unless a prompt is recalled.
    fn newer(&mut self, count: usize) -> Option<Recalled> {
        let Self::Recalled { index, draft } = std::mem::take(self) else {
            return None;
        };
        let next = index.saturating_add(1);
        if next < count {
            *self = Self::Recalled { index: next, draft };
            Some(Recalled::Prompt(next))
        } else {
            Some(Recalled::Draft(draft))
        }
    }

    /// Whether Up has an older prompt to bring, among `count`.
    fn has_older(&self, count: usize) -> bool {
        match self {
            Self::Writing => count > 0,
            Self::Recalled { index, .. } => *index > 0,
        }
    }
}

/// What is known of Claude Code's sign-in. Behind a pipe it offers no `/login`, and a session
/// signed in with a token handed over names no account, so this comes from how turns end.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) enum SignIn {
    /// Nothing has told yet.
    #[default]
    Unknown,
    /// It named an account, or a model answered.
    SignedIn,
    /// A sandboxed session's token ran out: Claude Code starts again with the sign-in there is
    /// now, which needs no help unless that is refused too.
    Renewing,
    /// A turn failed for want of a sign-in; the prompt it failed on goes again with "Try again".
    Refused { failed: Option<SentPrompt> },
    /// The user asked with `/login`, and is shown how, whatever the state before.
    Asked,
}

impl SignIn {
    /// After a turn the sign-in failed on. A token that merely ran out (`renewable`) gets one
    /// start again by itself.
    fn after_refusal(&self, renewable: bool, failed: Option<SentPrompt>) -> Self {
        if renewable && *self != Self::Renewing {
            Self::Renewing
        } else {
            Self::Refused { failed }
        }
    }

    /// Whether the user is being shown how to sign in.
    pub(crate) fn waits_for_user(&self) -> bool {
        matches!(self, Self::Refused { .. } | Self::Asked)
    }
}

/// Where a start stands on its folder's trust.
#[derive(Debug, Default)]
enum TrustGate {
    /// Nothing is held back.
    #[default]
    Open,
    /// The daemon reads whether the terminal's dialog trusted the folder (the window can't,
    /// see `trust`); the start, with what it was to resume, waits for its answer, and so does
    /// a prompt sent meanwhile (the command line's first, or an early Enter).
    Checking {
        request: ActId,
        resume: Option<Resume>,
        held: Option<String>,
    },
    /// The user is asked to trust the folder, shown what its configuration would run; the
    /// start waits for them.
    Asking {
        resume: Option<Resume>,
        runs: Vec<Runs>,
    },
}

/// Where the daemon's answer on the folder's trust leaves a start held back.
#[derive(Debug, PartialEq)]
enum TrustAnswer {
    /// Trusted in the terminal: the start goes ahead, and then the prompt held for it.
    Trusted {
        resume: Option<Resume>,
        held: Option<String>,
    },
    /// The user is asked now; a prompt held waits in the composer for them to send again.
    Asked,
    /// An answer for a start since dropped or replaced: nothing changes.
    Stale,
}

impl TrustGate {
    /// A prompt sent before the start: held to go once the folder is found trusted, if its
    /// trust is still being read; the user, once asked, sends it themselves.
    fn hold(&mut self, prompt: &str) {
        if let Self::Checking { held, .. } = self {
            *held = Some(prompt.to_owned());
        }
    }

    /// The daemon's answer to `request`: the gate opens, asks the user, or stays as it is.
    fn answered(&mut self, request: ActId, accepted: bool, runs: Vec<Runs>) -> TrustAnswer {
        match std::mem::take(self) {
            Self::Checking {
                request: asked,
                resume,
                held,
            } if asked == request => {
                if accepted {
                    TrustAnswer::Trusted { resume, held }
                } else {
                    *self = Self::Asking { resume, runs };
                    TrustAnswer::Asked
                }
            }
            kept @ (Self::Open | Self::Checking { .. } | Self::Asking { .. }) => {
                *self = kept;
                TrustAnswer::Stale
            }
        }
    }
}

/// What a request to the daemon was for.
#[derive(Debug)]
enum Acting {
    /// Something for the desktop the user asked for; only its failure is told.
    Desktop,
    /// A notification about a session elsewhere, which the user didn't ask for: its failure is
    /// only logged, not put in the conversation (a desktop without a notification service
    /// would have an error there for each).
    Notifying,
    /// Stopping a session elsewhere, to continue it here.
    Bringing(SessionId),
    /// Reading the clipboard's image, for the next message.
    Pasting,
    /// The folder picker, for a folder to be used so.
    Picking(FolderUse),
}

/// What the user did in the window.
#[derive(Debug)]
pub(crate) enum Action {
    Send(String),
    Interrupt,
    NewSession,
    OpenSession(SessionId),
    OpenProject(Folder),
    BrowseProject,
    ListProjects,
    ChooseModel(usize),
    ChooseMode(usize),
    DraftEdited(String),
    /// A command from the list matching the draft, put in the draft.
    PickCommand(usize),
    /// Up on the composer's first line: the prompt before the one shown, or the newest.
    RecallOlder,
    /// Down on its last line, while a prompt is recalled: the next one, or the draft again.
    RecallNewer,
    /// A command from the list matching the draft, sent as it is.
    RunCommand(usize),
    Toggle(usize),
    CopyEntry(usize),
    Copy(String),
    OpenUrl(String),
    Allow(RequestId),
    AllowAlways(RequestId),
    Deny {
        request: RequestId,
        feedback: String,
    },
    ToggleOption {
        question: usize,
        option: usize,
    },
    SetOther {
        question: usize,
        text: String,
    },
    SetNotes {
        question: usize,
        text: String,
    },
    AnswerQuestions(RequestId),
    ApprovePlan {
        request: RequestId,
        mode: PermissionMode,
        overall: String,
    },
    EditComment(usize),
    SetComment {
        section: usize,
        text: String,
    },
    CancelComment,
    ContinueHere,
    GoToWindow,
    AskSide(String),
    ToggleSide,
    DismissSide,
    CopySide(usize),
    /// Follow a subagent's conversation in the side panel.
    OpenPeek(wire::TaskId),
    ClosePeek,
    StopTask(wire::TaskId),
    StopAllTasks,
    SideToConversation,
    BranchSide,
    /// Opens the session's worktree with the opener at this place in the menu (0: here).
    OpenWorktree(usize),
    Zoom(Zoom),
    /// The other look (Gruvbox or plain).
    SwitchLook,
    /// Code's colours changed: the look, or the desktop's dark or light.
    RecolourCode,
    /// The key that sends the draft, as chosen in the settings.
    SendWith(settings::SendKey),
    /// The language of text in Han characters alone, as chosen in the settings.
    HanLanguage(crate::cjk::CjkLanguage),
    /// Whether a sandboxed session's messages to other sessions go without asking, flipped.
    ToggleMessagesUnasked,
    /// Picks a folder for the sandbox to let Claude into so.
    AddSandboxFolder(SandboxAccess),
    /// Takes a folder, as shown, from those the sandbox lets Claude into so.
    ForgetSandboxFolder {
        access: SandboxAccess,
        folder: String,
    },
    /// The composer's height as dragged, in logical pixels; none to fit it to its text again.
    ComposerHeight(Option<u16>),
    /// The sidebar's width as dragged, in logical pixels; none for the default again.
    SidebarWidth(Option<u16>),
    /// Ctrl+V in the composer: the clipboard's image, if it holds one, goes with the message.
    PasteImage,
    /// An image taken off the next message, by its place.
    RemoveImage(usize),
    /// The `>_` button: the draft made a shell command, or a message again.
    ToggleShell,
    FilterSessions(String),
    /// Opens or folds the run of tool calls whose first call is at this place.
    ToggleRun(usize),
    ShowUsage,
    HideUsage,
    /// Stops the watched session in its window and continues it here.
    BringHere,
    /// Signed in elsewhere: starts Claude Code again, and sends what failed for want of it.
    TrySignInAgain,
    /// Puts away how to sign in.
    DismissSignIn,
    /// Trusts the folder shown, to start Claude Code in it.
    TrustFolder,
    /// Starts Claude Code in the folder shown this once, without its configuration.
    StartWithoutFolderSettings,
    /// Turns the sandbox on or off, starting the process again.
    ToggleSandbox,
    /// Lets the sandboxed Claude use the display, or stops it, starting the process again.
    ToggleSandboxDesktop,
    /// Lets the sandboxed Claude reach USB devices and serial ports in the folder shown, or
    /// stops it, starting the process again.
    ToggleSandboxHardware,
    /// Ends every session the daemon runs, in every window, and then the daemon (asked in the
    /// settings, there confirmed).
    ShutDown,
    Quit,
}

/// What the header, the status bar and the menus show about the process and its session.
#[derive(Debug, Default)]
pub(crate) struct Status {
    pub(crate) session_id: Option<SessionId>,
    pub(crate) title: Option<String>,
    /// The program the session talks to: the command line's for a new one, the transcript's
    /// for one continued.
    pub(crate) backend: claude::Backend,
    /// The command line's: new sessions talk to it.
    pub(crate) backend_choice: claude::Backend,
    /// The model in use, as the CLI resolved it.
    pub(crate) model: String,
    pub(crate) models: Vec<wire::ModelInfo>,
    /// Picked in the menu or on the command line: kept for the processes after this one.
    pub(crate) model_choice: Option<String>,
    pub(crate) mode: Option<Mode>,
    /// Picked in the menu for this session: applied again if its process is restarted.
    pub(crate) mode_choice: Option<PermissionMode>,
    pub(crate) commands: Vec<wire::CommandInfo>,
    pub(crate) account: wire::Account,
    pub(crate) sign_in: SignIn,
    /// Claude Code's version, as it told at `initialize`.
    pub(crate) version: String,
    /// Prompts sent and not answered yet.
    pub(crate) turns: usize,
    /// The user stopped the turn under way, with Stop or by denying a tool call with nothing
    /// said: it ends as failed, which isn't to be shown as a failure.
    pub(crate) stop_asked: bool,
    pub(crate) doing: Doing,
    /// What runs besides the turn's own steps: subagents, commands, workflows.
    pub(crate) tasks: Tasks,
    /// The user stopped a turn and tasks ran on, which their list says until the next prompt.
    pub(crate) tasks_left_running: bool,
    pub(crate) context_tokens: u64,
    pub(crate) context_window: u64,
    pub(crate) cost_usd: f64,
    pub(crate) limits: Limits,
    pub(crate) worktree: Option<Worktree>,
    pub(crate) sandboxing: Sandboxing,
    /// What the settings ask of the sandbox, for its menu.
    pub(crate) sandbox_choice: SandboxChoice,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct SandboxChoice {
    pub(crate) on: bool,
    pub(crate) desktop: bool,
    /// USB devices and serial ports, for the folder shown.
    pub(crate) hardware: bool,
}

/// What keeps Claude Code's process in, as far as this window knows.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) enum Sandboxing {
    /// Not known yet: no process runs, or one is being started.
    #[default]
    Pending,
    /// The sandbox is turned off here.
    Off,
    /// The user turned on Claude Code's own (for its commands), which Landlock would break.
    ClaudeCodes,
    Landlock(sandbox::Enforcement),
    /// agy, in the sandbox edualc's adapter keeps it in (see `agy`), as far as the adapter
    /// says the kernel enforced it.
    Agy(sandbox::Enforcement),
}

/// What Claude is doing, for the status bar. That it waits for the user is told by what it
/// asks.
#[derive(Debug, Clone, Default, PartialEq, Eq, derive_more::Display)]
pub(crate) enum Doing {
    #[default]
    #[display("")]
    Nothing,
    #[display("Starting")]
    Starting,
    #[display("Thinking")]
    Thinking,
    #[display("Thinking  ~{}", crate::ui::short_count(*_0))]
    ThinkingTokens(u64),
    #[display("Writing")]
    Writing,
    #[display("Preparing {_0}")]
    Preparing(ToolName),
    #[display("Running {_0}")]
    Running(ToolName),
    /// Going on after the user answered.
    #[display("Working")]
    Working,
    #[display("Compacting the conversation")]
    Compacting,
    /// A request to the API failed, and is tried again.
    #[display("{_0}")]
    Retrying(Retry),
    /// Waiting on the API, and nothing heard from Claude Code for this long.
    #[display("No answer for {}", crate::ui::short_duration(*_0))]
    Silent(Duration),
}

/// Claude Code tries a request to the API again: which time, of how many, and why.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Retry {
    pub(crate) attempt: u32,
    pub(crate) most: u32,
    pub(crate) status: Option<u16>,
    pub(crate) cause: wire::RetryCause,
}

impl std::fmt::Display for Retry {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "Retrying {}/{} · ", self.attempt, self.most)?;
        match (self.cause, self.status) {
            (wire::RetryCause::Overloaded, _) => formatter.write_str("the API is overloaded"),
            (wire::RetryCause::RateLimit, _) => formatter.write_str("rate limited"),
            (wire::RetryCause::AuthenticationFailed, _) => formatter.write_str("sign-in refused"),
            (wire::RetryCause::CloudCredentialError, _) => {
                formatter.write_str("the cloud's credentials failed")
            }
            (wire::RetryCause::ServerError | wire::RetryCause::Unknown, Some(status)) => {
                write!(formatter, "the API answered {status}")
            }
            (wire::RetryCause::ServerError | wire::RetryCause::Unknown, None) => {
                formatter.write_str("no connection to the API")
            }
        }
    }
}

/// How much of the plan's usage limits is used, as far as the CLI said.
#[derive(Debug, Default, Clone, Copy)]
pub(crate) struct Limits {
    pub(crate) five_hour: Option<LimitUse>,
    pub(crate) seven_day: Option<LimitUse>,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct LimitUse {
    /// 0 to 1.
    pub(crate) used: f32,
    pub(crate) resets_at: Option<jiff::Timestamp>,
}

impl From<&wire::PlanLimit> for LimitUse {
    fn from(limit: &wire::PlanLimit) -> Self {
        Self {
            used: (limit.percent / 100.0).clamp(0.0, 1.0),
            resets_at: limit.resets_at,
        }
    }
}

/// The usage panel: what the CLI's `/usage` and `/context` answered, once asked.
#[derive(Debug, Default)]
pub(crate) struct UsagePanel {
    pub(crate) open: bool,
    pub(crate) report: Option<wire::UsageReport>,
    pub(crate) context: Option<wire::ContextReport>,
    /// Answers still owed.
    pub(crate) waiting: usize,
    pub(crate) error: Option<String>,
}

/// A request from the CLI waiting for the user.
#[derive(Debug)]
pub(crate) struct Ask {
    pub(crate) request: RequestId,
    pub(crate) tool_use: Option<ToolUseId>,
    pub(crate) tool: ToolName,
    pub(crate) input: Value,
    pub(crate) kind: AskKind,
}

#[derive(Debug)]
pub(crate) enum AskKind {
    /// May the tool run?
    Tool {
        /// Rules the CLI offers to remember.
        suggestions: Vec<wire::Offered>,
        description: String,
    },
    /// AskUserQuestion: what is picked, by question and option, what was typed instead, and
    /// the notes that go with each answer.
    Questions {
        asked: wire::Questions,
        picked: Vec<Vec<bool>>,
        other: Vec<String>,
        notes: Vec<String>,
    },
    /// ExitPlanMode: the plan's parts with the comment on each, and the part whose comment is
    /// being written.
    Plan {
        input: wire::PlanInput,
        parts: Vec<plan::PlanPart>,
        editing: Option<usize>,
    },
}

/// What our control requests were for, to act on their answers.
#[derive(Debug)]
pub(crate) enum Request {
    Initialize,
    Model(String),
    Mode(PermissionMode),
    Interrupt,
    StopTask(wire::TaskId),
    /// What the command followed in the side panel printed.
    TaskOutput(wire::TaskId),
    /// A side question, by its place in the side chat.
    Side(usize),
    Usage,
    ContextUsage,
}

/// A session running elsewhere, shown as it goes but not driven from here.
#[derive(Debug)]
pub(crate) struct Watch {
    pub(crate) session: LiveSession,
    /// Where its transcript is; corrected by the first read, which looks for it.
    path: PathBuf,
    /// How far the transcript has been read.
    offset: u64,
    reading: Refresh,
    /// The watch on its transcript, once it is found.
    watched: Option<Watched>,
}

/// Reading again what other processes write: one read at a time, and one more after it if
/// what is read changed meanwhile.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
enum Refresh {
    #[default]
    Idle,
    Reading,
    /// Reading, and changed since the read began.
    ReadingStale,
}

impl Refresh {
    /// Asked to read again: whether a read is to start now.
    fn wanted(&mut self) -> bool {
        let start = *self == Self::Idle;
        *self = if start {
            Self::Reading
        } else {
            Self::ReadingStale
        };
        start
    }

    /// A read came back: whether what it read changed meanwhile, to be asked for again.
    fn done(&mut self) -> bool {
        let stale = *self == Self::ReadingStale;
        *self = Self::Idle;
        stale
    }
}

/// What to draw besides the transcript and the status bar, which are drawn every time.
#[derive(Debug, Default, Clone, Copy)]
struct Redraw {
    /// A run of tool calls was opened or folded.
    runs: bool,
    menus: bool,
    prompt: bool,
    /// Both lists: this folder's sessions mark those running elsewhere.
    sessions: bool,
    side: bool,
    usage: bool,
    tasks: bool,
    peek: bool,
}

impl Redraw {
    /// After switching sessions: everything shown belonged to the one before.
    fn everything() -> Self {
        Self {
            runs: true,
            menus: true,
            prompt: true,
            sessions: true,
            side: true,
            usage: true,
            tasks: true,
            peek: true,
        }
    }
}

/// How this window's own session is doing, as the sidebar last listed it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct OwnMark {
    state: LiveStatus,
    /// Its subagents running now.
    agents: usize,
}

/// Lines of stderr kept, to say why the process ended.
const LOG_LINES: usize = 20;
/// What the window asks a process first. It stops tasks one by one, so stopping a turn leaves
/// background agents, commands and monitors running, as the user asked; what runs on is then
/// listed for them to stop or leave.
const INITIALIZE: ControlRequest = ControlRequest::Initialize {
    per_task_stop_affordance: true,
};
/// How often the running sessions are looked at without being told of a change: a process that
/// crashed leaves its record behind.
const TICK: Duration = Duration::from_secs(60);
/// The least time between two notifications about the same session.
const NOTIFY_AGAIN: Duration = Duration::from_secs(60);
/// How often a turn waiting on the API is looked at (`Event::Listen`), and how long Claude Code
/// may say nothing before the window says so.
const LISTEN: Duration = Duration::from_secs(10);
const QUIET: Duration = Duration::from_secs(30);

pub(crate) struct App {
    view: View,
    events: Sender<Event>,
    chat: Chat,
    status: Status,
    asks: VecDeque<Ask>,
    requests: HashMap<RequestId, Request>,
    process: Option<Claude>,
    generation: Generation,
    cwd: PathBuf,
    sessions: Vec<SessionInfo>,
    /// The slash commands the draft could become, by their place in `status.commands`.
    command_matches: Vec<usize>,
    recall: Recall,
    log: VecDeque<String>,
    live: Vec<LiveSession>,
    /// The sessions the daemon runs, which outlive their windows.
    hosted: Vec<Hosted>,
    /// Images pasted into the composer, to go with the next message.
    attached: Vec<Picture>,
    /// The rows Claude Code replayed for the `!` commands run since the last prompt (their
    /// echoes and output): it runs them beside the conversation, so they go to Claude ahead of
    /// the next prompt, as the terminal gives them.
    shell_rows: Vec<String>,
    /// The conversation's images, decoded for showing.
    images: images::Images,
    listing_live: Refresh,
    /// When each session running elsewhere was last notified about.
    notified: HashMap<SessionId, web_time::Instant>,
    watch: Option<Watch>,
    /// The subagent whose conversation the side panel follows.
    peek: Option<Peek>,
    watcher: Option<Watcher>,
    /// The watch on the running sessions' records.
    records: Option<Watched>,
    side: SideChat,
    /// What a worktree can be opened in here, as the menu lists them after "this window".
    openers: Vec<desktop::Opener>,
    settings: Settings,
    /// The desktop's text size, in logical pixels, which the zoom scales.
    desktop_text_size: f32,
    /// What the list of sessions is narrowed to.
    session_filter: String,
    /// How the own session was last listed in the sidebar; none while no process runs it here.
    own_listed: Option<OwnMark>,
    /// The runs of tool calls opened, by their first call; the others are folded.
    open_runs: HashSet<usize>,
    usage: UsagePanel,
    /// A start held back until the folder is trusted.
    trust_gate: TrustGate,
    /// Folders the daemon found trusted in the terminal: starting there again asks nothing.
    accepted_in_terminal: HashSet<PathBuf>,
    /// The command line's prompt for a session continued: sent once its transcript is shown,
    /// which replaces the conversation, the prompt's bubble with it.
    held_prompt: Option<String>,
    daemon: Daemon,
    /// The process is to start again, as the sandbox changed or a sandboxed session's sign-in
    /// ran out, while something was under way that starting it again would interrupt (a
    /// question waiting for its answers, say): it starts again once nothing is.
    restart_waits: bool,
    /// The last prompt sent, for `SignIn::Refused` to keep if its turn fails for that.
    last_prompt: Option<SentPrompt>,
    /// Claude Code said the turn under way failed for want of a sign-in; its result ends it.
    turn_refused_sign_in: bool,
    /// The line to the daemon, which runs Claude Code and does what the desktop is asked.
    link: Link,
    /// What the daemon was asked to do, by request, for what its answer means.
    acting: HashMap<ActId, Acting>,
    last_act: ActId,
    tick: slint::Timer,
    /// Runs while a turn is under way (see `Event::Listen`).
    listen: slint::Timer,
    /// Runs while a command's output is followed (see `Event::PollOutput`).
    polling: slint::Timer,
    /// When the process last said something.
    heard: web_time::Instant,
    redraw: Redraw,
}

impl App {
    pub(crate) fn new(
        window: AppWindow,
        events: Sender<Event>,
        link: Link,
        desktop_found: Desktop,
        cwd: PathBuf,
        backend: claude::Backend,
        model: Option<String>,
    ) -> Self {
        let Desktop {
            openers,
            fonts,
            text_size: desktop_text_size,
        } = desktop_found;
        let mut view = View::new(window, &openers, fonts, events.clone());
        let settings = settings::load().unwrap_or_else(|error| {
            tracing::warn!(%error, "starting with the default settings");
            Settings::default()
        });
        view.set_text_size(desktop_text_size * settings.scale());
        view.set_composer_height(settings.composer_height_px);
        view.set_sidebar_width(settings.sidebar_width_px);
        view.set_look(settings.look);
        view.set_send_key(settings.send_with);
        view.set_han_language(settings.han_language());
        view.set_sandbox_folders(&settings);
        view.set_messages_unasked(settings.sandbox_messages_unasked);
        Self {
            view,
            images: images::Images::start(events.clone()),
            events,
            chat: Chat::default(),
            status: Status {
                backend,
                backend_choice: backend,
                model_choice: model,
                sandbox_choice: SandboxChoice {
                    on: settings.sandbox,
                    desktop: settings.sandbox_desktop,
                    hardware: settings.hardware_in(&cwd),
                },
                ..Status::default()
            },
            asks: VecDeque::new(),
            requests: HashMap::new(),
            process: None,
            generation: Generation::default(),
            cwd,
            sessions: Vec::new(),
            command_matches: Vec::new(),
            recall: Recall::Writing,
            log: VecDeque::new(),
            live: Vec::new(),
            hosted: Vec::new(),
            attached: Vec::new(),
            shell_rows: Vec::new(),
            listing_live: Refresh::Idle,
            notified: HashMap::new(),
            watch: None,
            peek: None,
            watcher: None,
            records: None,
            side: SideChat::default(),
            openers,
            settings,
            desktop_text_size,
            session_filter: String::new(),
            own_listed: None,
            open_runs: HashSet::new(),
            usage: UsagePanel::default(),
            trust_gate: TrustGate::Open,
            accepted_in_terminal: HashSet::new(),
            held_prompt: None,
            daemon: Daemon::Running,
            restart_waits: false,
            last_prompt: None,
            turn_refused_sign_in: false,
            link,
            acting: HashMap::new(),
            last_act: ActId::default(),
            tick: slint::Timer::default(),
            listen: slint::Timer::default(),
            polling: slint::Timer::default(),
            heard: web_time::Instant::now(),
            redraw: Redraw::default(),
        }
    }

    /// Opens the first session (`resume`, or a new one), sends the first prompt, and starts
    /// looking at running sessions.
    pub(crate) fn begin(
        &mut self,
        hosted: Vec<Hosted>,
        resume: Option<SessionId>,
        mode: Option<PermissionMode>,
        prompt: Option<&str>,
    ) {
        self.load_sessions();
        // Known before resuming: a session the daemon runs is attached to, one running
        // elsewhere watched, neither resumed by a second process writing the same transcript.
        self.view.set_running_sessions(hosted.len());
        self.hosted = hosted;
        let resuming = resume.is_some();
        if resuming {
            self.live = sessions::live();
        }
        match resume {
            Some(id) => self.open_session(id),
            None => self.new_session(),
        }
        // Said in the window, not only the log: what is changed now isn't kept.
        if let Err(error) = settings::load() {
            self.chat.error(format!(
                "{error}\nThe window runs with the default settings, and keeps no change until the \
                 file is put right."
            ));
        }
        if let Some(mode) = mode {
            self.set_mode(mode);
        }
        match prompt {
            // Sent once the transcript is shown (see `session_loaded`).
            Some(prompt) if resuming => self.held_prompt = Some(prompt.to_owned()),
            Some(prompt) => self.send(prompt),
            None => {}
        }
        match Watcher::start(self.events.clone()) {
            Ok(watcher) => {
                self.records = watcher
                    .folder(&sessions::config_dir().join("sessions"))
                    .inspect_err(|error| tracing::warn!(%error, "running sessions go unnoticed"))
                    .ok();
                self.watcher = Some(watcher);
            }
            Err(error) => tracing::warn!(%error, "changes go unnoticed until the next tick"),
        }
        let events = self.events.clone();
        self.tick.start(slint::TimerMode::Repeated, TICK, move || {
            if events.try_send(Event::Tick).is_err() {
                tracing::debug!("a tick after the window closed");
            }
        });
        self.tick();
        self.render();
    }

    /// Takes events until the window quits, drawing once per batch.
    pub(crate) async fn run(mut self, events: Receiver<Event>) {
        while let Ok(event) = events.recv().await {
            let mut flow = self.handle(event);
            while flow.is_continue()
                && let Ok(event) = events.try_recv()
            {
                flow = self.handle(event);
            }
            if flow.is_break() {
                break;
            }
            if self.restart_waits && !self.under_way() {
                self.restart();
            }
            self.render();
        }
    }

    fn handle(&mut self, event: Event) -> ControlFlow<()> {
        match event {
            Event::Action(action) => return self.act(action),
            Event::Claude { generation, output } => self.take(generation, output),
            Event::SessionsListed { cwd, sessions } => self.sessions_listed(&cwd, sessions),
            Event::ProjectsListed(projects) => self.view.render_projects(&projects),
            Event::SessionLoaded { id, path, result } => self.session_loaded(&id, path, result),
            Event::TranscriptGrew { id, result } => self.transcript_grew(&id, result),
            Event::PeekGrew { task, result } => self.peek_grew(&task, result),
            Event::LiveListed(live) => self.live_listed(live),
            Event::WorktreeRead { id, worktree } => self.worktree_read(&id, worktree),
            Event::Acted { request, result } => self.acted(request, result),
            Event::Picked { request, folder } => match (self.acting.remove(&request), folder) {
                (Some(Acting::Picking(FolderUse::Project)), Some(folder)) => {
                    match Folder::at(folder) {
                        Ok(folder) => self.open_project(folder),
                        Err(problem) => self.chat.error(problem.to_string()),
                    }
                }
                (Some(Acting::Picking(FolderUse::Sandbox(access))), Some(folder)) => {
                    let folders = self.settings.sandbox_folders(access);
                    if !folders.contains(&folder) {
                        folders.push(folder);
                        self.sandbox_folders_changed();
                    }
                }
                // Nothing picked, or for a request since forgotten.
                (Some(_) | None, _) => {}
            },
            Event::Trusted {
                request,
                accepted,
                runs,
            } => self.trust_read(request, accepted, runs),
            Event::CodeColoured(coloured) => self.view.code_coloured(coloured),
            Event::ImageShown { key, shown } => {
                if self.images.wanted(key) {
                    self.view
                        .show_image(key, shown.as_ref().map_err(ToString::to_string));
                    self.chat.image_shown(key);
                }
            }
            Event::Pasted { request, pasted } => {
                self.acting.remove(&request);
                match pasted {
                    Ok(picture) => {
                        self.attached.push(picture);
                        self.view.render_attached(&self.attached);
                    }
                    // Text was pasted, most likely; the composer took it.
                    Err(PasteFailure::NoImage) => {}
                    Err(failure) => self.chat.error(failure.to_string()),
                }
            }
            Event::DaemonLost => {
                self.process = None;
                self.forget_process();
                match self.daemon {
                    Daemon::Running => self.chat.error(
                        "Lost the edualc daemon, which runs Claude Code. Open edualc again to go on.",
                    ),
                    // Said as it began.
                    Daemon::ShuttingDown => {}
                }
            }
            Event::DaemonShuttingDown => {
                self.daemon = Daemon::ShuttingDown;
                self.chat.info(
                    "edualc is shutting down, as asked in a window: every session ends. Open \
                     edualc again to go on; the sidebar continues this session.",
                );
            }
            Event::Hosted(hosted) => {
                self.view.set_running_sessions(hosted.len());
                self.hosted = hosted;
                self.redraw.sessions = true;
            }
            Event::Changed(watched) => self.changed(watched),
            Event::Tick => self.tick(),
            Event::Listen => self.listen(),
            Event::PollOutput => self.poll_output(),
        }
        ControlFlow::Continue(())
    }

    fn act(&mut self, action: Action) -> ControlFlow<()> {
        match action {
            Action::Send(text) => self.send(&text),
            Action::Interrupt => self.interrupt(),
            Action::NewSession => self.new_session(),
            Action::OpenSession(id) => self.open_session(id),
            Action::OpenProject(folder) => self.open_project(folder),
            Action::BrowseProject => self.pick_folder(FolderUse::Project),
            // The relay is told as the sandboxed process starts (`SandboxWish`).
            Action::ToggleMessagesUnasked => {
                self.settings.sandbox_messages_unasked = !self.settings.sandbox_messages_unasked;
                self.view
                    .set_messages_unasked(self.settings.sandbox_messages_unasked);
                self.sandbox_changed();
            }
            Action::AddSandboxFolder(access) => self.pick_folder(FolderUse::Sandbox(access)),
            Action::ForgetSandboxFolder { access, folder } => {
                self.settings
                    .sandbox_folders(access)
                    .retain(|kept| kept.to_string_lossy() != folder);
                self.sandbox_folders_changed();
            }
            Action::ListProjects => self.in_background(sessions::projects, Event::ProjectsListed),
            Action::ChooseModel(index) => self.choose_model(index),
            Action::ChooseMode(index) => self.choose_mode(index),
            Action::DraftEdited(draft) => self.draft_edited(&draft),
            Action::PickCommand(index) => self.pick_command(index),
            Action::RecallOlder => self.recall_older(),
            Action::RecallNewer => self.recall_newer(),
            Action::RunCommand(index) => self.run_command(index),
            Action::Toggle(index) => self.chat.toggle(index),
            Action::CopyEntry(index) => {
                if let Some(entry) = self.chat.entries.get(index) {
                    self.view.copy(entry.copy_text());
                }
            }
            Action::Copy(text) => self.view.copy(&text),
            Action::OpenUrl(url) => self.ask_desktop(Act::OpenUrl(url), Acting::Desktop),
            Action::Allow(request) => self.allow(&request, false),
            Action::AllowAlways(request) => self.allow(&request, true),
            Action::Deny { request, feedback } => self.deny(&request, &feedback),
            Action::ToggleOption { question, option } => self.toggle_option(question, option),
            Action::SetOther { question, text } => self.set_other(question, text),
            Action::SetNotes { question, text } => self.set_notes(question, text),
            Action::AnswerQuestions(request) => self.answer_questions(&request),
            Action::ApprovePlan {
                request,
                mode,
                overall,
            } => self.approve_plan(&request, mode, &overall),
            Action::EditComment(section) => self.edit_comment(Some(section)),
            Action::CancelComment => self.edit_comment(None),
            Action::SetComment { section, text } => self.set_comment(section, &text),
            Action::ContinueHere => self.continue_here(),
            Action::GoToWindow => self.go_to_window(),
            Action::AskSide(question) => self.ask_side(&question),
            Action::ToggleSide => self.toggle_side(),
            Action::DismissSide => self.dismiss_side(),
            Action::CopySide(index) => self.copy_side(index),
            Action::SideToConversation => self.side_to_conversation(),
            Action::BranchSide => self.branch_side(),
            Action::OpenPeek(task) => self.open_peek(task),
            Action::ClosePeek => self.close_peek(),
            Action::StopTask(task) => self.stop_task(task),
            Action::StopAllTasks => self.stop_all_tasks(),
            Action::OpenWorktree(index) => self.open_worktree(index),
            Action::ShowUsage => self.show_usage(),
            Action::HideUsage => {
                self.usage.open = false;
                self.redraw.usage = true;
            }
            Action::BringHere => self.bring_here(),
            Action::TrySignInAgain => self.try_sign_in_again(),
            Action::DismissSignIn => {
                if self.status.sign_in.waits_for_user() {
                    self.status.sign_in = SignIn::Unknown;
                }
            }
            Action::TrustFolder => self.trust_folder(),
            Action::StartWithoutFolderSettings => self.start_without_folder_settings(),
            Action::ToggleSandbox => {
                self.settings.sandbox = !self.settings.sandbox;
                self.sandbox_changed();
            }
            Action::ToggleSandboxDesktop => {
                self.settings.sandbox_desktop = !self.settings.sandbox_desktop;
                self.sandbox_changed();
            }
            Action::ToggleSandboxHardware => {
                self.settings.toggle_hardware(&self.cwd);
                self.sandbox_changed();
            }
            Action::ToggleRun(index) => {
                if !self.open_runs.remove(&index) {
                    self.open_runs.insert(index);
                }
                self.redraw.runs = true;
            }
            Action::FilterSessions(filter) => {
                self.session_filter = filter;
                self.redraw.sessions = true;
            }
            Action::Zoom(zoom) => {
                self.settings.zoom(zoom);
                self.view
                    .set_text_size(self.desktop_text_size * self.settings.scale());
                if let Err(error) = settings::save(&self.settings) {
                    tracing::warn!(%error, "couldn't keep the zoom");
                }
            }
            Action::SwitchLook => {
                self.settings.look = self.settings.look.other();
                self.view.set_look(self.settings.look);
                if let Err(error) = settings::save(&self.settings) {
                    tracing::warn!(%error, "couldn't keep the look");
                }
            }
            Action::RecolourCode => {
                self.view.recolour_code();
                // Messages already drawn, and the side panel's answers, are drawn again in them.
                self.chat.touch_all();
                self.redraw.side = true;
            }
            Action::SendWith(key) => {
                self.settings.send_with = key;
                self.view.set_send_key(key);
                if let Err(error) = settings::save(&self.settings) {
                    tracing::warn!(%error, "couldn't keep the key that sends");
                }
            }
            Action::HanLanguage(language) => {
                self.settings.han_text = Some(language);
                self.view.set_han_language(language);
                // Messages already drawn are drawn again in it.
                self.chat.touch_all();
                if let Err(error) = settings::save(&self.settings) {
                    tracing::warn!(%error, "couldn't keep the language of Han text");
                }
            }
            Action::ComposerHeight(height_px) => {
                self.settings.composer_height_px = height_px;
                if let Err(error) = settings::save(&self.settings) {
                    tracing::warn!(%error, "couldn't keep the composer's height");
                }
            }
            Action::SidebarWidth(width_px) => {
                self.settings.sidebar_width_px = width_px;
                if let Err(error) = settings::save(&self.settings) {
                    tracing::warn!(%error, "couldn't keep the sidebar's width");
                }
            }
            Action::PasteImage => self.ask_desktop(Act::PasteImage, Acting::Pasting),
            Action::RemoveImage(index) => {
                if index < self.attached.len() {
                    self.attached.remove(index);
                    self.view.render_attached(&self.attached);
                }
            }
            Action::ToggleShell => self.toggle_shell(),
            Action::ShutDown => {
                if link::send(&self.link, ToDaemon::ShutDown).is_err() {
                    self.chat.error("Lost the edualc daemon.");
                }
            }
            Action::Quit => {
                self.quit();
                return ControlFlow::Break(());
            }
        }
        ControlFlow::Continue(())
    }

    fn render(&mut self) {
        let redraw = std::mem::take(&mut self.redraw);
        let changes = self.chat.take_changes();
        if changes.contains(&Change::Reset) {
            let shown = self.chat.image_keys();
            self.images.keep(&shown);
            self.view.keep_images(&shown);
        }
        for (key, bytes) in self.chat.take_images() {
            if !self.images.ask(key, bytes) {
                self.view
                    .show_image(key, Err("the window's image decoder is gone".to_owned()));
            }
        }
        self.view
            .render_transcript(&self.chat, changes, &self.cwd, &self.open_runs, redraw.runs);
        self.view.render_activity(
            &self.status,
            !self.asks.is_empty(),
            self.chat.shell_running(),
        );
        let asking = match &self.trust_gate {
            TrustGate::Asking { runs, .. } => Some(runs.as_slice()),
            TrustGate::Open | TrustGate::Checking { .. } => None,
        };
        self.view.render_status(
            &self.status,
            asking,
            &self.cwd,
            self.watch.as_ref(),
            self.current_worktree(),
        );
        if redraw.menus {
            self.view.render_menus(&self.status);
        }
        if redraw.prompt {
            self.view
                .render_prompt(&self.asks, &self.cwd, self.status.backend);
        }
        if redraw.usage {
            self.view.render_usage(&self.usage, &self.status);
        }
        if redraw.tasks {
            self.view
                .render_tasks(&self.status.tasks, self.status.tasks_left_running);
        }
        if redraw.peek || redraw.tasks {
            self.view
                .render_peek(self.peek.as_ref(), &self.status.tasks);
        }
        if redraw.side {
            let can_ask = self.watch.is_none();
            let can_branch = self.status.turns == 0
                && self.status.session_id.is_some()
                && self.side.has_answers()
                && !self.side.pending();
            self.view.render_side(&self.side, can_ask, can_branch);
        }
        // The own session's row changes with its turns, questions and agents, which no list
        // update says.
        let own = self.own_mark();
        if redraw.sessions || own != self.own_listed {
            self.own_listed = own;
            let shown = self
                .watch
                .as_ref()
                .map(|watch| &watch.session.id)
                .or(self.status.session_id.as_ref());
            let rows = sidebar::rows(&sidebar::Listing {
                own: own.and_then(|mark| self.own_session(mark)),
                live: &self.live,
                saved: &self.sessions,
                shown,
                folder: &self.cwd,
                filter: &self.session_filter,
            });
            self.view
                .render_sidebar(&rows, !self.session_filter.trim().is_empty());
        }
    }

    /// How this window's own session is doing, while its process runs here.
    fn own_mark(&self) -> Option<OwnMark> {
        if self.watch.is_some() || self.process.is_none() || self.status.session_id.is_none() {
            return None;
        }
        let state = if !self.asks.is_empty() {
            LiveStatus::Waiting
        } else if self.status.turns > 0 {
            LiveStatus::Busy
        } else {
            LiveStatus::Idle
        };
        let agents = self
            .status
            .tasks
            .running()
            .filter(|running| running.task.task_type.is_agent() && !running.task.ambient)
            .count();
        Some(OwnMark { state, agents })
    }

    /// This window's own session as the sidebar lists it.
    fn own_session(&self, mark: OwnMark) -> Option<sidebar::Own<'_>> {
        let id = self.status.session_id.as_ref()?;
        let title = self
            .status
            .title
            .as_deref()
            .or_else(|| {
                self.sessions
                    .iter()
                    .find(|saved| &saved.id == id)
                    .map(|saved| saved.title.as_str())
            })
            .unwrap_or("New session");
        Some(sidebar::Own {
            id,
            title,
            name: MessagingName::of(id, &self.cwd).to_string(),
            state: mark.state,
            worktree: self
                .status
                .worktree
                .as_ref()
                .map(|worktree| worktree.name.as_str()),
            agents: mark.agents,
        })
    }

    /// Runs `work` on a thread of its own; its result comes back as the event `done` makes.
    #[cfg(not(target_arch = "wasm32"))]
    fn in_background<T: Send + 'static>(
        &self,
        work: impl FnOnce() -> T + Send + 'static,
        done: impl FnOnce(T) -> Event + Send + 'static,
    ) {
        let events = self.events.clone();
        std::thread::spawn(move || {
            if events.send_blocking(done(work())).is_err() {
                tracing::debug!("the window closed before background work finished");
            }
        });
    }

    /// Runs `work` now, in the browser demo, which has no threads (and whose work reads only
    /// what is built into the page); its result comes back as the event `done` makes, after
    /// what is handled now.
    #[cfg(target_arch = "wasm32")]
    fn in_background<T>(&self, work: impl FnOnce() -> T, done: impl FnOnce(T) -> Event) {
        if self.events.try_send(done(work())).is_err() {
            tracing::debug!("the window closed before background work finished");
        }
    }

    /// Starts a process in the working directory once it is trusted: here, or in the terminal,
    /// which the daemon is asked about.
    fn start(&mut self, resume: Option<Resume>) {
        // What a folder configures is Claude Code's; agy reads none of it.
        if self.status.backend == claude::Backend::Agy
            || trust::trusted_in_edualc(&self.cwd, &self.settings)
            || self.accepted_in_terminal.contains(&self.cwd)
        {
            self.launch(resume, FolderConfiguration::Read);
            return;
        }
        self.status.sandboxing = if self.settings.sandbox {
            Sandboxing::Pending
        } else {
            Sandboxing::Off
        };
        self.last_act = self.last_act.next();
        let request = self.last_act;
        let check = ToDaemon::Act {
            request,
            act: Act::CheckTrust {
                folder: self.cwd.clone(),
            },
        };
        self.trust_gate = if link::send(&self.link, check).is_ok() {
            TrustGate::Checking {
                request,
                resume,
                held: None,
            }
        } else {
            TrustGate::Asking {
                resume,
                runs: vec![Runs::Unreadable {
                    file: "its settings".to_owned(),
                    reason: "the edualc daemon couldn't be asked".to_owned(),
                }],
            }
        };
    }

    /// The daemon's answer on the folder's trust: the start held back goes ahead, with a prompt
    /// sent meanwhile, or the user is asked, shown what the folder would run. An answer for a
    /// start since dropped or replaced is no longer waited for.
    fn trust_read(&mut self, request: ActId, accepted: bool, runs: Vec<Runs>) {
        match self.trust_gate.answered(request, accepted, runs) {
            TrustAnswer::Trusted { resume, held } => {
                self.accepted_in_terminal.insert(self.cwd.clone());
                self.launch(resume, FolderConfiguration::Read);
                if let Some(prompt) = held {
                    self.send(&prompt);
                }
            }
            TrustAnswer::Asked | TrustAnswer::Stale => {}
        }
    }

    /// Starts a process in the working directory: trusted, or without the folder's configuration.
    fn launch(&mut self, resume: Option<Resume>, folder_configuration: FolderConfiguration) {
        self.trust_gate = TrustGate::Open;
        self.generation = self.generation.next();
        self.log.clear();
        // The folder may be another than at the last start.
        self.status.sandbox_choice.hardware = self.settings.hardware_in(&self.cwd);
        if self.status.worktree.is_none() {
            self.status.worktree = sessions::linked_worktree(&self.cwd);
        }
        let own_sandbox = self.settings.sandbox && sandbox::claude_codes_own_is_on();
        let launch = Launch {
            backend: self.status.backend,
            cwd: self.cwd.clone(),
            conversation: Resume::conversation(resume),
            model: self.status.model_choice.clone(),
            sandbox: (self.settings.sandbox && !own_sandbox).then(|| SandboxWish {
                desktop: self.settings.sandbox_desktop,
                hardware: self.settings.hardware_in(&self.cwd),
                messages_unasked: self.settings.sandbox_messages_unasked,
                read: self.settings.sandbox_read.clone(),
                write: self.settings.sandbox_write.clone(),
            }),
            folder_configuration,
        };
        // How far it is sandboxed is known once the daemon started it (`Output::Started`), for
        // agy once its adapter says (`System::EdualcSandbox`).
        self.status.sandboxing = if self.status.backend == claude::Backend::Agy {
            Sandboxing::Pending
        } else if own_sandbox {
            Sandboxing::ClaudeCodes
        } else if self.settings.sandbox {
            Sandboxing::Pending
        } else {
            Sandboxing::Off
        };
        match Claude::start(&self.link, self.generation, launch) {
            Ok(process) => self.process = Some(process),
            Err(error) => {
                self.chat.error(error.to_string());
                return;
            }
        }
        self.request(&INITIALIZE, Request::Initialize);
        if let Some(mode) = self.status.mode_choice {
            self.request(
                &ControlRequest::SetPermissionMode { mode },
                Request::Mode(mode),
            );
        }
    }

    /// Has the daemon do something for the desktop; its answer comes as `Event::Acted`.
    fn ask_desktop(&mut self, act: Act, acting: Acting) {
        self.last_act = self.last_act.next();
        let request = self.last_act;
        if link::send(&self.link, ToDaemon::Act { request, act }).is_err() {
            self.chat.error("Lost the edualc daemon.");
            return;
        }
        self.acting.insert(request, acting);
    }

    fn acted(&mut self, request: ActId, result: Result<(), ActFailure>) {
        match (self.acting.remove(&request), result) {
            (Some(Acting::Bringing(id)), result) => self.brought(&id, result),
            (Some(Acting::Desktop), Err(failure)) => {
                tracing::warn!(%failure, "the desktop didn't do it");
                self.chat.error(failure.to_string());
            }
            (Some(Acting::Notifying), Err(failure)) => {
                tracing::warn!(%failure, "couldn't notify");
            }
            // A paste is answered as `Pasted`, a folder picked as `Picked`.
            (
                Some(Acting::Desktop | Acting::Notifying | Acting::Pasting | Acting::Picking(_)),
                Ok(()),
            )
            | (None, _) => {}
            (Some(Acting::Pasting | Acting::Picking(_)), Err(failure)) => {
                self.chat.error(failure.to_string());
            }
        }
    }

    /// The desktop's folder picker, for a folder to be used so; `Event::Picked` answers.
    fn pick_folder(&mut self, folder_use: FolderUse) {
        self.ask_desktop(Act::PickFolder(folder_use), Acting::Picking(folder_use));
    }

    /// Keeps the changed folders the sandbox lets Claude into, shows them, and starts a
    /// sandboxed process again under them, continuing the session.
    fn sandbox_folders_changed(&mut self) {
        self.view.set_sandbox_folders(&self.settings);
        if self.settings.sandbox {
            self.sandbox_changed();
        } else if let Err(error) = settings::save(&self.settings) {
            tracing::warn!(%error, "couldn't keep the sandbox's folders");
        }
    }

    /// Trusts the folder shown and starts what waited for it.
    fn trust_folder(&mut self) {
        let resume = match std::mem::take(&mut self.trust_gate) {
            TrustGate::Asking { resume, .. } => resume,
            kept @ (TrustGate::Open | TrustGate::Checking { .. }) => {
                self.trust_gate = kept;
                return;
            }
        };
        self.settings.trusted_folders.push(self.cwd.clone());
        if let Err(error) = settings::save(&self.settings) {
            tracing::warn!(%error, "couldn't keep the trusted folder");
        }
        self.launch(resume, FolderConfiguration::Read);
        self.view.focus_composer();
    }

    /// Starts what waited for the folder's trust without its configuration, this once: the
    /// next start asks again.
    fn start_without_folder_settings(&mut self) {
        let resume = match std::mem::take(&mut self.trust_gate) {
            TrustGate::Asking { resume, .. } => resume,
            kept @ (TrustGate::Open | TrustGate::Checking { .. }) => {
                self.trust_gate = kept;
                return;
            }
        };
        self.chat.info(
            "Started without this folder's settings: its hooks, permissions, environment and \
             MCP servers don't apply, and neither do your own MCP servers or claude.ai \
             connectors.",
        );
        self.launch(resume, FolderConfiguration::Skipped);
        self.view.focus_composer();
    }

    /// Keeps the changed sandbox setting and starts the process again under it, continuing the
    /// session: a sandbox can't be changed on a running process.
    fn sandbox_changed(&mut self) {
        self.status.sandbox_choice = SandboxChoice {
            on: self.settings.sandbox,
            desktop: self.settings.sandbox_desktop,
            hardware: self.settings.hardware_in(&self.cwd),
        };
        if let Err(error) = settings::save(&self.settings) {
            tracing::warn!(%error, "couldn't keep the sandbox setting");
        }
        if self.process.is_some() && self.under_way() {
            if !self.restart_waits {
                self.chat.info(
                    "The sandbox changes once Claude is done here: Claude Code starts again then, \
                     and the conversation goes on.",
                );
            }
            self.restart_waits = true;
        } else if self.process.is_some() {
            self.restart();
        } else {
            self.status.sandboxing = if self.settings.sandbox {
                Sandboxing::Pending
            } else {
                Sandboxing::Off
            };
        }
        self.redraw.menus = true;
    }

    /// Ends the process, if any, and drops a start held back for trust: it was the session's
    /// being left, perhaps in another folder.
    fn stop(&mut self) {
        self.process = None;
        self.trust_gate = TrustGate::Open;
        self.restart_waits = false;
        self.forget_process();
    }

    /// A sandboxed session's sign-in ran out: it gets the user's once, as Claude Code starts,
    /// and can't renew it inside the sandbox (see `spawn`). So it starts again, with the
    /// sign-in there is now, once nothing is under way, and the conversation goes on.
    fn sign_in_ran_out(&mut self) {
        self.chat.info(
            "This session runs in the sandbox, which gets your sign-in once, as Claude Code \
             starts, and that sign-in has run out. Claude Code starts again with your current \
             sign-in once nothing is under way here; then send your message again.",
        );
        self.restart_waits = true;
    }

    /// "Try again": Claude Code reads the sign-in as it starts, so it starts again, and what
    /// failed goes again, leaving anything written in the composer since. With something under
    /// way that a start would cut short, it waits for that, and what failed goes back to the
    /// user to send.
    fn try_sign_in_again(&mut self) {
        let failed = match std::mem::take(&mut self.status.sign_in) {
            SignIn::Refused { failed } => failed,
            SignIn::Asked => None,
            other @ (SignIn::Unknown | SignIn::SignedIn | SignIn::Renewing) => {
                self.status.sign_in = other;
                return;
            }
        };
        if self.under_way() {
            self.restart_waits = true;
            if let Some(failed) = failed {
                self.attached = failed.images;
                self.view.render_attached(&self.attached);
                self.put_draft(&failed.text);
            }
            return;
        }
        self.stop();
        let Some(failed) = failed else {
            self.ensure_process();
            return;
        };
        let written = self.view.draft();
        self.attached = failed.images;
        self.view.render_attached(&self.attached);
        self.send(&failed.text);
        if !written.trim().is_empty() {
            self.put_draft(&written);
        }
        self.view.focus_composer();
    }

    /// Starts the process again under the settings as they are now, continuing the session.
    fn restart(&mut self) {
        let resume = self.status.session_id.clone().map(Resume::Continue);
        self.stop();
        self.start(resume);
    }

    /// Drops what the process had under way, as it is gone: nothing it still says is taken,
    /// and nothing waits for its answers.
    fn forget_process(&mut self) {
        self.generation = self.generation.next();
        self.asks.clear();
        self.requests.clear();
        self.status.turns = 0;
        self.status.stop_asked = false;
        self.status.doing = Doing::Nothing;
        // They ran in the process.
        self.status.tasks.clear();
        self.status.tasks_left_running = false;
        self.redraw.tasks = true;
        self.chat.stop_shells();
        self.usage.waiting = 0;
        self.side_lost();
        self.redraw.prompt = true;
        self.redraw.usage = true;
    }

    /// Starts a process for the session shown, unless one runs, the session runs elsewhere or
    /// a start waits for the folder's trust. Whether one runs now.
    fn ensure_process(&mut self) -> bool {
        if self.process.is_none()
            && self.watch.is_none()
            && matches!(self.trust_gate, TrustGate::Open)
        {
            let resume = self.status.session_id.clone().map(Resume::Continue);
            self.start(resume);
        }
        self.process.is_some()
    }

    /// Whether the session shown has something under way that ending its process would cut
    /// short: a turn (from its prompt being written on), a `!` command, a request waiting for
    /// the user, or a task running in the background (a subagent, a shell, a monitor), which a
    /// stopped turn leaves running.
    fn under_way(&self) -> bool {
        self.status.turns > 0
            || self.chat.shell_running()
            || !self.asks.is_empty()
            || self
                .status
                .tasks
                .running()
                .any(|running| !running.task.ambient)
    }

    /// Lets go of the process: one with something under way runs on in the daemon, for this
    /// window or another to come back to (it is listed as running); an idle one ends, as its
    /// transcript resumes it as it was.
    fn let_go(&mut self) {
        let under_way = self.under_way();
        if let Some(process) = self.process.take()
            && under_way
        {
            process.detach();
        }
    }

    /// Leaves the session shown for another, letting go of its process.
    fn leave(&mut self) {
        self.close_peek();
        self.let_go();
        self.stop();
        // A process let go of runs on, listed by its record, which the list read while it was
        // this window's left out; nothing else would read it again for up to a minute.
        self.list_live();
    }

    fn quit(&mut self) {
        self.tick.stop();
        self.generation = self.generation.next();
        self.let_go();
        if let Err(error) = slint::quit_event_loop() {
            tracing::warn!(%error, "couldn't quit the event loop");
        }
    }

    /// Writes to the process; says so in the transcript if that fails.
    fn write(&mut self, action: impl FnOnce(&mut Claude) -> Result<(), claude::Error>) {
        let Some(process) = self.process.as_mut() else {
            return;
        };
        if let Err(error) = action(process) {
            tracing::warn!(%error, "lost claude");
            self.chat.error(format!("Lost Claude Code: {error}"));
        }
    }

    /// Sends a control request, remembering what it was for. Whether it went out.
    fn request(&mut self, request: &ControlRequest, purpose: Request) -> bool {
        let Some(process) = self.process.as_mut() else {
            return false;
        };
        match process.request(request) {
            Ok(id) => {
                self.requests.insert(id, purpose);
                true
            }
            Err(error) => {
                tracing::warn!(%error, "couldn't send a control request");
                false
            }
        }
    }

    fn new_session(&mut self) {
        self.leave();
        self.watch = None;
        self.chat.clear();
        self.reset_session(None, None);
        self.status.backend = self.status.backend_choice;
        self.start(None);
        self.redraw = Redraw::everything();
        self.view.focus_composer();
    }

    fn reset_session(&mut self, id: Option<SessionId>, title: Option<String>) {
        self.status.session_id = id;
        self.status.title = title;
        self.status.mode_choice = None;
        self.status.context_tokens = 0;
        self.status.cost_usd = 0.0;
        self.status.worktree = None;
        self.open_runs.clear();
        self.shell_rows.clear();
        // What failed belongs to the session it was sent in.
        self.last_prompt = None;
        if let SignIn::Refused { failed } = &mut self.status.sign_in {
            *failed = None;
        }
        // Side questions belong to the session they were asked in.
        self.side = SideChat::default();
        self.redraw.side = true;
        self.recall = Recall::Writing;
        self.show_recall();
    }

    /// Shows a saved session and continues it; one running elsewhere is only watched.
    fn open_session(&mut self, id: SessionId) {
        if self.status.session_id.as_ref() == Some(&id) && self.watch.is_none() {
            return;
        }
        if let Some(hosted) = self
            .hosted
            .iter()
            .find(|hosted| hosted.session.as_ref() == Some(&id) && !hosted.attached)
            .cloned()
        {
            self.attach(id, &hosted);
            return;
        }
        if self.live.iter().any(|session| session.id == id) {
            self.watch_live(&id);
            return;
        }
        self.leave();
        self.watch = None;
        self.chat.clear();
        let known = self.sessions.iter().find(|session| session.id == id);
        let title = known.map(|session| session.title.clone());
        let worktree = known.and_then(|session| session.worktree.clone());
        let backend = known.map_or_else(
            || sessions::backend_of(&sessions::locate(&self.cwd, &id)),
            |session| session.backend,
        );
        self.reset_session(Some(id.clone()), title);
        self.status.worktree = worktree;
        self.status.backend = backend;
        self.load_session(id.clone(), self.cwd.clone());
        self.start(Some(Resume::Continue(id)));
        self.redraw = Redraw::everything();
        self.view.focus_composer();
    }

    /// Shows a session the daemon runs, which no window shows: its transcript as saved, then
    /// what it asked meanwhile and what it says.
    fn attach(&mut self, id: SessionId, hosted: &Hosted) {
        self.leave();
        self.watch = None;
        self.chat.clear();
        if self.cwd != hosted.cwd {
            self.cwd.clone_from(&hosted.cwd);
            self.sessions.clear();
            self.load_sessions();
        }
        let title = self
            .sessions
            .iter()
            .find(|session| session.id == id)
            .map(|session| session.title.clone());
        self.reset_session(Some(id.clone()), title);
        self.status.backend = sessions::backend_of(&sessions::locate(&self.cwd, &id));
        self.load_session(id, self.cwd.clone());
        self.generation = self.generation.next();
        match Claude::attach(&self.link, self.generation, hosted.id) {
            Ok(process) => self.process = Some(process),
            Err(error) => {
                self.chat.error(error.to_string());
                return;
            }
        }
        // What the process offers, again: the window that started it asked before.
        self.request(&INITIALIZE, Request::Initialize);
        self.redraw = Redraw::everything();
        self.view.focus_composer();
    }

    /// Loads a session's transcript in the background, looking for it: a worktree the session
    /// entered may have moved it.
    fn load_session(&self, id: SessionId, cwd: PathBuf) {
        self.in_background(
            move || {
                let path = sessions::locate(&cwd, &id);
                let result = sessions::read_from(&path, 0)
                    .map(|(lines, offset)| (Chat::from_transcript(&lines), offset));
                Event::SessionLoaded { id, path, result }
            },
            std::convert::identity,
        );
    }

    fn session_loaded(
        &mut self,
        id: &SessionId,
        path: PathBuf,
        result: std::io::Result<(Chat, u64)>,
    ) {
        // Another session may have been opened meanwhile.
        if self.status.session_id.as_ref() != Some(id) {
            return;
        }
        let watching = self.watch.as_mut().filter(|watch| &watch.session.id == id);
        match result {
            Ok((chat, offset)) => {
                self.chat = chat;
                self.view.open_at_end();
                if let Some(watch) = watching {
                    watch.watched = watch_transcript(self.watcher.as_ref(), &path);
                    watch.path = path;
                    watch.offset = offset;
                    if watch.reading.done() {
                        self.read_on();
                    }
                }
            }
            Err(error) => {
                // Not written yet, perhaps: it is watched for, and read once it is.
                if let Some(watch) = watching {
                    watch.watched = watch_transcript(self.watcher.as_ref(), &path);
                    watch.path = path;
                    watch.reading = Refresh::Idle;
                }
                self.chat
                    .error(format!("Couldn't read the session: {error}"));
            }
        }
        self.show_recall();
        if let Some(prompt) = self.held_prompt.take() {
            self.send(&prompt);
        }
    }

    fn transcript_grew(
        &mut self,
        id: &SessionId,
        result: std::io::Result<(Vec<TranscriptLine>, u64)>,
    ) {
        let Some(watch) = self.watch.as_mut().filter(|watch| &watch.session.id == id) else {
            return;
        };
        let again = watch.reading.done();
        match result {
            Ok((lines, offset)) => {
                watch.offset = offset;
                self.chat.replay(&lines);
            }
            Err(error) => tracing::warn!(%error, "couldn't read on in a watched session"),
        }
        if again {
            self.read_on();
        }
    }

    /// Shows a session another Claude Code process runs, following it as it goes.
    fn watch_live(&mut self, id: &SessionId) {
        let Some(session) = self.live.iter().find(|session| &session.id == id).cloned() else {
            return;
        };
        self.leave();
        self.chat.clear();
        if self.cwd != session.cwd {
            self.cwd.clone_from(&session.cwd);
            self.sessions.clear();
            self.load_sessions();
        }
        // Its title, as the sidebar lists it; its name to message it by until it has one.
        let title = session
            .title
            .clone()
            .unwrap_or_else(|| session.name.clone());
        self.reset_session(Some(session.id.clone()), Some(title));
        self.load_session(session.id.clone(), session.cwd.clone());
        self.forget_watch();
        self.watch = Some(Watch {
            path: sessions::transcript_path(&session.cwd, &session.id),
            session,
            offset: 0,
            reading: Refresh::Reading,
            watched: None,
        });
        self.redraw = Redraw::everything();
    }

    /// Continues the watched session here, as a copy: the original runs on where it runs.
    fn continue_here(&mut self) {
        let Some(watch) = self.watch.take() else {
            return;
        };
        self.status.session_id = None;
        self.status.title = Some(format!("{} (copy)", watch.session.name));
        self.start(Some(Resume::Fork(watch.session.id)));
        self.redraw.sessions = true;
        self.view.focus_composer();
    }

    fn go_to_window(&mut self) {
        let Some(watch) = &self.watch else {
            return;
        };
        let pid = watch.session.pid;
        self.ask_desktop(Act::FocusWindowOf { pid }, Acting::Desktop);
    }

    fn tick(&mut self) {
        self.list_live();
        self.read_on();
    }

    /// Looks every little while whether Claude Code has gone quiet (see `listen`), from a turn's
    /// start on; what it said last counts from now.
    fn start_listening(&mut self) {
        self.heard = web_time::Instant::now();
        if self.listen.running() {
            return;
        }
        let events = self.events.clone();
        self.listen
            .start(slint::TimerMode::Repeated, LISTEN, move || {
                if events.try_send(Event::Listen).is_err() {
                    tracing::debug!("listening after the window closed");
                }
            });
    }

    /// While a turn waits on the API (not on a tool, the user, compacting or a retry), says how
    /// long Claude Code has been quiet once that is a while: a blocked network, say, would
    /// otherwise look like "Starting" for ever. Stops once no turn is under way.
    fn listen(&mut self) {
        if self.status.turns == 0 {
            self.listen.stop();
            return;
        }
        if let Some(quiet) = gone_quiet(
            &self.status.doing,
            !self.asks.is_empty(),
            self.heard.elapsed(),
        ) {
            self.status.doing = Doing::Silent(quiet);
        }
    }

    fn changed(&mut self, watched: Watched) {
        if self.records == Some(watched) {
            self.list_live();
        } else if self
            .watch
            .as_ref()
            .is_some_and(|watch| watch.watched == Some(watched))
        {
            self.read_on();
        } else if self.peeks_at(watched) {
            self.peek_changed();
        } else if let Some(watcher) = &self.watcher {
            // A transcript no longer followed.
            watcher.forget(watched);
        }
    }

    /// Lists the running sessions again, once whatever listing is under way is done.
    fn list_live(&mut self) {
        if self.listing_live.wanted() {
            self.in_background(sessions::live, Event::LiveListed);
        }
    }

    /// Reads what the followed session's transcript says after what is shown.
    fn read_on(&mut self) {
        let Some(watch) = self.watch.as_mut() else {
            return;
        };
        let grown =
            std::fs::metadata(&watch.path).is_ok_and(|metadata| metadata.len() > watch.offset);
        if grown && watch.reading.wanted() {
            let (id, path, from) = (watch.session.id.clone(), watch.path.clone(), watch.offset);
            self.in_background(
                move || sessions::read_from(&path, from),
                move |result| Event::TranscriptGrew { id, result },
            );
        }
    }

    /// Stops watching the followed session's transcript.
    fn forget_watch(&self) {
        if let (Some(watcher), Some(watched)) = (
            &self.watcher,
            self.watch.as_ref().and_then(|watch| watch.watched),
        ) {
            watcher.forget(watched);
        }
    }

    /// The running sessions anew, but our own process: says when one starts waiting for its
    /// user.
    fn live_listed(&mut self, mut live: Vec<LiveSession>) {
        if self.listing_live.done() {
            self.list_live();
        }
        let own = self.process.as_ref().and_then(Claude::pid);
        live.retain(|session| Some(session.pid) != own);
        for session in &live {
            let was = self
                .live
                .iter()
                .find(|known| known.id == session.id)
                .map(|known| known.status);
            let started_waiting = session.status == LiveStatus::Waiting
                && was.is_some_and(|was| was != LiveStatus::Waiting);
            // A session flipping between waiting and working doesn't make a notification each
            // time.
            let now = web_time::Instant::now();
            let recently = self
                .notified
                .get(&session.id)
                .is_some_and(|at| now.duration_since(*at) < NOTIFY_AGAIN);
            if started_waiting && !recently {
                self.notified.insert(session.id.clone(), now);
                let summary = format!("{} needs you", session.name);
                let body = session.cwd.display().to_string();
                self.ask_desktop(Act::Notify { summary, body }, Acting::Notifying);
            }
        }
        if let Some(watch) = self.watch.as_mut()
            && let Some(session) = live.iter().find(|session| session.id == watch.session.id)
        {
            watch.session.clone_from(session);
        }
        if live != self.live {
            self.live = live;
            self.redraw.sessions = true;
        }
    }

    /// Switches to another working directory, with a fresh session in it.
    fn open_project(&mut self, folder: Folder) {
        self.cwd = folder.into_path();
        self.sessions.clear();
        self.load_sessions();
        self.new_session();
    }

    /// Lists the working directory's sessions anew, e.g. once a new one has its title.
    fn load_sessions(&self) {
        let cwd = self.cwd.clone();
        let listed = cwd.clone();
        // Those listed already are summed up again only if their transcript changed.
        let known = self.sessions.clone();
        self.in_background(
            move || sessions::list(&listed, 200, known),
            move |sessions| Event::SessionsListed { cwd, sessions },
        );
    }

    fn sessions_listed(&mut self, cwd: &Path, sessions: Vec<SessionInfo>) {
        if self.cwd != cwd {
            return;
        }
        if let Some(id) = &self.status.session_id
            && let Some(session) = sessions.iter().find(|session| &session.id == id)
            && self.watch.is_none()
        {
            self.status.title = Some(session.title.clone());
        }
        self.sessions = sessions;
        self.redraw.sessions = true;
    }

    fn send(&mut self, text: &str) {
        let text = normalized(text.trim());
        let text = text.as_ref();
        if text.is_empty() && self.attached.is_empty() {
            return;
        }
        // Nothing runs before the folder is trusted: the prompt waits in the composer.
        if !matches!(self.trust_gate, TrustGate::Open) {
            self.wait_for_trust(text);
            return;
        }
        self.put_draft("");
        // In the terminal these start over; the CLI behind a pipe doesn't.
        if matches!(text, "/clear" | "/new") {
            self.new_session();
            return;
        }
        // Claude Code asks for this, but only its terminal has it.
        if text == "/login" {
            self.status.sign_in = SignIn::Asked;
            return;
        }
        // The terminal's `/btw` takes over the input; here it asks beside the conversation.
        if let Some(question) = text.strip_prefix("/btw")
            && (question.is_empty() || question.starts_with(char::is_whitespace))
        {
            self.ask_side(question);
            return;
        }
        if self.watch.is_some() {
            return;
        }
        if let Some(command) = shell_command(text) {
            self.run_shell(text, command.trim());
            return;
        }
        // Sent past the card on how to sign in: the process running can't take up a new sign-in.
        if self.status.sign_in.waits_for_user() && !self.under_way() {
            self.status.sign_in = SignIn::Unknown;
            self.stop();
        }
        // A process starts if none runs; while one can't yet (its folder's trust is still read,
        // or asked about), the prompt waits in the composer.
        if !self.ensure_process() {
            self.wait_for_trust(text);
            return;
        }
        let images = std::mem::take(&mut self.attached);
        self.view.render_attached(&self.attached);
        self.chat.user_images(&images);
        if !text.is_empty() {
            self.chat.user(text);
            self.show_recall();
        }
        self.last_prompt = Some(SentPrompt {
            text: text.to_owned(),
            images: images.clone(),
        });
        // Until Claude Code names the session, its first prompt does.
        if self.status.title.is_none()
            && let Some(title) = provisional_title(text)
        {
            self.status.title = Some(title.to_owned());
        }
        self.status.turns += 1;
        self.status.doing = Doing::Starting;
        if std::mem::take(&mut self.status.tasks_left_running) {
            self.redraw.tasks = true;
        }
        self.start_listening();
        let shell_rows = std::mem::take(&mut self.shell_rows);
        self.write(|process| process.prompt(&shell_rows, &images, text));
        self.view.scroll_to_end();
    }

    /// The terminal's `!`: Claude Code runs the command beside the conversation, and replays
    /// its echo and then its output, which the conversation shows when they come. Images
    /// attached stay for the next prompt.
    fn run_shell(&mut self, written: &str, command: &str) {
        if command.is_empty() {
            return;
        }
        // Claude Code runs one at a time and would refuse this; it waits in the composer.
        if self.chat.shell_running() {
            self.put_draft(written);
            self.chat
                .info("A `!` command still runs: wait for it, or stop it (Stop), then send this.");
            self.view.scroll_to_end();
            return;
        }
        if !self.ensure_process() {
            self.put_draft(written);
            return;
        }
        self.write(|process| process.run_shell(command));
        self.view.scroll_to_end();
    }

    /// Stops Claude's turn, and the `!` command running: an interrupt stops both.
    fn interrupt(&mut self) {
        if self.status.turns > 0 || self.chat.shell_running() {
            self.status.stop_asked = self.status.turns > 0;
            self.request(&ControlRequest::Interrupt, Request::Interrupt);
        }
    }

    fn choose_model(&mut self, index: usize) {
        let Some(value) = self
            .status
            .models
            .get(index)
            .map(|model| model.value.clone())
        else {
            return;
        };
        // Without a process the choice waits for the next: it is started with it.
        if self.process.is_none() {
            self.status.model_choice = Some(value);
            self.redraw.menus = true;
            return;
        }
        self.request(
            &ControlRequest::SetModel {
                model: value.clone(),
            },
            Request::Model(value),
        );
    }

    fn choose_mode(&mut self, index: usize) {
        use strum::IntoEnumIterator;
        if let Some(mode) = PermissionMode::iter().nth(index) {
            self.set_mode(mode);
        }
    }

    fn set_mode(&mut self, mode: PermissionMode) {
        // Without a process the choice waits for the next, which `start` gives it.
        if self.process.is_none() {
            self.status.mode_choice = Some(mode);
            self.status.mode = Some(Mode::Known(mode));
            self.redraw.menus = true;
            return;
        }
        self.request(
            &ControlRequest::SetPermissionMode { mode },
            Request::Mode(mode),
        );
    }

    /// Lists the slash commands the draft could become.
    fn draft_edited(&mut self, draft: &str) {
        // An edit makes the draft the user's own, a recalled prompt changed too.
        self.recall = Recall::Writing;
        self.show_recall();
        let draft = normalized(draft);
        self.view.set_shell_mode(shell_command(&draft).is_some());
        self.view
            .set_argument_hint(argument_hint(&draft, &self.status.commands));
        let matches: Vec<usize> = match draft.strip_prefix('/') {
            Some(typed) if !typed.contains(char::is_whitespace) => self
                .status
                .commands
                .iter()
                .enumerate()
                .filter(|(_, command)| command.name.starts_with(typed) && command.name != typed)
                .take(8)
                .map(|(index, _)| index)
                .collect(),
            _ => Vec::new(),
        };
        if self.command_matches == matches {
            return;
        }
        let shown: Vec<&wire::CommandInfo> = matches
            .iter()
            .filter_map(|index| self.status.commands.get(*index))
            .collect();
        self.view.render_commands(&shown);
        self.command_matches = matches;
    }

    /// The command at `index` in the list matching the draft.
    fn matching_command(&self, index: usize) -> Option<&wire::CommandInfo> {
        self.command_matches
            .get(index)
            .and_then(|at| self.status.commands.get(*at))
    }

    /// Puts the command in the draft: with a space after it, for what follows, if it takes
    /// an argument.
    fn pick_command(&mut self, index: usize) {
        let Some(command) = self.matching_command(index) else {
            return;
        };
        let picked = if command.argument_hint.is_empty() {
            format!("/{}", command.name)
        } else {
            format!("/{} ", command.name)
        };
        self.put_draft(&picked);
        self.view.focus_composer();
    }

    fn run_command(&mut self, index: usize) {
        let Some(command) = self.matching_command(index) else {
            return;
        };
        let sent = format!("/{}", command.name);
        self.send(&sent);
    }

    /// Puts `text` in the composer, and reads it as an edit is read (the command list, the
    /// shell mode).
    fn put_draft(&mut self, text: &str) {
        self.view.set_draft(text);
        self.draft_edited(text);
    }

    /// The session's prompts as the conversation shows them, oldest first: those of a
    /// resumed session's transcript too, whichever window or terminal sent them.
    fn prompts(&self) -> Vec<String> {
        self.chat
            .entries
            .iter()
            .filter_map(|entry| {
                if let Body::User(text) = &entry.body {
                    Some(text.clone())
                } else {
                    None
                }
            })
            .collect()
    }

    /// Tells the composer whether Up has an older prompt to bring, and whether one is shown.
    fn show_recall(&self) {
        let count = self
            .chat
            .entries
            .iter()
            .filter(|entry| matches!(entry.body, Body::User(_)))
            .count();
        let recalling = matches!(self.recall, Recall::Recalled { .. });
        self.view
            .set_recall(self.recall.has_older(count), recalling);
    }

    /// Up: the prompt before the one shown, or the newest, the draft kept for coming back.
    fn recall_older(&mut self) {
        let prompts = self.prompts();
        let mut recall = self.recall.clone();
        let Some(text) = recall
            .older(prompts.len(), &self.view.draft())
            .and_then(|index| prompts.get(index))
        else {
            return;
        };
        // Put in the draft as an edit is, which makes it the user's own: the step is kept after.
        self.put_draft(text);
        self.recall = recall;
        self.show_recall();
        self.view.focus_composer();
    }

    /// Down: the prompt after the one shown, or, past the newest, the draft as it was.
    fn recall_newer(&mut self) {
        let prompts = self.prompts();
        let mut recall = self.recall.clone();
        let text = match recall.newer(prompts.len()) {
            Some(Recalled::Prompt(index)) => prompts.get(index).cloned().unwrap_or_default(),
            Some(Recalled::Draft(draft)) => draft,
            None => return,
        };
        self.put_draft(&text);
        self.recall = recall;
        self.show_recall();
        self.view.focus_composer();
    }

    /// A prompt sent before the process could start: shown in the composer, and sent once the
    /// folder is found trusted in the terminal (see `TrustGate::hold`).
    fn wait_for_trust(&mut self, text: &str) {
        self.put_draft(text);
        self.trust_gate.hold(text);
    }

    /// The `>_` button: `!` put in front of the draft, or taken away.
    fn toggle_shell(&mut self) {
        let draft = self.view.draft();
        let toggled = match shell_command(&normalized(&draft)) {
            Some(command) => command.trim_start().to_owned(),
            None => format!("! {}", draft.trim_start()),
        };
        self.put_draft(&toggled);
        self.view.focus_composer();
    }

    /// The waiting request the user answered, if it is still the one shown: an answer meant for
    /// a request that was cancelled or replaced meanwhile is not given to the next.
    fn take_ask(&mut self, request: &RequestId) -> Option<Ask> {
        if self.asks.front().is_some_and(|ask| &ask.request == request) {
            return self.asks.pop_front();
        }
        tracing::warn!(%request, "an answer for a request no longer shown");
        self.redraw.prompt = true;
        None
    }

    /// Allows the waiting tool call; `always` also takes the first rule the CLI offered.
    fn allow(&mut self, request: &RequestId, always: bool) {
        let Some(ask) = self.take_ask(request) else {
            return;
        };
        let mut updated_permissions = Vec::new();
        if always
            && let AskKind::Tool { suggestions, .. } = &ask.kind
            && let Some(offered) = suggestions.first()
            && let Some(rule) = &offered.rule
        {
            // A mode it switches to is shown in the menu at once.
            if let wire::Suggestion::SetMode { mode } = rule {
                self.status.mode = Some(mode.clone());
            }
            updated_permissions.push(PermissionUpdate::Offered(offered.raw.clone()));
        }
        let response = PermissionResult::Allow {
            updated_input: ToolInput::AsAsked(ask.input.clone()),
            updated_permissions,
        };
        self.answer(&ask, &response, ToolState::Running);
    }

    /// Denies the waiting request. A tool call denied with nothing said stops the turn, as in
    /// the terminal; with what to do instead, Claude carries on with that.
    fn deny(&mut self, request: &RequestId, feedback: &str) {
        let Some(ask) = self.take_ask(request) else {
            return;
        };
        let feedback = feedback.trim();
        let (message, interrupt) = match &ask.kind {
            AskKind::Questions { .. } => ("The user chose not to answer.".to_owned(), false),
            AskKind::Plan { input, parts, .. } => {
                let revise = plan::feedback(&input.plan, parts, feedback);
                if revise.is_empty() {
                    ("The user wants to keep planning.".to_owned(), false)
                } else {
                    (revise, false)
                }
            }
            AskKind::Tool { .. } if feedback.is_empty() => (
                "The user denied this tool use. Stop and wait for their instructions.".to_owned(),
                true,
            ),
            AskKind::Tool { .. } => (feedback.to_owned(), false),
        };
        if interrupt {
            self.status.stop_asked = self.status.turns > 0;
        }
        let response = PermissionResult::Deny { message, interrupt };
        self.answer(&ask, &response, ToolState::Denied);
    }

    /// Approves the plan, with the comments on it as notes in the plan Claude works from.
    fn approve_plan(&mut self, request: &RequestId, mode: PermissionMode, overall: &str) {
        let Some(ask) = self.take_ask(request) else {
            return;
        };
        let updated_input = match &ask.kind {
            AskKind::Plan { input, parts, .. } => ToolInput::Plan(wire::PlanInput {
                plan: plan::annotated(&input.plan, parts, overall),
                rest: input.rest.clone(),
            }),
            AskKind::Tool { .. } | AskKind::Questions { .. } => {
                ToolInput::AsAsked(ask.input.clone())
            }
        };
        self.status.mode = Some(Mode::Known(mode));
        let response = PermissionResult::Allow {
            updated_input,
            updated_permissions: vec![PermissionUpdate::SetMode {
                mode,
                destination: Destination::Session,
            }],
        };
        self.answer(&ask, &response, ToolState::Running);
    }

    fn toggle_option(&mut self, question: usize, option: usize) {
        let Some(Ask {
            kind: AskKind::Questions { asked, picked, .. },
            ..
        }) = self.asks.front_mut()
        else {
            return;
        };
        let multi = asked
            .questions
            .get(question)
            .is_some_and(|q| q.multi_select);
        let Some(picks) = picked.get_mut(question) else {
            return;
        };
        let Some(was) = picks.get(option).copied() else {
            return;
        };
        if !multi {
            picks.iter_mut().for_each(|pick| *pick = false);
        }
        if let Some(pick) = picks.get_mut(option) {
            *pick = !was;
        }
        self.redraw.prompt = true;
    }

    fn set_other(&mut self, question: usize, text: String) {
        if let Some(Ask {
            kind: AskKind::Questions { other, .. },
            ..
        }) = self.asks.front_mut()
            && let Some(slot) = other.get_mut(question)
        {
            *slot = text;
            // Writing something answers the question: its tab is marked.
            self.redraw.prompt = true;
        }
    }

    fn set_notes(&mut self, question: usize, text: String) {
        if let Some(Ask {
            kind: AskKind::Questions { notes, .. },
            ..
        }) = self.asks.front_mut()
            && let Some(slot) = notes.get_mut(question)
        {
            *slot = text;
        }
    }

    /// Sends the answers to Claude's questions: the labels picked, or what was typed; with the
    /// notes written, and the preview of the option picked.
    fn answer_questions(&mut self, request: &RequestId) {
        let Some(ask) = self.take_ask(request) else {
            return;
        };
        let updated_input = match &ask.kind {
            AskKind::Questions {
                asked,
                picked,
                other,
                notes,
            } => ToolInput::Answered(answered(asked, picked, other, notes)),
            AskKind::Tool { .. } | AskKind::Plan { .. } => ToolInput::AsAsked(ask.input.clone()),
        };
        let response = PermissionResult::Allow {
            updated_input,
            updated_permissions: Vec::new(),
        };
        self.answer(&ask, &response, ToolState::Running);
    }

    fn edit_comment(&mut self, section: Option<usize>) {
        if let Some(Ask {
            kind: AskKind::Plan { parts, editing, .. },
            ..
        }) = self.asks.front_mut()
        {
            *editing = section.filter(|&section| section < parts.len());
            self.redraw.prompt = true;
        }
    }

    fn set_comment(&mut self, section: usize, text: &str) {
        if let Some(Ask {
            kind: AskKind::Plan { parts, editing, .. },
            ..
        }) = self.asks.front_mut()
        {
            if let Some(part) = parts.get_mut(section) {
                text.trim().clone_into(&mut part.comment);
            }
            *editing = None;
            self.redraw.prompt = true;
        }
    }

    fn answer(&mut self, ask: &Ask, response: &PermissionResult, state: ToolState) {
        tracing::info!(tool = %ask.tool, ?state, "answered a permission request");
        self.write(|process| process.answer(&ask.request, response));
        if let Some(id) = &ask.tool_use {
            self.chat.set_tool_state(id, state);
        }
        self.status.doing = Doing::Working;
        self.redraw.prompt = true;
        self.redraw.menus = true;
        // The prompt (and the field in it that had the focus) is gone: the writing goes on in
        // the composer, unless another request waits.
        if self.asks.is_empty() {
            self.view.focus_composer();
        }
    }

    /// A permission request from the CLI, waiting for the user.
    fn ask(
        &mut self,
        request: RequestId,
        tool: ToolName,
        input: Value,
        suggestions: Vec<Value>,
        description: String,
        tool_use: Option<ToolUseId>,
    ) {
        tracing::info!(%tool, "claude asks for permission");
        if let Some(id) = &tool_use {
            self.chat.show_asked(id, &tool, &input);
            self.chat.set_tool_state(id, ToolState::Awaiting);
        }
        // Questions and plans have their own prompts; one that doesn't parse is asked about as
        // any tool is.
        let special = if tool == ToolName::AskUserQuestion {
            questions_asked(&input)
        } else if tool == ToolName::ExitPlanMode {
            plan_offered(&input)
        } else {
            None
        };
        // From inside the sandbox the user's own settings can't be written: a rule meant for
        // every project couldn't be kept, so it isn't offered.
        let sandboxed = matches!(self.status.sandboxing, Sandboxing::Landlock(_));
        let suggestions = suggestions
            .into_iter()
            .map(wire::Offered::read)
            .filter(|offered| !(sandboxed && offered.for_every_project()))
            .collect();
        let kind = special.unwrap_or(AskKind::Tool {
            suggestions,
            description,
        });
        self.asks.push_back(Ask {
            request,
            tool_use,
            tool,
            input,
            kind,
        });
        self.redraw.prompt = true;
    }

    /// The process no longer waits for this request (the turn was interrupted).
    fn cancel(&mut self, request: &RequestId) {
        if let Some(at) = self.asks.iter().position(|ask| &ask.request == request)
            && let Some(ask) = self.asks.remove(at)
            && let Some(id) = &ask.tool_use
        {
            self.chat.set_tool_state(id, ToolState::Cancelled);
        }
        self.redraw.prompt = true;
    }

    /// The process ended without being asked to: says why, as far as it said.
    fn ended(&mut self, code: Option<i32>) {
        self.process = None;
        let turns = self.status.turns;
        self.forget_process();
        self.chat.end_turn();
        if turns > 0 || code != Some(0) {
            let how = match code {
                Some(code) => format!(" (exit code {code})"),
                None => " (killed)".to_owned(),
            };
            let mut lines = vec![format!("Claude Code stopped{how}.")];
            lines.extend(self.log.iter().cloned());
            lines.push("Sending a message starts it again.".to_owned());
            self.chat.error(lines.join("\n"));
        }
    }
}

/// A draft as its start is read: the full-width `！` and `／` that input methods type in
/// Japanese or Chinese mode count as `!` and `/`.
fn normalized(draft: &str) -> std::borrow::Cow<'_, str> {
    if let Some(rest) = draft.strip_prefix('！') {
        format!("!{rest}").into()
    } else if let Some(rest) = draft.strip_prefix('／') {
        format!("/{rest}").into()
    } else {
        draft.into()
    }
}

/// The command a (normalized) draft runs with `!`, if it is one: not a Markdown image
/// (`![…](…)`), as a pasted text may start with.
fn shell_command(draft: &str) -> Option<&str> {
    draft
        .strip_prefix('!')
        .filter(|command| !command.starts_with('['))
}

/// How long Claude Code has been `quiet`, in whole seconds, if that is worth saying: while it
/// works on an answer (not on a tool, compacting or a retry, which say what they do), with
/// nothing `asking` the user, for [`QUIET`] or longer.
fn gone_quiet(doing: &Doing, asking: bool, quiet: Duration) -> Option<Duration> {
    let on_an_answer = matches!(
        doing,
        Doing::Starting
            | Doing::Thinking
            | Doing::ThinkingTokens(_)
            | Doing::Writing
            | Doing::Preparing(_)
            | Doing::Working
            | Doing::Silent(_)
    );
    (on_an_answer && !asking && quiet >= QUIET).then(|| Duration::from_secs(quiet.as_secs()))
}

/// What the command in a (normalized) draft takes, to hint after it: only while the draft is
/// the command and a space, before anything follows; empty otherwise.
fn argument_hint<'a>(draft: &str, commands: &'a [wire::CommandInfo]) -> &'a str {
    draft
        .strip_prefix('/')
        .and_then(|typed| typed.strip_suffix(' '))
        .filter(|name| !name.contains(char::is_whitespace))
        .and_then(|name| commands.iter().find(|command| command.name == name))
        .map_or("", |command| command.argument_hint.as_str())
}

/// What names a session until Claude Code does: the prompt's first line with text. A slash
/// command names nothing.
fn provisional_title(prompt: &str) -> Option<&str> {
    prompt
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .filter(|line| !normalized(line).starts_with('/'))
}

/// Claude's questions with the user's answers (the labels picked, or what was typed), the notes
/// written, and the preview of the option picked, by question.
fn answered(
    asked: &wire::Questions,
    picked: &[Vec<bool>],
    other: &[String],
    notes: &[String],
) -> wire::Questions {
    let mut answered = asked.clone();
    for (index, question) in asked.questions.iter().enumerate() {
        let typed = other.get(index).map_or("", |text| text.trim());
        let chosen: Vec<&wire::QuestionOption> = question
            .options
            .iter()
            .zip(picked.get(index).into_iter().flatten())
            .filter(|(_, picked)| **picked)
            .map(|(option, _)| option)
            .collect();
        let answer = if typed.is_empty() {
            chosen
                .iter()
                .map(|option| option.label.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        } else {
            typed.to_owned()
        };
        if !answer.is_empty() {
            answered.answers.insert(question.question.clone(), answer);
        }
        let written = notes.get(index).map_or("", |text| text.trim());
        let annotation = wire::Annotation {
            notes: (!written.is_empty()).then(|| written.to_owned()),
            // A preview goes with a single option picked, not with typed text.
            preview: match (typed.is_empty(), chosen.as_slice()) {
                (true, [option]) if !option.preview.is_empty() => Some(option.preview.clone()),
                _ => None,
            },
        };
        if annotation != wire::Annotation::default() {
            answered
                .annotations
                .insert(question.question.clone(), annotation);
        }
    }
    answered
}

fn questions_asked(input: &Value) -> Option<AskKind> {
    match wire::Questions::deserialize(input) {
        Ok(asked) => Some(AskKind::Questions {
            picked: asked
                .questions
                .iter()
                .map(|question| vec![false; question.options.len()])
                .collect(),
            other: vec![String::new(); asked.questions.len()],
            notes: vec![String::new(); asked.questions.len()],
            asked,
        }),
        Err(error) => {
            tracing::warn!(%error, "questions that don't parse");
            None
        }
    }
}

fn plan_offered(input: &Value) -> Option<AskKind> {
    match wire::PlanInput::deserialize(input) {
        Ok(input) => Some(AskKind::Plan {
            parts: plan::parts(&input.plan),
            input,
            editing: None,
        }),
        Err(error) => {
            tracing::warn!(%error, "a plan that doesn't parse");
            None
        }
    }
}

/// Watches a followed session's transcript, or the folder it will be written in while it isn't
/// there yet.
fn watch_transcript(watcher: Option<&Watcher>, path: &Path) -> Option<Watched> {
    let watcher = watcher?;
    watcher.file(path).ok().or_else(|| {
        watcher
            .folder(path.parent()?)
            .inspect_err(|error| tracing::debug!(%error, "a followed session goes unwatched"))
            .ok()
    })
}

/// What the desktop has and says: asked before the window is confined, of programs it may not
/// run then.
#[derive(Debug)]
pub(crate) struct Desktop {
    openers: Vec<desktop::Opener>,
    fonts: crate::appearance::Fonts,
    /// In logical pixels.
    text_size: f32,
}

impl Desktop {
    /// Asks several programs at once, before the first frame.
    pub(crate) fn find() -> Self {
        std::thread::scope(|scope| {
            let openers = scope.spawn(desktop::openers);
            let fonts = scope.spawn(crate::appearance::fonts);
            let text_size = crate::appearance::desktop_text_size();
            Self {
                openers: joined(openers),
                fonts: joined(fonts),
                text_size,
            }
        })
    }

    /// The browser demo's: nothing to open a folder in, the theme's own fonts (built into the
    /// page), and text of the size most desktops have.
    #[cfg(target_arch = "wasm32")]
    pub(crate) fn in_the_browser() -> Self {
        Self {
            openers: Vec::new(),
            fonts: crate::appearance::Fonts {
                sans: None,
                mono: None,
            },
            text_size: crate::appearance::DEFAULT_TEXT_SIZE,
        }
    }
}

/// What a scoped thread made; its panic, if it panicked, goes on here.
fn joined<T>(handle: std::thread::ScopedJoinHandle<'_, T>) -> T {
    handle
        .join()
        .unwrap_or_else(|panic| std::panic::resume_unwind(panic))
}

/// Why a control request of ours brought no answer to take.
#[derive(Debug, thiserror::Error)]
pub(crate) enum AnswerError {
    #[error("{0}")]
    Refused(String),
    #[error(
        "Claude Code answered in a form this edualc doesn't know, so its version and edualc's may \
         not fit together ({0})"
    )]
    Unreadable(#[source] serde_json::Error),
}

/// The answer to a control request of ours, as `T`.
pub(crate) fn answer_as<T: serde::de::DeserializeOwned>(
    outcome: Result<Value, String>,
) -> Result<T, AnswerError> {
    let answer = outcome.map_err(AnswerError::Refused)?;
    T::deserialize(answer).map_err(AnswerError::Unreadable)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Up steps back from the newest prompt to the oldest and stops there; Down steps forward
    /// and, past the newest, gives back what was being written. With no prompts, Up does
    /// nothing; while writing, Down does nothing.
    #[test]
    fn up_and_down_step_through_the_prompts() {
        let mut recall = Recall::Writing;
        assert!(!recall.has_older(0));
        assert_eq!(recall.older(0, "half a thought"), None);
        assert_eq!(recall, Recall::Writing);
        assert_eq!(recall.newer(3), None);

        assert!(recall.has_older(3));
        assert_eq!(recall.older(3, "half a thought"), Some(2));
        assert_eq!(recall.older(3, "the newest, shown"), Some(1));
        assert_eq!(recall.older(3, "the middle, shown"), Some(0));
        assert!(!recall.has_older(3));
        assert_eq!(recall.older(3, "the oldest, shown"), None);
        assert_eq!(
            recall,
            Recall::Recalled {
                index: 0,
                draft: "half a thought".to_owned()
            },
            "Up at the oldest stays there, the draft kept"
        );

        assert_eq!(recall.newer(3), Some(Recalled::Prompt(1)));
        assert_eq!(recall.newer(3), Some(Recalled::Prompt(2)));
        assert_eq!(
            recall.newer(3),
            Some(Recalled::Draft("half a thought".to_owned()))
        );
        assert_eq!(recall, Recall::Writing);
    }

    /// Being signed out brings up how to sign in at once, keeping the prompt to send again. A
    /// sandboxed session's token that ran out gets one start again by itself; refused after
    /// that too, it needs the user.
    #[test]
    fn a_refused_sign_in_asks_the_user_unless_a_new_start_may_mend_it() {
        let prompt = || SentPrompt {
            text: "edit the notes".to_owned(),
            images: Vec::new(),
        };
        let refused = SignIn::Refused {
            failed: Some(prompt()),
        };

        assert_eq!(
            SignIn::Unknown.after_refusal(false, Some(prompt())),
            refused
        );
        assert!(refused.waits_for_user());

        let renewing = SignIn::SignedIn.after_refusal(true, Some(prompt()));
        assert_eq!(renewing, SignIn::Renewing);
        assert!(!renewing.waits_for_user());
        assert_eq!(renewing.after_refusal(true, Some(prompt())), refused);

        assert!(SignIn::Asked.waits_for_user());
        assert!(!SignIn::SignedIn.waits_for_user());
        assert!(!SignIn::Unknown.waits_for_user());
    }

    /// A prompt sent while the folder's trust is read (the command line's first prompt is
    /// always sent then) goes once the terminal is found to trust it; once the user is asked,
    /// it waits for them; the answer to an older check changes nothing.
    #[test]
    fn a_prompt_sent_while_trust_is_read_goes_once_trusted() {
        let first = ActId::default();
        let checking = || TrustGate::Checking {
            request: first,
            resume: None,
            held: None,
        };

        let mut gate = checking();
        gate.hold("edit the notes");
        assert_eq!(
            gate.answered(first, true, Vec::new()),
            TrustAnswer::Trusted {
                resume: None,
                held: Some("edit the notes".to_owned())
            }
        );
        assert!(matches!(gate, TrustGate::Open));

        let mut gate = checking();
        assert_eq!(
            gate.answered(first, true, Vec::new()),
            TrustAnswer::Trusted {
                resume: None,
                held: None
            }
        );

        let mut gate = checking();
        gate.hold("edit the notes");
        assert_eq!(gate.answered(first, false, Vec::new()), TrustAnswer::Asked);
        gate.hold("sent again before deciding");
        assert!(matches!(gate, TrustGate::Asking { .. }));

        let mut gate = checking();
        gate.hold("edit the notes");
        assert_eq!(
            gate.answered(first.next(), true, Vec::new()),
            TrustAnswer::Stale
        );
        assert!(
            matches!(&gate, TrustGate::Checking { held: Some(held), .. } if held == "edit the notes")
        );
    }

    /// A shell command starts with `!`, or the `！` a Japanese or Chinese input method types; a
    /// Markdown image doesn't run. `／` opens commands as `/` does.
    #[test]
    fn a_draft_is_read_by_how_it_starts() {
        let shell = |draft: &str| shell_command(&normalized(draft)).map(str::to_owned);
        assert_eq!(shell("!ls -a"), Some("ls -a".to_owned()));
        assert_eq!(shell("！ls"), Some("ls".to_owned()));
        assert_eq!(shell("![screenshot](shot.png) why is this blank?"), None);
        assert_eq!(shell("hi! there"), None);
        assert_eq!(normalized("／clear"), "/clear");
        assert_eq!(normalized("今日は！"), "今日は！");
        assert!(matches!(
            normalized("plain"),
            std::borrow::Cow::Borrowed("plain")
        ));
    }

    /// Silence is told only while Claude Code works on an answer and nothing waits for the user,
    /// after half a minute, in whole seconds; a tool, compacting or a retry say what they do.
    #[test]
    fn silence_is_told_while_an_answer_is_awaited() {
        let half_a_minute_on = Duration::from_millis(31_400);
        assert_eq!(
            gone_quiet(&Doing::Starting, false, half_a_minute_on),
            Some(Duration::from_secs(31))
        );
        assert_eq!(
            gone_quiet(
                &Doing::Silent(Duration::from_secs(31)),
                false,
                Duration::from_secs(45)
            ),
            Some(Duration::from_secs(45))
        );
        assert_eq!(
            gone_quiet(&Doing::Starting, false, Duration::from_secs(29)),
            None
        );
        assert_eq!(gone_quiet(&Doing::Starting, true, half_a_minute_on), None);
        assert_eq!(
            gone_quiet(&Doing::Compacting, false, half_a_minute_on),
            None
        );
        assert_eq!(gone_quiet(&Doing::Nothing, false, half_a_minute_on), None);
        let retry = Retry {
            attempt: 2,
            most: 10,
            status: None,
            cause: wire::RetryCause::Unknown,
        };
        assert_eq!(
            gone_quiet(&Doing::Retrying(retry), false, half_a_minute_on),
            None
        );
    }

    /// A retry says which it is, of how many, and why, in words; a failed connection has no
    /// status to give.
    #[test]
    fn a_retry_says_why() {
        let retry = |status, cause| Retry {
            attempt: 3,
            most: 10,
            status,
            cause,
        };
        assert_eq!(
            retry(None, wire::RetryCause::Unknown).to_string(),
            "Retrying 3/10 · no connection to the API"
        );
        assert_eq!(
            retry(Some(529), wire::RetryCause::Overloaded).to_string(),
            "Retrying 3/10 · the API is overloaded"
        );
        assert_eq!(
            retry(Some(502), wire::RetryCause::ServerError).to_string(),
            "Retrying 3/10 · the API answered 502"
        );
    }

    /// A command's hint shows while the draft is the command and a space, and goes once anything
    /// follows; a command that takes nothing, or none known, hints nothing.
    #[test]
    fn a_command_hints_what_it_takes_until_something_follows() {
        let commands: Vec<wire::CommandInfo> = serde_json::from_value(serde_json::json!([
            { "name": "compact", "argumentHint": "[instructions]" },
            { "name": "cost" },
        ]))
        .expect("commands");
        assert_eq!(argument_hint("/compact ", &commands), "[instructions]");
        assert_eq!(argument_hint("/compact", &commands), "");
        assert_eq!(argument_hint("/compact k", &commands), "");
        assert_eq!(argument_hint("/compact  ", &commands), "");
        assert_eq!(argument_hint("/cost ", &commands), "");
        assert_eq!(argument_hint("/unknown ", &commands), "");
        assert_eq!(argument_hint("compact ", &commands), "");
    }

    /// A session is named by its prompt's first line with text, trimmed; a slash command, in
    /// either width, names nothing.
    #[test]
    fn the_first_prompt_names_a_session() {
        assert_eq!(
            provisional_title("\n  Fix the flaky test \nin CI"),
            Some("Fix the flaky test")
        );
        assert_eq!(
            provisional_title("このテストを直して"),
            Some("このテストを直して")
        );
        assert_eq!(provisional_title("/compact keep the plan"), None);
        assert_eq!(provisional_title("／clear"), None);
        assert_eq!(provisional_title(" \n "), None);
    }

    /// Notes go with any answer; a preview only with a single option picked, the way
    /// AskUserQuestion takes them; nothing is added where nothing was written or previewed.
    #[test]
    fn answers_carry_notes_and_the_picked_preview() {
        let asked: wire::Questions = serde_json::from_value(serde_json::json!({
            "questions": [
                { "question": "Layout?", "options": [
                    { "label": "Grid", "preview": "▦" }, { "label": "List", "preview": "☰" } ] },
                { "question": "Name?", "options": [ { "label": "A", "preview": "a" } ] },
                { "question": "Which?", "multiSelect": true, "options": [
                    { "label": "X", "preview": "x" }, { "label": "Y", "preview": "y" } ] },
                { "question": "Plain?", "options": [ { "label": "Yes" } ] },
            ]
        }))
        .expect("questions");
        let picked = [vec![false, true], vec![true], vec![true, true], vec![true]];
        let other = [
            String::new(),
            "B instead".into(),
            String::new(),
            String::new(),
        ];
        let notes = [
            "  denser  ".into(),
            String::new(),
            "both".into(),
            String::new(),
        ];
        let given = answered(&asked, &picked, &other, &notes);
        assert_eq!(
            given.answers.get("Layout?").map(String::as_str),
            Some("List")
        );
        assert_eq!(
            given.answers.get("Name?").map(String::as_str),
            Some("B instead")
        );
        assert_eq!(
            given.answers.get("Which?").map(String::as_str),
            Some("X, Y")
        );
        assert_eq!(
            given.annotations.get("Layout?"),
            Some(&wire::Annotation {
                notes: Some("denser".into()),
                preview: Some("☰".into()),
            })
        );
        assert_eq!(given.annotations.get("Name?"), None, "typed: no preview");
        assert_eq!(
            given.annotations.get("Which?"),
            Some(&wire::Annotation {
                notes: Some("both".into()),
                preview: None,
            }),
            "two picked: no preview"
        );
        assert_eq!(given.annotations.get("Plain?"), None);
    }

    /// A change while a read is under way makes one more read once it is done, and the next
    /// change after that is read too: nothing stays stuck waiting for a read that never comes.
    #[test]
    fn changes_during_a_read_are_read_after_it() {
        let mut refresh = Refresh::default();
        assert!(refresh.wanted(), "the first change starts a read");
        assert!(!refresh.wanted(), "one while reading waits");
        assert!(!refresh.wanted());
        assert!(refresh.done(), "the read came back stale: ask again");
        assert!(refresh.wanted(), "and asking again starts a read");
        assert!(!refresh.done(), "this one wasn't stale");
        assert!(refresh.wanted(), "a later change starts a read again");
    }
}
