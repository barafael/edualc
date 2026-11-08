//! The app's state made into what the window shows (see the parent module): the rows of the
//! conversation, the menus, the words of the status bar and of each panel, as plain data, handed
//! to the frontend once per batch of events, each part only when it changed.

use std::collections::{BTreeSet, HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};

use strum::IntoEnumIterator;

use super::model::{
    self, Activity, BlockKind, Cjk, CjkChars, CommandItem, DiffKind, DiffLine, Entry, EntryKind,
    GroupRole, HanLanguage, ImageChip, ImageState, LiveState, MdBlock, MenuItem, PeekStep,
    PermissionKind, PlanSection, ProjectItem, PromptKind, Question, QuestionOption, SendKey,
    SessionRow, SessionRowKind, ShownImage, SideExchange, SideState, SignInState, StatRow,
    StatSection, StepKind, TableRow, TaskItem, TaskKind, ToolState,
};
use super::{Command, EntriesChange, Frontend, Image, Look, Styled};
use crate::app::{
    Answer, Ask, AskKind, CommandOutput, Conversation, Doing, Followed, LimitUse, Mode, Peek,
    PeekView, PermissionMode, Place, Running, Sandboxing, SideChat, SideView, SignIn, Status,
    Tasks, UsagePanel, Watch,
};
use crate::chat::{self, Body, Change, Chat, ImageKey};
use crate::cjk::{CjkLanguage, CjkText, Standing};
use crate::decode::{Picture, Pixels};
use crate::desktop::Opener;
use crate::external::wire::{self, ContextKind, LimitKind, RequestId, ToolKind, ToolName};
use crate::grouping::{self, Grouping, RunState};
use crate::ipc::Runs;
use crate::markdown::{self, Block};
use crate::sandbox::Enforcement;
use crate::sessions::{self, LiveStatus, ProjectInfo, Worktree};
use crate::settings;
use crate::sidebar;
use crate::tools::{self, shortened_path};

/// Long outputs are cut to this many lines in the window; copying takes all of it.
const OUTPUT_LINES: usize = 80;
const OUTPUT_CHARS: usize = 20_000;

/// The state of each of the window's parts, as shown last or to be shown.
#[derive(Debug, Clone, Default, PartialEq)]
struct Parts {
    session: model::Session,
    composer: model::Composer,
    sessions: model::Sessions,
    prompt: model::Prompt,
    peek: model::Peek,
    side: model::Side,
    preferences: model::Preferences,
    usage: model::Usage,
}

pub(crate) struct Presenter {
    frontend: Box<dyn Frontend>,
    session: model::Session,
    composer: model::Composer,
    sessions: model::Sessions,
    prompt: model::Prompt,
    peek: model::Peek,
    side: model::Side,
    preferences: model::Preferences,
    usage: model::Usage,
    /// What the frontend was told last, for telling it only what changed.
    shown: Option<Parts>,
    /// The rows of the transcript as the frontend has them.
    entries: Vec<Entry>,
    /// How each entry was shown last with respect to runs of tool calls.
    groups: Vec<Grouping>,
    /// The request the bar shows.
    prompt_shown: Option<RequestId>,
    /// What the menus hold, set again only when it changes (setting closes an open menu).
    models: Vec<(String, String)>,
    modes: Vec<(String, String)>,
    /// Which model and mode the menus have chosen, told with the next flush.
    menu_choice: Option<(i32, i32)>,
    /// Whether the plan is reviewed, told with the next flush (after the plan itself).
    reviewing: Option<bool>,
    /// The conversation's images, as far as they are decoded; one not here is still decoded.
    images: HashMap<ImageKey, ShownImage>,
    /// The language text with Han characters alone is set in (see `cjk`).
    han: CjkLanguage,
}

impl Presenter {
    pub(crate) fn new(
        frontend: Box<dyn Frontend>,
        openers: &[Opener],
        finds_windows: bool,
    ) -> Self {
        let openers: Vec<String> = std::iter::once("This window".to_owned())
            .chain(openers.iter().map(|opener| opener.label()))
            .collect();
        let session = model::Session {
            can_go_to_window: finds_windows,
            openers,
            limit_five_hour: -1.0,
            limit_seven_day: -1.0,
            title: "New session".to_owned(),
            ..model::Session::default()
        };
        Self {
            frontend,
            session,
            composer: model::Composer::default(),
            sessions: model::Sessions::default(),
            prompt: model::Prompt::default(),
            peek: model::Peek::default(),
            side: model::Side::default(),
            preferences: model::Preferences::default(),
            usage: model::Usage::default(),
            shown: None,
            entries: Vec::new(),
            groups: Vec::new(),
            prompt_shown: None,
            models: Vec::new(),
            modes: Vec::new(),
            menu_choice: None,
            reviewing: None,
            images: HashMap::new(),
            han: CjkLanguage::Japanese,
        }
    }

    /// Tells the frontend what changed since it was told last.
    pub(crate) fn flush(&mut self) {
        let parts = Parts {
            session: self.session.clone(),
            composer: self.composer.clone(),
            sessions: self.sessions.clone(),
            prompt: self.prompt.clone(),
            peek: self.peek.clone(),
            side: self.side.clone(),
            preferences: self.preferences.clone(),
            usage: self.usage.clone(),
        };
        let shown = self.shown.as_ref();
        let frontend = &mut self.frontend;
        if shown.is_none_or(|shown| shown.session != parts.session) {
            frontend.session(&parts.session);
        }
        if shown.is_none_or(|shown| shown.composer != parts.composer) {
            frontend.composer(&parts.composer);
        }
        if shown.is_none_or(|shown| shown.sessions != parts.sessions) {
            frontend.sessions(&parts.sessions);
        }
        if shown.is_none_or(|shown| shown.prompt != parts.prompt) {
            frontend.prompt(&parts.prompt);
        }
        if shown.is_none_or(|shown| shown.peek != parts.peek) {
            frontend.peek(&parts.peek);
        }
        if shown.is_none_or(|shown| shown.side != parts.side) {
            frontend.side(&parts.side);
        }
        if shown.is_none_or(|shown| shown.preferences != parts.preferences) {
            frontend.preferences(&parts.preferences);
        }
        if shown.is_none_or(|shown| shown.usage != parts.usage) {
            frontend.usage(&parts.usage);
        }
        self.shown = Some(parts);
        if let Some((model, mode)) = self.menu_choice.take() {
            self.frontend.command(Command::ModelIndex(model));
            self.frontend.command(Command::ModeIndex(mode));
        }
        if let Some(reviewing) = self.reviewing.take() {
            self.frontend.command(Command::Reviewing(reviewing));
        }
    }

    pub(crate) fn command(&mut self, command: Command) {
        self.frontend.command(command);
    }

    /// The language text with Han characters alone is set in, from the next row drawn on, and
    /// the setting's choice on the settings page.
    pub(crate) fn set_han_language(&mut self, han: CjkLanguage) {
        self.han = han;
        self.preferences.han_language = match han {
            CjkLanguage::Japanese => HanLanguage::Japanese,
            CjkLanguage::SimplifiedChinese => HanLanguage::SimplifiedChinese,
            CjkLanguage::TraditionalChinese => HanLanguage::TraditionalChinese,
            CjkLanguage::Korean => HanLanguage::Korean,
        };
    }

    /// Image `key`'s pixels, or why it can't be shown.
    pub(crate) fn show_image(&mut self, key: ImageKey, shown: Result<&Pixels, String>) {
        let row = match &shown {
            Ok(pixels) if pixels.is_whole() => ShownImage {
                state: ImageState::Shown,
                image: Image::Key(key),
                width_px: i32::try_from(pixels.width_px).unwrap_or(i32::MAX),
                height_px: i32::try_from(pixels.height_px).unwrap_or(i32::MAX),
                note: String::new(),
            },
            Ok(_) => failed_image("its pixels don't add up".to_owned()),
            Err(why) => failed_image(why.clone()),
        };
        self.images.insert(key, row);
        self.frontend.show_image(key, shown);
    }

    /// Keeps only the images `shown` holds: the conversation was replaced.
    pub(crate) fn keep_images(&mut self, shown: &HashSet<ImageKey>) {
        self.images.retain(|key, _| shown.contains(key));
        self.frontend.keep_images(shown);
    }

    fn shown_images(&self, keys: &[ImageKey]) -> Vec<ShownImage> {
        keys.iter()
            .map(|key| self.images.get(key).cloned().unwrap_or_default())
            .collect()
    }

    /// The base text size, in logical pixels; everything else is sized from it.
    pub(crate) fn set_text_size(&mut self, pixels: f32) {
        self.command(Command::TextSize(pixels));
    }

