//! The Slint window as a frontend (see `present`): the presentation model made Slint's types
//! and set into the window's globals, only what changed, list models changed row by row so a
//! row that stays keeps its selection; and the window's callbacks, which become the app's
//! actions (`wire`).

use std::collections::{HashMap, HashSet};

use slint::{
    ComponentHandle, Model, ModelExt, ModelRc, Rgba8Pixel, SharedPixelBuffer, SharedString,
    VecModel,
};

use crate::chat::ImageKey;
use crate::decode::Pixels;
use crate::generated::{self as slint_ui, AppWindow};
use crate::present::model as present;
use crate::present::{Command, EntriesChange, Frontend, Image, Look, Styled};

mod callbacks;
#[allow(clippy::wildcard_enum_match_arm)]
mod model;

pub(crate) use callbacks::wire;

/// The conversation's images, decoded, as Slint images; one not here isn't decoded yet.
pub(crate) type Images = HashMap<ImageKey, slint::Image>;

/// A value of the presentation model as Slint's.
pub(crate) trait ToSlint {
    type Slint: Clone + 'static;
    fn to_slint(&self, images: &Images) -> Self::Slint;
    /// This value in place of `old`, shown as `shown`: lists in it keep their models.
    fn sync(&self, _old: &Self, _shown: Self::Slint, images: &Images) -> Self::Slint {
        self.to_slint(images)
    }
}

impl ToSlint for String {
    type Slint = SharedString;
    fn to_slint(&self, _: &Images) -> SharedString {
        self.as_str().into()
    }
}

impl ToSlint for bool {
    type Slint = Self;
    fn to_slint(&self, _: &Images) -> Self {
        *self
    }
}

impl ToSlint for i32 {
    type Slint = Self;
    fn to_slint(&self, _: &Images) -> Self {
        *self
    }
}

impl ToSlint for f32 {
    type Slint = Self;
    fn to_slint(&self, _: &Images) -> Self {
        *self
    }
}

impl ToSlint for Styled {
    type Slint = slint::StyledText;
    fn to_slint(&self, _: &Images) -> slint::StyledText {
        match self {
            Self::Empty => slint::StyledText::default(),
            Self::Plain(text) => plain(text),
            Self::Markdown(text) => styled(text),
        }
    }
}

impl ToSlint for Image {
    type Slint = slint::Image;
    fn to_slint(&self, images: &Images) -> slint::Image {
        match self {
            Self::None => slint::Image::default(),
            Self::Key(key) => images.get(key).cloned().unwrap_or_default(),
            Self::Pixels(pixels) => slint_image(pixels).unwrap_or_default(),
        }
    }
}

impl<T: ToSlint + PartialEq> ToSlint for Vec<T> {
    type Slint = ModelRc<T::Slint>;
    fn to_slint(&self, images: &Images) -> Self::Slint {
        let rows: Vec<T::Slint> = self.iter().map(|row| row.to_slint(images)).collect();
        ModelRc::new(VecModel::from(rows))
    }
    fn sync(&self, old: &Self, shown: Self::Slint, images: &Images) -> Self::Slint {
        resync(&shown, old, self, images).unwrap_or(shown)
    }
}

/// Brings `model`, showing `old`, in line with `new` row by row: a row that differs is synced in
/// place (its own lists keep their models), rows added or taken off at the end. A new model
/// for one that isn't a `VecModel` (none made here).
pub(crate) fn resync<T: ToSlint + PartialEq>(
    model: &ModelRc<T::Slint>,
    old: &[T],
    new: &[T],
    images: &Images,
) -> Option<ModelRc<T::Slint>> {
    let Some(rows) = model.as_any().downcast_ref::<VecModel<T::Slint>>() else {
        return Some(new.to_vec_slint(images));
    };
    if rows.row_count() != old.len() {
        // Not what was shown: anew.
        rows.set_vec(
            new.iter()
                .map(|row| row.to_slint(images))
                .collect::<Vec<_>>(),
        );
        return None;
    }
    for (index, row) in new.iter().enumerate() {
        match (old.get(index), rows.row_data(index)) {
            (Some(was), Some(_)) if was == row => {}
            (Some(was), Some(shown)) => rows.set_row_data(index, row.sync(was, shown, images)),
            (None, _) | (Some(_), None) => rows.push(row.to_slint(images)),
        }
    }
    while rows.row_count() > new.len() {
        rows.remove(rows.row_count() - 1);
    }
    None
}

