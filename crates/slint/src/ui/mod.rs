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
use crate::colourist::{self, Coloured, Colourist, Order, Orders, Place};
use crate::decode::Pixels;
use crate::generated::{self as slint_ui, AppWindow};
use crate::present::model as present;
use crate::present::{Command, EntriesChange, Frontend, Image, Look, Styled};

mod callbacks;
mod model;

pub(crate) use callbacks::wire;

/// What the presentation model's values are drawn with: the conversation's images, decoded (one
/// not here isn't decoded yet), and code's colours, made on a thread of their own (`colourist`):
/// code not coloured yet is shown plain.
pub(crate) struct Drawing {
    images: HashMap<ImageKey, slint::Image>,
    /// Where code is ordered coloured.
    orders: Orders,
    /// The rows' places and the colours as the orders are made: a new one for other rows (a
    /// session anew) or other colours.
    generation: u64,
    /// Code coloured, by its text, with its language: what a block of it shows.
    coloured: HashMap<String, (String, slint::StyledText)>,
    /// The code last coloured at each place, which the code coloured there next replaces (a
    /// block streaming in grows a line at a time: each is no use once the next is coloured).
    placed: HashMap<Place, String>,
}

impl Drawing {
    fn new(orders: Orders) -> Self {
        Self {
            images: HashMap::new(),
            orders,
            generation: 0,
            coloured: HashMap::new(),
            placed: HashMap::new(),
        }
    }

    /// `code` in `lang`'s colours if it is coloured yet; else as it is.
    fn code(&self, lang: &str, code: &str) -> slint::StyledText {
        self.coloured_as(lang, code).unwrap_or_else(|| plain(code))
    }

    /// `code` in `lang`'s colours, if it is coloured yet.
    fn coloured_as(&self, lang: &str, code: &str) -> Option<slint::StyledText> {
        match self.coloured.get(code) {
            Some((coloured_as, coloured)) if coloured_as == lang => Some(coloured.clone()),
            Some(_) | None => None,
        }
    }

    /// Orders `block`, at `place`, coloured, unless it is (or isn't code).
    fn order(&self, place: Place, block: &present::MdBlock) {
        if block.kind != present::BlockKind::Code
            || block.lang.trim().is_empty()
            || self.coloured_as(&block.lang, &block.raw).is_some()
        {
            return;
        }
        self.orders.send(Order::Colour(colourist::Job {
            generation: self.generation,
            place,
            lang: block.lang.clone(),
            raw: block.raw.clone(),
        }));
    }

    /// Other rows, or colours, from now: what was coloured, and is being, is of no use.
    fn renew(&mut self) {
        self.generation = self.generation.wrapping_add(1);
        self.coloured.clear();
        self.placed.clear();
    }

    /// Keeps `done` for its code to show, if it is of now: whether it is.
    fn take(&mut self, done: &Coloured) -> bool {
        let job = &done.job;
        if job.generation != self.generation {
            return false;
        }
        if let Some(replaced) = self.placed.insert(job.place, job.raw.clone())
            && replaced != job.raw
        {
            self.coloured.remove(&replaced);
        }
        self.coloured
            .insert(job.raw.clone(), (job.lang.clone(), done.text.clone()));
        true
    }

    /// Orders the code of the side panel and the request bar's plan coloured.
    fn order_side(&self, side: Option<&present::Side>, prompt: Option<&present::Prompt>) {
        for (exchange, shown) in side
            .iter()
            .flat_map(|side| side.exchanges.iter().enumerate())
        {
            for (block, md) in shown.answer.iter().enumerate() {
                self.order(Place::Side { exchange, block }, md);
            }
        }
        for (section, shown) in prompt
            .iter()
            .flat_map(|prompt| prompt.plan.iter().enumerate())
        {
            self.order(Place::Plan { section }, &shown.block);
        }
    }
}

/// Orders the code of `new`, at row `row`, coloured where it differs from `old` (all of it, for
/// none).
fn order_changed(
    drawing: &Drawing,
    row: usize,
    old: Option<&present::Entry>,
    new: &present::Entry,
) {
    for (block, md) in new.blocks.iter().enumerate() {
        if old.and_then(|old| old.blocks.get(block)) != Some(md) {
            drawing.order(Place::Entry { row, block }, md);
        }
    }
}

