//! The GPUI window as a frontend (see `present`): the presentation model kept as it was told, in
//! one view (`Shell`) that draws it, and its own state besides (the text fields, what is open,
//! sizes). `GpuiFrontend` is the presenter's frontend: each part it is told goes into the view
//! at once, through GPUI's handle on the app, and the view is drawn again.
//!
//! What is done in the window goes to the app as an [`Action`] on its channel (`Actions`); the
//! view holds nothing of the app's but what it was last told.

use std::collections::{HashMap, HashSet};

use async_channel::Sender;
use gpui::{
    AnyWindowHandle, App, AppContext as _, AsyncApp, ClipboardItem, Context, Entity, FocusHandle,
    ImageSource, InteractiveElement as _, IntoElement, KeyDownEvent, ListAlignment, ListState,
    ParentElement as _, Render, RenderImage, SharedString, Styled as _, Subscription, WeakEntity,
    Window, div, px,
};
use gpui_component::input::{InputEvent, InputState, TextareaState};

use crate::app::{Action, Event};
use crate::chat::ImageKey;
use crate::decode::Pixels;
use crate::present::model;
use crate::present::{Command, EntriesChange, Frontend, Image, Look, Seat};

mod areas;
mod assets;
mod composer;
mod header;
mod heron;
mod palette;
mod panels;
mod prompt;
mod resize;
mod sidebar;
mod transcript;
mod widgets;

pub(crate) use assets::Assets;
pub(crate) use palette::Palette;

/// A way to the app, for a control to keep: what is done goes to it as an action.
#[derive(Clone)]
pub(crate) struct Actions(Sender<Event>);

impl Actions {
    pub(crate) fn send(&self, action: Action) {
        if self.0.try_send(Event::Action(action)).is_err() {
            tracing::debug!("an action after the app stopped");
        }
    }
}

/// The conversation's images, decoded, as GPUI's handles on them (as Slint's window keeps
/// `slint::Image`s); one not here isn't decoded yet.
type Images = HashMap<ImageKey, ImageSource>;

/// What the app sets of the window itself, besides the presenter's state (`Command`).
#[derive(Debug, Clone, Default)]
pub(crate) struct Page {
    pub(crate) model_index: i32,
    pub(crate) mode_index: i32,
    pub(crate) reviewing: bool,
    /// How tall the composer is: fitted to its text, as dragged to (`composer_chosen`, in
    /// logical pixels), taller, or the whole column.
    pub(crate) composer_size: model::ComposerSize,
    pub(crate) composer_chosen: f32,
    pub(crate) sidebar_width: Option<u16>,
    pub(crate) text_size: f32,
    pub(crate) look: Look,
}

/// The text fields, each GPUI state of its own.
pub(crate) struct Fields {
    pub(crate) draft: Entity<TextareaState>,
    pub(crate) filter: Entity<InputState>,
    pub(crate) project: Entity<InputState>,
    pub(crate) side: Entity<InputState>,
    pub(crate) feedback: Entity<InputState>,
    pub(crate) overall: Entity<TextareaState>,
    pub(crate) sandbox_read: Entity<InputState>,
    pub(crate) sandbox_write: Entity<InputState>,
    /// The prompt's per-question fields (what else, notes) and per-section comments, made anew
    /// for each prompt (`prompt_serial`).
    pub(crate) others: Vec<Entity<InputState>>,
    pub(crate) notes: Vec<Entity<InputState>>,
    pub(crate) comments: Vec<Entity<TextareaState>>,
    pub(crate) prompt_serial: i32,
}

