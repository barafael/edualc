//! What the window's parts are built of: Lucide's icons, buttons, text plain or Markdown, the
//! blocks of a message, and boxes of code.

use std::ops::Range;

use gpui::{
    AnyElement, App, ClickEvent, ElementId, FontWeight, Hsla, InteractiveElement as _, IntoElement,
    ParentElement as _, SharedString, StatefulInteractiveElement as _, Styled as _, Window, div,
    px, svg,
};
use gpui_component::text::TextView;

use super::{Actions, Palette};
use crate::app::Action;
use crate::present::Styled;
use crate::present::model::{BlockKind, Cjk, CjkChars, MdBlock};

/// Lucide's icon `name` (as its file is named), `size` pixels square, in `color`.
pub(crate) fn icon(name: &str, size: f32, color: Hsla) -> impl IntoElement {
    svg()
        .path(SharedString::from(format!("icons/{name}.svg")))
        .size(px(size))
        .flex_none()
        .text_color(color)
}

/// Lucide's icon `name`, as gpui-component's buttons take one.
pub(crate) fn lucide(name: &str) -> gpui_component::Icon {
    gpui_component::Icon::empty().path(SharedString::from(format!("icons/{name}.svg")))
}

/// A button of an icon alone, named for those who can't see it (its tooltip).
pub(crate) fn icon_button(
    id: impl Into<ElementId>,
    name: &str,
    label: impl Into<SharedString>,
    palette: &Palette,
    size: f32,
    on_click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> impl IntoElement {
    let label: SharedString = label.into();
    let hover = palette.hover();
    div()
        .id(id)
        .flex()
        .flex_none()
        .items_center()
        .justify_center()
        .size(px((size * 2.0).max(28.0)))
        .rounded(px(6.0))
        .cursor_pointer()
        .hover(move |style| style.bg(hover))
        .tooltip(move |window, cx| {
            gpui_component::tooltip::Tooltip::new(label.clone()).build(window, cx)
        })
        .on_click(on_click)
        .child(icon(name, size * 1.1, palette.text_secondary))
}

/// A button of words; `primary` in the accent, `danger` in red.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Tone {
    Plain,
    Primary,
    Danger,
}