    pub(crate) fn set_send_key(&mut self, key: settings::SendKey) {
        self.preferences.send_key = match key {
            settings::SendKey::Enter => SendKey::Enter,
            settings::SendKey::CtrlEnter => SendKey::CtrlEnter,
        };
    }

    /// The folders the sandbox lets Claude read and change, beyond its own, as the settings page
    /// lists them.
    pub(crate) fn set_sandbox_folders(&mut self, settings: &settings::Settings) {
        let shown = |folders: &[PathBuf]| -> Vec<String> {
            folders
                .iter()
                .map(|folder| folder.to_string_lossy().into_owned())
                .collect()
        };
        self.preferences.sandbox_read = shown(&settings.sandbox_read);
        self.preferences.sandbox_write = shown(&settings.sandbox_write);
    }

    /// Whether a sandboxed session's messages to other sessions go without asking.
    pub(crate) fn set_messages_unasked(&mut self, unasked: bool) {
        self.preferences.messages_unasked = unasked;
    }

    pub(crate) fn set_look(&mut self, look: settings::Look) {
        self.command(Command::Look(match look {
            settings::Look::Gruvbox => Look::Gruvbox,
            settings::Look::Plain => Look::Plain,
        }));
    }

    /// The images going with the next message, as chips in the composer: their thumbnails are
    /// the helper's pixels, decoded nowhere here.
    pub(crate) fn render_attached(&mut self, attached: &[Picture]) {
        self.composer.images = attached
            .iter()
            .filter(|picture| picture.thumbnail.is_whole())
            .map(|picture| ImageChip {
                thumbnail: Image::Pixels(picture.thumbnail.clone()),
                caption: format!("{}×{}", picture.width_px, picture.height_px),
            })
            .collect();
    }

    /// The composer's kept height, if it was dragged to one; fitted to its text if not.
    pub(crate) fn set_composer_height(&mut self, height_px: Option<u16>) {
        self.command(Command::ComposerHeight(height_px));
    }

    pub(crate) fn copy(&mut self, text: &str) {
        self.command(Command::Copy(text.to_owned()));
    }

    pub(crate) fn set_draft(&mut self, text: &str) {
        self.command(Command::SetDraft(text.to_owned()));
    }

    pub(crate) fn draft(&self) -> String {
        self.frontend.draft()
    }

    /// Whether the composer shows the draft as a shell command.
    pub(crate) fn set_shell_mode(&mut self, shell: bool) {
        self.composer.shell = shell;
    }

    /// What the command in the draft takes, hinted after it; empty for none.
    pub(crate) fn set_argument_hint(&mut self, hint: &str) {
        hint.clone_into(&mut self.composer.argument_hint);
    }

    /// Adds `text` to the end of the draft, after what is there.
    pub(crate) fn append_draft(&mut self, text: &str) {
        self.command(Command::AppendDraft(text.to_owned()));
    }

    pub(crate) fn focus_side(&mut self) {
        self.command(Command::FocusSide);
    }

    pub(crate) fn render_side(&mut self, chat: &SideChat, can_ask: bool, can_branch: bool) {
        let han = self.han;
        let side = &mut self.side;
        side.view = match chat.view {
            SideView::Hidden => model::SideView::Hidden,
            SideView::Shown => model::SideView::Shown,
            SideView::Folded { .. } => model::SideView::Folded,
        };
        side.exchanges = chat
            .exchanges
            .iter()
            .map(|exchange| {
                let (state, answer, error) = match &exchange.answer {
                    Answer::Pending => (SideState::Pending, Vec::new(), ""),
                    Answer::Retrying => (SideState::Retrying, Vec::new(), ""),
                    Answer::Answered(text) => {
                        let blocks: Vec<MdBlock> = markdown::blocks(text)
                            .iter()
                            .map(|block| md_block(block, han))
                            .collect();
                        (SideState::Answered, blocks, "")
                    }
                    Answer::Failed(error) => (SideState::Failed, Vec::new(), error.as_str()),
                };
                SideExchange {
                    question: exchange.question.clone(),
                    state,
                    answer,
                    error: error.to_owned(),
                }
            })
            .collect();
        side.unseen = chat.view == SideView::Folded { unseen: true };
        side.can_ask = can_ask;
        side.can_branch = can_branch;
        side.has_answers = chat.has_answers();
        side.pending = chat.pending();
    }

    pub(crate) fn focus_composer(&mut self) {
        self.command(Command::FocusComposer);
    }

    pub(crate) fn scroll_to_end(&mut self) {
        self.command(Command::ScrollToEnd);
    }

    /// A session's transcript was loaded: shown at its end once the list has measured its way
    /// there, not on the way.
    pub(crate) fn open_at_end(&mut self) {
        self.command(Command::OpenAtEnd);
    }

    /// Brings the transcript in line with `chat`: the rows `changes` name, and those whose place
    /// in a run of tool calls changed (`open_runs` holds the runs opened, by their first call).
    /// Shows what changed in the conversation, building each changed row once. Runs are
    /// worked out again only when they can have changed: text growing never changes them.
    pub(crate) fn render_transcript(
        &mut self,
        chat: &Chat,
        changes: Vec<Change>,
        cwd: &Path,
        open_runs: &HashSet<usize>,
        runs_toggled: bool,
    ) {
        if changes.is_empty() && !runs_toggled {
            return;
        }
        let mut regrouped = BTreeSet::new();
        let structural = changes
            .iter()
            .any(|change| !matches!(change, Change::Grew(_)));
        if structural || runs_toggled {
            let groups = grouping::group(&chat.entries, open_runs, cwd);
            regrouped = (0..groups.len())
                .filter(|&index| self.groups.get(index) != groups.get(index))
                .collect();
            self.groups = groups;
        }
        let mut rebuilt = BTreeSet::new();
        let mut reset = false;
        for change in changes {
            match change {
                Change::Reset => reset = true,
                Change::Pushed(index) | Change::Updated(index) | Change::Grew(index) => {
                    rebuilt.insert(index);
                }
            }
        }
        if reset {
            self.entries = chat
                .entries
                .iter()
                .enumerate()
                .map(|(index, entry)| self.row(index, entry, cwd))
                .collect();
            self.frontend
                .entries(EntriesChange::Reset(self.entries.clone()));
            return;
        }
        // New entries, in order; then the changed ones and those their run changed for.
        while let Some(entry) = chat.entries.get(self.entries.len()) {
            let at = self.entries.len();
            let row = self.row(at, entry, cwd);
            self.entries.push(row.clone());
            self.frontend.entries(EntriesChange::Push(row));
            rebuilt.remove(&at);
            regrouped.remove(&at);
        }
        for index in rebuilt.into_iter().chain(regrouped) {
            let Some(entry) = chat.entries.get(index) else {
                continue;
            };
            let row = self.row(index, entry, cwd);
            if let Some(shown) = self.entries.get_mut(index)
                && *shown != row
            {
                shown.clone_from(&row);
                self.frontend.entries(EntriesChange::Set(index, row));
            }
        }
    }

    fn row(&self, index: usize, entry: &chat::Entry, cwd: &Path) -> Entry {
        let han = self.han;
        let mut row = Entry {
            index: i32::try_from(index).unwrap_or(i32::MAX),
            nested: entry.nested,
            ..Entry::default()
        };
        match self.groups.get(index) {
            Some(Grouping::Head(line)) => {
                row.group = GroupRole::Head;
                row.group_summary.clone_from(&line.summary);
                row.group_state = match line.state {
                    RunState::Working => ToolState::Running,
                    RunState::Done => ToolState::Done,
                    RunState::Failed => ToolState::Failed,
                };
                row.group_open = line.open;
            }
            Some(Grouping::Member { open }) => {
                row.group = GroupRole::Member;
                row.group_open = *open;
                row.hidden = !*open;
            }
            Some(Grouping::Alone) | None => {}
        }
        match &entry.body {
            Body::User(text) => {
                row.kind = EntryKind::User;
                row.body = Styled::plain(text);
                row.cjk = cjk(text, han);
            }
            Body::Assistant(text) => {
                row.kind = EntryKind::Assistant;
                row.cjk = cjk(text, han);
                row.blocks = blocks(text, han);
            }
            Body::Thinking { text, expanded } => {
                row.kind = EntryKind::Thinking;
                if *expanded {
                    row.body = Styled::plain(text);
                }
                row.expanded = *expanded;
                row.cjk = cjk(text, han);
            }
            // Sessions write Markdown to each other, as they do to the user.
            Body::Peer { from, text } => {
                row.kind = EntryKind::Peer;
                row.body = Styled::markdown(text);
                row.sender = from.as_deref().unwrap_or("another session").to_owned();
                row.cjk = cjk(text, han);
            }
            // Notes come from slash commands, which use Markdown (`sonnet`).
            Body::Info(text) => {
                row.kind = EntryKind::Info;
                row.body = Styled::markdown(text);
                row.cjk = cjk(text, han);
            }
            Body::Error(text) => {
                row.kind = EntryKind::Error;
                row.body = Styled::plain(text);
                row.cjk = cjk(text, han);
            }
            Body::Tool { tool, expanded } => {
                let depth = card_depth(*expanded, row.hidden);
                let view = tools::describe(&tool.name, &tool.input, cwd, depth);
                let state = tool_state(tool.state);
                card(&mut row, view, state, &tool.output, *expanded, han);
                if depth == tools::Depth::Full {
                    row.images = self.shown_images(&tool.images);
                }
            }
            Body::Image(key) => {
                row.kind = EntryKind::Image;
                row.images = self.shown_images(std::slice::from_ref(key));
            }
            // A card as Claude's own commands get, titled as the user's.
            Body::Shell { shell, expanded } => {
                let depth = card_depth(*expanded, row.hidden);
                let input = serde_json::json!({ "command": shell.command });
                let mut view = tools::describe(&ToolName::Bash, &input, cwd, depth);
                view.title = "You ran".to_owned();
                let state = shell_state(shell.state);
                card(&mut row, view, state, &shell.output, *expanded, han);
            }
        }
        row
    }