/// A Markdown block's body as the Slint window draws it: a text block with the space between its
/// paragraphs that Slint leaves out, code in colour once it is coloured.
fn md_body(block: &present::MdBlock, drawing: &Drawing) -> slint::StyledText {
    match (block.kind, &block.body) {
        (present::BlockKind::Text, Styled::Markdown(text)) => {
            Styled::Markdown(crate::markdown::spaced_for_slint(text)).to_slint(drawing)
        }
        (present::BlockKind::Code, _) => drawing.code(&block.lang, &block.raw),
        _ => block.body.to_slint(drawing),
    }
}

/// A value of the presentation model as Slint's.
pub(crate) trait ToSlint {
    type Slint: Clone + 'static;
    fn to_slint(&self, images: &Drawing) -> Self::Slint;
    /// This value in place of `old`, shown as `shown`: lists in it keep their models.
    fn sync(&self, _old: &Self, _shown: Self::Slint, images: &Drawing) -> Self::Slint {
        self.to_slint(images)
    }
}

impl ToSlint for String {
    type Slint = SharedString;
    fn to_slint(&self, _: &Drawing) -> SharedString {
        self.as_str().into()
    }
}

impl ToSlint for bool {
    type Slint = Self;
    fn to_slint(&self, _: &Drawing) -> Self {
        *self
    }
}

impl ToSlint for i32 {
    type Slint = Self;
    fn to_slint(&self, _: &Drawing) -> Self {
        *self
    }
}

impl ToSlint for f32 {
    type Slint = Self;
    fn to_slint(&self, _: &Drawing) -> Self {
        *self
    }
}

impl ToSlint for Styled {
    type Slint = slint::StyledText;
    fn to_slint(&self, _: &Drawing) -> slint::StyledText {
        match self {
            Self::Empty => slint::StyledText::default(),
            Self::Plain(text) => plain(text),
            Self::Markdown(text) => styled(text),
        }
    }
}

impl ToSlint for Image {
    type Slint = slint::Image;
    fn to_slint(&self, images: &Drawing) -> slint::Image {
        match self {
            Self::None => slint::Image::default(),
            Self::Key(key) => images.images.get(key).cloned().unwrap_or_default(),
            Self::Pixels(pixels) => slint_image(pixels).unwrap_or_default(),
        }
    }
}

impl<T: ToSlint + PartialEq> ToSlint for Vec<T> {
    type Slint = ModelRc<T::Slint>;
    fn to_slint(&self, images: &Drawing) -> Self::Slint {
        let rows: Vec<T::Slint> = self.iter().map(|row| row.to_slint(images)).collect();
        ModelRc::new(VecModel::from(rows))
    }
    fn sync(&self, old: &Self, shown: Self::Slint, images: &Drawing) -> Self::Slint {
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
    images: &Drawing,
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
    fn to_vec_slint(&self, images: &Drawing) -> ModelRc<T::Slint>;
}

impl<T: ToSlint> ToVecSlint<T> for [T] {
    fn to_vec_slint(&self, images: &Drawing) -> ModelRc<T::Slint> {
        let rows: Vec<T::Slint> = self.iter().map(|row| row.to_slint(images)).collect();
        ModelRc::new(VecModel::from(rows))
    }
}

/// The Slint window, as the presenter's frontend.
pub(crate) struct SlintFrontend {
    window: AppWindow,
    drawing: Drawing,
    /// Code coloured by its thread (`colourist`), each when it is woken to take it.
    colourist: Colourist,
    /// The transcript's rows: the model the list filters, and what they show.
    entries: ModelRc<slint_ui::Entry>,
    shown: Vec<present::Entry>,
    /// What the side panel and the request bar were set to last, to colour their code again.
    prompt: Option<present::Prompt>,
    side: Option<present::Side>,
}

impl SlintFrontend {
    pub(crate) fn new(
        window: AppWindow,
        fonts: crate::appearance::Fonts,
        events: async_channel::Sender<crate::app::Event>,
    ) -> Self {
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
        let (orders, colourist) =
            colourist::spawn(&window.global::<slint_ui::Theme>().get_syntax(), events);
        Self {
            window,
            drawing: Drawing::new(orders),
            colourist,
            entries,
            shown: Vec::new(),
            prompt: None,
            side: None,
        }
    }