/// The whole window: what the presenter last told of each part, and the window's own state.
pub(crate) struct Shell {
    pub(crate) actions: Actions,
    pub(crate) session: model::Session,
    pub(crate) composer: model::Composer,
    pub(crate) sessions: model::Sessions,
    pub(crate) prompt: model::Prompt,
    pub(crate) peek: model::Peek,
    pub(crate) side: model::Side,
    pub(crate) preferences: model::Preferences,
    pub(crate) usage: model::Usage,
    pub(crate) entries: Vec<model::Entry>,
    pub(crate) images: Images,
    pub(crate) page: Page,
    /// The transcript's list: it measures each row as it is drawn, and follows the end while the
    /// end is in view.
    pub(crate) list: ListState,
    /// Whether the transcript's end is in view: new output is followed only then (the list,
    /// aligned to its end, stays there by itself), and a button to the end shows otherwise.
    pub(crate) at_end: bool,
    pub(crate) fields: Fields,
    pub(crate) focus: FocusHandle,
    /// The areas F6 goes round, and the question's options' focus.
    pub(crate) areas: areas::Areas,
    /// The option the arrow keys are at, in the question shown.
    pub(crate) option_at: usize,
    pub(crate) heron: heron::Heron,
    pub(crate) mark: heron::Mark,
    /// What is being dragged to a size, if anything.
    pub(crate) drag: Option<resize::Drag>,
    pub(crate) sans: SharedString,
    pub(crate) mono: SharedString,
    /// Whether the desktop asks for dark.
    pub(crate) dark: bool,
    pub(crate) sidebar_shown: bool,
    pub(crate) settings_open: bool,
    pub(crate) projects_open: bool,
    pub(crate) tasks_open: bool,
    /// The command picked in the composer's list, by the arrow keys.
    pub(crate) command_index: usize,
    /// The prompt's question shown, and the prompt its buttons are armed for (a moment after it
    /// shows, so that a key meant for the composer doesn't answer it).
    pub(crate) question_shown: usize,
    pub(crate) armed_for: Option<i32>,
    _subscriptions: Vec<Subscription>,
}

/// How long a prompt waits before its buttons take a press.
const ARM_DELAY: std::time::Duration = std::time::Duration::from_millis(700);

impl Shell {
    pub(crate) fn new(
        events: Sender<Event>,
        fonts: crate::appearance::Fonts,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let actions = Actions(events);
        let draft = cx.new(|cx| {
            TextareaState::new(window, cx)
                .auto_grow(2, 12)
                .placeholder("Message Claude, or / for commands")
        });
        let line = |placeholder: &str, window: &mut Window, cx: &mut Context<Self>| {
            let placeholder = placeholder.to_owned();
            cx.new(|cx| InputState::new(window, cx).placeholder(placeholder))
        };
        let fields = Fields {
            draft: draft.clone(),
            filter: line("Filter sessions", window, cx),
            project: line("A folder on edualcd's machine", window, cx),
            side: line("Ask on the side", window, cx),
            feedback: line("Or tell Claude what to do instead", window, cx),
            overall: cx.new(|cx| {
                TextareaState::new(window, cx)
                    .auto_grow(1, 6)
                    .placeholder("A word on the whole plan (optional)")
            }),
            sandbox_read: line("A folder Claude Code may read", window, cx),
            sandbox_write: line("A folder Claude Code may write", window, cx),
            others: Vec::new(),
            notes: Vec::new(),
            comments: Vec::new(),
            prompt_serial: -1,
        };
        let subscriptions = vec![
            cx.subscribe_in(&draft, window, Self::draft_event),
            cx.subscribe_in(&fields.filter, window, |this, state, event, _, cx| {
                if let InputEvent::Change = event {
                    let text = state.read(cx).value().to_string();
                    this.actions.send(Action::FilterSessions(text));
                }
            }),
            cx.subscribe_in(&fields.project, window, |this, state, event, _, cx| {
                if let InputEvent::PressEnter { .. } = event {
                    let text = state.read(cx).value().to_string();
                    this.actions.send(Action::OpenProject(text));
                }
            }),
            cx.subscribe_in(&fields.side, window, |this, state, event, window, cx| {
                if let InputEvent::PressEnter { .. } = event {
                    let text = state.read(cx).value().to_string();
                    if !text.trim().is_empty() {
                        this.actions.send(Action::AskSide(text));
                        state.update(cx, |state, cx| state.set_value("", window, cx));
                    }
                }
            }),
            cx.subscribe_in(
                &fields.feedback,
                window,
                |this, state, event, window, cx| {
                    if let InputEvent::PressEnter { .. } = event {
                        let text = state.read(cx).value().to_string();
                        if !text.is_empty() && this.armed() {
                            this.deny(text);
                            state.update(cx, |state, cx| state.set_value("", window, cx));
                        }
                    }
                },
            ),
            cx.observe_window_appearance(window, |this, window, cx| {
                this.dark = palette::is_dark(window.appearance());
                this.apply_theme(cx);
            }),
        ];
        let focus = cx.focus_handle();
        let shell = Self {
            actions,
            session: model::Session {
                title: "New session".to_owned(),
                limit_five_hour: -1.0,
                limit_seven_day: -1.0,
                ..model::Session::default()
            },
            composer: model::Composer::default(),
            sessions: model::Sessions::default(),
            prompt: model::Prompt::default(),
            peek: model::Peek::default(),
            side: model::Side::default(),
            preferences: model::Preferences::default(),
            usage: model::Usage::default(),
            entries: Vec::new(),
            images: Images::new(),
            page: Page {
                text_size: crate::appearance::DEFAULT_TEXT_SIZE,
                ..Page::default()
            },
            list: ListState::new(0, ListAlignment::Bottom, px(1200.0)),
            at_end: true,
            fields,
            focus,
            areas: areas::Areas::new(cx),
            option_at: 0,
            heron: heron::Heron::default(),
            mark: heron::Mark::default(),
            drag: None,
            sans: fonts.sans.unwrap_or_else(|| "Fira Sans".to_owned()).into(),
            mono: fonts.mono.unwrap_or_else(|| "Fira Code".to_owned()).into(),
            dark: palette::is_dark(window.appearance()),
            sidebar_shown: true,
            settings_open: false,
            projects_open: false,
            tasks_open: false,
            command_index: 0,
            question_shown: 0,
            armed_for: None,
            _subscriptions: subscriptions,
        };
        let this = cx.entity().downgrade();
        shell.list.set_scroll_handler(move |event, _, cx| {
            let at_end = event.visible_range.end >= event.count;
            if this
                .update(cx, |shell, cx| {
                    if shell.at_end != at_end {
                        shell.at_end = at_end;
                        cx.notify();
                    }
                })
                .is_err()
            {
                tracing::debug!("the transcript scrolled after the window closed");
            }
        });
        shell.apply_theme(cx);
        Self::start_mark(cx);
        // Closing ends the process first (it gets a moment to tidy up), then the window.
        let actions = shell.actions.clone();
        window.on_window_should_close(cx, move |_, _| {
            actions.send(Action::Quit);
            false
        });
        shell
    }