    /// What the session is doing: whether something runs that Stop stops (Claude's turn, or a
    /// `!` command, which an interrupt stops too), what the progress line shows of it, and the
    /// status bar's word for it.
    pub(crate) fn render_activity(&mut self, status: &Status, waiting: bool, shell_running: bool) {
        let session = &mut self.session;
        session.busy = status.turns > 0 || shell_running;
        session.turn = status.turns > 0;
        session.activity = match status.doing {
            Doing::Nothing if shell_running => Activity::Tool,
            Doing::Nothing => Activity::Idle,
            Doing::Starting
            | Doing::Thinking
            | Doing::ThinkingTokens(_)
            | Doing::Writing
            | Doing::Working
            | Doing::Compacting
            | Doing::Retrying(_)
            | Doing::Silent(_) => Activity::Model,
            Doing::Preparing(_) | Doing::Running(_) => Activity::Tool,
        };
        let doing = if waiting {
            "Waiting for you".to_owned()
        } else if matches!(status.doing, Doing::Nothing) && shell_running {
            "Running your command".to_owned()
        } else {
            status.doing.to_string()
        };
        session.status = doing;
    }

    /// `asking`: the user is asked to trust the folder, which would run this.
    pub(crate) fn render_status(
        &mut self,
        status: &Status,
        asking: Option<&[Runs]>,
        cwd: &Path,
        watch: Option<&Watch>,
        worktree: Option<&Worktree>,
    ) {
        let session = &mut self.session;
        session.cwd = shortened_path(cwd, 36);
        let project = sessions::folder_name(cwd);
        session.project = project;
        session.title = status.title.as_deref().unwrap_or("New session").into();
        session.sign_in = match status.sign_in {
            SignIn::SignedIn => SignInState::SignedIn,
            SignIn::Refused { .. } | SignIn::Renewing => SignInState::SignedOut,
            SignIn::Unknown | SignIn::Asked => SignInState::Unknown,
        };
        session.shows_sign_in = status.sign_in.waits_for_user();
        session.needs_trust = asking.is_some();
        session.trust_runs = asking.unwrap_or_default().iter().map(run_line).collect();
        let (label, detail) = sandbox_text(status.sandboxing);
        session.sandbox_label = label.into();
        session.sandbox_detail = detail.into();
        session.sandbox_on = status.sandbox_choice.on;
        session.sandbox_desktop = status.sandbox_choice.desktop;
        session.sandbox_hardware = status.sandbox_choice.hardware;
        // Not yet known isn't warned about: every start would flash the warning.
        session.sandboxed = matches!(
            status.sandboxing,
            Sandboxing::Landlock(Enforcement::Full | Enforcement::Partial)
                | Sandboxing::ClaudeCodes
                | Sandboxing::Pending
        );
        session.sandbox_pending = status.sandboxing == Sandboxing::Pending;
        let (context, fraction) = match (status.context_tokens, status.context_window) {
            (0, _) => (String::new(), 0.0),
            (used, 0) => (short_count(used), 0.0),
            (used, window) => (
                format!("{} / {}", short_count(used), short_count(window)),
                fraction_of(used, window),
            ),
        };
        session.context = context;
        session.context_fraction = fraction;
        let cost = if status.cost_usd > 0.0 {
            format!("${:.2}", status.cost_usd)
        } else {
            String::new()
        };
        session.cost = cost;
        // Unknown shows as nothing (less than 0), not as an empty bar.
        let used = |limit: Option<LimitUse>| limit.map_or(-1.0, |limit| limit.used);
        let resets = |limit: Option<LimitUse>| {
            limit
                .and_then(|limit| limit.resets_at)
                .map_or_else(String::new, |at| reset_text(at, false))
        };
        session.limit_five_hour = used(status.limits.five_hour);
        session.limit_five_hour_resets = resets(status.limits.five_hour);
        session.limit_seven_day = used(status.limits.seven_day);
        let account = &status.account;
        session.email = account.email.as_str().into();
        session.organization = account.organization.as_str().into();
        session.provider = provider_name(&account.api_provider).into();
        session.version = status.version.as_str().into();
        session.worktree_name = worktree
            .map_or("", |worktree| worktree.name.as_str())
            .into();
        session.worktree_branch = worktree
            .map_or("", |worktree| worktree.branch.as_str())
            .into();
        session.watching = watch.is_some();
        if let Some(watch) = watch {
            let doing = match watch.session.status {
                LiveStatus::Busy => "working",
                LiveStatus::Waiting => "waiting for its user",
                LiveStatus::Idle => "idle",
                LiveStatus::Other => "running",
            };
            let text = format!(
                "{} runs in another Claude Code window ({doing}). This view follows it.",
                watch.session.name
            );
            session.watch_text = text;
        }
    }

    /// The usage panel: the plan's limits, this session, the context, the last day and week.
    pub(crate) fn render_usage(&mut self, panel: &UsagePanel, status: &Status) {
        let usage = &mut self.usage;
        usage.open = panel.open;
        if !panel.open {
            return;
        }
        usage.loading = panel.waiting > 0;
        usage.note = panel.error.as_deref().unwrap_or("").into();
        let mut sections = vec![limits_section(panel.report.as_ref(), status)];
        if let Some(report) = &panel.report {
            sections.push(session_section(&report.session));
        }
        if let Some(context) = &panel.context {
            sections.push(context_section(context));
        }
        if let Some(report) = &panel.report {
            let periods = [
                ("The last 24 hours", &report.behaviors.day),
                ("The last 7 days", &report.behaviors.week),
            ];
            sections.extend(
                periods
                    .into_iter()
                    .filter_map(|(title, period)| Some(period_section(title, period.as_ref()?))),
            );
        }
        // A tab with nothing behind it says nothing.
        sections.retain(|section| !section.rows.is_empty() || !section.note.is_empty());
        usage.sections = sections;
    }

    /// What runs in the background: each task, and how much of each kind.
    /// What runs besides the turn, and whether a stopped turn left it running.
    pub(crate) fn render_tasks(&mut self, tasks: &Tasks, left_running: bool) {
        let session = &mut self.session;
        let kinds: Vec<wire::TaskKind> = tasks
            .running()
            .map(|running| running.task.task_type)
            .collect();
        session.tasks_summary = tasks_summary(&kinds);
        let items: Vec<TaskItem> = tasks.running().map(task_item).collect();
        session.tasks = items;
        session.tasks_left_running = left_running;
    }

    /// Opens the list of what runs.
    pub(crate) fn show_tasks(&mut self) {
        self.command(Command::ShowTasks);
    }

    /// The subagent followed in the side panel, if one is, and whether it still runs.
    pub(crate) fn render_peek(&mut self, peek: Option<&Peek>, tasks: &Tasks) {
        let global = &mut self.peek;
        let Some(peek) = peek else {
            global.open = false;
            global.steps = Vec::new();
            global.output = Vec::new();
            return;
        };
        let running = tasks
            .running()
            .any(|running| running.task.task_id == peek.task);
        global.open = true;
        global.kind = task_kind(peek.kind);
        global.title = peek.title.as_str().into();
        global.detail = peek_detail(peek);
        global.running = running;
        let (steps, lines, note): (Vec<PeekStep>, Vec<String>, String) = match &peek.view {
            PeekView::Conversation(conversation) => (
                conversation.steps.iter().map(peek_step).collect(),
                Vec::new(),
                String::new(),
            ),
            PeekView::Output(output) => (
                Vec::new(),
                output.text.lines().map(String::from).collect(),
                output_note(output),
            ),
        };
        global.steps = steps;
        global.output = lines;
        global.note = note;
    }