trait ToVecSlint<T: ToSlint> {
    fn to_vec_slint(&self, images: &Images) -> ModelRc<T::Slint>;
}

impl<T: ToSlint> ToVecSlint<T> for [T] {
    fn to_vec_slint(&self, images: &Images) -> ModelRc<T::Slint> {
        let rows: Vec<T::Slint> = self.iter().map(|row| row.to_slint(images)).collect();
        ModelRc::new(VecModel::from(rows))
    }
}

/// The Slint window, as the presenter's frontend.
pub(crate) struct SlintFrontend {
    window: AppWindow,
    images: Images,
    /// The transcript's rows: the model the list filters, and what they show.
    entries: ModelRc<slint_ui::Entry>,
    shown: Vec<present::Entry>,
    /// What each global was set to last.
    session: Option<present::Session>,
    composer: Option<present::Composer>,
    sessions: Option<present::Sessions>,
    prompt: Option<present::Prompt>,
    peek: Option<present::Peek>,
    side: Option<present::Side>,
    preferences: Option<present::Preferences>,
    usage: Option<present::Usage>,
}

impl SlintFrontend {
    pub(crate) fn new(window: AppWindow, fonts: crate::appearance::Fonts) -> Self {
        if let Some(family) = fonts.sans {
            window.global::<slint_ui::Theme>().set_sans(family.into());
        }
        if let Some(family) = fonts.mono {
            set_monospace(&family);
            window.global::<slint_ui::Theme>().set_mono(family.into());
        }
        let entries = ModelRc::new(VecModel::<slint_ui::Entry>::default());
        // The list gets only the rows it shows. Slint's list estimates the rows it hasn't built
        // from those it has, so a row that takes no room misleads it: hidden rows past the end
        // put the end further down than it is, and following the end never settles; a view of
        // hidden rows alone measures nothing and is built again on every pass ("long
        // callback/instantiation chain detected").
        window
            .global::<slint_ui::Transcript>()
            .set_entries(ModelRc::new(entries.clone().filter(|entry| !entry.hidden)));
        Self {
            window,
            images: Images::new(),
            entries,
            shown: Vec::new(),
            session: None,
            composer: None,
            sessions: None,
            prompt: None,
            peek: None,
            side: None,
            preferences: None,
            usage: None,
        }
    }

    fn rows(&self) -> &VecModel<slint_ui::Entry> {
        self.entries
            .as_any()
            .downcast_ref()
            .expect("the transcript's model is a VecModel, made in SlintFrontend::new")
    }
}

impl Frontend for SlintFrontend {
    fn session(&mut self, session: &present::Session) {
        model::apply_session(&self.window, self.session.as_ref(), session, &self.images);
        self.session = Some(session.clone());
    }

    fn composer(&mut self, composer: &present::Composer) {
        model::apply_composer(&self.window, self.composer.as_ref(), composer, &self.images);
        self.composer = Some(composer.clone());
    }

    fn sessions(&mut self, sessions: &present::Sessions) {
        model::apply_sessions(&self.window, self.sessions.as_ref(), sessions, &self.images);
        self.sessions = Some(sessions.clone());
    }

    fn prompt(&mut self, prompt: &present::Prompt) {
        model::apply_prompt(&self.window, self.prompt.as_ref(), prompt, &self.images);
        self.prompt = Some(prompt.clone());
    }

    fn peek(&mut self, peek: &present::Peek) {
        model::apply_peek(&self.window, self.peek.as_ref(), peek, &self.images);
        self.peek = Some(peek.clone());
    }

