//! The window's side: its models, kept in line with the app's state (`render_*`), and its
//! callbacks, which become the app's actions (`wire`).

use std::collections::{BTreeSet, HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};

use slint::{
    ComponentHandle, Model, ModelExt, ModelRc, Rgba8Pixel, SharedPixelBuffer, SharedString,
    VecModel,
};
use strum::IntoEnumIterator;

use crate::app::{
    Answer, Ask, AskKind, Doing, LimitUse, Mode, PermissionMode, Place, Running, Sandboxing,
    SideChat, SideView, Status, Tasks, UsagePanel, Watch,
};
use crate::chat::{self, Body, Change, Chat, ImageKey};
use crate::cjk::CjkLanguage;
use crate::decode::{Picture, Pixels};
use crate::desktop::Opener;
use crate::external::wire::{self, ContextKind, LimitKind, RequestId, ToolKind, ToolName};
use crate::generated::{
    Activity, AppWindow, BlockKind, CommandItem, Composer, ComposerSize, DiffKind, DiffLine, Entry,
    EntryKind, GroupRole, HanLanguage, ImageChip, ImageState, LiveState, Look, MdBlock, MenuItem,
    PeekStep, PermissionKind, PlanSection, Preferences, ProjectItem, Prompt, PromptKind, Question,
    QuestionOption, SendKey, Session, SessionRow, SessionRowKind, Sessions, ShownImage, Side,
    SideExchange, SideState, StatRow, StatSection, StepKind, TableRow, TaskItem, TaskKind, Theme,
    ToolState, Transcript, Usage,
};
use crate::grouping::{self, Grouping, RunState};
use crate::ipc::Runs;
use crate::markdown::{self, Block};
use crate::sandbox::Enforcement;
use crate::sessions::{self, LiveStatus, ProjectInfo, Worktree};
use crate::settings;
use crate::sidebar;
use crate::tools::{self, shortened_path};

mod callbacks;

pub(crate) use callbacks::wire;

/// Long outputs are cut to this many lines in the window; copying takes all of it.
const OUTPUT_LINES: usize = 80;
const OUTPUT_CHARS: usize = 20_000;

pub(crate) struct View {
    window: AppWindow,
    entries: ModelRc<Entry>,
    commands: ModelRc<CommandItem>,
    /// Each entry's Markdown blocks: the model the window shows and what it holds, so that
    /// streamed text changes only the blocks that grew (the others keep their selection).
    blocks: Vec<Option<Blocks>>,
    /// The options of each question shown, changed in place as they are picked: the row
    /// stays, and with it what was typed into its "Something else".
    options: Vec<ModelRc<QuestionOption>>,
    /// How each entry was shown last with respect to runs of tool calls.
    groups: Vec<Grouping>,
    /// The plan shown for review, changed in place as comments are written.
    plan: Option<ModelRc<PlanSection>>,
    /// The request the bar shows.
    prompt: Option<RequestId>,
    /// What the menus hold, set again only when it changes (setting closes an open menu).
    models: Vec<(String, String)>,
    modes: Vec<(String, String)>,
    /// The conversation's images, as far as they are decoded; one not here is still decoded.
    images: HashMap<ImageKey, ShownImage>,
    /// The language text with Han characters alone is set in (see `cjk`).
    han: CjkLanguage,
}

struct Blocks {
    model: ModelRc<MdBlock>,
    shown: Vec<Block>,
}

/// The `VecModel` behind a model made in [`View::new`].
fn vec_model<T: Clone + 'static>(model: &ModelRc<T>) -> &VecModel<T> {
    model
        .as_any()
        .downcast_ref()
        .expect("the view's models are VecModels, made in View::new")
}

impl View {
    pub(crate) fn new(
        window: AppWindow,
        openers: &[Opener],
        fonts: crate::appearance::Fonts,
    ) -> Self {
        if let Some(family) = fonts.sans {
            window.global::<Theme>().set_sans(family.into());
        }
        if let Some(family) = fonts.mono {
            window.global::<Theme>().set_mono(family.into());
        }
        let entries = ModelRc::new(VecModel::<Entry>::default());
        // The list gets only the rows it shows. Slint's list estimates the rows it hasn't built
        // from those it has, so a row that takes no room misleads it: hidden rows past the end
        // put the end further down than it is, and following the end never settles; a view of
        // hidden rows alone measures nothing and is built again on every pass ("long
        // callback/instantiation chain detected").
        window
            .global::<Transcript>()
            .set_entries(ModelRc::new(entries.clone().filter(|entry| !entry.hidden)));
        let commands = ModelRc::new(VecModel::<CommandItem>::default());
        window.global::<Composer>().set_commands(commands.clone());
        window
            .global::<Session>()
            .set_can_go_to_window(std::env::var_os("NIRI_SOCKET").is_some());
        let openers: Vec<String> = std::iter::once("This window".to_owned())
            .chain(openers.iter().map(|opener| opener.label()))
            .collect();
        window.global::<Session>().set_openers(strings(&openers));
        Self {
            window,
            entries,
            commands,
            blocks: Vec::new(),
            options: Vec::new(),
            groups: Vec::new(),
            plan: None,
            prompt: None,
            models: Vec::new(),
            modes: Vec::new(),
            images: HashMap::new(),
            han: CjkLanguage::Japanese,
        }
    }