    /// The model and mode menus, and which of each is in use.
    pub(crate) fn render_menus(&mut self, status: &Status) {
        let mut models: Vec<(String, String)> = status
            .models
            .iter()
            .map(|model| (model.display_name.clone(), model.description.clone()))
            .collect();
        let picked = status
            .model_choice
            .as_ref()
            .and_then(|choice| {
                status
                    .models
                    .iter()
                    .position(|model| &model.value == choice)
            })
            .or_else(|| {
                status.models.iter().position(|model| {
                    model.value == "default" && model.resolved_model == status.model
                })
            })
            .or_else(|| {
                status.models.iter().position(|model| {
                    model.resolved_model == status.model || model.value == status.model
                })
            });
        let model_index = match picked {
            Some(index) => index,
            None if !status.model.is_empty() => {
                models.push((status.model.clone(), String::new()));
                models.len() - 1
            }
            // Before Claude Code has said which models there are: a name, not a bare icon.
            None if models.is_empty() => {
                let name = status
                    .model_choice
                    .clone()
                    .unwrap_or_else(|| "Default model".to_owned());
                models.push((
                    name,
                    "Claude Code says which model this is once it runs".to_owned(),
                ));
                0
            }
            None => 0,
        };
        let mut modes: Vec<(String, String)> = PermissionMode::iter()
            .map(|mode| (mode.label().to_owned(), mode.description().to_owned()))
            .collect();
        let mode_index = match &status.mode {
            Some(Mode::Known(mode)) => PermissionMode::iter()
                .position(|known| known == *mode)
                .unwrap_or_default(),
            Some(Mode::Unknown(name)) => {
                modes.push((name.clone(), "As the CLI names it".to_owned()));
                modes.len() - 1
            }
            None => 0,
        };
        let session = &mut self.session;
        if self.models != models {
            session.models = menu(&models);
            self.models = models;
        }
        if self.modes != modes {
            session.modes = menu(&modes);
            self.modes = modes;
        }
        self.menu_choice = Some((index_for_slint(model_index), index_for_slint(mode_index)));
        self.session.mode = match &status.mode {
            Some(Mode::Known(PermissionMode::Default)) | None => PermissionKind::Default,
            Some(Mode::Known(PermissionMode::AcceptEdits)) => PermissionKind::AcceptEdits,
            Some(Mode::Known(PermissionMode::Plan)) => PermissionKind::Plan,
            Some(Mode::Known(PermissionMode::Auto)) => PermissionKind::Auto,
            Some(Mode::Known(PermissionMode::BypassPermissions)) => PermissionKind::Bypass,
            Some(Mode::Unknown(_)) => PermissionKind::Unknown,
        };
    }

    /// The bar above the composer, for the first request waiting.
    pub(crate) fn render_prompt(&mut self, asks: &VecDeque<Ask>, cwd: &Path) {
        let Some(ask) = asks.front() else {
            self.prompt.kind = PromptKind::None;
            self.reviewing = Some(false);
            self.prompt_shown = None;
            return;
        };
        let han = self.han;
        let fresh = self.prompt_shown.as_ref() != Some(&ask.request);
        let prompt = &mut self.prompt;
        prompt.request = ask.request.to_string();
        prompt.queued = index_for_slint(asks.len() - 1);
        if fresh {
            prompt.serial = prompt.serial.wrapping_add(1);
        }
        match &ask.kind {
            AskKind::Questions {
                asked,
                picked,
                other,
                ..
            } => {
                prompt.kind = PromptKind::Question;
                prompt.title = if asked.questions.len() > 1 {
                    "Claude has questions"
                } else {
                    "Claude has a question"
                }
                .to_owned();
                prompt.questions = questions(&asked.questions, picked, other);
            }
            AskKind::Plan {
                input,
                parts,
                editing,
            } => {
                prompt.kind = PromptKind::Plan;
                prompt.title =
                    "Claude's plan is ready. Comment on its parts, then approve it or keep planning."
                        .to_owned();
                let count = parts.iter().filter(|part| !part.comment.is_empty()).count();
                prompt.comment_count = index_for_slint(count);
                prompt.plan = parts
                    .iter()
                    .enumerate()
                    .map(|(index, part)| PlanSection {
                        block: md_block(&part.block, han),
                        excerpt: crate::plan::excerpt(&input.plan, part),
                        comment: part.comment.clone(),
                        editing: *editing == Some(index),
                    })
                    .collect();
                if fresh {
                    self.reviewing = Some(true);
                }
            }
            AskKind::Tool {
                suggestions,
                description,
            } => {
                let ToolPrompt {
                    title,
                    summary,
                    summary_reads_as,
                } = tool_prompt(ask, cwd);
                // The description often only repeats what the title or summary say.
                let repeats =
                    title.contains(description.as_str()) || summary.contains(description.as_str());
                let detail = if repeats { "" } else { description.as_str() };
                prompt.kind = PromptKind::Tool;
                prompt.title = title;
                prompt.summary = summary;
                prompt.summary_prose = summary_reads_as == tools::Writing::Prose;
                detail.clone_into(&mut prompt.detail);
                // Only a rule the window can say in full is offered, and none for a message to
                // another session: the user allows each (see `relay`).
                prompt.always_label = suggestions
                    .first()
                    .filter(|_| ask.tool != ToolName::RelaySendMessage)
                    .and_then(|offered| offered.rule.as_ref())
                    .map(always_label)
                    .unwrap_or_default();
            }
        }
        self.prompt_shown = Some(ask.request.clone());
    }

    /// The session list (see `sidebar`); `filtered` says why it may be empty.
    pub(crate) fn render_sidebar(&mut self, rows: &[sidebar::Row], filtered: bool) {
        let items: Vec<SessionRow> = rows.iter().map(session_row).collect();
        let empty = match (items.is_empty(), filtered) {
            (false, _) => "",
            (true, true) => "No session matches the filter.",
            (true, false) => "None yet: what you start here is listed here.",
        };
        let sessions = &mut self.sessions;
        sessions.empty = empty.into();
        sessions.rows = items;
    }

    /// The sidebar's width as dragged and kept; the default if none.
    pub(crate) fn set_sidebar_width(&mut self, width_px: Option<u16>) {
        self.command(Command::SidebarWidth(width_px));
    }

    /// Why the folder typed in the list of folders didn't open; empty for nothing to say.
    pub(crate) fn set_project_problem(&mut self, problem: &str) {
        self.sessions.problem = problem.into();
    }

    /// The folder typed in the list of folders opened: the list closes.
    pub(crate) fn projects_opened(&mut self) {
        self.sessions.problem.clear();
        self.command(Command::ProjectsOpened);
    }

    pub(crate) fn render_projects(&mut self, projects: &[ProjectInfo]) {
        let items: Vec<ProjectItem> = projects
            .iter()
            .map(|project| ProjectItem {
                path: project.path.to_string_lossy().as_ref().into(),
                shown: shortened_path(&project.path, 56),
                name: sessions::folder_name(&project.path),
                age: sessions::age(project.modified),
            })
            .collect();
        self.sessions.projects = items;
    }

    pub(crate) fn render_commands(&mut self, commands: &[&wire::CommandInfo]) {
        let items: Vec<CommandItem> = commands
            .iter()
            .map(|command| CommandItem {
                name: command.name.as_str().into(),
                hint: command.argument_hint.as_str().into(),
                description: command.description.as_str().into(),
            })
            .collect();
        self.composer.commands = items;
    }
}

/// A text's CJK characters (see `cjk`), for the window to set the text by them.
fn cjk(text: &str, han: CjkLanguage) -> CjkChars {
    CjkText::of(text, han).map_or_else(CjkChars::default, |cjk| CjkChars {
        language: match cjk.language {
            CjkLanguage::Japanese => Cjk::Japanese,
            CjkLanguage::SimplifiedChinese => Cjk::SimplifiedChinese,
            CjkLanguage::TraditionalChinese => Cjk::TraditionalChinese,
            CjkLanguage::Korean => Cjk::Korean,
        },
        main: match cjk.standing {
            Standing::Main => true,
            Standing::Guest => false,
        },
    })
}

