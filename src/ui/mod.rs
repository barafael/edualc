//! The window's side: its models, kept in line with the app's state (`render_*`), and its
//! callbacks, which become the app's actions (`wire`).

use std::collections::{BTreeSet, HashSet, VecDeque};
use std::path::Path;

use slint::{
    ComponentHandle, Model, ModelRc, Rgba8Pixel, SharedPixelBuffer, SharedString, VecModel,
};
use strum::IntoEnumIterator;

use crate::app::{
    Answer, Ask, AskKind, LimitUse, Mode, PermissionMode, Sandboxing, SideChat, SideView, Status,
    UsagePanel, Watch,
};
use crate::chat::{self, Body, Change, Chat};
use crate::decode::Picture;
use crate::desktop::Opener;
use crate::external::wire::{
    self, ContextKind, LimitKind, RequestId, SessionId, ToolKind, ToolName,
};
use crate::generated::{
    AppWindow, BlockKind, CommandItem, Composer, ComposerSize, DiffKind, DiffLine, Entry,
    EntryKind, GroupRole, ImageChip, LiveItem, LiveState, MdBlock, MenuItem, PermissionKind,
    PlanSection, ProjectItem, Prompt, PromptKind, Question, QuestionOption, Session, SessionItem,
    Sessions, Side, SideExchange, SideState, StatRow, StatSection, Theme, ToolState, Transcript,
    Usage,
};
use crate::grouping::{self, Grouping, RunState};
use crate::markdown::{self, Block};
use crate::sandbox::Enforcement;
use crate::sessions::{self, LiveSession, LiveStatus, ProjectInfo, SessionInfo, Worktree};
use crate::tools;

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
        window.global::<Transcript>().set_entries(entries.clone());
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
        }
    }

    /// The base text size, in logical pixels; everything else is sized from it.
    pub(crate) fn set_text_size(&self, pixels: f32) {
        self.window.global::<Theme>().set_base_size(pixels);
    }

    /// The images going with the next message, as chips in the composer: their thumbnails are
    /// the helper's pixels, decoded nowhere here.
    pub(crate) fn render_attached(&self, attached: &[Picture]) {
        let chips: Vec<ImageChip> = attached
            .iter()
            .filter_map(|picture| {
                let thumbnail = &picture.thumbnail;
                let mut pixels =
                    SharedPixelBuffer::<Rgba8Pixel>::new(thumbnail.width_px, thumbnail.height_px);
                let bytes = pixels.make_mut_bytes();
                // Checked by the daemon; a mismatch here would panic in the copy.
                if bytes.len() != thumbnail.rgba.len() {
                    return None;
                }
                bytes.copy_from_slice(&thumbnail.rgba);
                Some(ImageChip {
                    thumbnail: slint::Image::from_rgba8(pixels),
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
        match &entry.body {
            Body::User(text) => {
                row.kind = EntryKind::User;
                row.body = markdown::plain(text);
            }
            Body::Assistant(text) => {
                row.kind = EntryKind::Assistant;
                row.blocks = self.sync_blocks(index, text);
            }
            Body::Thinking { text, expanded } => {
                row.kind = EntryKind::Thinking;
                if *expanded {
                    row.body = markdown::plain(text);
                }
                row.expanded = *expanded;
            }
            // Notes come from slash commands, which use Markdown (`sonnet`).
            Body::Info(text) => {
                row.kind = EntryKind::Info;
                row.body = markdown::styled(text);
            }
            Body::Error(text) => {
                row.kind = EntryKind::Error;
                row.body = markdown::plain(text);
            }
            Body::Tool { tool, expanded } => {
                // What only an open card shows is made only for one.
                let depth = if *expanded && !row.hidden {
                    tools::Depth::Full
                } else {
                    tools::Depth::Headline
                };
                let view = tools::describe(&tool.name, &tool.input, cwd, depth);
                row.kind = EntryKind::Tool;
                row.expanded = *expanded;
                row.tool_title = view.title.into();
                row.tool_summary = view.summary.into();
                row.tool_state = tool_state(tool.state);
                row.has_detail = !view.detail.is_empty();
                row.detail = markdown::plain(&view.detail);
                if !view.markdown.is_empty() {
                    row.blocks = self.sync_blocks(index, &view.markdown);
                }
                if depth == tools::Depth::Full {
                    let (output, note) = clip(&tool.output);
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
        }
        row
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

    pub(crate) fn render_status(
        &self,
        status: &Status,
        waiting: bool,
        untrusted: bool,
        cwd: &Path,
        watch: Option<&Watch>,
        worktree: Option<&Worktree>,
    ) {
        let session = self.window.global::<Session>();
        session.set_cwd(short_path(cwd, 36).into());
        let project = sessions::folder_name(cwd);
        session.set_project(project.into());
        session.set_title(status.title.as_deref().unwrap_or("New session").into());
        session.set_busy(status.turns > 0);
        session.set_needs_trust(untrusted);
        let (label, detail) = sandbox_text(status.sandboxing);
        session.set_sandbox_label(label.into());
        session.set_sandbox_detail(detail.into());
        session.set_sandbox_on(status.sandbox_choice.on);
        session.set_sandbox_desktop(status.sandbox_choice.desktop);
        // Not yet known isn't warned about: every start would flash the warning.
        session.set_sandboxed(matches!(
            status.sandboxing,
            Sandboxing::Landlock(Enforcement::Full | Enforcement::Partial)
                | Sandboxing::ClaudeCodes
                | Sandboxing::Pending
        ));
        let doing = if waiting {
            "Waiting for you".to_owned()
        } else {
            status.doing.to_string()
        };
        session.set_status(doing.into());
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
        session.set_plan(account.subscription_type.as_str().into());
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
            AskKind::Questions { asked, picked, .. } => {
                prompt.set_kind(PromptKind::Question);
                prompt.set_title("Claude has a question".into());
                if fresh {
                    self.show_questions(&asked.questions, picked);
                } else {
                    self.update_picked(picked);
                }
            }
            AskKind::Plan { parts, editing, .. } => {
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
                // Only a rule the window can say in full is offered.
                let always = suggestions
                    .first()
                    .and_then(|offered| offered.rule.as_ref())
                    .map(always_label)
                    .unwrap_or_default();
                prompt.set_always_label(always.into());
            }
        }
        self.prompt = Some(ask.request.clone());
    }

    fn show_questions(&mut self, questions: &[wire::Question], picked: &[Vec<bool>]) {
        let mut models = Vec::new();
        let rows: Vec<Question> = questions
            .iter()
            .zip(picked)
            .map(|(question, picks)| {
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

    fn update_picked(&self, picked: &[Vec<bool>]) {
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
    }

    pub(crate) fn render_sessions(
        &self,
        list: &[&SessionInfo],
        active: Option<&SessionId>,
        live: &[LiveSession],
    ) {
        let items: Vec<SessionItem> = list
            .iter()
            .map(|session| SessionItem {
                id: session.id.as_str().into(),
                title: session.title.as_str().into(),
                age: sessions::age(session.modified).into(),
                active: Some(&session.id) == active,
                running: live.iter().any(|running| running.id == session.id),
                worktree: session
                    .worktree
                    .as_ref()
                    .map_or_else(SharedString::new, |worktree| worktree.name.as_str().into()),
            })
            .collect();
        self.window
            .global::<Sessions>()
            .set_items(ModelRc::new(VecModel::from(items)));
    }

    pub(crate) fn render_live(&self, live: &[LiveSession], watched: Option<&SessionId>) {
        let items: Vec<LiveItem> = live
            .iter()
            .map(|session| LiveItem {
                id: session.id.as_str().into(),
                name: session.name.as_str().into(),
                project: shown_project(&session.name, &session.cwd).into(),
                state: match session.status {
                    LiveStatus::Idle => LiveState::Idle,
                    LiveStatus::Busy => LiveState::Busy,
                    LiveStatus::Waiting => LiveState::Waiting,
                    LiveStatus::Other => LiveState::Other,
                },
                watched: Some(&session.id) == watched,
                worktree: session
                    .worktree
                    .as_ref()
                    .map_or_else(SharedString::new, |worktree| worktree.name.as_str().into()),
            })
            .collect();
        let waiting = live
            .iter()
            .filter(|session| session.status == LiveStatus::Waiting)
            .count();
        let sessions = self.window.global::<Sessions>();
        sessions.set_live(ModelRc::new(VecModel::from(items)));
        sessions.set_needs_you(index_for_slint(waiting));
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
        Block::Table(text) => MdBlock {
            kind: BlockKind::Table,
            body: markdown::plain(text),
            raw: text.as_str().into(),
            ..MdBlock::default()
        },
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
            "Claude Code and everything it starts may change only this project, its git \
             folder, caches and temporary files; it reads no private files, reaches no desktop \
             service or socket but the ssh agent, and can't gain privileges (sudo). The network \
             is open. A command failing with \"Permission denied\" reached past it: \
             sandbox_read and sandbox_write in ~/.config/edualc/settings.json let more in.",
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
            "Sandbox",
            "How far this kernel holds the sandbox is known once Claude Code runs.",
        ),
    }
}

fn home_relative(path: &Path) -> String {
    let home = dirs::home_dir();
    match home
        .as_deref()
        .and_then(|home| path.strip_prefix(home).ok())
    {
        Some(rest) if rest.as_os_str().is_empty() => "~".into(),
        Some(rest) => format!("~/{}", rest.display()),
        None => path.display().to_string(),
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

/// `path` relative to home, its middle left out if it is longer than `max_chars`: the start
/// says where it is, the end which it is.
fn short_path(path: &Path, max_chars: usize) -> String {
    let full = home_relative(path);
    if full.chars().count() <= max_chars {
        return full;
    }
    let parts: Vec<&str> = full.split('/').collect();
    match (
        parts.first(),
        parts.len().checked_sub(2).and_then(|at| parts.get(at..)),
    ) {
        (Some(first), Some(last)) if parts.len() > 3 => format!("{first}/…/{}", last.join("/")),
        _ => full,
    }
}

/// A running session's folder, unless its name already says it (names are made from the
/// folder: `edualc-3f` runs in `edualc`).
fn shown_project(name: &str, cwd: &Path) -> String {
    let folder = cwd.file_name().map_or_else(
        || home_relative(cwd),
        |folder| folder.to_string_lossy().into_owned(),
    );
    let plain = |text: &str| -> String {
        text.chars()
            .filter(char::is_ascii_alphanumeric)
            .map(|character| character.to_ascii_lowercase())
            .collect()
    };
    if plain(name).starts_with(&plain(&folder)) {
        String::new()
    } else {
        folder
    }
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
    let plan = &status.account.subscription_type;
    let note = if plan.is_empty() {
        String::new()
    } else {
        format!("{plan} plan")
    };
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

fn strings(items: &[String]) -> ModelRc<SharedString> {
    let items: Vec<SharedString> = items.iter().map(|item| item.as_str().into()).collect();
    ModelRc::new(VecModel::from(items))
}

/// Slint counts in `i32`; no list here comes near its end.
fn index_for_slint(index: usize) -> i32 {
    i32::try_from(index).unwrap_or(i32::MAX)
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

    #[test]
    fn long_paths_lose_their_middle() {
        assert_eq!(
            short_path(Path::new("/var/tmp/claude-1000/scratchpad/review"), 24),
            "/…/scratchpad/review"
        );
        assert_eq!(short_path(Path::new("/srv/app"), 24), "/srv/app");
    }

    #[test]
    fn a_folder_the_name_repeats_is_left_out() {
        let cwd = Path::new("/home/u/code/edu_alc");
        assert_eq!(shown_project("edu-alc-3f", cwd), "");
        assert_eq!(shown_project("Framework 13 sensors", cwd), "edu_alc");
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