    /// The language text with Han characters alone is set in, from the next row drawn on, and
    /// the setting's choice on the settings page.
    pub(crate) fn set_han_language(&mut self, han: CjkLanguage) {
        self.han = han;
        self.window
            .global::<Preferences>()
            .set_han_language(match han {
                CjkLanguage::Japanese => HanLanguage::Japanese,
                CjkLanguage::SimplifiedChinese => HanLanguage::SimplifiedChinese,
                CjkLanguage::TraditionalChinese => HanLanguage::TraditionalChinese,
                CjkLanguage::Korean => HanLanguage::Korean,
            });
    }

    /// Image `key`'s pixels, or why it can't be shown.
    pub(crate) fn show_image(&mut self, key: ImageKey, shown: Result<&Pixels, String>) {
        let shown = match shown.map(|pixels| (slint_image(pixels), pixels)) {
            Ok((Some(image), pixels)) => ShownImage {
                state: ImageState::Shown,
                image,
                width_px: i32::try_from(pixels.width_px).unwrap_or(i32::MAX),
                height_px: i32::try_from(pixels.height_px).unwrap_or(i32::MAX),
                note: SharedString::new(),
            },
            Ok((None, _)) => failed_image("its pixels don't add up".to_owned()),
            Err(why) => failed_image(why),
        };
        self.images.insert(key, shown);
    }

    /// Keeps only the images `shown` holds: the conversation was replaced.
    pub(crate) fn keep_images(&mut self, shown: &HashSet<ImageKey>) {
        self.images.retain(|key, _| shown.contains(key));
    }

    fn shown_images(&self, keys: &[ImageKey]) -> ModelRc<ShownImage> {
        let shown: Vec<ShownImage> = keys
            .iter()
            .map(|key| self.images.get(key).cloned().unwrap_or_default())
            .collect();
        ModelRc::new(VecModel::from(shown))
    }

    /// The base text size, in logical pixels; everything else is sized from it.
    pub(crate) fn set_text_size(&self, pixels: f32) {
        self.window.global::<Theme>().set_base_size(pixels);
    }

    pub(crate) fn set_send_key(&self, key: settings::SendKey) {
        self.window.global::<Preferences>().set_send_key(match key {
            settings::SendKey::Enter => SendKey::Enter,
            settings::SendKey::CtrlEnter => SendKey::CtrlEnter,
        });
    }

    /// The folders the sandbox lets Claude read and change, beyond its own, as the settings page
    /// lists them.
    pub(crate) fn set_sandbox_folders(&self, settings: &settings::Settings) {
        let shown = |folders: &[PathBuf]| {
            let shown: Vec<SharedString> = folders
                .iter()
                .map(|folder| folder.to_string_lossy().as_ref().into())
                .collect();
            ModelRc::new(VecModel::from(shown))
        };
        let preferences = self.window.global::<Preferences>();
        preferences.set_sandbox_read(shown(&settings.sandbox_read));
        preferences.set_sandbox_write(shown(&settings.sandbox_write));
    }

    /// Whether a sandboxed session's messages to other sessions go without asking.
    pub(crate) fn set_messages_unasked(&self, unasked: bool) {
        self.window
            .global::<Preferences>()
            .set_messages_unasked(unasked);
    }

    pub(crate) fn set_look(&self, look: settings::Look) {
        let theme = self.window.global::<Theme>();
        theme.set_look(match look {
            settings::Look::Gruvbox => Look::Gruvbox,
            settings::Look::Plain => Look::Plain,
        });
    }

    /// The images going with the next message, as chips in the composer: their thumbnails are
    /// the helper's pixels, decoded nowhere here.
    pub(crate) fn render_attached(&self, attached: &[Picture]) {
        let chips: Vec<ImageChip> = attached
            .iter()
            .filter_map(|picture| {
                Some(ImageChip {
                    thumbnail: slint_image(&picture.thumbnail)?,
                    caption: format!("{}×{}", picture.width_px, picture.height_px).into(),
                })
            })
            .collect();
        self.window
            .global::<Composer>()
            .set_images(ModelRc::new(VecModel::from(chips)));
    }

    /// The composer's kept height, if it was dragged to one; fitted to its text if not.
    pub(crate) fn set_composer_height(&self, height_px: Option<u16>) {
        let composer = self.window.global::<Composer>();
        match height_px {
            Some(height_px) => {
                composer.set_chosen_height(f32::from(height_px));
                composer.set_size(ComposerSize::Chosen);
            }
            None => composer.set_size(ComposerSize::Fit),
        }
    }

    pub(crate) fn copy(&self, text: &str) {
        self.window.invoke_copy_text(text.into());
    }

    pub(crate) fn set_draft(&self, text: &str) {
        self.window.global::<Composer>().set_draft(text.into());
    }

    pub(crate) fn draft(&self) -> String {
        self.window.global::<Composer>().get_draft().into()
    }