    fn side(&mut self, side: &present::Side) {
        model::apply_side(&self.window, self.side.as_ref(), side, &self.images);
        self.side = Some(side.clone());
    }

    fn preferences(&mut self, preferences: &present::Preferences) {
        model::apply_preferences(
            &self.window,
            self.preferences.as_ref(),
            preferences,
            &self.images,
        );
        self.preferences = Some(preferences.clone());
    }

    fn usage(&mut self, usage: &present::Usage) {
        model::apply_usage(&self.window, self.usage.as_ref(), usage, &self.images);
        self.usage = Some(usage.clone());
    }

    fn entries(&mut self, change: EntriesChange) {
        match change {
            EntriesChange::Reset(entries) => {
                let rows: Vec<slint_ui::Entry> = entries
                    .iter()
                    .map(|entry| entry.to_slint(&self.images))
                    .collect();
                self.rows().set_vec(rows);
                self.shown = entries;
            }
            EntriesChange::Push(entry) => {
                self.rows().push(entry.to_slint(&self.images));
                self.shown.push(entry);
            }
            EntriesChange::Set(index, entry) => {
                let synced = match (self.shown.get(index), self.rows().row_data(index)) {
                    (Some(old), Some(shown)) => entry.sync(old, shown, &self.images),
                    (None, _) | (Some(_), None) => return,
                };
                self.rows().set_row_data(index, synced);
                if let Some(shown) = self.shown.get_mut(index) {
                    *shown = entry;
                }
            }
        }
    }

    fn show_image(&mut self, key: ImageKey, shown: Result<&Pixels, String>) {
        if let Ok(Some(image)) = shown.map(slint_image) {
            self.images.insert(key, image);
        }
    }

    fn keep_images(&mut self, shown: &HashSet<ImageKey>) {
        self.images.retain(|key, _| shown.contains(key));
    }

    fn command(&mut self, command: Command) {
        let window = &self.window;
        match command {
            Command::Copy(text) => window.invoke_copy_text(text.into()),
            Command::SetDraft(text) => window.global::<slint_ui::Composer>().set_draft(text.into()),
            Command::AppendDraft(text) => {
                let composer = window.global::<slint_ui::Composer>();
                let draft = composer.get_draft();
                let joined = if draft.trim().is_empty() {
                    text
                } else {
                    format!("{}\n\n{text}", draft.trim_end())
                };
                composer.set_draft(joined.into());
            }
            Command::FocusComposer => {
                let composer = window.global::<slint_ui::Composer>();
                composer.set_focus_serial(composer.get_focus_serial().wrapping_add(1));
            }
            Command::FocusSide => {
                let side = window.global::<slint_ui::Side>();
                side.set_focus_wanted(true);
                side.set_focus_serial(side.get_focus_serial().wrapping_add(1));
            }
            Command::ScrollToEnd => {
                let transcript = window.global::<slint_ui::Transcript>();
                transcript.set_scroll_to_end_serial(
                    transcript.get_scroll_to_end_serial().wrapping_add(1),
                );
            }
            Command::OpenAtEnd => {
                let transcript = window.global::<slint_ui::Transcript>();
                transcript.set_opened_serial(transcript.get_opened_serial().wrapping_add(1));
            }
            Command::ShowTasks => {
                let session = window.global::<slint_ui::Session>();
                session.set_tasks_wanted(session.get_tasks_wanted().wrapping_add(1));
            }
            Command::ProjectsOpened => {
                let sessions = window.global::<slint_ui::Sessions>();
                sessions.set_opened_serial(sessions.get_opened_serial().wrapping_add(1));
            }
            Command::ModelIndex(index) => {
                window.global::<slint_ui::Session>().set_model_index(index);
            }
            Command::ModeIndex(index) => {
                window.global::<slint_ui::Session>().set_mode_index(index);
            }
            Command::Reviewing(reviewing) => {
                window.global::<slint_ui::Prompt>().set_reviewing(reviewing);
            }
            Command::ComposerHeight(height_px) => {
                let composer = window.global::<slint_ui::Composer>();
                match height_px {
                    Some(height_px) => {
                        composer.set_chosen_height(f32::from(height_px));
                        composer.set_size(slint_ui::ComposerSize::Chosen);
                    }
                    None => composer.set_size(slint_ui::ComposerSize::Fit),
                }
            }
            Command::SidebarWidth(width_px) => window
                .global::<slint_ui::Sessions>()
                .set_chosen_width(width_px.map_or(0.0, f32::from)),
            Command::TextSize(pixels) => window.global::<slint_ui::Theme>().set_base_size(pixels),
            Command::Look(look) => window.global::<slint_ui::Theme>().set_look(match look {
                Look::Gruvbox => slint_ui::Look::Gruvbox,
                Look::Plain => slint_ui::Look::Plain,
            }),
            Command::Quit => {
                if let Err(error) = slint::quit_event_loop() {
                    tracing::warn!(%error, "the window didn't close");
                }
            }
        }
    }

