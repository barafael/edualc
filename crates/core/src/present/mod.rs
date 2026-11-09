//! What the window shows, apart from how it is drawn: the app's state made into the rows,
//! menus and words a window shows (`Presenter`), as plain data (`model`), handed to a
//! [`Frontend`] that draws it. The Slint window is one frontend, the Dioxus page another; both
//! show the same model, so they say the same things.

use std::collections::HashSet;

use crate::chat::ImageKey;
use crate::decode::Pixels;

pub mod model;
mod presenter;

pub use presenter::{Presenter, short_count, short_duration};

/// Text the window sets: plain, or with inline Markdown (emphasis, code, links) for the frontend
/// to style.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum Styled {
    #[default]
    Empty,
    Plain(String),
    Markdown(String),
}

impl Styled {
    pub fn plain(text: &str) -> Self {
        Self::Plain(text.to_owned())
    }

    pub fn markdown(text: &str) -> Self {
        Self::Markdown(text.to_owned())
    }
}

/// An image the window shows: one of the conversation's, by its key (its pixels come apart,
/// through [`Frontend::show_image`]), or pixels at hand (a thumbnail).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum Image {
    #[default]
    None,
    Key(ImageKey),
    Pixels(Pixels),
}

/// The window's look: Gruvbox, or plain.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Look {
    #[default]
    Gruvbox,
    Plain,
}

/// A change to the transcript's rows: they change one by one as the conversation goes, so they
/// aren't part of a global's state.
#[derive(Debug, Clone, PartialEq)]
pub enum EntriesChange {
    /// All rows anew (another session, say).
    Reset(Vec<model::Entry>),
    Push(model::Entry),
    /// Row `index` anew: a frontend keeps what of it stays (its blocks' selection).
    Set(usize, model::Entry),
}

/// What the window does on the desktop it is on, which is the user's: edualcd may run on another
/// machine, whose desktop no one is at. What fails is told as `Event::SeatFailed`, but for a
/// notification (a desktop without a notification service would have an error for each); a
/// paste comes back as `Event::Pasted`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Seat {
    /// A web page or mail, in the desktop's browser (checked to be one, see
    /// `desktop::is_web_link`).
    OpenUrl(String),
    /// A notification; its texts come from other sessions' records, and are taken as text.
    Notify { summary: String, body: String },
    /// The clipboard's image, if it holds one, made ready to send.
    PasteImage,
}

/// What the window is told to do, rather than to show: its own state the app sets now and
/// then (the draft, a menu's choice), focus, and moving about.
#[derive(Debug, Clone, PartialEq)]
pub enum Command {
    Copy(String),
    SetDraft(String),
    FocusComposer,
    FocusSide,
    /// Goes to the transcript's end.
    ScrollToEnd,
    /// A session's transcript was loaded: shown at its end once the list has measured its way
    /// there, not on the way.
    OpenAtEnd,
    /// Opens the list of what runs.
    ShowTasks,
    /// The folder typed in the list of folders opened: the list closes.
    ProjectsOpened,
    /// Which model and mode the menus have chosen.
    ModelIndex(i32),
    ModeIndex(i32),
    /// Whether the plan shown is being reviewed.
    Reviewing(bool),
    /// The composer's kept height in logical pixels, if it was dragged to one; fitted to its
    /// text if not.
    ComposerHeight(Option<u16>),
    /// The sidebar's kept width in logical pixels; the default if none.
    SidebarWidth(Option<u16>),
    /// The base text size, in logical pixels; everything else is sized from it.
    TextSize(f32),
    Look(Look),
    /// The window closes.
    Quit,
    /// The session the window shows now, by its id; none for a new one not started yet. A
    /// frontend that is ended and started again by its system (the Android app) opens it
    /// again.
    SessionShown(Option<crate::external::wire::SessionId>),
}

/// What draws the window: told the state of each of its parts as it changes (with what it was
/// told before, if anything), and what to do.
pub trait Frontend {
    fn session(&mut self, old: Option<&model::Session>, session: &model::Session);
    fn composer(&mut self, old: Option<&model::Composer>, composer: &model::Composer);
    fn sessions(&mut self, old: Option<&model::Sessions>, sessions: &model::Sessions);
    fn entries(&mut self, change: EntriesChange);
    fn prompt(&mut self, old: Option<&model::Prompt>, prompt: &model::Prompt);
    fn peek(&mut self, old: Option<&model::Peek>, peek: &model::Peek);
    fn side(&mut self, old: Option<&model::Side>, side: &model::Side);
    fn preferences(&mut self, old: Option<&model::Preferences>, preferences: &model::Preferences);
    fn usage(&mut self, old: Option<&model::Usage>, usage: &model::Usage);
    /// Image `key`'s pixels, or why it can't be shown.
    fn show_image(&mut self, key: ImageKey, shown: Result<&Pixels, String>);
    /// Lets go of the images but those `shown` holds: the conversation was replaced.
    fn keep_images(&mut self, shown: &HashSet<ImageKey>);
    fn command(&mut self, command: Command);
    /// Does something on the desktop the frontend is on, the user's (see [`Seat`]).
    fn seat(&mut self, seat: Seat);
    /// What the composer holds now.
    fn draft(&self) -> String;
}