    /// The colours of the look and the desktop's light or dark, here and in gpui-component's
    /// widgets, and the fonts and text size.
    fn apply_theme(&self, cx: &mut Context<Self>) {
        let palette = self.palette();
        let mode = if self.dark {
            gpui_component::ThemeMode::Dark
        } else {
            gpui_component::ThemeMode::Light
        };
        if gpui_component::Theme::global(cx).mode != mode {
            gpui_component::Theme::change(mode, None, cx);
        }
        let theme = gpui_component::Theme::global_mut(cx);
        palette.paint(&mut theme.colors);
        theme.font_family = self.sans.clone();
        theme.mono_font_family = self.mono.clone();
        theme.font_size = px(self.page.text_size);
        theme.mono_font_size = px(self.page.text_size * 0.95);
        cx.refresh_windows();
    }

    pub(crate) fn palette(&self) -> Palette {
        Palette::of(self.page.look, self.dark)
    }

    /// The composer's text changed, or Enter was pressed in it.
    fn draft_event(
        &mut self,
        state: &Entity<TextareaState>,
        event: &InputEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let InputEvent::Change = event {
            self.command_index = 0;
            let text = state.read(cx).value().to_string();
            self.actions.send(Action::DraftEdited(text));
        }
    }

    /// The composer's keys, before the field takes them: Enter (or Ctrl+Enter) sends, the
    /// command list is gone through with the arrows and picked with Tab or run with Enter, and
    /// Ctrl+V asks for the clipboard's image too (its text is pasted as ever).
    pub(crate) fn composer_key(
        &mut self,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let keys = &event.keystroke;
        let control = keys.modifiers.control;
        let plain = !control && !keys.modifiers.shift && !keys.modifiers.alt;
        let commands = self.composer.commands.len();
        match keys.key.as_str() {
            "enter" if control => {
                if self.preferences.send_key == model::SendKey::CtrlEnter {
                    self.send_draft(window, cx);
                    cx.stop_propagation();
                }
            }
            "enter" if plain && commands > 0 => {
                let index = self.command_index.min(commands - 1);
                self.actions.send(Action::RunCommand(index));
                self.set_draft("", window, cx);
                cx.stop_propagation();
            }
            "enter" if plain && self.preferences.send_key == model::SendKey::Enter => {
                self.send_draft(window, cx);
                cx.stop_propagation();
            }
            "tab" if commands > 0 => {
                let index = self.command_index.min(commands - 1);
                self.actions.send(Action::PickCommand(index));
                cx.stop_propagation();
            }
            "up" if commands > 0 => {
                self.command_index = self.command_index.saturating_sub(1);
                cx.notify();
                cx.stop_propagation();
            }
            "down" if commands > 0 => {
                self.command_index = (self.command_index + 1).min(commands - 1);
                cx.notify();
                cx.stop_propagation();
            }
            "v" if control => self.actions.send(Action::PasteImage),
            "escape" if plain && self.stops() => {
                self.actions.send(Action::Interrupt);
                cx.stop_propagation();
            }
            _ => {}
        }
    }

