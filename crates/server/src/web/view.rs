//! The page's state: what the presenter last told of each part of the window, in signals the
//! components read, and the page's own (focus, the draft, sizes), which the app sets through
//! commands. `PageFrontend` is the presenter's frontend that writes them.

use std::collections::{HashMap, HashSet};

use dioxus::prelude::*;
use edualc_core::app::{Action, Event};
use edualc_core::chat::ImageKey;
use edualc_core::decode::Pixels;
use edualc_core::present::{Command, EntriesChange, Frontend, Image, Look, model};

/// What the app sets of the page itself, besides the presenter's state.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Page {
    /// Bumped to focus the composer, the side panel's field.
    pub focus_composer: u32,
    pub focus_side: u32,
    /// Bumped to go to the transcript's end.
    pub scroll_to_end: u32,
    /// Bumped as a session's transcript is loaded: shown at its end.
    pub opened: u32,
    /// Bumped to open the list of what runs.
    pub show_tasks: u32,
    /// Bumped as the folder typed in the list of folders opened.
    pub projects_opened: u32,
    pub model_index: i32,
    pub mode_index: i32,
    pub reviewing: bool,
    pub composer_height: Option<u16>,
    pub sidebar_width: Option<u16>,
    /// The base text size, in CSS pixels.
    pub text_size: f32,
    pub look: Look,
    /// The window closed (the app quit): the page says so.
    pub closed: bool,
    /// Why the page has no line to the daemon, if it hasn't.
    pub lost: Option<String>,
}

/// Every part of the window, as signals.
#[derive(Clone, Copy)]
pub struct View {
    pub session: Signal<model::Session>,
    pub composer: Signal<model::Composer>,
    pub sessions: Signal<model::Sessions>,
    pub prompt: Signal<model::Prompt>,
    pub peek: Signal<model::Peek>,
    pub side: Signal<model::Side>,
    pub preferences: Signal<model::Preferences>,
    pub usage: Signal<model::Usage>,
    pub entries: Signal<Vec<model::Entry>>,
    /// The conversation's images, decoded, as `data:` URLs.
    pub images: Signal<HashMap<ImageKey, String>>,
    pub draft: Signal<String>,
    pub page: Signal<Page>,
    /// Where the components' actions go: the app's events.
    pub events: Signal<Option<async_channel::Sender<Event>>>,
}

impl View {
    pub fn new() -> Self {
        Self {
            session: Signal::new(model::Session {
                title: "New session".to_owned(),
                limit_five_hour: -1.0,
                limit_seven_day: -1.0,
                ..model::Session::default()
            }),
            composer: Signal::new(model::Composer::default()),
            sessions: Signal::new(model::Sessions::default()),
            prompt: Signal::new(model::Prompt::default()),
            peek: Signal::new(model::Peek::default()),
            side: Signal::new(model::Side::default()),
            preferences: Signal::new(model::Preferences::default()),
            usage: Signal::new(model::Usage::default()),
            entries: Signal::new(Vec::new()),
            images: Signal::new(HashMap::new()),
            draft: Signal::new(String::new()),
            page: Signal::new(Page {
                text_size: 15.0,
                ..Page::default()
            }),
            events: Signal::new(None),
        }
    }

    /// Hands an action to the app.
    pub fn act(&self, action: Action) {
        if let Some(events) = self.events.peek().as_ref()
            && events.try_send(Event::Action(action)).is_err()
        {
            tracing::debug!("an action after the app ended");
        }
    }
}

/// Sets `signal` to `value` unless it holds that already, which would only draw it again.
fn set<T: PartialEq + Clone + 'static>(mut signal: Signal<T>, value: &T) {
    if *signal.peek() != *value {
        signal.set(value.clone());
    }
}

/// The page, as the presenter's frontend.
pub struct PageFrontend {
    view: View,
}

impl PageFrontend {
    pub fn new(view: View) -> Self {
        Self { view }
    }
}

impl Frontend for PageFrontend {
    fn session(&mut self, session: &model::Session) {
        set(self.view.session, session);
    }

    fn composer(&mut self, composer: &model::Composer) {
        set(self.view.composer, composer);
    }

    fn sessions(&mut self, sessions: &model::Sessions) {
        set(self.view.sessions, sessions);
    }