pub(crate) fn text_button(
    id: impl Into<ElementId>,
    label: impl Into<SharedString>,
    tone: Tone,
    enabled: bool,
    palette: &Palette,
    on_click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> impl IntoElement {
    let (fill, ink, border) = match tone {
        Tone::Plain => (
            gpui::transparent_black(),
            palette.text,
            palette.control_border,
        ),
        Tone::Primary => (palette.accent, palette.on_accent, palette.accent),
        Tone::Danger => (
            gpui::transparent_black(),
            palette.danger_text,
            palette.danger,
        ),
    };
    let hover = palette.hover();
    let button = div()
        .id(id)
        .flex()
        .flex_none()
        .items_center()
        .justify_center()
        .h(px(32.0))
        .px(px(14.0))
        .rounded(px(8.0))
        .border_1()
        .border_color(border)
        .bg(fill)
        .text_color(ink)
        .font_weight(FontWeight::MEDIUM)
        .child(label.into());
    if enabled {
        button
            .cursor_pointer()
            .hover(move |style| {
                if tone == Tone::Primary {
                    style.opacity(0.9)
                } else {
                    style.bg(hover)
                }
            })
            .on_click(on_click)
    } else {
        button.opacity(0.5)
    }
}

/// A small text, in `color`.
pub(crate) fn small(text: impl Into<SharedString>, color: Hsla) -> impl IntoElement {
    div().text_sm().text_color(color).child(text.into())
}

/// Prose set for its CJK characters, as the Slint window sets it (`Theme.cjk-family`,
/// `Theme.line-factor`): where they are its main script, in the Noto Sans CJK of their language,
/// which draws their glyph shapes and whose own Latin was made to sit with them, its lines 1.75
/// em apart (within JLReq's half to one em between lines); other prose 1.5 em apart.
pub(crate) fn prose(element: gpui::Div, cjk: &CjkChars) -> gpui::Div {
    let family = match cjk.language {
        _ if !cjk.main => None,
        Cjk::Japanese => Some("Noto Sans CJK JP"),
        Cjk::SimplifiedChinese => Some("Noto Sans CJK SC"),
        Cjk::TraditionalChinese => Some("Noto Sans CJK TC"),
        Cjk::Korean => Some("Noto Sans CJK KR"),
        Cjk::None => None,
    };
    match family {
        Some(family) => element
            .font_family(family)
            .line_height(gpui::relative(1.75)),
        None => element.line_height(gpui::relative(1.5)),
    }
}

/// An entry of the conversation's text (`body`), set for its CJK characters.
pub(crate) fn entry_text(
    id: impl Into<ElementId>,
    entry: &crate::present::model::Entry,
    actions: &Actions,
) -> AnyElement {
    prose(div(), &entry.cjk)
        .child(styled_text(id, &entry.body, actions))
        .into_any_element()
}

/// Text plain or with inline Markdown, selectable; its links open through the app.
pub(crate) fn styled_text(
    id: impl Into<ElementId>,
    text: &Styled,
    actions: &Actions,
) -> AnyElement {
    match text {
        Styled::Empty => div().into_any_element(),
        Styled::Plain(plain) => plain_text(id, plain),
        Styled::Markdown(source) => markdown(id, source, actions),
    }
}

/// Markdown, selectable. A link opens through the app, which checks it and asks the desktop's
/// portal, as the Slint window does; only a web page's or mail's is left a link at all, and an
/// image is its words (see [`safe_markdown`]).
pub(crate) fn markdown(id: impl Into<ElementId>, source: &str, actions: &Actions) -> AnyElement {
    let opener = OpenLinks(actions.clone());
    TextView::markdown(id, safe_markdown(&crate::markdown::sanitize(source)))
        .selectable(true)
        .on_link_click(move |url, _, _, _| opener.open(url))
        .into_any_element()
}

/// What opens a link: the app, by its channel (which the text view may call from any thread).
struct OpenLinks(Actions);

impl OpenLinks {
    fn open(&self, url: &SharedString) {
        self.0.send(Action::OpenUrl(url.to_string()));
    }
}

/// `source` with every link that isn't to a web page or mail (`desktop::is_web_link`) made its
/// text, and every image its words: the window opens no file or program a message names, and
/// fetches nothing.
pub(crate) fn safe_markdown(source: &str) -> String {
    use pulldown_cmark::{Event, Options, Parser, Tag, TagEnd};

    /// A link or image opened and not yet closed: where it is, whether it stays, and where its
    /// words are.
    struct Open {
        whole: Range<usize>,
        keep: bool,
        words: Option<Range<usize>>,
    }

    let mut open: Vec<Open> = Vec::new();
    // What goes in place of a range of `source`, in the order the ranges close.
    let mut cuts: Vec<(Range<usize>, Range<usize>)> = Vec::new();
    for (event, range) in Parser::new_ext(source, Options::all()).into_offset_iter() {
        match event {
            Event::Start(Tag::Link { dest_url, .. }) => open.push(Open {
                whole: range,
                keep: crate::desktop::is_web_link(&dest_url),
                words: None,
            }),
            Event::Start(Tag::Image { .. }) => open.push(Open {
                whole: range,
                keep: false,
                words: None,
            }),
            Event::End(TagEnd::Link | TagEnd::Image) => {
                let Some(closed) = open.pop() else {
                    continue;
                };
                let words = closed.words.clone().unwrap_or(0..0);
                if let Some(outer) = open.last_mut() {
                    widen(&mut outer.words, &closed.whole);
                }
                if !closed.keep {
                    cuts.push((closed.whole, words));
                }
            }
            Event::Start(_)
            | Event::End(_)
            | Event::Text(_)
            | Event::Code(_)
            | Event::InlineMath(_)
            | Event::DisplayMath(_)
            | Event::Html(_)
            | Event::InlineHtml(_)
            | Event::FootnoteReference(_)
            | Event::SoftBreak
            | Event::HardBreak
            | Event::Rule
            | Event::TaskListMarker(_) => {
                if let Some(innermost) = open.last_mut() {
                    widen(&mut innermost.words, &range);
                }
            }
        }
    }
    // Outermost last: a cut inside one already made is dropped with it.
    cuts.sort_by_key(|(whole, _)| (whole.start, std::cmp::Reverse(whole.end)));
    let mut safe = String::with_capacity(source.len());
    let mut at = 0;
    for (whole, words) in cuts {
        if whole.start < at {
            continue;
        }
        safe.push_str(source.get(at..whole.start).unwrap_or_default());
        safe.push_str(source.get(words).unwrap_or_default());
        at = whole.end;
    }
    safe.push_str(source.get(at..).unwrap_or_default());
    safe
}

/// `words` grown to take in `range`.
fn widen(words: &mut Option<Range<usize>>, range: &Range<usize>) {
    *words = Some(match words.take() {
        Some(words) => words.start.min(range.start)..words.end.max(range.end),
        None => range.clone(),
    });
}

/// Text shown as it is (code, tool output), selectable: a fenced block of Markdown, its fence
/// longer than any run of backticks inside.
pub(crate) fn plain_text(id: impl Into<ElementId>, text: &str) -> AnyElement {
    TextView::markdown(id, fenced(text, ""))
        .selectable(true)
        .into_any_element()
}

/// `text` in a Markdown code fence that nothing in it closes.
pub(crate) fn fenced(text: &str, lang: &str) -> String {
    let longest = text
        .split(|character| character != '`')
        .map(str::len)
        .max()
        .unwrap_or(0);
    let fence = "`".repeat(longest.max(2) + 1);
    let text = crate::markdown::sanitize(text);
    format!("{fence}{lang}\n{}\n{fence}", text.trim_end_matches('\n'))
}

/// What the blocks of a message are drawn with.
pub(crate) struct Ink<'a> {
    pub(crate) palette: &'a Palette,
    pub(crate) actions: &'a Actions,
    pub(crate) size: f32,
}