    /// Esc stops the turn while Claude works, asks nothing and nothing is written, and nothing
    /// is over the conversation.
    fn stops(&self) -> bool {
        self.session.busy
            && self.prompt.kind == model::PromptKind::None
            && !self.usage.open
            && !self.settings_open
    }

    pub(crate) fn send_draft(&self, window: &mut Window, cx: &mut Context<Self>) {
        let text = self.fields.draft.read(cx).value().to_string();
        self.actions.send(Action::Send(text));
        self.set_draft("", window, cx);
    }

    fn set_draft(&self, text: &str, window: &mut Window, cx: &mut Context<Self>) {
        let text = text.to_owned();
        self.fields.draft.update(cx, |state, cx| {
            state.set_value(text, window, cx);
        });
    }

    fn focus_composer(&self, window: &mut Window, cx: &mut Context<Self>) {
        self.fields
            .draft
            .update(cx, |state, cx| state.focus(window, cx));
    }

    /// Goes to the transcript's end, and follows it from there.
    pub(crate) fn scroll_to_end(&mut self) {
        self.list.reset(self.entries.len());
        self.at_end = true;
    }

    /// Whether the prompt's buttons take a press yet.
    pub(crate) fn armed(&self) -> bool {
        self.armed_for == Some(self.prompt.serial)
    }

    /// The prompt shown is answered with a no, and `feedback` for Claude.
    pub(crate) fn deny(&self, feedback: String) {
        let request = crate::external::wire::RequestId::from(self.prompt.request.clone());
        self.actions.send(Action::Deny { request, feedback });
    }

    /// The prompt changed: a new one is armed a moment after it shows, and its fields made.
    fn prompt_shown(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let serial = self.prompt.serial;
        if self.fields.prompt_serial == serial {
            return;
        }
        self.fields.prompt_serial = serial;
        self.question_shown = 0;
        self.option_at = 0;
        self.armed_for = None;
        let questions = self.prompt.questions.len();
        self.fields.others = (0..questions)
            .map(|_| cx.new(|cx| InputState::new(window, cx).placeholder("Something else")))
            .collect();
        self.fields.notes = (0..questions)
            .map(|_| cx.new(|cx| InputState::new(window, cx).placeholder("Notes (optional)")))
            .collect();
        self.fields.comments = (0..self.prompt.plan.len())
            .map(|_| {
                cx.new(|cx| {
                    TextareaState::new(window, cx)
                        .auto_grow(1, 6)
                        .placeholder("A comment on this part")
                })
            })
            .collect();
        let mut subscriptions = Vec::new();
        for (question, state) in self.fields.others.clone().into_iter().enumerate() {
            subscriptions.push(cx.subscribe_in(
                &state,
                window,
                move |this, state, event, _, cx| {
                    if let InputEvent::Change = event {
                        let text = state.read(cx).value().to_string();
                        this.actions.send(Action::SetOther { question, text });
                    }
                },
            ));
        }
        for (question, state) in self.fields.notes.clone().into_iter().enumerate() {
            subscriptions.push(cx.subscribe_in(
                &state,
                window,
                move |this, state, event, _, cx| {
                    if let InputEvent::Change = event {
                        let text = state.read(cx).value().to_string();
                        this.actions.send(Action::SetNotes { question, text });
                    }
                },
            ));
        }
        for subscription in subscriptions {
            subscription.detach();
        }
        if self.prompt.kind != model::PromptKind::None {
            cx.spawn(async move |this, cx| {
                cx.background_executor().timer(ARM_DELAY).await;
                let armed = this.update(cx, |this, cx| {
                    if this.prompt.serial == serial {
                        this.armed_for = Some(serial);
                        cx.notify();
                    }
                });
                if armed.is_err() {
                    tracing::debug!("the window closed before the prompt was armed");
                }
            })
            .detach();
        }
    }