    fn draft(&self) -> String {
        self.window
            .global::<slint_ui::Composer>()
            .get_draft()
            .into()
    }
}

/// Sets Markdown's inline `code` in `family`, as code blocks are. Slint draws it in the generic
/// monospace family, which an application maps in Slint's font collection (as its maintainers
/// advise, slint-ui/slint#12953); fontconfig's monospace is another font.
fn set_monospace(family: &str) {
    use slint::fontique_012::fontique::GenericFamily;
    let mut collection = slint::fontique_012::shared_collection();
    if let Some(id) = collection.family_id(family) {
        collection.set_generic_families(GenericFamily::Monospace, std::iter::once(id));
    }
}

/// Pixels as a Slint image; none for pixels that don't add up.
fn slint_image(pixels: &Pixels) -> Option<slint::Image> {
    let mut buffer = SharedPixelBuffer::<Rgba8Pixel>::new(pixels.width_px, pixels.height_px);
    let bytes = buffer.make_mut_bytes();
    if bytes.len() != pixels.rgba.len() {
        return None;
    }
    bytes.copy_from_slice(&pixels.rgba);
    Some(slint::Image::from_rgba8(buffer))
}

/// Inline Markdown as Slint styled text; as plain text if Slint's parser refuses it (an HTML
/// tag it doesn't know, say a stray `<T>` outside backticks).
fn styled(source: &str) -> slint::StyledText {
    let source = crate::markdown::sanitize(source);
    slint::StyledText::from_markdown(&source)
        .unwrap_or_else(|_| slint::StyledText::from_plain_text(&source))
}

/// Text shown as it is (code, tool output): tabs as spaces, as the text layout draws no tab
/// stops.
fn plain(text: &str) -> slint::StyledText {
    slint::StyledText::from_plain_text(&crate::markdown::sanitize(text).replace('\t', "    "))
}

#[cfg(test)]
mod tests {
    /// What the shared Markdown splitter takes for inline Markdown is what Slint's parser takes.
    #[test]
    fn markdown_accepted_as_slint_accepts_it() {
        let samples = [
            "plain words",
            "*emphasis* and **strong** and ~~struck~~",
            "a [link](https://example.com) and `code`",
            "- one\n- two\n  - nested",
            "1. first\n2. second",
            "<u>under</u> and <font color=\"red\">red</font>",
            "A stray <T> here.",
            "===",
            "\n\n\\\nAfter a break",
            "# Heading",
            "> quoted",
            "```\ncode\n```",
            "| a | b |\n|---|---|\n| 1 | 2 |",
            "---",
            "![image](x.png)",
            "<div>block</div>",
            "line one\nline two  \nhard break",
            "*unclosed emphasis",
            "an email <me@example.com>",
            "<https://example.com>",
        ];
        for sample in samples {
            let ours = crate::markdown::accepted(sample);
            let slints =
                slint::StyledText::from_markdown(&crate::markdown::sanitize(sample)).is_ok();
            assert_eq!(ours, slints, "{sample:?}");
        }
    }
}
