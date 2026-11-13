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
use crate::colour::{self, BlockAt, Coloured, Job};
use crate::colouring::Colouring;
use crate::decode::Pixels;
use crate::generated::{self as slint_ui, AppWindow};
use crate::highlight::{self, Highlighter};
use crate::present::model as present;
use crate::present::{Command, EntriesChange, Frontend, Image, Look, Styled};

mod callbacks;
mod model;

pub(crate) use callbacks::wire;

/// What the presentation model's values are drawn with: the conversation's images, decoded (one
/// not here isn't decoded yet), and code's colours.
pub(crate) struct Drawing {
    images: HashMap<ImageKey, slint::Image>,
    highlighter: Highlighter,
    /// The conversation's code is left plain as its blocks are made, and coloured on a thread of
    /// its own (see `colouring`); other code (the side panel's, a plan's) is coloured here.
    code_later: bool,
}

impl Drawing {
    fn new(colours: &slint_ui::SyntaxColours) -> Self {
        Self {
            images: HashMap::new(),
            highlighter: Highlighter::new(colours),
            code_later: false,
        }
    }
}

/// A Markdown block's body as the Slint window draws it: a text block with the space between its
/// paragraphs that Slint leaves out, code in colour (the conversation's later, by the thread).
fn md_body(block: &present::MdBlock, drawing: &Drawing) -> slint::StyledText {
    match (block.kind, &block.body) {
        (present::BlockKind::Text, Styled::Markdown(text)) => {
            Styled::Markdown(crate::markdown::spaced_for_slint(text)).to_slint(drawing)
        }
        (present::BlockKind::Code, _) if !drawing.code_later => {
            drawing.highlighter.code(&block.lang, &block.raw, &mut None)
        }
        _ => block.body.to_slint(drawing),
    }
}