    /// Whether the composer shows the draft as a shell command.
    pub(crate) fn set_shell_mode(&self, shell: bool) {
        self.window.global::<Composer>().set_shell(shell);
    }

    /// What the command in the draft takes, hinted after it; empty for none.
    pub(crate) fn set_argument_hint(&self, hint: &str) {
        self.window
            .global::<Composer>()
            .set_argument_hint(hint.into());
    }

    /// Adds `text` to the end of the draft, after what is there.
    pub(crate) fn append_draft(&self, text: &str) {
        let composer = self.window.global::<Composer>();
        let draft = composer.get_draft();
        let joined = if draft.trim().is_empty() {
            text.to_owned()
        } else {
            format!("{}\n\n{text}", draft.trim_end())
        };
        composer.set_draft(joined.into());
    }

    pub(crate) fn focus_side(&self) {
        let side = self.window.global::<Side>();
        side.set_focus_wanted(true);
        side.set_focus_serial(side.get_focus_serial().wrapping_add(1));
    }

    pub(crate) fn render_side(&self, chat: &SideChat, can_ask: bool, can_branch: bool) {
        let side = self.window.global::<Side>();
        side.set_view(match chat.view {
            SideView::Hidden => crate::generated::SideView::Hidden,
            SideView::Shown => crate::generated::SideView::Shown,
            SideView::Folded { .. } => crate::generated::SideView::Folded,
        });
        let exchanges: Vec<SideExchange> = chat
            .exchanges
            .iter()
            .map(|exchange| {
                let (state, answer, error) = match &exchange.answer {
                    Answer::Pending => (SideState::Pending, ModelRc::default(), ""),
                    Answer::Retrying => (SideState::Retrying, ModelRc::default(), ""),
                    Answer::Answered(text) => {
                        let blocks: Vec<MdBlock> =
                            markdown::blocks(text).iter().map(md_block).collect();
                        (
                            SideState::Answered,
                            ModelRc::new(VecModel::from(blocks)),
                            "",
                        )
                    }
                    Answer::Failed(error) => {
                        (SideState::Failed, ModelRc::default(), error.as_str())
                    }
                };
                SideExchange {
                    question: exchange.question.as_str().into(),
                    state,
                    answer,
                    error: error.into(),
                }
            })
            .collect();
        side.set_exchanges(ModelRc::new(VecModel::from(exchanges)));
        side.set_unseen(chat.view == SideView::Folded { unseen: true });
        side.set_can_ask(can_ask);
        side.set_can_branch(can_branch);
        side.set_has_answers(chat.has_answers());
        side.set_pending(chat.pending());
    }

    pub(crate) fn focus_composer(&self) {
        let composer = self.window.global::<Composer>();
        composer.set_focus_serial(composer.get_focus_serial().wrapping_add(1));
    }

    pub(crate) fn scroll_to_end(&self) {
        let transcript = self.window.global::<Transcript>();
        transcript.set_scroll_to_end_serial(transcript.get_scroll_to_end_serial().wrapping_add(1));
    }