    fn entries(&mut self, change: EntriesChange) {
        let mut entries = self.view.entries;
        match change {
            EntriesChange::Reset(rows) => entries.set(rows),
            EntriesChange::Push(row) => entries.write().push(row),
            EntriesChange::Set(index, row) => {
                if let Some(shown) = entries.write().get_mut(index) {
                    *shown = row;
                }
            }
        }
    }

    fn prompt(&mut self, prompt: &model::Prompt) {
        set(self.view.prompt, prompt);
    }

    fn peek(&mut self, peek: &model::Peek) {
        set(self.view.peek, peek);
    }

    fn side(&mut self, side: &model::Side) {
        set(self.view.side, side);
    }

    fn preferences(&mut self, preferences: &model::Preferences) {
        set(self.view.preferences, preferences);
    }

    fn usage(&mut self, usage: &model::Usage) {
        set(self.view.usage, usage);
    }

    fn show_image(&mut self, key: ImageKey, shown: Result<&Pixels, String>) {
        if let Ok(pixels) = shown
            && let Some(url) = data_url(pixels)
        {
            self.view.images.write().insert(key, url);
        }
    }

    fn keep_images(&mut self, shown: &HashSet<ImageKey>) {
        self.view
            .images
            .write()
            .retain(|key, _| shown.contains(key));
    }

    fn command(&mut self, command: Command) {
        let mut page = self.view.page;
        let mut draft = self.view.draft;
        match command {
            Command::Copy(text) => copy(&text),
            Command::SetDraft(text) => draft.set(text),
            Command::AppendDraft(text) => {
                let joined = {
                    let shown = draft.peek();
                    if shown.trim().is_empty() {
                        text
                    } else {
                        format!("{}\n\n{text}", shown.trim_end())
                    }
                };
                draft.set(joined);
            }
            Command::FocusComposer => page.write().focus_composer += 1,
            Command::FocusSide => page.write().focus_side += 1,
            Command::ScrollToEnd => page.write().scroll_to_end += 1,
            Command::OpenAtEnd => page.write().opened += 1,
            Command::ShowTasks => page.write().show_tasks += 1,
            Command::ProjectsOpened => page.write().projects_opened += 1,
            Command::ModelIndex(index) => page.write().model_index = index,
            Command::ModeIndex(index) => page.write().mode_index = index,
            Command::Reviewing(reviewing) => page.write().reviewing = reviewing,
            Command::ComposerHeight(height) => page.write().composer_height = height,
            Command::SidebarWidth(width) => page.write().sidebar_width = width,
            Command::TextSize(pixels) => page.write().text_size = pixels,
            Command::Look(look) => page.write().look = look,
            Command::Quit => page.write().closed = true,
        }
    }

    fn draft(&self) -> String {
        self.view.draft.peek().clone()
    }
}

/// Pixels as a PNG `data:` URL, for an `<img>`.
pub fn data_url(pixels: &Pixels) -> Option<String> {
    use base64::Engine as _;
    let image = image::RgbaImage::from_raw(pixels.width_px, pixels.height_px, pixels.rgba.clone())?;
    let mut png = std::io::Cursor::new(Vec::new());
    image.write_to(&mut png, image::ImageFormat::Png).ok()?;
    let encoded = base64::engine::general_purpose::STANDARD.encode(png.into_inner());
    Some(format!("data:image/png;base64,{encoded}"))
}

/// An image of the presentation model as a URL to show; empty for none (yet).
pub fn image_url(image: &Image, images: &HashMap<ImageKey, String>) -> String {
    match image {
        Image::None => String::new(),
        Image::Key(key) => images.get(key).cloned().unwrap_or_default(),
        Image::Pixels(pixels) => data_url(pixels).unwrap_or_default(),
    }
}

/// Puts `text` on the browser's clipboard.
pub fn copy(text: &str) {
    #[cfg(feature = "web")]
    if let Some(window) = web_sys::window() {
        let written = window.navigator().clipboard().write_text(text);
        wasm_bindgen_futures::spawn_local(async move {
            if wasm_bindgen_futures::JsFuture::from(written).await.is_err() {
                tracing::warn!("the clipboard refused the text");
            }
        });
    }
    #[cfg(not(feature = "web"))]
    let _: &str = text;
}