/// How a code block of the conversation is shown: as it comes, or in colour.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CodeShown {
    Plain,
    Coloured,
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
    /// Colours the conversation's code on a thread of its own.
    colouring: Colouring,
    /// Which drawing of the conversation's blocks is shown: colourings for another are dropped.
    epoch: colour::Drawing,
    /// How each of a row's code blocks is shown, one for each block.
    code: Vec<Vec<CodeShown>>,
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
        let colours = window.global::<slint_ui::Theme>().get_syntax();
        let drawing = Drawing::new(&colours);
        let colouring = Colouring::start(&colours, events);
        Self {
            window,
            drawing,
            colouring,
            epoch: colour::Drawing::default(),
            code: Vec::new(),
            entries,
            shown: Vec::new(),
            prompt: None,
            side: None,
        }
    }

    /// Drops what the conversation's blocks are drawn as; colourings still on their way are for
    /// the old drawing.
    fn forget_blocks(&mut self) {
        self.epoch = self.epoch.next();
        self.colouring.forget();
        for row in &mut self.code {
            row.fill(CodeShown::Plain);
        }
    }

    /// The colours to ask for of the code blocks of entry `index` that differ from `old`'s (all
    /// of them, for none).
    fn jobs(&self, index: usize, old: Option<&present::Entry>) -> Vec<Job> {
        let Some(entry) = self.shown.get(index) else {
            return Vec::new();
        };
        entry
            .blocks
            .iter()
            .enumerate()
            .filter(|(position, block)| {
                block.kind == present::BlockKind::Code
                    && old.and_then(|old| old.blocks.get(*position)) != Some(*block)
            })
            .map(|(position, block)| Job {
                at: BlockAt {
                    entry: index,
                    block: position,
                },
                drawing: self.epoch,
                lang: block.lang.clone(),
                code: block.raw.clone(),
            })
            .collect()
    }

    /// Code's colours changed (the look, or the desktop's dark or light): what is shown is drawn
    /// again in them.
    fn recolour_code(&mut self) {
        let colours = self.window.global::<slint_ui::Theme>().get_syntax();
        self.drawing.highlighter.set_colours(&colours);
        self.colouring.set_colours(&colours);
        self.forget_blocks();
        self.drawing.code_later = true;
        let rows: Vec<slint_ui::Entry> = self
            .shown
            .iter()
            .map(|entry| entry.to_slint(&self.drawing))
            .collect();
        self.drawing.code_later = false;
        self.rows().set_vec(rows);
        for index in 0..self.shown.len() {
            for job in self.jobs(index, None) {
                self.colouring.ask(job);
            }
        }
        if let Some(side) = &self.side {
            model::apply_side(&self.window, None, side, &self.drawing);
        }
        if let Some(prompt) = &self.prompt {
            model::apply_prompt(&self.window, None, prompt, &self.drawing);
        }
    }

    /// Entry `index`, changed: its row brought in line, and the colours of its code blocks that
    /// changed asked for again. A block that only grew keeps its colours until its new ones come.
    fn set_entry(&mut self, index: usize, entry: present::Entry) {
        let (Some(row), Some(old)) = (self.rows().row_data(index), self.shown.get(index)) else {
            return;
        };
        // The blocks' rows as they are shown, before they are changed in place.
        let before: Vec<slint_ui::MdBlock> = (0..row.blocks.row_count())
            .filter_map(|at| row.blocks.row_data(at))
            .collect();
        let synced = entry.sync(old, row, &self.drawing);
        if let Some(code) = self.code.get_mut(index) {
            code.resize(entry.blocks.len(), CodeShown::Plain);
        }
        if let Some(blocks) = synced
            .blocks
            .as_any()
            .downcast_ref::<VecModel<slint_ui::MdBlock>>()
        {
            for (position, block) in entry.blocks.iter().enumerate() {
                if block.kind != present::BlockKind::Code || old.blocks.get(position) == Some(block)
                {
                    continue;
                }
                let kept = self
                    .code
                    .get(index)
                    .and_then(|code| code.get(position))
                    .is_some_and(|shown| *shown == CodeShown::Coloured)
                    .then(|| before.get(position))
                    .flatten()
                    .filter(|before| {
                        before.lang.as_str() == block.lang
                            && block.raw.starts_with(before.raw.as_str())
                    });
                let slot = self
                    .code
                    .get_mut(index)
                    .and_then(|code| code.get_mut(position));
                match (kept, blocks.row_data(position)) {
                    (Some(before), Some(mut shown)) => {
                        shown.body = before.body.clone();
                        blocks.set_row_data(position, shown);
                    }
                    (None, _) | (Some(_), None) => {
                        if let Some(slot) = slot {
                            *slot = CodeShown::Plain;
                        }
                    }
                }
            }
        }
        self.rows().set_row_data(index, synced);
        if let Some(slot) = self.shown.get_mut(index) {
            let old = std::mem::replace(slot, entry);
            for job in self.jobs(index, Some(&old)) {
                self.colouring.ask(job);
            }
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
        self.prompt = Some(prompt.clone());
    }

    fn peek(&mut self, old: Option<&present::Peek>, peek: &present::Peek) {
        model::apply_peek(&self.window, old, peek, &self.drawing);
    }

    fn side(&mut self, old: Option<&present::Side>, side: &present::Side) {
        model::apply_side(&self.window, old, side, &self.drawing);
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
        // The conversation's code is left plain, and coloured by the thread, as the rows are made.
        self.drawing.code_later = true;
        match change {
            EntriesChange::Reset(entries) => {
                let rows: Vec<slint_ui::Entry> = entries
                    .iter()
                    .map(|entry| entry.to_slint(&self.drawing))
                    .collect();
                self.rows().set_vec(rows);
                self.code = entries
                    .iter()
                    .map(|entry| vec![CodeShown::Plain; entry.blocks.len()])
                    .collect();
                self.shown = entries;
                self.forget_blocks();
                for index in 0..self.shown.len() {
                    for job in self.jobs(index, None) {
                        self.colouring.ask(job);
                    }
                }
            }
            EntriesChange::Push(entry) => {
                let index = self.shown.len();
                self.rows().push(entry.to_slint(&self.drawing));
                self.code.push(vec![CodeShown::Plain; entry.blocks.len()]);
                self.shown.push(entry);
                for job in self.jobs(index, None) {
                    self.colouring.ask(job);
                }
            }
            EntriesChange::Set(index, entry) => self.set_entry(index, entry),
        }
        self.drawing.code_later = false;
    }

    /// A code block of the conversation, coloured; dropped if the block has changed since, as a
    /// later colouring of it is on its way.
    fn code_coloured(&mut self, coloured: Coloured) {
        let Coloured {
            at,
            drawing,
            code: asked,
            markdown,
        } = coloured;
        if drawing != self.epoch {
            return;
        }
        let Some(block) = self
            .shown
            .get(at.entry)
            .and_then(|entry| entry.blocks.get(at.block))
        else {
            return;
        };
        if block.kind != present::BlockKind::Code || block.raw != asked {
            return;
        }
        let Some(row) = self.rows().row_data(at.entry) else {
            return;
        };
        let Some(blocks) = row
            .blocks
            .as_any()
            .downcast_ref::<VecModel<slint_ui::MdBlock>>()
        else {
            return;
        };
        let Some(mut shown) = blocks.row_data(at.block) else {
            return;
        };
        let (body, how) = match markdown.and_then(|source| highlight::parsed(&source)) {
            Some(body) => (body, CodeShown::Coloured),
            // Grown too long to colour, say: plain, not its colours of before.
            None => (plain(&block.raw), CodeShown::Plain),
        };
        shown.body = body;
        blocks.set_row_data(at.block, shown);
        if let Some(slot) = self
            .code
            .get_mut(at.entry)
            .and_then(|row| row.get_mut(at.block))
        {
            *slot = how;
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