    /// Code's colours changed (the look, or the desktop's dark or light): what is shown is
    /// coloured again in them, keeping the colours it has until then (and what is selected).
    fn recolour_code(&mut self) {
        self.drawing.renew();
        self.drawing.orders.send(Order::Colours {
            generation: self.drawing.generation,
            colours: self.window.global::<slint_ui::Theme>().get_syntax(),
        });
        self.order_all();
    }

    /// Orders all code shown coloured: the transcript's from its end, which is shown first.
    fn order_all(&self) {
        for (row, entry) in self.shown.iter().enumerate().rev() {
            order_changed(&self.drawing, row, None, entry);
        }
        self.drawing
            .order_side(self.side.as_ref(), self.prompt.as_ref());
    }

    /// Draws the code coloured since, where it is still shown as it was coloured.
    fn draw_coloured(&mut self) {
        let mut side_coloured = false;
        for done in self.colourist.take() {
            if !self.drawing.take(&done) {
                continue;
            }
            match done.job.place {
                Place::Entry { row, block } => self.draw_in_row(row, block, &done),
                Place::Side { .. } | Place::Plan { .. } => side_coloured = true,
            }
        }
        if side_coloured {
            if let Some(side) = &self.side {
                model::apply_side(&self.window, None, side, &self.drawing);
            }
            if let Some(prompt) = &self.prompt {
                model::apply_prompt(&self.window, None, prompt, &self.drawing);
            }
        }
    }

    /// Shows `done` as block `block` of row `row`, if that is the code it shows: the row and its
    /// other blocks stay (and what is selected in them).
    fn draw_in_row(&self, row: usize, block: usize, done: &Coloured) {
        let shows_it = self
            .shown
            .get(row)
            .and_then(|entry| entry.blocks.get(block))
            .is_some_and(|md| {
                md.kind == present::BlockKind::Code
                    && md.lang == done.job.lang
                    && md.raw == done.job.raw
            });
        if !shows_it {
            return;
        }
        let Some(entry) = self.rows().row_data(row) else {
            return;
        };
        let Some(blocks) = entry
            .blocks
            .as_any()
            .downcast_ref::<VecModel<slint_ui::MdBlock>>()
        else {
            return;
        };
        if let Some(mut md) = blocks.row_data(block) {
            md.body = done.text.clone();
            blocks.set_row_data(block, md);
        }
    }