/// Claude's questions, with the options picked and what was written for each.
fn questions(
    questions: &[wire::Question],
    picked: &[Vec<bool>],
    other: &[String],
) -> Vec<Question> {
    questions
        .iter()
        .zip(picked)
        .zip(other)
        .map(|((question, picks), other)| Question {
            question: question.question.clone(),
            header: question.header.clone(),
            answered: answered(picks, other),
            multi: question.multi_select,
            previews: question
                .options
                .iter()
                .any(|option| !option.preview.is_empty()),
            options: question
                .options
                .iter()
                .zip(picks)
                .map(|(option, &selected)| QuestionOption {
                    label: option.label.clone(),
                    description: option.description.clone(),
                    preview: option.preview.clone(),
                    selected,
                })
                .collect(),
        })
        .collect()
}

/// A tool card's row, as `view` describes it; `output` only for an open card.
fn card(
    row: &mut Entry,
    view: tools::ToolView,
    state: ToolState,
    output: &str,
    expanded: bool,
    han: CjkLanguage,
) {
    row.kind = EntryKind::Tool;
    row.expanded = expanded;
    row.tool_title = view.title;
    row.tool_summary = view.summary;
    row.tool_summary_prose = view.summary_reads_as == tools::Writing::Prose;
    row.tool_state = state;
    row.has_detail = !view.detail.is_empty();
    row.detail = Styled::plain(&view.detail);
    if !view.markdown.is_empty() {
        row.blocks = blocks(&view.markdown, han);
    }
    if card_depth(expanded, row.hidden) == tools::Depth::Full {
        let (output, note) = clip(output);
        row.has_output = !output.is_empty();
        row.output = Styled::plain(&output);
        row.output_note = note;
    }
    row.diff = view
        .diff
        .into_iter()
        .map(|(kind, text)| DiffLine {
            kind: diff_kind(kind),
            text,
        })
        .collect();
}

/// The Markdown blocks of `text`.
fn blocks(text: &str, han: CjkLanguage) -> Vec<MdBlock> {
    markdown::blocks(text)
        .iter()
        .map(|block| md_block(block, han))
        .collect()
}

/// A Markdown block for the window; its prose in the family of its own text, as a paragraph's
/// script sets its lines (`han` for Han characters alone).
fn md_block(block: &Block, han: CjkLanguage) -> MdBlock {
    match block {
        Block::Text(source) => MdBlock {
            kind: BlockKind::Text,
            body: Styled::markdown(source),
            cjk: cjk(source, han),
            ..MdBlock::default()
        },
        Block::Heading(level, source) => MdBlock {
            kind: BlockKind::Heading,
            body: Styled::markdown(&format!("**{source}**")),
            level: i32::from(*level),
            cjk: cjk(source, han),
            ..MdBlock::default()
        },
        Block::Code { lang, code } => MdBlock {
            kind: BlockKind::Code,
            body: Styled::plain(code),
            lang: lang.as_str().into(),
            raw: code.as_str().into(),
            ..MdBlock::default()
        },
        Block::Quote(source) => MdBlock {
            kind: BlockKind::Quote,
            body: Styled::markdown(source),
            cjk: cjk(source, han),
            ..MdBlock::default()
        },
        Block::Table(table) => {
            let rows: Vec<TableRow> = table
                .rows
                .iter()
                .enumerate()
                .map(|(position, row)| {
                    let cells: Vec<Styled> = row
                        .iter()
                        .map(|cell| match position {
                            0 if !cell.trim().is_empty() => {
                                Styled::markdown(&format!("**{cell}**"))
                            }
                            _ => Styled::markdown(cell),
                        })
                        .collect();
                    TableRow { cells }
                })
                .collect();
            let column_stretch: Vec<f32> = table
                .longest_cell_chars
                .iter()
                .map(|&chars| f32::from(u16::try_from(chars).unwrap_or(u16::MAX)))
                .collect();
            MdBlock {
                kind: BlockKind::Table,
                cjk: cjk(&table.source, han),
                raw: table.source.as_str().into(),
                rows,
                column_stretch,
                ..MdBlock::default()
            }
        }
        Block::Rule => MdBlock {
            kind: BlockKind::Rule,
            ..MdBlock::default()
        },
    }
}

/// A long output's start, and what was left out.
fn clip(output: &str) -> (String, String) {
    let lines = output.lines().count();
    let mut clipped = output
        .lines()
        .take(OUTPUT_LINES)
        .collect::<Vec<_>>()
        .join("\n");
    if clipped.len() > OUTPUT_CHARS {
        clipped = tools::truncate_chars(&clipped, OUTPUT_CHARS);
    }
    let note = if lines > OUTPUT_LINES {
        format!("{} more lines; copy for all of it", lines - OUTPUT_LINES)
    } else if clipped.len() < output.len() {
        "Cut short; copy for all of it".to_owned()
    } else {
        String::new()
    };
    (clipped, note)
}

/// What the bar asks about a tool call, and what it shows of it.
struct ToolPrompt {
    title: String,
    summary: String,
    /// Words (a message to another session, an agent's task) or code, as on the tool's card.
    summary_reads_as: tools::Writing,
}

fn tool_prompt(ask: &Ask, cwd: &Path) -> ToolPrompt {
    let view = tools::describe(&ask.tool, &ask.input, cwd, tools::Depth::Headline);
    let field = |key: &str| tools::str_field(&ask.input, key).to_owned();
    let (title, summary) = if ask.tool == ToolName::Bash {
        // Claude can ask for a command to run outside Claude Code's own sandbox.
        let outside =
            ask.input.get("dangerouslyDisableSandbox") == Some(&serde_json::Value::Bool(true));
        let title = if outside {
            "Allow Claude to run this command outside Claude Code's sandbox?"
        } else {
            "Allow Claude to run this command?"
        };
        (title.into(), field("command"))
    } else if ask.tool == ToolName::RelaySendMessage {
        (
            format!("Allow Claude to send this message to {}?", field("to")),
            field("message"),
        )
    } else {
        match ask.tool.kind() {
            ToolKind::Edit | ToolKind::Write => (
                format!("Allow Claude to edit {}?", view.summary),
                String::new(),
            ),
            ToolKind::Web if ask.tool == ToolName::WebFetch => {
                ("Allow Claude to fetch this page?".into(), field("url"))
            }
            ToolKind::Read
            | ToolKind::Command
            | ToolKind::Search
            | ToolKind::Web
            | ToolKind::Agent
            | ToolKind::Other => (format!("Allow Claude to use {}?", view.title), view.summary),
        }
    };
    ToolPrompt {
        title,
        summary,
        summary_reads_as: view.summary_reads_as,
    }
}

/// The button for remembering a rule the CLI suggests, naming all of it and where it's kept,
/// e.g. "Always allow Bash(npm test) in this project".
fn always_label(rule: &wire::Suggestion) -> String {
    let kept = |destination: wire::RuleDestination| match destination {
        wire::RuleDestination::Session => " this session",
        wire::RuleDestination::LocalSettings => " in this project",
        wire::RuleDestination::ProjectSettings => " in this project, for everyone",
        wire::RuleDestination::UserSettings => " in every project",
        wire::RuleDestination::Other => "",
    };
    match rule {
        wire::Suggestion::AddRules { rules, destination } => {
            let named: Vec<String> = rules
                .iter()
                .map(|rule| match &rule.rule_content {
                    Some(content) => format!("{}({content})", rule.tool_name),
                    None => rule.tool_name.clone(),
                })
                .collect();
            format!("Always allow {}{}", named.join(", "), kept(*destination))
        }
        wire::Suggestion::SetMode {
            mode: Mode::Known(PermissionMode::AcceptEdits),
        } => "Allow all edits this session".into(),
        wire::Suggestion::SetMode {
            mode: Mode::Known(mode),
        } => format!("Switch to {}", mode.label()),
        wire::Suggestion::SetMode {
            mode: Mode::Unknown(mode),
        } => format!("Switch to {mode}"),
        wire::Suggestion::AddDirectories {
            directories,
            destination,
        } => format!(
            "Always allow in {}{}",
            directories.join(", "),
            kept(*destination)
        ),
        wire::Suggestion::Other => String::new(),
    }
}