    fn entries_changed(&mut self, change: EntriesChange, cx: &Context<Self>) {
        match change {
            EntriesChange::Reset(rows) => {
                self.list.reset(rows.len());
                self.entries = rows;
            }
            EntriesChange::Push(row) => {
                let at = self.entries.len();
                self.entries.push(row);
                self.list.splice(at..at, 1);
            }
            EntriesChange::Set(index, row) => {
                if let Some(shown) = self.entries.get_mut(index) {
                    *shown = row;
                    self.list.splice(index..index + 1, 1);
                }
            }
        }
        self.heron_wanted(self.entries.is_empty(), cx);
    }

    fn command(&mut self, command: Command, window: &mut Window, cx: &mut Context<Self>) {
        match command {
            Command::Copy(text) => cx.write_to_clipboard(ClipboardItem::new_string(text)),
            // As the app sets it: not an edit of the user's, so not told back.
            Command::SetDraft(text) => self.set_draft(&text, window, cx),
            Command::FocusComposer => self.focus_composer(window, cx),
            Command::FocusSide => self
                .fields
                .side
                .update(cx, |state, cx| state.focus(window, cx)),
            Command::ScrollToEnd | Command::OpenAtEnd => self.scroll_to_end(),
            Command::ShowTasks => self.tasks_open = true,
            Command::ProjectsOpened => {
                self.projects_open = false;
                self.fields
                    .project
                    .update(cx, |state, cx| state.set_value("", window, cx));
            }
            Command::ModelIndex(index) => self.page.model_index = index,
            Command::ModeIndex(index) => self.page.mode_index = index,
            Command::Reviewing(reviewing) => self.page.reviewing = reviewing,
            Command::ComposerHeight(Some(height)) => {
                self.page.composer_chosen = f32::from(height);
                self.page.composer_size = model::ComposerSize::Chosen;
            }
            Command::ComposerHeight(None) => {
                self.page.composer_chosen = 0.0;
                self.page.composer_size = model::ComposerSize::Fit;
            }
            Command::SidebarWidth(width) => self.page.sidebar_width = width,
            Command::TextSize(pixels) => {
                self.page.text_size = pixels;
                self.apply_theme(cx);
            }
            Command::Look(look) => {
                self.page.look = look;
                self.apply_theme(cx);
            }
            Command::Quit => cx.quit(),
            // The window's session is its command line's to name again (`--resume`).
            Command::SessionShown(_) => {}
        }
    }
}