/// A message's blocks of Markdown, one under the other.
pub(crate) fn blocks(id: &str, blocks: &[MdBlock], ink: &Ink<'_>) -> AnyElement {
    let mut column = div().flex().flex_col().gap(px(ink.size * 0.6));
    for (at, one) in blocks.iter().enumerate() {
        column = column.child(block(SharedString::from(format!("{id}-{at}")), one, ink));
    }
    column.into_any_element()
}

fn block(id: SharedString, block: &MdBlock, ink: &Ink<'_>) -> AnyElement {
    let Ink {
        palette,
        actions,
        size,
    } = *ink;
    match block.kind {
        BlockKind::Text => prose(div(), &block.cjk)
            .child(styled_text(id, &block.body, actions))
            .into_any_element(),
        BlockKind::Heading => {
            let scale = match block.level {
                1 => 1.4,
                2 => 1.25,
                3 => 1.12,
                _ => 1.0,
            };
            prose(div(), &block.cjk)
                .text_size(px(size * scale))
                .font_weight(FontWeight::SEMIBOLD)
                .child(styled_text(id, &block.body, actions))
                .into_any_element()
        }
        BlockKind::Quote => prose(div(), &block.cjk)
            .border_l_2()
            .border_color(palette.divider)
            .pl(px(12.0))
            .text_color(palette.text_secondary)
            .child(styled_text(id, &block.body, actions))
            .into_any_element(),
        BlockKind::Code => {
            let raw = block.raw.clone();
            let copy = actions.clone();
            let mut boxed = div()
                .relative()
                .rounded(px(8.0))
                .bg(palette.code)
                .p(px(4.0))
                .child(
                    TextView::markdown(id.clone(), fenced(&block.raw, &block.lang))
                        .selectable(true),
                );
            if !block.lang.is_empty() {
                boxed = boxed.child(
                    div()
                        .absolute()
                        .top(px(4.0))
                        .right(px(40.0))
                        .text_xs()
                        .text_color(palette.text_faint)
                        .child(block.lang.clone()),
                );
            }
            boxed
                .child(
                    div()
                        .absolute()
                        .top(px(2.0))
                        .right(px(2.0))
                        .child(icon_button(
                            SharedString::from(format!("{id}-copy")),
                            "copy",
                            "Copy",
                            palette,
                            size * 0.8,
                            move |_, _, _| copy.send(Action::Copy(raw.clone())),
                        )),
                )
                .into_any_element()
        }
        // A table, as Markdown draws it; copying takes its Markdown.
        BlockKind::Table => {
            let raw = block.raw.clone();
            let copy = actions.clone();
            div()
                .flex()
                .flex_col()
                .gap(px(2.0))
                .child(markdown(id.clone(), &block.raw, actions))
                .child(icon_button(
                    SharedString::from(format!("{id}-copy")),
                    "copy",
                    "Copy table",
                    palette,
                    size * 0.8,
                    move |_, _, _| copy.send(Action::Copy(raw.clone())),
                ))
                .into_any_element()
        }
        BlockKind::Rule => div()
            .h(px(1.0))
            .my(px(6.0))
            .bg(palette.divider)
            .into_any_element(),
    }
}