/// The sandbox chip's words, and what its menu says about it.
fn sandbox_text(sandboxing: Sandboxing) -> (&'static str, &'static str) {
    match sandboxing {
        Sandboxing::Landlock(Enforcement::Full) => (
            "Sandboxed",
            "The network is open.\n\
             Claude Code, and everything it starts, may change only this project, its git \
             folder, caches and temporary files.\n\
             It can't read your private files, reach desktop services or sockets (except the \
             ssh agent), or gain privileges (sudo).\n\
             A command that fails with \"Permission denied\" was blocked by the sandbox. To \
             allow more, add folders in the settings (Ctrl+,).",
        ),
        Sandboxing::Landlock(Enforcement::Partial) => (
            "Partly sandboxed",
            "This kernel's Landlock is older: files are kept apart, but not everything \
             (socket files, say) is.",
        ),
        Sandboxing::Landlock(Enforcement::None) => (
            "Not sandboxed",
            "This kernel has no Landlock: Claude Code has all your rights.",
        ),
        Sandboxing::ClaudeCodes => (
            "Claude Code's sandbox",
            "Your Claude Code settings turn on its own sandbox, for its shell commands; this \
             window's would break it, so it stays off. Claude Code's file tools, hooks and MCP \
             servers run with all your rights.",
        ),
        // Short enough for one line under the sidebar's chip at its default width.
        Sandboxing::Off => ("Not sandboxed", "Claude Code has all your rights."),
        Sandboxing::Pending => (
            "Sandbox: starting",
            "How far this kernel enforces the sandbox is known once Claude Code runs.",
        ),
    }
}

fn menu(items: &[(String, String)]) -> Vec<MenuItem> {
    let items: Vec<MenuItem> = items
        .iter()
        .map(|(title, description)| MenuItem {
            title: title.as_str().into(),
            description: description.as_str().into(),
        })
        .collect();
    items
}

/// A row of the session list as the sidebar shows it.
fn session_row(row: &sidebar::Row) -> SessionRow {
    match row {
        sidebar::Row::RunningHeading { running, waiting } => SessionRow {
            kind: SessionRowKind::Heading,
            title: format!("Running · {running}"),
            detail: match waiting {
                0 => String::new(),
                1 => "1 needs you".into(),
                waiting => format!("{waiting} need you"),
            },
            ..SessionRow::default()
        },
        sidebar::Row::EarlierHeading { folder } => SessionRow {
            kind: SessionRowKind::Heading,
            title: format!("Earlier in {folder}"),
            ..SessionRow::default()
        },
        sidebar::Row::Running(running) => SessionRow {
            kind: SessionRowKind::Running,
            id: running.id.as_str().into(),
            title: running.title.as_str().into(),
            detail: running_detail(running),
            state: match running.state {
                LiveStatus::Idle => LiveState::Idle,
                LiveStatus::Busy => LiveState::Busy,
                LiveStatus::Waiting => LiveState::Waiting,
                LiveStatus::Other => LiveState::Other,
            },
            selected: running.shown,
            worktree: running.worktree.as_deref().unwrap_or_default().into(),
            agents: index_for_slint(running.agents),
            ..SessionRow::default()
        },
        sidebar::Row::Earlier(earlier) => SessionRow {
            kind: SessionRowKind::Earlier,
            id: earlier.id.as_str().into(),
            title: earlier.title.as_str().into(),
            age: sessions::age(earlier.modified),
            selected: earlier.shown,
            worktree: earlier.worktree.as_deref().unwrap_or_default().into(),
            ..SessionRow::default()
        },
    }
}

/// A running session's second line: what it does, the name to message it by unless its title
/// is that name, the folder it runs in when another, its worktree.
fn running_detail(running: &sidebar::Running) -> String {
    let doing = match running.state {
        LiveStatus::Waiting => "Needs you",
        LiveStatus::Busy => "Working",
        LiveStatus::Idle => "Idle",
        LiveStatus::Other => "Running",
    };
    let worktree = running
        .worktree
        .as_ref()
        .map(|worktree| format!("⎇ {worktree}"));
    [
        Some(doing),
        running.name_beside_title(),
        running.elsewhere.as_deref(),
        worktree.as_deref(),
    ]
    .into_iter()
    .flatten()
    .collect::<Vec<&str>>()
    .join(" · ")
}

fn stat(label: &str, value: String, detail: String, fraction: f32) -> StatRow {
    StatRow {
        label: label.into(),
        value,
        detail,
        fraction,
    }
}

/// Who answers, for people: Claude Code names them by its own ids, and one this window doesn't
/// know stays as written.
fn provider_name(provider: &str) -> &str {
    match provider {
        "firstParty" => "Anthropic",
        "bedrock" => "Amazon Bedrock",
        "vertex" => "Google Vertex AI",
        "foundry" => "Microsoft Foundry",
        other => other,
    }
}

fn section(title: &str, note: String, rows: Vec<StatRow>) -> StatSection {
    StatSection {
        title: title.into(),
        note,
        rows,
    }
}

fn limit_row(label: &str, limit: LimitUse) -> StatRow {
    let resets = limit
        .resets_at
        .map_or_else(String::new, |at| reset_text(at, true));
    stat(
        label,
        format!("{:.0}%", limit.used * 100.0),
        resets,
        limit.used,
    )
}

/// The plan's limits: every one the CLI's `/usage` lists, or the two its status events
/// carry until it answers.
fn limits_section(report: Option<&wire::UsageReport>, status: &Status) -> StatSection {
    let rows: Vec<StatRow> = match report {
        Some(report) if !report.rate_limits.limits.is_empty() => report
            .rate_limits
            .limits
            .iter()
            .map(|limit| {
                let model = limit
                    .scope
                    .as_ref()
                    .and_then(|scope| scope.model.as_ref())
                    .and_then(|model| model.display_name.as_deref());
                let label = match (limit.kind, model) {
                    (LimitKind::Session, _) => "This session window (5 hours)".to_owned(),
                    (LimitKind::WeeklyAll, _) => "This week, all models".to_owned(),
                    (LimitKind::WeeklyScoped | LimitKind::Other, Some(model)) => {
                        format!("This week, {model}")
                    }
                    (LimitKind::WeeklyScoped, None) => "This week, one model".to_owned(),
                    (LimitKind::Other, None) => "Another limit".to_owned(),
                };
                limit_row(&label, LimitUse::from(limit))
            })
            .collect(),
        Some(_) | None => [
            ("This session window (5 hours)", status.limits.five_hour),
            ("This week", status.limits.seven_day),
        ]
        .into_iter()
        .filter_map(|(label, limit)| Some(limit_row(label, limit?)))
        .collect(),
    };
    // The plan as Claude Code names it ("max"), its first letter raised: "Max plan".
    let plan = &status.account.subscription_type;
    let note = plan.chars().next().map_or_else(String::new, |first| {
        let rest = plan.get(first.len_utf8()..).unwrap_or_default();
        format!("{}{rest} plan", first.to_uppercase())
    });
    section("Plan limits", note, rows)
}

fn session_section(session: &wire::SessionUsage) -> StatSection {
    let mut rows = vec![
        stat(
            "Cost at API prices",
            format!("${:.2}", session.total_cost_usd),
            "What the same use would cost on the API; a plan covers it".to_owned(),
            -1.0,
        ),
        stat(
            "Time with the model",
            duration_text(session.total_api_duration_ms),
            format!("of {} open", duration_text(session.total_duration_ms)),
            -1.0,
        ),
        stat(
            "Lines changed",
            format!(
                "+{}  −{}",
                session.total_lines_added, session.total_lines_removed
            ),
            String::new(),
            -1.0,
        ),
    ];
    let mut models: Vec<(&String, &wire::ModelTokens)> = session.model_usage.iter().collect();
    models.sort_by(|left, right| left.0.cmp(right.0));
    rows.extend(models.into_iter().map(|(model, tokens)| {
        stat(
            model,
            format!("${:.2}", tokens.cost_usd),
            format!(
                "{} in · {} out · {} read from cache · {} written to it",
                short_count(tokens.input_tokens),
                short_count(tokens.output_tokens),
                short_count(tokens.cache_read_input_tokens),
                short_count(tokens.cache_creation_input_tokens),
            ),
            -1.0,
        )
    }));
    section("This session", String::new(), rows)
}

fn context_section(context: &wire::ContextReport) -> StatSection {
    let rows = context
        .categories
        .iter()
        .map(|category| {
            let detail = match category.kind {
                ContextKind::Deferred => "Loaded only when needed; not in use now",
                ContextKind::Buffer => "Kept free so the conversation can be compacted",
                ContextKind::Used | ContextKind::Free | ContextKind::Other => "",
            };
            stat(
                &category.name,
                short_count(category.tokens),
                detail.to_owned(),
                fraction_of(category.tokens, context.max_tokens),
            )
        })
        .collect();
    let note = format!(
        "{} of {} tokens in use",
        short_count(context.total_tokens),
        short_count(context.max_tokens)
    );
    section("Context", note, rows)
}