impl Render for Shell {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let palette = self.palette();
        let size = self.page.text_size;
        let peek = self.peek.open;
        let side = self.side.view == model::SideView::Shown;
        let watching = self.session.watching;
        let writes = !watching && !self.session.needs_trust;
        let stretched = writes
            && matches!(
                self.page.composer_size,
                model::ComposerSize::Tall | model::ComposerSize::Full
            );
        let mut column = div()
            .relative()
            .flex()
            .flex_col()
            .flex_1()
            .min_w_0()
            .h_full()
            .bg(palette.surface)
            .child(header::header(self, window, cx))
            .child(transcript::transcript(self, window, cx));
        // What is written: what Claude asks, the commands, the bars, the composer (F6's
        // "writing" area).
        let mut writing = div().flex().flex_col().flex_none();
        // The composer over the column is the area's while it is there.
        if !stretched {
            writing = writing.track_focus(&self.areas.writing);
        }
        if self.prompt.kind != model::PromptKind::None {
            writing = writing.child(prompt::prompt_bar(self, window, cx));
        }
        if !self.composer.commands.is_empty() {
            writing = writing.child(composer::command_list(self, cx));
        }
        writing = writing.children(header::bars(self, cx));
        if writes && !stretched {
            writing = writing.child(composer::composer(self, false, window, cx));
        }
        column = column.child(writing);
        if stretched {
            let header = (size * 3.1).max(46.0);
            let composer = div()
                .flex()
                .flex_col()
                .size_full()
                .track_focus(&self.areas.writing)
                .child(composer::composer(self, true, window, cx))
                .into_any_element();
            column = column.child(resize::over_column(
                self.page.composer_size,
                header,
                palette.card,
                composer,
            ));
        }
        let mut layout = div()
            .id("window")
            .key_context("Edualc")
            .track_focus(&self.focus)
            .on_key_down(cx.listener(Self::window_key))
            .on_mouse_move(cx.listener(Self::drag_moved))
            .on_mouse_up(gpui::MouseButton::Left, cx.listener(Self::drag_released))
            .relative()
            .flex()
            .flex_row()
            .size_full()
            .bg(palette.surface)
            .text_color(palette.text)
            .font_family(self.sans.clone())
            .text_size(px(size));
        match self.drag {
            Some(resize::Drag::Sidebar { .. }) => layout = layout.cursor_col_resize(),
            Some(resize::Drag::Composer { .. }) => layout = layout.cursor_row_resize(),
            None => {}
        }
        if self.sidebar_shown {
            layout = layout.child(sidebar::sidebar(self, window, cx));
        }
        layout = layout.child(column);
        if peek {
            layout = layout.child(panels::peek_panel(self));
        } else if side {
            layout = layout.child(panels::side_panel(self));
        }
        if self.usage.open {
            layout = layout.child(panels::usage_panel(self, cx));
        }
        if self.settings_open {
            layout = layout.child(panels::settings_panel(self, window, cx));
        }
        if self.prompt.kind == model::PromptKind::Plan && self.page.reviewing {
            layout = layout.child(prompt::plan_review(self, cx));
        }
        layout
    }
}

impl Shell {
    /// The shortcuts that work wherever the focus is: Ctrl+, the settings, Ctrl+B the sessions,
    /// Ctrl+N a new session, Ctrl+K the sessions' filter, the zoom, F6 and Shift+F6 from area
    /// to area, and Esc to stop.
    fn window_key(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let keys = &event.keystroke;
        if keys.key == "f6" && !keys.modifiers.control && !keys.modifiers.alt {
            self.step_area(!keys.modifiers.shift, window, cx);
            cx.stop_propagation();
            return;
        }
        if !keys.modifiers.control || keys.modifiers.alt {
            if keys.key == "escape" && self.stops() {
                self.actions.send(Action::Interrupt);
            }
            return;
        }
        match keys.key.as_str() {
            "," => self.settings_open = !self.settings_open,
            "b" => self.sidebar_shown = !self.sidebar_shown,
            "n" => self.actions.send(Action::NewSession),
            "k" => {
                self.sidebar_shown = true;
                self.fields
                    .filter
                    .update(cx, |state, cx| state.focus(window, cx));
            }
            "=" | "+" => self.actions.send(Action::Zoom(crate::settings::Zoom::In)),
            "-" => self.actions.send(Action::Zoom(crate::settings::Zoom::Out)),
            "0" => self
                .actions
                .send(Action::Zoom(crate::settings::Zoom::Reset)),
            _ => return,
        }
        cx.notify();
    }
}

/// The GPUI window, as the presenter's frontend: each part it is told goes into the view at
/// once, through GPUI's handle on the app (the app runs on GPUI's executor, between frames).
pub(crate) struct GpuiFrontend {
    cx: AsyncApp,
    window: AnyWindowHandle,
    shell: WeakEntity<Shell>,
    events: Sender<Event>,
}

impl GpuiFrontend {
    /// The frontend of the window `window`, whose root holds the view.
    pub(crate) fn new(window: AnyWindowHandle, cx: &mut App) -> Option<Self> {
        let shell = window
            .update(cx, |root, _, _| {
                root.downcast::<gpui_component::Root>().ok()
            })
            .ok()??;
        let shell = shell.read(cx).view().clone().downcast::<Shell>().ok()?;
        let events = shell.read(cx).actions.0.clone();
        Some(Self {
            cx: cx.to_async(),
            window,
            shell: shell.downgrade(),
            events,
        })
    }