    /// Brings the transcript in line with `chat`, touching only what `changes` name.
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
        let entries = self.entries.clone();
        let rows = vec_model(&entries);
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
        let mut grown = BTreeSet::new();
        let mut reset = false;
        for change in changes {
            match change {
                Change::Reset => reset = true,
                Change::Pushed(index) | Change::Updated(index) => {
                    rebuilt.insert(index);
                }
                Change::Grew(index) => {
                    grown.insert(index);
                }
            }
        }
        if reset {
            self.blocks.clear();
            let all: Vec<Entry> = chat
                .entries
                .iter()
                .enumerate()
                .map(|(index, entry)| self.row(index, entry, cwd))
                .collect();
            rows.set_vec(all);
            return;
        }
        // New entries, in order; then the changed ones and those their run changed for.
        while let Some(entry) = chat.entries.get(rows.row_count()) {
            let at = rows.row_count();
            let row = self.row(at, entry, cwd);
            rows.push(row);
            rebuilt.remove(&at);
            regrouped.remove(&at);
            grown.remove(&at);
        }
        for index in rebuilt.into_iter().chain(regrouped) {
            if let Some(entry) = chat.entries.get(index)
                && index < rows.row_count()
            {
                let row = self.row(index, entry, cwd);
                rows.set_row_data(index, row);
                grown.remove(&index);
            }
        }
        for index in grown {
            let Some(entry) = chat.entries.get(index) else {
                continue;
            };
            match &entry.body {
                // The blocks model changes in place; the row itself stays.
                Body::Assistant(text) => {
                    self.sync_blocks(index, text);
                }
                // Folded thinking shows none of its text.
                Body::Thinking {
                    expanded: false, ..
                } => {}
                Body::User(_)
                | Body::Thinking { .. }
                | Body::Tool { .. }
                | Body::Shell { .. }
                | Body::Image(_)
                | Body::Info(_)
                | Body::Error(_) => {
                    let row = self.row(index, entry, cwd);
                    rows.set_row_data(index, row);
                }
            }
        }
    }

    fn row(&mut self, index: usize, entry: &chat::Entry, cwd: &Path) -> Entry {
        let mut row = Entry {
            index: i32::try_from(index).unwrap_or(i32::MAX),
            nested: entry.nested,
            ..Entry::default()
        };
        match self.groups.get(index) {
            Some(Grouping::Head(line)) => {
                row.group = GroupRole::Head;
                row.group_summary = line.summary.as_str().into();
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
        // A message with CJK characters is set in the CJK family of its language (see `cjk`).
        let font =
            |text: &str| CjkLanguage::of_text(text, self.han).map_or("", CjkLanguage::family);
        match &entry.body {
            Body::User(text) => {
                row.kind = EntryKind::User;
                row.body = markdown::plain(text);
                row.font = font(text).into();
            }
            Body::Assistant(text) => {
                row.kind = EntryKind::Assistant;
                row.font = font(text).into();
                row.blocks = self.sync_blocks(index, text);
            }
            Body::Thinking { text, expanded } => {
                row.kind = EntryKind::Thinking;
                if *expanded {
                    row.body = markdown::plain(text);
                }
                row.expanded = *expanded;
                row.font = font(text).into();
            }
            // Notes come from slash commands, which use Markdown (`sonnet`).
            Body::Info(text) => {
                row.kind = EntryKind::Info;
                row.body = markdown::styled(text);
                row.font = font(text).into();
            }
            Body::Error(text) => {
                row.kind = EntryKind::Error;
                row.body = markdown::plain(text);
                row.font = font(text).into();
            }
            Body::Tool { tool, expanded } => {
                let depth = card_depth(*expanded, row.hidden);
                let view = tools::describe(&tool.name, &tool.input, cwd, depth);
                let state = tool_state(tool.state);
                self.card(&mut row, index, view, state, &tool.output, *expanded);
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
                self.card(&mut row, index, view, state, &shell.output, *expanded);
            }
        }
        row
    }

    /// A tool card's row, as `view` describes it; `output` only for an open card.
    fn card(
        &mut self,
        row: &mut Entry,
        index: usize,
        view: tools::ToolView,
        state: ToolState,
        output: &str,
        expanded: bool,
    ) {
        row.kind = EntryKind::Tool;
        row.expanded = expanded;
        row.tool_title = view.title.into();
        row.tool_summary = view.summary.into();
        row.tool_state = state;
        row.has_detail = !view.detail.is_empty();
        row.detail = markdown::plain(&view.detail);
        if !view.markdown.is_empty() {
            row.blocks = self.sync_blocks(index, &view.markdown);
        }
        if card_depth(expanded, row.hidden) == tools::Depth::Full {
            let (output, note) = clip(output);
            row.has_output = !output.is_empty();
            row.output = markdown::plain(&output);
            row.output_note = note.into();
        }
        let diff: Vec<DiffLine> = view
            .diff
            .into_iter()
            .map(|(kind, text)| DiffLine {
                kind: diff_kind(kind),
                text: text.into(),
            })
            .collect();
        row.diff = ModelRc::new(VecModel::from(diff));
    }

    /// Makes entry `index`'s blocks those of `text`, setting only the rows that differ.
    fn sync_blocks(&mut self, index: usize, text: &str) -> ModelRc<MdBlock> {
        let blocks = markdown::blocks(text);
        if self.blocks.len() <= index {
            self.blocks.resize_with(index + 1, || None);
        }
        let Some(slot) = self.blocks.get_mut(index) else {
            return ModelRc::default();
        };
        let slot = slot.get_or_insert_with(|| Blocks {
            model: ModelRc::new(VecModel::<MdBlock>::default()),
            shown: Vec::new(),
        });
        let rows = vec_model(&slot.model);
        for (position, block) in blocks.iter().enumerate() {
            match slot.shown.get(position) {
                Some(shown) if shown == block => {}
                Some(_) => rows.set_row_data(position, md_block(block)),
                None => rows.push(md_block(block)),
            }
        }
        while rows.row_count() > blocks.len() {
            rows.remove(rows.row_count() - 1);
        }
        slot.shown = blocks;
        slot.model.clone()
    }

    /// What the session is doing: whether something runs that Stop stops (Claude's turn, or a
    /// `!` command, which an interrupt stops too), what the progress line shows of it, and the
    /// status bar's word for it.
    pub(crate) fn render_activity(&self, status: &Status, waiting: bool, shell_running: bool) {
        let session = self.window.global::<Session>();
        session.set_busy(status.turns > 0 || shell_running);
        session.set_turn(status.turns > 0);
        session.set_activity(match status.doing {
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
        });
        let doing = if waiting {
            "Waiting for you".to_owned()
        } else if matches!(status.doing, Doing::Nothing) && shell_running {
            "Running your command".to_owned()
        } else {
            status.doing.to_string()
        };
        session.set_status(doing.into());
    }

    /// `asking`: the user is asked to trust the folder, which would run this.
    pub(crate) fn render_status(
        &self,
        status: &Status,
        asking: Option<&[Runs]>,
        cwd: &Path,
        watch: Option<&Watch>,
        worktree: Option<&Worktree>,
    ) {
        let session = self.window.global::<Session>();
        session.set_cwd(shortened_path(cwd, 36).into());
        let project = sessions::folder_name(cwd);
        session.set_project(project.into());
        session.set_title(status.title.as_deref().unwrap_or("New session").into());
        session.set_needs_trust(asking.is_some());
        let runs: Vec<String> = asking.unwrap_or_default().iter().map(run_line).collect();
        session.set_trust_runs(strings(&runs));
        let (label, detail) = sandbox_text(status.sandboxing);
        session.set_sandbox_label(label.into());
        session.set_sandbox_detail(detail.into());
        session.set_sandbox_on(status.sandbox_choice.on);
        session.set_sandbox_desktop(status.sandbox_choice.desktop);
        session.set_sandbox_hardware(status.sandbox_choice.hardware);
        // Not yet known isn't warned about: every start would flash the warning.
        session.set_sandboxed(matches!(
            status.sandboxing,
            Sandboxing::Landlock(Enforcement::Full | Enforcement::Partial)
                | Sandboxing::ClaudeCodes
                | Sandboxing::Pending
        ));
        session.set_sandbox_pending(status.sandboxing == Sandboxing::Pending);
        let (context, fraction) = match (status.context_tokens, status.context_window) {
            (0, _) => (String::new(), 0.0),
            (used, 0) => (short_count(used), 0.0),
            (used, window) => (
                format!("{} / {}", short_count(used), short_count(window)),
                fraction_of(used, window),
            ),
        };
        session.set_context(context.into());
        session.set_context_fraction(fraction);
        let cost = if status.cost_usd > 0.0 {
            format!("${:.2}", status.cost_usd)
        } else {
            String::new()
        };
        session.set_cost(cost.into());
        // Unknown shows as nothing (less than 0), not as an empty bar.
        let used = |limit: Option<LimitUse>| limit.map_or(-1.0, |limit| limit.used);
        let resets = |limit: Option<LimitUse>| {
            limit
                .and_then(|limit| limit.resets_at)
                .map_or_else(String::new, |at| reset_text(at, false))
        };
        session.set_limit_five_hour(used(status.limits.five_hour));
        session.set_limit_five_hour_resets(resets(status.limits.five_hour).into());
        session.set_limit_seven_day(used(status.limits.seven_day));
        let account = &status.account;
        session.set_email(account.email.as_str().into());
        session.set_organization(account.organization.as_str().into());
        session.set_provider(account.api_provider.as_str().into());
        session.set_version(status.version.as_str().into());
        session.set_worktree_name(
            worktree
                .map_or("", |worktree| worktree.name.as_str())
                .into(),
        );
        session.set_worktree_branch(
            worktree
                .map_or("", |worktree| worktree.branch.as_str())
                .into(),
        );
        session.set_watching(watch.is_some());
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
            session.set_watch_text(text.into());
        }
    }

    /// The usage panel: the plan's limits, this session, the context, the last day and week.
    pub(crate) fn render_usage(&self, panel: &UsagePanel, status: &Status) {
        let usage = self.window.global::<Usage>();
        usage.set_open(panel.open);
        if !panel.open {
            return;
        }
        usage.set_loading(panel.waiting > 0);
        usage.set_note(panel.error.as_deref().unwrap_or("").into());
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
        usage.set_sections(ModelRc::new(VecModel::from(sections)));
    }

    /// What runs in the background: each task, and how much of each kind.
    /// What runs besides the turn, and whether a stopped turn left it running.
    pub(crate) fn render_tasks(&self, tasks: &Tasks, left_running: bool) {
        let session = self.window.global::<Session>();
        let kinds: Vec<wire::TaskKind> = tasks
            .running()
            .map(|running| running.task.task_type)
            .collect();
        session.set_tasks_summary(tasks_summary(&kinds).into());
        let items: Vec<TaskItem> = tasks.running().map(task_item).collect();
        session.set_tasks(ModelRc::new(VecModel::from(items)));
        session.set_tasks_left_running(left_running);
    }

    /// Opens the list of what runs.
    pub(crate) fn show_tasks(&self) {
        let session = self.window.global::<Session>();
        session.set_tasks_wanted(session.get_tasks_wanted().wrapping_add(1));
    }

    /// The subagent followed in the side panel, if one is, and whether it still runs.
    pub(crate) fn render_peek(&self, peek: Option<&crate::app::Peek>, tasks: &Tasks) {
        let global = self.window.global::<crate::generated::Peek>();
        let Some(peek) = peek else {
            global.set_open(false);
            global.set_steps(ModelRc::default());
            return;
        };
        let running = tasks
            .running()
            .any(|running| running.task.task_id == peek.task);
        global.set_open(true);
        global.set_title(peek.title.as_str().into());
        global.set_detail(peek_detail(peek.agent.as_deref()).into());
        global.set_running(running);
        let steps: Vec<PeekStep> = peek.steps.iter().map(peek_step).collect();
        global.set_steps(ModelRc::new(VecModel::from(steps)));
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
        let session = self.window.global::<Session>();
        if self.models != models {
            session.set_models(menu(&models));
            self.models = models;
        }
        if self.modes != modes {
            session.set_modes(menu(&modes));
            self.modes = modes;
        }
        session.set_model_index(index_for_slint(model_index));
        session.set_mode_index(index_for_slint(mode_index));
        session.set_mode(match &status.mode {
            Some(Mode::Known(PermissionMode::Default)) | None => PermissionKind::Default,
            Some(Mode::Known(PermissionMode::AcceptEdits)) => PermissionKind::AcceptEdits,
            Some(Mode::Known(PermissionMode::Plan)) => PermissionKind::Plan,
            Some(Mode::Known(PermissionMode::Auto)) => PermissionKind::Auto,
            Some(Mode::Known(PermissionMode::BypassPermissions)) => PermissionKind::Bypass,
            Some(Mode::Unknown(_)) => PermissionKind::Unknown,
        });
    }

    /// The bar above the composer, for the first request waiting.
    pub(crate) fn render_prompt(&mut self, asks: &VecDeque<Ask>, cwd: &Path) {
        let prompt = self.window.global::<Prompt>();
        let Some(ask) = asks.front() else {
            prompt.set_kind(PromptKind::None);
            prompt.set_reviewing(false);
            self.prompt = None;
            self.plan = None;
            return;
        };
        prompt.set_request(ask.request.to_string().into());
        prompt.set_queued(index_for_slint(asks.len() - 1));
        let fresh = self.prompt.as_ref() != Some(&ask.request);
        if fresh {
            prompt.set_serial(prompt.get_serial().wrapping_add(1));
        }
        match &ask.kind {
            AskKind::Questions {
                asked,
                picked,
                other,
                ..
            } => {
                prompt.set_kind(PromptKind::Question);
                prompt.set_title(
                    if asked.questions.len() > 1 {
                        "Claude has questions"
                    } else {
                        "Claude has a question"
                    }
                    .into(),
                );
                if fresh {
                    self.show_questions(&asked.questions, picked, other);
                } else {
                    self.update_answers(picked, other);
                }
            }
            AskKind::Plan {
                input,
                parts,
                editing,
            } => {
                prompt.set_kind(PromptKind::Plan);
                prompt.set_title(
                    "Claude's plan is ready. Comment on its parts, then approve it or keep planning."
                        .into(),
                );
                let count = parts.iter().filter(|part| !part.comment.is_empty()).count();
                prompt.set_comment_count(index_for_slint(count));
                if fresh {
                    let rows: Vec<PlanSection> = parts
                        .iter()
                        .map(|part| PlanSection {
                            block: md_block(&part.block),
                            excerpt: crate::plan::excerpt(&input.plan, part).into(),
                            comment: part.comment.as_str().into(),
                            editing: false,
                        })
                        .collect();
                    let model = ModelRc::new(VecModel::from(rows));
                    prompt.set_plan(model.clone());
                    prompt.set_reviewing(true);
                    self.plan = Some(model);
                } else if let Some(model) = &self.plan {
                    let rows = vec_model(model);
                    for (index, comment) in parts.iter().map(|part| &part.comment).enumerate() {
                        let is_editing = *editing == Some(index);
                        if let Some(mut row) = rows.row_data(index)
                            && (row.comment.as_str() != comment || row.editing != is_editing)
                        {
                            row.comment = comment.as_str().into();
                            row.editing = is_editing;
                            rows.set_row_data(index, row);
                        }
                    }
                }
            }
            AskKind::Tool {
                suggestions,
                description,
            } => {
                let (title, summary) = tool_prompt(ask, cwd);
                // The description often only repeats what the title or summary say.
                let repeats =
                    title.contains(description.as_str()) || summary.contains(description.as_str());
                let detail = if repeats { "" } else { description.as_str() };
                prompt.set_kind(PromptKind::Tool);
                prompt.set_title(title.into());
                prompt.set_summary(summary.into());
                prompt.set_detail(detail.into());
                // Only a rule the window can say in full is offered, and none for a message to
                // another session: the user allows each (see `relay`).
                let always = suggestions
                    .first()
                    .filter(|_| ask.tool != ToolName::RelaySendMessage)
                    .and_then(|offered| offered.rule.as_ref())
                    .map(always_label)
                    .unwrap_or_default();
                prompt.set_always_label(always.into());
            }
        }
        self.prompt = Some(ask.request.clone());
    }

    fn show_questions(
        &mut self,
        questions: &[wire::Question],
        picked: &[Vec<bool>],
        other: &[String],
    ) {
        let mut models = Vec::new();
        let rows: Vec<Question> = questions
            .iter()
            .zip(picked)
            .zip(other)
            .map(|((question, picks), other)| {
                let options: Vec<QuestionOption> = question
                    .options
                    .iter()
                    .zip(picks)
                    .map(|(option, &selected)| QuestionOption {
                        label: option.label.as_str().into(),
                        description: option.description.as_str().into(),
                        preview: option.preview.as_str().into(),
                        selected,
                    })
                    .collect();
                let options = ModelRc::new(VecModel::from(options));
                models.push(options.clone());
                Question {
                    question: question.question.as_str().into(),
                    header: question.header.as_str().into(),
                    answered: answered(picks, other),
                    multi: question.multi_select,
                    previews: question
                        .options
                        .iter()
                        .any(|option| !option.preview.is_empty()),
                    options,
                }
            })
            .collect();
        self.window
            .global::<Prompt>()
            .set_questions(ModelRc::new(VecModel::from(rows)));
        self.options = models;
    }

    /// The options picked anew, and which questions are answered, in the rows shown.
    fn update_answers(&self, picked: &[Vec<bool>], other: &[String]) {
        for (model, picks) in self.options.iter().zip(picked) {
            let rows = vec_model(model);
            for (index, &selected) in picks.iter().enumerate() {
                if let Some(mut option) = rows.row_data(index)
                    && option.selected != selected
                {
                    option.selected = selected;
                    rows.set_row_data(index, option);
                }
            }
        }
        let questions = self.window.global::<Prompt>().get_questions();
        for (index, (picks, other)) in picked.iter().zip(other).enumerate() {
            let now = answered(picks, other);
            if let Some(mut question) = questions.row_data(index)
                && question.answered != now
            {
                question.answered = now;
                questions.set_row_data(index, question);
            }
        }
    }

    /// The session list (see `sidebar`); `filtered` says why it may be empty.
    pub(crate) fn render_sidebar(&self, rows: &[sidebar::Row], filtered: bool) {
        let items: Vec<SessionRow> = rows.iter().map(session_row).collect();
        let empty = match (items.is_empty(), filtered) {
            (false, _) => "",
            (true, true) => "No session matches the filter.",
            (true, false) => "None yet: what you start here is listed here.",
        };
        let sessions = self.window.global::<Sessions>();
        sessions.set_empty(empty.into());
        sessions.set_rows(ModelRc::new(VecModel::from(items)));
    }

    /// The sidebar's width as dragged and kept; the default if none.
    pub(crate) fn set_sidebar_width(&self, width_px: Option<u16>) {
        self.window
            .global::<Sessions>()
            .set_chosen_width(width_px.map_or(0.0, f32::from));
    }

    pub(crate) fn render_projects(&self, projects: &[ProjectInfo]) {
        let items: Vec<ProjectItem> = projects
            .iter()
            .map(|project| ProjectItem {
                path: project.path.to_string_lossy().as_ref().into(),
                name: sessions::folder_name(&project.path).into(),
                age: sessions::age(project.modified).into(),
            })
            .collect();
        self.window
            .global::<Sessions>()
            .set_projects(ModelRc::new(VecModel::from(items)));
    }

    pub(crate) fn render_commands(&self, commands: &[&wire::CommandInfo]) {
        let items: Vec<CommandItem> = commands
            .iter()
            .map(|command| CommandItem {
                name: command.name.as_str().into(),
                hint: command.argument_hint.as_str().into(),
                description: command.description.as_str().into(),
            })
            .collect();
        vec_model(&self.commands).set_vec(items);
    }
}