fn period_section(title: &str, period: &wire::Period) -> StatSection {
    let mut rows = vec![
        stat(
            "Requests",
            period.request_count.to_string(),
            String::new(),
            -1.0,
        ),
        stat(
            "Sessions",
            period.session_count.to_string(),
            String::new(),
            -1.0,
        ),
    ];
    let shares = |label: &str, shares: &[wire::Share]| {
        (!shares.is_empty()).then(|| {
            let listed: Vec<String> = shares
                .iter()
                .map(|share| format!("{} {:.0}%", share.name, share.pct))
                .collect();
            stat(label, String::new(), listed.join(", "), -1.0)
        })
    };
    rows.extend(shares("Agents", &period.agents));
    rows.extend(shares("Skills", &period.skills));
    // Claude Code's own advice, with the share of requests it is about.
    rows.extend(period.recommendations.iter().map(|advice| {
        stat(
            &advice.title,
            format!("{:.0}% of requests", advice.pct),
            advice.detail.clone(),
            -1.0,
        )
    }));
    section(title, String::new(), rows)
}

/// When a limit starts over: "resets 15:00", or with the time left, "resets in 4h 58m (15:00)".
fn reset_text(at: jiff::Timestamp, with_time_left: bool) -> String {
    let seconds = at.as_second() - jiff::Timestamp::now().as_second();
    let local = at.to_zoned(jiff::tz::TimeZone::system());
    let clock = if seconds < 86_400 {
        local.strftime("%H:%M").to_string()
    } else {
        local.strftime("%a %H:%M").to_string()
    };
    if !with_time_left {
        return format!("resets {clock}");
    }
    let left = match seconds {
        ..=0 => return "resets now".to_owned(),
        1..3_600 => format!("{}m", seconds / 60),
        3_600..86_400 => format!("{}h {}m", seconds / 3_600, seconds % 3_600 / 60),
        _ => format!("{}d {}h", seconds / 86_400, seconds % 86_400 / 3_600),
    };
    format!("resets in {left} ({clock})")
}

/// A time span as [`duration_text`] writes it.
pub(crate) fn short_duration(duration: std::time::Duration) -> String {
    duration_text(u64::try_from(duration.as_millis()).unwrap_or(u64::MAX))
}

/// "45s", "3m 12s", "1h 4m".
fn duration_text(milliseconds: u64) -> String {
    let seconds = milliseconds / 1_000;
    match seconds {
        0..60 => format!("{seconds}s"),
        60..3_600 => format!("{}m {}s", seconds / 60, seconds % 60),
        _ => format!("{}h {}m", seconds / 3_600, seconds % 3_600 / 60),
    }
}

/// `used` of `total` as a share from 0 to 1, for a bar.
fn fraction_of(used: u64, total: u64) -> f32 {
    let percent = used.saturating_mul(100) / total.max(1);
    f32::from(u16::try_from(percent.min(100)).unwrap_or(100)) / 100.0
}

/// A line of what a folder would run, as the trust question lists it.
fn run_line(runs: &Runs) -> String {
    match runs {
        Runs::Hook {
            event,
            matcher,
            command,
        } if matcher.is_empty() => format!("Hook on {event}: {command}"),
        Runs::Hook {
            event,
            matcher,
            command,
        } => format!("Hook on {event} ({matcher}): {command}"),
        Runs::McpServer { name, runs } => format!("MCP server {name}: {runs}"),
        Runs::ApiKeyHelper(command) => format!("API key helper: {command}"),
        Runs::StatusLine(command) => format!("Status line: {command}"),
        Runs::Plugin(name) => format!("Plugin {name}"),
        Runs::Environment(names) => format!("Environment variables: {}", names.join(", ")),
        Runs::Unreadable { file, reason } => format!("Couldn't read {file}: {reason}"),
    }
}

/// Slint counts in `i32`; no list here comes near its end.
fn index_for_slint(index: usize) -> i32 {
    i32::try_from(index).unwrap_or(i32::MAX)
}

/// Whether a question of Claude's is answered: an option picked, or something else written.
fn answered(picks: &[bool], other: &str) -> bool {
    picks.contains(&true) || !other.trim().is_empty()
}

fn failed_image(why: String) -> ShownImage {
    ShownImage {
        state: ImageState::Failed,
        note: why,
        ..ShownImage::default()
    }
}

/// What only an open card shows is made only for one.
fn card_depth(expanded: bool, hidden: bool) -> tools::Depth {
    if expanded && !hidden {
        tools::Depth::Full
    } else {
        tools::Depth::Headline
    }
}

fn shell_state(state: chat::ShellState) -> ToolState {
    match state {
        chat::ShellState::Running => ToolState::Running,
        chat::ShellState::Done => ToolState::Done,
        chat::ShellState::Failed => ToolState::Failed,
        chat::ShellState::Stopped => ToolState::Cancelled,
    }
}

fn tool_state(state: chat::ToolState) -> ToolState {
    match state {
        chat::ToolState::Streaming => ToolState::Streaming,
        chat::ToolState::Running => ToolState::Running,
        chat::ToolState::Awaiting => ToolState::Awaiting,
        chat::ToolState::Done => ToolState::Done,
        chat::ToolState::Failed => ToolState::Failed,
        chat::ToolState::Denied => ToolState::Denied,
        chat::ToolState::Cancelled => ToolState::Cancelled,
    }
}

fn diff_kind(kind: tools::DiffKind) -> DiffKind {
    match kind {
        tools::DiffKind::Context => DiffKind::Context,
        tools::DiffKind::Added => DiffKind::Added,
        tools::DiffKind::Removed => DiffKind::Removed,
        tools::DiffKind::Hunk => DiffKind::Hunk,
    }
}

/// The kind of a task as the window tells them apart.
fn task_kind(kind: wire::TaskKind) -> TaskKind {
    match kind {
        wire::TaskKind::LocalAgent
        | wire::TaskKind::RemoteAgent
        | wire::TaskKind::InProcessTeammate => TaskKind::Agent,
        wire::TaskKind::LocalBash => TaskKind::Command,
        wire::TaskKind::LocalWorkflow => TaskKind::Workflow,
        wire::TaskKind::MonitorMcp | wire::TaskKind::MonitorWs => TaskKind::Monitor,
        wire::TaskKind::Other => TaskKind::Other,
    }
}

fn task_item(running: Running<'_>) -> TaskItem {
    let Running {
        task,
        place,
        progress,
    } = running;
    let what = match task.task_type {
        wire::TaskKind::LocalAgent => "Agent",
        wire::TaskKind::RemoteAgent => "Agent in the cloud",
        wire::TaskKind::InProcessTeammate => "Teammate",
        wire::TaskKind::LocalBash => "Shell command",
        wire::TaskKind::LocalWorkflow => "Workflow",
        wire::TaskKind::MonitorMcp | wire::TaskKind::MonitorWs => "Monitor",
        wire::TaskKind::Other => "Task",
    };
    let mut detail = match &task.subagent_type {
        Some(agent) => format!("{what} · {agent}"),
        None => what.to_owned(),
    };
    if place == Place::Foreground {
        detail.push_str(" · in the foreground");
    }
    let used = progress.map(|progress| task_used(progress.usage));
    let now = progress.and_then(|progress| match (&progress.summary, &progress.last_tool) {
        (Some(summary), _) => Some(summary.clone()),
        (None, Some(tool)) => Some(format!("Last used {tool}")),
        (None, None) => None,
    });
    TaskItem {
        id: task.task_id.to_string(),
        kind: task_kind(task.task_type),
        title: if task.description.is_empty() {
            what.into()
        } else {
            task.description.as_str().into()
        },
        detail,
        used: used.unwrap_or_default(),
        now: now.unwrap_or_default(),
        peekable: Followed::of(task.task_type).is_some(),
    }
}

/// "Agent · Explore", as the chip's list names a subagent; "Shell command".
fn peek_detail(peek: &Peek) -> String {
    match &peek.view {
        PeekView::Conversation(Conversation {
            agent: Some(agent), ..
        }) => format!("Agent · {agent}"),
        PeekView::Conversation(_) => "Agent".to_owned(),
        PeekView::Output(_) => "Shell command".to_owned(),
    }
}

/// What the side panel says of a command's output beside it: why there is none, or that it is
/// only the end.
fn output_note(output: &CommandOutput) -> String {
    match output.refusal() {
        Some(refusal) => format!("Claude Code doesn't show what it printed: {refusal}"),
        None if output.truncated => format!(
            "Only the end of the {} it printed",
            bytes_text(output.total_bytes)
        ),
        None => String::new(),
    }
}

