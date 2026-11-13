//! The page's state: what the presenter last told of each part of the window, in signals the
//! components read, and the page's own (focus, the draft, sizes), which the app sets through
//! commands. `PageFrontend` is the presenter's frontend that writes them.

use std::collections::HashMap;
#[cfg(any(feature = "web", feature = "mobile"))]
use std::collections::HashSet;

use dioxus::prelude::*;
use edualc_core::app::{Action, Event};
use edualc_core::chat::ImageKey;
use edualc_core::decode::Pixels;
#[cfg(any(feature = "web", feature = "mobile"))]
use edualc_core::present::{Command, EntriesChange, Frontend, Seat};
use edualc_core::present::{Image, Look, model};

#[cfg(any(feature = "web", feature = "mobile"))]
use crate::web::platform;

/// What the app sets of the page itself, besides the presenter's state.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Page {
    /// Bumped to focus the composer, the side panel's field, what the prompt asks for first.
    pub focus_composer: u32,
    pub focus_side: u32,
    pub focus_prompt: u32,
    /// The sidebar's filter is to take the focus, once it is drawn (Ctrl+K).
    pub filter_wanted: bool,
    /// Bumped to go to the transcript's end.
    pub scroll_to_end: u32,
    /// Bumped as a session's transcript is loaded: shown at its end.
    pub opened: u32,
    /// Bumped to open the list of what runs.
    pub show_tasks: u32,
    /// Bumped as the folder typed in the list of folders opened.
    pub projects_opened: u32,
    /// Bumped to open the list of folders from elsewhere (the trust bar).
    pub projects_asked: u32,
    pub model_index: i32,
    pub mode_index: i32,
    pub reviewing: bool,
    pub composer_height: Option<u16>,
    pub sidebar_width: Option<u16>,
    /// The base text size, in CSS pixels.
    pub text_size: f32,
    /// Device pixels to a CSS pixel, as the app reads it (a browser's page asks the browser).
    pub pixel_ratio: f64,
    /// The session shown, which the app opens again as Android starts it anew.
    pub session: Option<edualc_core::external::wire::SessionId>,
    /// The line is a demo's, unencrypted: the window says so for as long as it shows.
    pub unencrypted: bool,
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
    /// The transcript's rows, each drawn again only as it changes.
    pub entries: Store<Vec<model::Entry>>,
    /// The conversation's images, decoded, as `data:` URLs.
    pub images: Signal<HashMap<ImageKey, String>>,
    pub draft: Signal<String>,
    pub page: Signal<Page>,
    /// Where the components' actions go: the app's events.
    pub events: Signal<Option<async_channel::Sender<Event>>>,
}

impl View {
    /// What a line opened afresh starts from in a window that showed another (the app reaches
    /// another edualcd): not closed, nothing lost, no session shown, no draft; `unencrypted`, a
    /// demo's line. What the presenter shows it sets anew.
    #[cfg(feature = "mobile")]
    pub fn fresh_line(self, unencrypted: bool) {
        {
            let mut page = self.page;
            let mut page = page.write();
            page.closed = false;
            page.lost = None;
            page.session = None;
            page.unencrypted = unencrypted;
        }
        let mut draft = self.draft;
        draft.set(String::new());
    }

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
            entries: Store::new(Vec::new()),
            images: Signal::new(HashMap::new()),
            draft: Signal::new(String::new()),
            page: Signal::new(Page {
                text_size: 15.0,
                pixel_ratio: 1.0,
                ..Page::default()
            }),
            events: Signal::new(None),
        }
    }

    /// Hands an action to the app.
    pub fn act(&self, action: Action) {
        if !self.try_act(action) {
            tracing::debug!("an action while no app runs");
        }
    }

    /// Hands an action to the app; false if none takes it (the line isn't open yet, or the app
    /// ended), so that what it carries isn't let go of.
    #[must_use]
    pub fn try_act(&self, action: Action) -> bool {
        self.events
            .peek()
            .as_ref()
            .is_some_and(|events| events.try_send(Event::Action(action)).is_ok())
    }
}

/// The page, as the presenter's frontend. Only in the browser and the Android app: the server
/// renders the page as it is before its line opens.
#[cfg(any(feature = "web", feature = "mobile"))]
pub struct PageFrontend {
    view: View,
}

#[cfg(any(feature = "web", feature = "mobile"))]
impl PageFrontend {
    pub fn new(view: View) -> Self {
        Self { view }
    }
}

#[cfg(any(feature = "web", feature = "mobile"))]
impl Frontend for PageFrontend {
    fn session(&mut self, _old: Option<&model::Session>, session: &model::Session) {
        let mut shown = self.view.session;
        shown.set(session.clone());
    }

    fn composer(&mut self, _old: Option<&model::Composer>, composer: &model::Composer) {
        let mut shown = self.view.composer;
        shown.set(composer.clone());
    }

    fn sessions(&mut self, _old: Option<&model::Sessions>, sessions: &model::Sessions) {
        let mut shown = self.view.sessions;
        shown.set(sessions.clone());
    }

    fn entries(&mut self, change: EntriesChange) {
        let mut entries = self.view.entries;
        match change {
            EntriesChange::Reset(rows) => entries.set(rows),
            EntriesChange::Push(row) => entries.push(row),
            EntriesChange::Set(index, row) => {
                if let Some(mut shown) = entries.get(index) {
                    shown.set(row);
                }
            }
        }
    }