    /// Entry `index`, changed: its row brought in line, its changed code ordered coloured. Code
    /// that only grew keeps its colours until its new ones come (see `model::keeps_colours`).
    fn set_entry(&mut self, index: usize, entry: present::Entry) {
        order_changed(&self.drawing, index, self.shown.get(index), &entry);
        let synced = match (self.shown.get(index), self.rows().row_data(index)) {
            (Some(old), Some(shown)) => Some(entry.sync(old, shown, &self.drawing)),
            (None, _) | (Some(_), None) => None,
        };
        if let Some(synced) = synced {
            self.rows().set_row_data(index, synced);
        }
        if let Some(slot) = self.shown.get_mut(index) {
            *slot = entry;
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
    fn session(&mut self, old: Option<&present::Session>, session: &present::Session) {
        model::apply_session(&self.window, old, session, &self.drawing);
    }

    fn composer(&mut self, old: Option<&present::Composer>, composer: &present::Composer) {
        model::apply_composer(&self.window, old, composer, &self.drawing);
    }

    fn sessions(&mut self, old: Option<&present::Sessions>, sessions: &present::Sessions) {
        model::apply_sessions(&self.window, old, sessions, &self.drawing);
    }

    fn prompt(&mut self, old: Option<&present::Prompt>, prompt: &present::Prompt) {
        model::apply_prompt(&self.window, old, prompt, &self.drawing);
        self.drawing.order_side(None, Some(prompt));
        self.prompt = Some(prompt.clone());
    }

    fn peek(&mut self, old: Option<&present::Peek>, peek: &present::Peek) {
        model::apply_peek(&self.window, old, peek, &self.drawing);
    }

    fn side(&mut self, old: Option<&present::Side>, side: &present::Side) {
        model::apply_side(&self.window, old, side, &self.drawing);
        self.drawing.order_side(Some(side), None);
        self.side = Some(side.clone());
    }

    fn preferences(
        &mut self,
        old: Option<&present::Preferences>,
        preferences: &present::Preferences,
    ) {
        model::apply_preferences(&self.window, old, preferences, &self.drawing);
    }

    fn usage(&mut self, old: Option<&present::Usage>, usage: &present::Usage) {
        model::apply_usage(&self.window, old, usage, &self.drawing);
    }

    fn entries(&mut self, change: EntriesChange) {
        match change {
            EntriesChange::Reset(entries) => {
                // Shown plain at once, coloured as the thread gets to it.
                self.drawing.renew();
                let rows: Vec<slint_ui::Entry> = entries
                    .iter()
                    .map(|entry| entry.to_slint(&self.drawing))
                    .collect();
                self.rows().set_vec(rows);
                self.shown = entries;
                self.order_all();
            }
            EntriesChange::Push(entry) => {
                order_changed(&self.drawing, self.shown.len(), None, &entry);
                self.rows().push(entry.to_slint(&self.drawing));
                self.shown.push(entry);
            }
            EntriesChange::Set(index, entry) => self.set_entry(index, entry),
        }
    }

    fn show_image(&mut self, key: ImageKey, shown: Result<&Pixels, String>) {
        if let Ok(Some(image)) = shown.map(slint_image) {
            self.drawing.images.insert(key, image);
        }
    }

    fn keep_images(&mut self, shown: &HashSet<ImageKey>) {
        self.drawing.images.retain(|key, _| shown.contains(key));
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
            Command::RecolourCode => self.recolour_code(),
            Command::DrawColouredCode => self.draw_coloured(),
            // A window of its own system's, ended and started again, would open it again.
            Command::SessionShown(_) => {}
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
pub(crate) fn plain(text: &str) -> slint::StyledText {
    slint::StyledText::from_plain_text(&crate::markdown::sanitize(text).replace('\t', "    "))
}

#[cfg(test)]
mod tests {
    use super::{Drawing, Place, colourist, present};

    const AT: Place = Place::Entry { row: 0, block: 0 };

    fn drawing() -> (Drawing, colourist::Colourist) {
        let (events, _received) = async_channel::unbounded();
        let colours = crate::generated::SyntaxColours::default();
        let (orders, colourist) = colourist::spawn(&colours, events);
        (Drawing::new(orders), colourist)
    }

    fn coloured(generation: u64, raw: &str) -> colourist::Coloured {
        colourist::Coloured {
            job: colourist::Job {
                generation,
                place: AT,
                lang: "rust".into(),
                raw: raw.into(),
            },
            text: slint::StyledText::from_plain_text(raw),
        }
    }

    fn code(lang: &str, raw: &str) -> present::MdBlock {
        present::MdBlock {
            kind: present::BlockKind::Code,
            lang: lang.into(),
            raw: raw.into(),
            ..present::MdBlock::default()
        }
    }

    /// A block streaming in is coloured over and over: only what it shows now is kept.
    #[test]
    fn code_coloured_replaces_what_was_coloured_at_its_place() {
        let (mut drawing, _) = drawing();
        assert!(drawing.take(&coloured(0, "fn")));
        assert!(drawing.take(&coloured(0, "fn f")));
        assert!(drawing.coloured_as("rust", "fn").is_none());
        assert!(drawing.coloured_as("rust", "fn f").is_some());
        assert!(drawing.coloured_as("python", "fn f").is_none());
    }

    #[test]
    fn code_coloured_for_rows_gone_is_dropped() {
        let (mut drawing, _) = drawing();
        drawing.renew();
        assert!(!drawing.take(&coloured(0, "fn")));
        assert!(drawing.coloured_as("rust", "fn").is_none());
    }

    /// Code growing keeps its colours until it is coloured again; other code is shown plain.
    #[test]
    fn growing_code_keeps_its_colours() {
        let (mut drawing, _) = drawing();
        let keeps = super::model::keeps_colours;
        assert!(keeps(&code("rust", "fn"), &code("rust", "fn f"), &drawing));
        assert!(!keeps(
            &code("rust", "fn"),
            &code("python", "fn f"),
            &drawing
        ));
        assert!(!keeps(&code("rust", "fn"), &code("rust", "let"), &drawing));
        assert!(drawing.take(&coloured(0, "fn f")));
        assert!(!keeps(&code("rust", "fn"), &code("rust", "fn f"), &drawing));
    }

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