fn md_block(block: &Block) -> MdBlock {
    match block {
        Block::Text(source) => MdBlock {
            kind: BlockKind::Text,
            body: markdown::styled(source),
            ..MdBlock::default()
        },
        Block::Heading(level, source) => MdBlock {
            kind: BlockKind::Heading,
            body: markdown::styled(&format!("**{source}**")),
            level: i32::from(*level),
            ..MdBlock::default()
        },
        Block::Code { lang, code } => MdBlock {
            kind: BlockKind::Code,
            body: markdown::plain(code),
            lang: lang.as_str().into(),
            raw: code.as_str().into(),
            ..MdBlock::default()
        },
        Block::Quote(source) => MdBlock {
            kind: BlockKind::Quote,
            body: markdown::styled(source),
            ..MdBlock::default()
        },
        Block::Table(table) => {
            let rows: Vec<TableRow> = table
                .rows
                .iter()
                .enumerate()
                .map(|(position, row)| {
                    let cells: Vec<slint::StyledText> = row
                        .iter()
                        .map(|cell| match position {
                            0 if !cell.trim().is_empty() => {
                                markdown::styled(&format!("**{cell}**"))
                            }
                            _ => markdown::styled(cell),
                        })
                        .collect();
                    TableRow {
                        cells: ModelRc::new(VecModel::from(cells)),
                    }
                })
                .collect();
            let column_stretch: Vec<f32> = table
                .longest_cell_chars
                .iter()
                .map(|&chars| f32::from(u16::try_from(chars).unwrap_or(u16::MAX)))
                .collect();
            MdBlock {
                kind: BlockKind::Table,
                raw: table.source.as_str().into(),
                rows: ModelRc::new(VecModel::from(rows)),
                column_stretch: ModelRc::new(VecModel::from(column_stretch)),
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
fn tool_prompt(ask: &Ask, cwd: &Path) -> (String, String) {
    let view = tools::describe(&ask.tool, &ask.input, cwd, tools::Depth::Headline);
    let field = |key: &str| tools::str_field(&ask.input, key).to_owned();
    if ask.tool == ToolName::Bash {
        // Claude can ask for a command to run outside Claude Code's own sandbox.
        let outside =
            ask.input.get("dangerouslyDisableSandbox") == Some(&serde_json::Value::Bool(true));
        let title = if outside {
            "Allow Claude to run this command outside Claude Code's sandbox?"
        } else {
            "Allow Claude to run this command?"
        };
        return (title.into(), field("command"));
    }
    if ask.tool == ToolName::RelaySendMessage {
        return (
            format!("Allow Claude to send this message to {}?", field("to")),
            field("message"),
        );
    }
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
            "This kernel has no Landlock: Claude Code runs with all your rights.",
        ),
        Sandboxing::ClaudeCodes => (
            "Claude Code's sandbox",
            "Your Claude Code settings turn on its own sandbox, for its shell commands; this \
             window's would break it, so it stays off. Claude Code's file tools, hooks and MCP \
             servers run with all your rights.",
        ),
        Sandboxing::Off => ("Not sandboxed", "Claude Code runs with all your rights."),
        Sandboxing::Pending => (
            "Sandbox: starting",
            "How far this kernel enforces the sandbox is known once Claude Code runs.",
        ),
    }
}

fn menu(items: &[(String, String)]) -> ModelRc<MenuItem> {
    let items: Vec<MenuItem> = items
        .iter()
        .map(|(title, description)| MenuItem {
            title: title.as_str().into(),
            description: description.as_str().into(),
        })
        .collect();
    ModelRc::new(VecModel::from(items))
}

/// A row of the session list as the sidebar shows it.
fn session_row(row: &sidebar::Row) -> SessionRow {
    match row {
        sidebar::Row::RunningHeading { running, waiting } => SessionRow {
            kind: SessionRowKind::Heading,
            title: format!("Running · {running}").into(),
            detail: match waiting {
                0 => SharedString::new(),
                1 => "1 needs you".into(),
                waiting => format!("{waiting} need you").into(),
            },
            ..SessionRow::default()
        },
        sidebar::Row::EarlierHeading { folder } => SessionRow {
            kind: SessionRowKind::Heading,
            title: format!("Earlier in {folder}").into(),
            ..SessionRow::default()
        },
        sidebar::Row::Running(running) => SessionRow {
            kind: SessionRowKind::Running,
            id: running.id.as_str().into(),
            title: running.title.as_str().into(),
            detail: running_detail(running).into(),
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
            age: sessions::age(earlier.modified).into(),
            selected: earlier.shown,
            worktree: earlier.worktree.as_deref().unwrap_or_default().into(),
            ..SessionRow::default()
        },
    }
}

/// A running session's second line: what it does, the name to message it by, the folder it
/// runs in when another, its worktree.
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
        Some(running.name.as_str()),
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
        value: value.into(),
        detail: detail.into(),
        fraction,
    }
}