    fn prompt(&mut self, _old: Option<&model::Prompt>, prompt: &model::Prompt) {
        let mut shown = self.view.prompt;
        shown.set(prompt.clone());
    }

    fn peek(&mut self, _old: Option<&model::Peek>, peek: &model::Peek) {
        let mut shown = self.view.peek;
        shown.set(peek.clone());
    }

    fn side(&mut self, _old: Option<&model::Side>, side: &model::Side) {
        let mut shown = self.view.side;
        shown.set(side.clone());
    }

    fn preferences(&mut self, _old: Option<&model::Preferences>, preferences: &model::Preferences) {
        let mut shown = self.view.preferences;
        shown.set(preferences.clone());
    }

    fn usage(&mut self, _old: Option<&model::Usage>, usage: &model::Usage) {
        let mut shown = self.view.usage;
        shown.set(usage.clone());
    }

    fn show_image(&mut self, key: ImageKey, shown: Result<&Pixels, String>) {
        if let Ok(pixels) = shown
            && let Some(url) = object_url(pixels)
            && let Some(old) = self.view.images.write().insert(key, url)
        {
            let_go(&old);
        }
    }

    fn keep_images(&mut self, shown: &HashSet<ImageKey>) {
        self.view.images.write().retain(|key, url| {
            let kept = shown.contains(key);
            if !kept {
                let_go(url);
            }
            kept
        });
    }

    /// On the browser's desktop: a link in a new tab, a notification if the browser lets the
    /// page show them. A pasted image comes with the paste (see the composer), not from here.
    /// In the Android app, Android's: the browser, its notifications, its clipboard.
    fn seat(&mut self, seat: Seat) {
        match seat {
            Seat::OpenUrl(url) => platform::open_url(&url),
            Seat::Notify { summary, body } => platform::notify(&summary, &body),
            Seat::PasteImage => platform::paste_image(self.view),
        }
    }

    fn command(&mut self, command: Command) {
        let mut page = self.view.page;
        let mut draft = self.view.draft;
        match command {
            Command::Copy(text) => platform::copy(&text),
            Command::SetDraft(text) => draft.set(text),
            Command::AppendDraft(text) => {
                let kept = draft.peek().clone();
                draft.set(if kept.trim().is_empty() {
                    text
                } else {
                    format!("{}\n\n{text}", kept.trim_end())
                });
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
            Command::SessionShown(session) => page.write().session = session,
            // The page shows code as it is.
            // The page shows code as it is.
            Command::RecolourCode | Command::DrawColouredCode => {}
        }
    }

    fn draft(&self) -> String {
        self.view.draft.peek().clone()
    }
}

/// Pixels as a PNG `data:` URL, for an `<img>`.
pub fn data_url(pixels: &Pixels) -> Option<String> {
    use base64::Engine as _;
    let encoded = base64::engine::general_purpose::STANDARD.encode(png(pixels)?);
    Some(format!("data:image/png;base64,{encoded}"))
}

/// `pixels` as a PNG file's bytes.
fn png(pixels: &Pixels) -> Option<Vec<u8>> {
    let image = image::RgbaImage::from_raw(pixels.width_px, pixels.height_px, pixels.rgba.clone())?;
    let mut png = std::io::Cursor::new(Vec::new());
    image.write_to(&mut png, image::ImageFormat::Png).ok()?;
    Some(png.into_inner())
}

/// `pixels` as a PNG the browser holds, named by a short `blob:` URL: the conversation's images
/// are drawn again as rows change, and copying a URL of megabytes each time would cost. Let go
/// of with [`let_go`].
#[cfg(all(feature = "web", not(feature = "mobile")))]
fn object_url(pixels: &Pixels) -> Option<String> {
    let bytes = png(pixels)?;
    let parts = js_sys::Array::of1(&js_sys::Uint8Array::from(bytes.as_slice()));
    let options = web_sys::BlobPropertyBag::new();
    options.set_type("image/png");
    let blob = web_sys::Blob::new_with_u8_array_sequence_and_options(&parts, &options).ok()?;
    web_sys::Url::create_object_url_with_blob(&blob).ok()
}

/// Lets go of an image [`object_url`] made: the browser frees it.
#[cfg(all(feature = "web", not(feature = "mobile")))]
fn let_go(url: &str) {
    if url.starts_with("blob:") && web_sys::Url::revoke_object_url(url).is_err() {
        tracing::debug!("an image's URL was let go of already");
    }
}

/// In the Android app, an image's URL is a `data:` URL: its WebView holds no blobs of the app's.
#[cfg(feature = "mobile")]
fn object_url(pixels: &Pixels) -> Option<String> {
    data_url(pixels)
}

/// A `data:` URL is let go of with the text that holds it.
#[cfg(feature = "mobile")]
fn let_go(_url: &str) {}

/// An image of the presentation model as a URL to show; empty for none (yet).
pub fn image_url(image: &Image, images: &HashMap<ImageKey, String>) -> String {
    match image {
        Image::None => String::new(),
        Image::Key(key) => images.get(key).cloned().unwrap_or_default(),
        Image::Pixels(pixels) => data_url(pixels).unwrap_or_default(),
    }
}