    /// Changes the view, and draws it again.
    fn with(&mut self, change: impl FnOnce(&mut Shell, &mut Window, &mut Context<Shell>)) {
        let shell = self.shell.clone();
        let changed = self.cx.update_window(self.window, |_, window, cx| {
            shell.update(cx, |shell, cx| {
                change(shell, window, cx);
                cx.notify();
            })
        });
        match changed {
            Ok(Ok(())) => {}
            Ok(Err(error)) | Err(error) => tracing::debug!(%error, "the window is gone"),
        }
    }
}

impl Frontend for GpuiFrontend {
    fn session(&mut self, _old: Option<&model::Session>, session: &model::Session) {
        let session = session.clone();
        self.with(|shell, _, _| shell.session = session);
    }

    fn composer(&mut self, _old: Option<&model::Composer>, composer: &model::Composer) {
        let composer = composer.clone();
        self.with(|shell, _, _| {
            if composer.commands.len() != shell.composer.commands.len() {
                shell.command_index = 0;
            }
            shell.composer = composer;
        });
    }

    fn sessions(&mut self, _old: Option<&model::Sessions>, sessions: &model::Sessions) {
        let sessions = sessions.clone();
        self.with(|shell, _, _| shell.sessions = sessions);
    }

    fn entries(&mut self, change: EntriesChange) {
        self.with(|shell, _, cx| shell.entries_changed(change, cx));
    }

    fn prompt(&mut self, _old: Option<&model::Prompt>, prompt: &model::Prompt) {
        let prompt = prompt.clone();
        self.with(|shell, window, cx| {
            shell.prompt = prompt;
            shell.prompt_shown(window, cx);
        });
    }

    fn peek(&mut self, _old: Option<&model::Peek>, peek: &model::Peek) {
        let peek = peek.clone();
        self.with(|shell, _, _| shell.peek = peek);
    }

    fn side(&mut self, _old: Option<&model::Side>, side: &model::Side) {
        let side = side.clone();
        self.with(|shell, _, _| shell.side = side);
    }

    fn preferences(&mut self, _old: Option<&model::Preferences>, preferences: &model::Preferences) {
        let preferences = preferences.clone();
        self.with(|shell, _, _| shell.preferences = preferences);
    }

    fn usage(&mut self, _old: Option<&model::Usage>, usage: &model::Usage) {
        let usage = usage.clone();
        self.with(|shell, _, _| shell.usage = usage);
    }

    fn show_image(&mut self, key: ImageKey, shown: Result<&Pixels, String>) {
        if let Ok(Some(image)) = shown.map(render_image) {
            self.with(|shell, _, _| {
                shell.images.insert(key, image);
            });
        }
    }

    fn keep_images(&mut self, shown: &HashSet<ImageKey>) {
        let shown = shown.clone();
        self.with(|shell, _, _| shell.images.retain(|key, _| shown.contains(key)));
    }

    fn command(&mut self, command: Command) {
        self.with(|shell, window, cx| shell.command(command, window, cx));
    }

    fn seat(&mut self, seat: Seat) {
        crate::seat::act(seat, &self.events);
    }

    fn draft(&self) -> String {
        let shell = self.shell.clone();
        self.cx.update(|cx| {
            shell
                .upgrade()
                .map(|shell| shell.read(cx).fields.draft.read(cx).value().to_string())
                .unwrap_or_default()
        })
    }
}

/// An image of the presentation model, as GPUI draws it; none for one not decoded (yet).
pub(crate) fn image_of(image: &Image, images: &Images) -> Option<ImageSource> {
    match image {
        Image::None => None,
        Image::Key(key) => images.get(key).cloned(),
        Image::Pixels(pixels) => render_image(pixels),
    }
}

/// Pixels as GPUI draws them (BGRA), in GPUI's handle; none for pixels that don't add up.
fn render_image(pixels: &Pixels) -> Option<ImageSource> {
    let mut bgra = pixels.rgba.clone();
    let (each, _) = bgra.as_chunks_mut::<4>();
    for pixel in each {
        pixel.swap(0, 2);
    }
    let buffer = image::RgbaImage::from_raw(pixels.width_px, pixels.height_px, bgra)?;
    let frame = image::Frame::new(buffer);
    Some(ImageSource::Render(
        RenderImage::new(smallvec::smallvec![frame]).into(),
    ))
}