/// "9 KB", "1.2 MB": the size of a command's output, which is shown whole below 8 KB.
fn bytes_text(bytes: u64) -> String {
    const KB: u64 = 1024;
    const MB: u64 = KB * KB;
    match bytes {
        ..MB => format!("{} KB", bytes.div_ceil(KB)),
        MB.. => format!("{}.{} MB", bytes / MB, (bytes % MB) * 10 / MB),
    }
}

fn peek_step(step: &crate::app::Step) -> PeekStep {
    PeekStep {
        kind: match step.kind {
            crate::app::StepKind::Asked => StepKind::Asked,
            crate::app::StepKind::Said => StepKind::Said,
            crate::app::StepKind::Called => StepKind::Called,
        },
        title: step.title.as_str().into(),
        text: step.text.as_str().into(),
    }
}

/// "14 tool uses · 31k tokens · 2m 40s"; what is still none is left out.
fn task_used(usage: wire::TaskUsage) -> String {
    let tools = match usage.tool_uses {
        0 => None,
        1 => Some("1 tool use".to_owned()),
        uses => Some(format!("{uses} tool uses")),
    };
    let tokens =
        (usage.total_tokens > 0).then(|| format!("{} tokens", short_count(usage.total_tokens)));
    let time = (usage.duration_ms > 0).then(|| duration_text(usage.duration_ms));
    [tools, tokens, time]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>()
        .join(" · ")
}

/// How much runs of each kind, the most common kinds of work first: "2 agents, 1 command".
fn tasks_summary(kinds: &[wire::TaskKind]) -> String {
    [
        TaskKind::Agent,
        TaskKind::Command,
        TaskKind::Workflow,
        TaskKind::Monitor,
        TaskKind::Other,
    ]
    .into_iter()
    .filter_map(|kind| {
        let count = kinds
            .iter()
            .filter(|task_type| task_kind(**task_type) == kind)
            .count();
        let (one, many) = match kind {
            TaskKind::Agent => ("agent", "agents"),
            TaskKind::Command => ("command", "commands"),
            TaskKind::Workflow => ("workflow", "workflows"),
            TaskKind::Monitor => ("monitor", "monitors"),
            TaskKind::Other => ("task", "tasks"),
        };
        match count {
            0 => None,
            1 => Some(format!("1 {one}")),
            _ => Some(format!("{count} {many}")),
        }
    })
    .collect::<Vec<_>>()
    .join(", ")
}

/// 950, 4.2k, 45k, 1.2M.
pub(crate) fn short_count(count: u64) -> String {
    let scaled = |divisor: u32, unit: &str| {
        let hundreds = f64::from(u32::try_from(count / 100).unwrap_or(u32::MAX));
        let value = hundreds * 100.0 / f64::from(divisor);
        format!("{value:.1}{unit}").replace(".0", "")
    };
    match count {
        0..1_000 => count.to_string(),
        1_000..10_000 => scaled(1_000, "k"),
        10_000..1_000_000 => format!("{}k", count / 1_000),
        _ => scaled(1_000_000, "M"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A question's tab is marked answered once an option is picked or something else is
    /// written; blanks don't count.
    #[test]
    fn a_question_is_answered_by_a_pick_or_by_writing() {
        assert!(!answered(&[false, false], ""));
        assert!(answered(&[false, true], ""));
        assert!(answered(&[false, false], "neither, build it in CI"));
        assert!(!answered(&[false, false], "  \n"));
        assert!(!answered(&[], ""));
    }

    /// The bar shows a message to another session as the words it is, and a command as code,
    /// as their cards do.
    #[test]
    fn the_bar_sets_a_message_as_prose_and_a_command_as_code() {
        let asked = |tool: ToolName, input: serde_json::Value| {
            let ask = Ask {
                request: wire::RequestId::from("r1".to_owned()),
                tool_use: None,
                tool,
                input,
                kind: AskKind::Tool {
                    suggestions: Vec::new(),
                    description: String::new(),
                },
            };
            tool_prompt(&ask, Path::new("/work"))
        };
        let message = asked(
            ToolName::RelaySendMessage,
            serde_json::json!({"to": "peer-07", "message": "Second note."}),
        );
        assert_eq!(message.summary, "Second note.");
        assert_eq!(message.summary_reads_as, tools::Writing::Prose);
        let command = asked(ToolName::Bash, serde_json::json!({"command": "ls -l"}));
        assert_eq!(command.summary, "ls -l");
        assert_eq!(command.summary_reads_as, tools::Writing::Code);
    }

    /// Counted by kind, agents first, each kind once however its tasks are interleaved.
    #[test]
    fn background_tasks_are_counted_by_kind() {
        assert_eq!(tasks_summary(&[]), "");
        assert_eq!(tasks_summary(&[wire::TaskKind::LocalBash]), "1 command");
        assert_eq!(
            tasks_summary(&[
                wire::TaskKind::LocalBash,
                wire::TaskKind::LocalAgent,
                wire::TaskKind::Other,
                wire::TaskKind::RemoteAgent,
            ]),
            "2 agents, 1 command, 1 task"
        );
    }

    fn task(kind: &str, agent: Option<&str>, description: &str) -> wire::BackgroundTask {
        serde_json::from_value(serde_json::json!({
            "task_id": "t",
            "task_type": kind,
            "subagent_type": agent,
            "description": description,
        }))
        .expect("a task")
    }

    /// A task is named by what it does, by its agent's kind where it has one, and as in the
    /// foreground where the turn waits for it.
    #[test]
    fn a_task_says_what_it_does_and_where() {
        let running = |task, place| Running {
            task,
            place,
            progress: None,
        };
        let explore = task("local_agent", Some("Explore"), "Find the tests");
        let agent = task_item(running(&explore, Place::Background));
        assert_eq!(agent.kind, TaskKind::Agent);
        assert_eq!(agent.title, "Find the tests");
        assert_eq!(agent.detail, "Agent · Explore");
        assert!(agent.peekable && agent.used.is_empty() && agent.now.is_empty());
        assert_eq!(
            task_item(running(&explore, Place::Foreground)).detail,
            "Agent · Explore · in the foreground"
        );
        let command = task("local_bash", None, "");
        let unnamed = task_item(running(&command, Place::Background));
        assert_eq!(unnamed.title, "Shell command");
        assert_eq!(unnamed.detail, "Shell command");
        assert!(unnamed.peekable, "what it prints is followed");
        let monitor = task("monitor_mcp", None, "Watch the build");
        assert!(
            !task_item(running(&monitor, Place::Background)).peekable,
            "neither a conversation here nor output to ask for"
        );
    }

    /// What a task used reads briefly and leaves out what is none; what it does now is its
    /// summary, else the tool it used last.
    #[test]
    fn a_tasks_progress_reads_briefly() {
        let usage = |tool_uses, total_tokens, duration_ms| wire::TaskUsage {
            total_tokens,
            tool_uses,
            duration_ms,
        };
        assert_eq!(
            task_used(usage(14, 31_200, 160_000)),
            "14 tool uses · 31k tokens · 2m 40s"
        );
        assert_eq!(task_used(usage(1, 0, 0)), "1 tool use");
        assert_eq!(task_used(usage(0, 0, 0)), "");
        let explore = task("local_agent", Some("Explore"), "Find the tests");
        let with = |summary: Option<&str>| {
            let progress = crate::app::TaskProgress {
                usage: usage(2, 900, 3_000),
                last_tool: Some(wire::ToolName::Grep),
                summary: summary.map(str::to_owned),
            };
            let item = task_item(Running {
                task: &explore,
                place: Place::Background,
                progress: Some(&progress),
            });
            (item.used.to_string(), item.now)
        };
        assert_eq!(
            with(None),
            (
                "2 tool uses · 900 tokens · 3s".to_owned(),
                "Last used Grep".to_owned()
            )
        );
        assert_eq!(with(Some("Reading the parser")).1, "Reading the parser");
    }

    #[test]
    fn durations_read_briefly() {
        assert_eq!(duration_text(45_000), "45s");
        assert_eq!(duration_text(192_000), "3m 12s");
        assert_eq!(duration_text(3_840_000), "1h 4m");
    }

    #[test]
    fn shares_fit_a_bar() {
        assert!((fraction_of(25_000, 1_000_000) - 0.02).abs() < 1e-6);
        assert!((fraction_of(5, 0) - 1.0).abs() < 1e-6);
    }

    #[test]
    fn counts_are_short() {
        assert_eq!(short_count(950), "950");
        assert_eq!(short_count(1_000), "1k");
        assert_eq!(short_count(4_250), "4.2k");
        assert_eq!(short_count(45_300), "45k");
        assert_eq!(short_count(1_000_000), "1M");
        assert_eq!(short_count(1_250_000), "1.2M");
    }
}