/// A box of code or output: plain text in the code font, with a note under it and, for a
/// conversation's entry (`copy`, its index), a button to copy it.
pub(crate) struct CodeBox<'a> {
    pub(crate) id: SharedString,
    pub(crate) body: &'a Styled,
    pub(crate) color: Hsla,
    pub(crate) note: &'a str,
    pub(crate) copy: Option<usize>,
}

impl CodeBox<'_> {
    pub(crate) fn draw(self, ink: &Ink<'_>) -> AnyElement {
        let Self {
            id,
            body,
            color,
            note,
            copy,
        } = self;
        let Ink {
            palette,
            actions,
            size,
        } = *ink;
        let text = match body {
            Styled::Empty => String::new(),
            Styled::Plain(text) | Styled::Markdown(text) => text.clone(),
        };
        let mut boxed = div()
            .relative()
            .rounded(px(8.0))
            .bg(palette.code)
            .text_color(color)
            .p(px(4.0))
            .child(plain_text(id.clone(), &text));
        if !note.is_empty() {
            boxed = boxed.child(
                div()
                    .px(px(8.0))
                    .pb(px(4.0))
                    .text_sm()
                    .text_color(palette.text_faint)
                    .child(note.to_owned()),
            );
        }
        if let Some(index) = copy {
            let actions = actions.clone();
            boxed = boxed.child(
                div()
                    .absolute()
                    .top(px(2.0))
                    .right(px(2.0))
                    .child(icon_button(
                        SharedString::from(format!("{id}-copy")),
                        "copy",
                        "Copy",
                        palette,
                        size * 0.8,
                        move |_, _, _| actions.send(Action::CopyEntry(index)),
                    )),
            );
        }
        boxed.into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::safe_markdown;

    /// Links to web pages and mail stay links; any other becomes its words, and an image its
    /// description, so nothing a message names is opened or fetched.
    #[test]
    fn only_web_links_stay_links() {
        assert_eq!(
            safe_markdown("see [the docs](https://example.com) now"),
            "see [the docs](https://example.com) now"
        );
        assert_eq!(
            safe_markdown("[mail](mailto:a@b.c)"),
            "[mail](mailto:a@b.c)"
        );
        assert_eq!(
            safe_markdown("open [this](file:///etc/passwd) please"),
            "open this please"
        );
        assert_eq!(safe_markdown("run [`it`](ssh://host)"), "run `it`");
        assert_eq!(safe_markdown("![a cat](https://e.com/cat.png)"), "a cat");
        assert_eq!(safe_markdown("<file:///etc/hosts>"), "file:///etc/hosts");
        assert_eq!(safe_markdown("<https://e.com>"), "<https://e.com>");
        assert_eq!(
            safe_markdown("[![logo](x.png)](https://e.com)"),
            "[logo](https://e.com)"
        );
        assert_eq!(safe_markdown("no links at all"), "no links at all");
    }
}