fn section(title: &str, note: String, rows: Vec<StatRow>) -> StatSection {
    StatSection {
        title: title.into(),
        note: note.into(),
        rows: ModelRc::new(VecModel::from(rows)),
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

fn strings(items: &[String]) -> ModelRc<SharedString> {
    let items: Vec<SharedString> = items.iter().map(|item| item.as_str().into()).collect();
    ModelRc::new(VecModel::from(items))
}

/// Slint counts in `i32`; no list here comes near its end.
fn index_for_slint(index: usize) -> i32 {
    i32::try_from(index).unwrap_or(i32::MAX)
}

/// Whether a question of Claude's is answered: an option picked, or something else written.
fn answered(picks: &[bool], other: &str) -> bool {
    picks.contains(&true) || !other.trim().is_empty()
}

/// Pixels as Slint shows them; none if their bytes don't make their size (the decoder's
/// answer is checked, so this would be a fault here).
fn slint_image(pixels: &Pixels) -> Option<slint::Image> {
    let mut buffer = SharedPixelBuffer::<Rgba8Pixel>::new(pixels.width_px, pixels.height_px);
    let bytes = buffer.make_mut_bytes();
    if bytes.len() != pixels.rgba.len() {
        return None;
    }
    bytes.copy_from_slice(&pixels.rgba);
    Some(slint::Image::from_rgba8(buffer))
}

fn failed_image(why: String) -> ShownImage {
    ShownImage {
        state: ImageState::Failed,
        note: why.into(),
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
        id: task.task_id.to_string().into(),
        kind: task_kind(task.task_type),
        title: if task.description.is_empty() {
            what.into()
        } else {
            task.description.as_str().into()
        },
        detail: detail.into(),
        used: used.unwrap_or_default().into(),
        now: now.unwrap_or_default().into(),
        peekable: task.task_type == wire::TaskKind::LocalAgent,
    }
}

/// "Agent · Explore", as the chip's list names a subagent.
fn peek_detail(agent: Option<&str>) -> String {
    match agent {
        Some(agent) => format!("Agent · {agent}"),
        None => "Agent".to_owned(),
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
        assert!(!unnamed.peekable, "only a subagent has a conversation");
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
            (item.used.to_string(), item.now.to_string())
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
