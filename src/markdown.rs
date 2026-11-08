//! Assistant text as blocks. Slint's `StyledText` takes only inline Markdown (emphasis, code,
//! links) and lists, so the blocks it can't show (headings, code, quotes, tables, rules) are
//! split off here and drawn by the UI; the rest goes to Slint as Markdown source.

use std::ops::Range;

use pulldown_cmark::{CodeBlockKind, Event, Options, Parser, Tag, TagEnd};

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Block {
    /// Markdown source for `StyledText`: a run of paragraphs and lists from [`blocks`], a
    /// paragraph or an item from [`sections`].
    Text(String),
    /// Level 1–6 and the heading's inline Markdown source.
    Heading(u8, String),
    Code {
        lang: String,
        code: String,
    },
    /// The quote's Markdown source, without the `>` markers.
    Quote(String),
    Table(Table),
    Rule,
}

/// A table, shown as a grid whose cells wrap.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Table {
    /// The header row first; each cell as its inline Markdown source.
    pub(crate) rows: Vec<Vec<String>>,
    /// Per column, the characters its longest cell shows. A table too wide for the window
    /// narrows each column by its share of these, so a short column keeps its width.
    pub(crate) longest_cell_chars: Vec<usize>,
    /// The table's Markdown source, for copying.
    pub(crate) source: String,
}

/// Tables are cut to this many rows and columns: every cell is an element of the window.
const TABLE_ROWS: usize = 500;
const TABLE_COLUMNS: usize = 24;

/// A block as [`split`] finds it: paragraphs and lists apart from other text, for [`blocks`]
/// to join.
enum Part {
    Paragraph(String),
    List(String),
    Shown(Block),
}

impl Part {
    fn into_block(self) -> Block {
        match self {
            Self::Paragraph(source) | Self::List(source) => Block::Text(source),
            Self::Shown(block) => block,
        }
    }
}

/// What goes between a text's paragraphs, and before a list: a blank line, as Markdown has it.
const BETWEEN: &str = "\n\n";
/// Slint draws a text's paragraphs with no space between them. A paragraph that opens with a
/// hard break (`\` ending a line) opens with an empty line, which copies as a blank one too.
/// Only what draws with Slint does this (see [`spaced_for_slint`]): elsewhere it would be a
/// stray line break.
const BEFORE_PARAGRAPH_IN_SLINT: &str = "\n\n\\\n";

/// Each run of paragraphs and lists is one block, so that a selection can span it.
pub(crate) fn blocks(src: &str) -> Vec<Block> {
    let mut out = Vec::new();
    let mut run: Option<String> = None;
    for (part, _) in split(src, false) {
        match part {
            // Checked with the break it may get: a line of `=` under it would make a heading.
            Part::Paragraph(source)
                if accepted(&format!("{BEFORE_PARAGRAPH_IN_SLINT}{source}")) =>
            {
                join(&mut run, BETWEEN, source);
            }
            Part::List(source) if accepted(&source) => join(&mut run, BETWEEN, source),
            // Text Slint refuses is shown plain, and on its own: in a run, all of it would be.
            part @ (Part::Paragraph(_) | Part::List(_) | Part::Shown(_)) => {
                out.extend(run.take().map(Block::Text));
                out.push(part.into_block());
            }
        }
    }
    out.extend(run.map(Block::Text));
    out
}

/// A text block's Markdown as Slint draws paragraphs: each after the first opening with a hard
/// break, so that there is space between them. Everything else is as it was.
pub(crate) fn spaced_for_slint(text: &str) -> String {
    let mut spaced = String::with_capacity(text.len());
    for (part, _) in split(text, false) {
        let (separator, source) = match part {
            Part::Paragraph(source) => (BEFORE_PARAGRAPH_IN_SLINT, source),
            Part::List(source) => (BETWEEN, source),
            // A text block holds only paragraphs and lists; anything else is left as written.
            Part::Shown(block) => (BETWEEN, block_source(block)),
        };
        if !spaced.is_empty() {
            spaced.push_str(separator);
        }
        spaced.push_str(&source);
    }
    spaced
}

/// What a block shown as it is says, as text.
fn block_source(block: Block) -> String {
    match block {
        Block::Text(source) | Block::Quote(source) | Block::Heading(_, source) => source,
        Block::Code { code, .. } => code,
        Block::Table(_) | Block::Rule => String::new(),
    }
}

fn join(run: &mut Option<String>, separator: &str, source: String) {
    match run {
        Some(joined) => {
            joined.push_str(separator);
            joined.push_str(&source);
        }
        None => *run = Some(source),
    }
}

/// The parts of a plan someone can comment on: [`blocks`], with every item of a list on its
/// own, and each with where its Markdown source is in `src` (to quote it back, or note
/// something under it). Text Markdown shows nothing of (a link's definition, say) is a part
/// too, shown as it is written: what is approved is all seen.
pub(crate) fn sections(src: &str) -> Vec<(Block, Range<usize>)> {
    let parts = split(src, true);
    let mut out = Vec::with_capacity(parts.len());
    let mut shown_to = 0;
    for (part, range) in parts {
        if let Some(hidden) = hidden_between(src, shown_to..range.start) {
            out.push(hidden);
        }
        shown_to = shown_to.max(range.end);
        out.push((part.into_block(), range));
    }
    out.extend(hidden_between(src, shown_to..src.len()));
    out
}

/// Source between two shown parts that isn't only blank: what Markdown hides.
fn hidden_between(src: &str, between: Range<usize>) -> Option<(Block, Range<usize>)> {
    let text = src.get(between.clone())?.trim();
    (!text.is_empty()).then(|| {
        let code = Block::Code {
            lang: "hidden by Markdown".to_owned(),
            code: text.to_owned(),
        };
        (code, between)
    })
}

/// A source range without its trailing blanks.
fn trimmed(src: &str, range: &Range<usize>) -> Range<usize> {
    let text = src.get(range.clone()).unwrap_or("");
    range.start..range.start + text.trim_end().len()
}

fn split(src: &str, items: bool) -> Vec<(Part, Range<usize>)> {
    let mut options = Options::empty();
    options.insert(Options::ENABLE_TABLES);
    options.insert(Options::ENABLE_STRIKETHROUGH);
    options.insert(Options::ENABLE_TASKLISTS);
    let mut out = Vec::new();
    // Where each task list item's `[ ]` or `[x]` is, and whether it is checked.
    let mut markers: Vec<(Range<usize>, bool)> = Vec::new();
    let mut depth = 0usize;
    // Where the open top-level block started; code and tables are pushed at their end.
    let mut open = 0..0;
    let mut in_list = false;
    let mut code: Option<(String, String)> = None;
    let mut table: Option<TableParse> = None;
    for (event, range) in Parser::new_ext(src, options).into_offset_iter() {
        let range = trimmed(src, &range);
        // Copied only for the blocks pushed, not for every event of the parse.
        let source = || src.get(range.clone()).unwrap_or("").to_owned();
        match event {
            Event::Start(tag) => {
                if depth == 0 {
                    open = range.clone();
                    match &tag {
                        Tag::CodeBlock(kind) => {
                            let lang = match kind {
                                CodeBlockKind::Fenced(info) => {
                                    info.split_whitespace().next().unwrap_or("").to_owned()
                                }
                                CodeBlockKind::Indented => String::new(),
                            };
                            code = Some((lang, String::new()));
                        }
                        Tag::Table(_) => table = Some(TableParse::default()),
                        Tag::Heading { level, .. } => out.push((
                            Part::Shown(Block::Heading(*level as u8, heading_text(&source()))),
                            range.clone(),
                        )),
                        Tag::BlockQuote(_) => {
                            out.push((Part::Shown(Block::Quote(unquote(&source()))), range.clone()))
                        }
                        Tag::List(_) if items => in_list = true,
                        // In a plan, HTML (a comment, say) is shown as it is written: as
                        // Markdown it would show nothing of what is approved.
                        Tag::HtmlBlock if items => out.push((
                            Part::Shown(Block::Code {
                                lang: "html".to_owned(),
                                code: source(),
                            }),
                            range.clone(),
                        )),
                        Tag::Paragraph => out.push((Part::Paragraph(source()), range.clone())),
                        Tag::List(_) => out.push((Part::List(source()), range.clone())),
                        // HTML; inline tags never stand at the top.
                        Tag::HtmlBlock
                        | Tag::Item
                        | Tag::FootnoteDefinition(_)
                        | Tag::DefinitionList
                        | Tag::DefinitionListTitle
                        | Tag::DefinitionListDefinition
                        | Tag::TableHead
                        | Tag::TableRow
                        | Tag::TableCell
                        | Tag::Emphasis
                        | Tag::Strong
                        | Tag::Strikethrough
                        | Tag::Superscript
                        | Tag::Subscript
                        | Tag::Link { .. }
                        | Tag::Image { .. }
                        | Tag::MetadataBlock(_) => {
                            out.push((Part::Shown(Block::Text(source())), range.clone()));
                        }
                    }
                } else if depth == 1 && in_list && matches!(tag, Tag::Item) {
                    out.push((Part::Shown(Block::Text(source())), range.clone()));
                }
                if let Some(table) = table.as_mut() {
                    match tag {
                        Tag::TableHead | Tag::TableRow => table.start_row(),
                        Tag::TableCell => table.start_cell(src.get(range.clone()).unwrap_or("")),
                        Tag::Paragraph
                        | Tag::Heading { .. }
                        | Tag::BlockQuote(_)
                        | Tag::CodeBlock(_)
                        | Tag::HtmlBlock
                        | Tag::List(_)
                        | Tag::Item
                        | Tag::FootnoteDefinition(_)
                        | Tag::DefinitionList
                        | Tag::DefinitionListTitle
                        | Tag::DefinitionListDefinition
                        | Tag::Table(_)
                        | Tag::Emphasis
                        | Tag::Strong
                        | Tag::Strikethrough
                        | Tag::Superscript
                        | Tag::Subscript
                        | Tag::Link { .. }
                        | Tag::Image { .. }
                        | Tag::MetadataBlock(_) => {}
                    }
                }
                depth += 1;
            }
            Event::End(end) => {
                depth = depth.saturating_sub(1);
                if matches!(end, TagEnd::TableCell)
                    && let Some(table) = table.as_mut()
                {
                    table.end_cell();
                }
                if depth == 0 {
                    in_list = false;
                    let whole = trimmed(src, &open);
                    if let Some((lang, text)) = code.take() {
                        let code = text.strip_suffix('\n').unwrap_or(&text).to_owned();
                        out.push((Part::Shown(Block::Code { lang, code }), whole.clone()));
                    }
                    if matches!(end, TagEnd::Table)
                        && let Some(table) = table.take()
                    {
                        let source = src.get(whole.clone()).unwrap_or("").to_owned();
                        out.push((Part::Shown(Block::Table(table.finish(source))), whole));
                    }
                }
            }
            Event::Text(text) => {
                if let Some((_, code)) = code.as_mut() {
                    code.push_str(&text);
                } else if let Some(table) = table.as_mut() {
                    table.count(&text);
                }
            }
            Event::Code(text) => {
                if let Some(table) = table.as_mut() {
                    table.count(&text);
                }
            }
            Event::TaskListMarker(checked) => markers.push((range, checked)),
            Event::Rule if depth == 0 => out.push((Part::Shown(Block::Rule), range)),
            Event::Html(_) | Event::InlineHtml(_) if depth == 0 => {
                out.push((Part::Shown(Block::Text(source())), range));
            }
            Event::Rule
            | Event::Html(_)
            | Event::InlineHtml(_)
            | Event::InlineMath(_)
            | Event::DisplayMath(_)
            | Event::FootnoteReference(_)
            | Event::SoftBreak
            | Event::HardBreak => {}
        }
    }
    if !markers.is_empty() {
        for (part, range) in &mut out {
            if let Part::List(source) | Part::Shown(Block::Text(source)) = part
                && let Some(boxed) = with_task_boxes(src, range, &markers)
            {
                *source = boxed;
            }
        }
    }
    out
}

/// The source in `range` with each task marker in it (`[ ]`, `[x]`) as the box it stands for,
/// since Slint's own Markdown knows no task lists and would show the brackets; none if `range`
/// holds no marker.
fn with_task_boxes(
    src: &str,
    range: &Range<usize>,
    markers: &[(Range<usize>, bool)],
) -> Option<String> {
    let inside: Vec<&(Range<usize>, bool)> = markers
        .iter()
        .filter(|(marker, _)| range.start <= marker.start && marker.end <= range.end)
        .collect();
    if inside.is_empty() {
        return None;
    }
    let mut boxed = String::with_capacity(range.len());
    let mut at = range.start;
    for (marker, checked) in inside {
        boxed.push_str(src.get(at..marker.start)?);
        boxed.push(if *checked { '☑' } else { '☐' });
        at = marker.end;
    }
    boxed.push_str(src.get(at..range.end)?);
    Some(without_bullets(&boxed).unwrap_or(boxed))
}

/// A list of nothing but one-line tasks as lines of boxes, which need no bullet beside them
/// (Slint draws one for any list item); none for a list holding anything else.
fn without_bullets(list: &str) -> Option<String> {
    let tasks: Option<Vec<&str>> = list
        .lines()
        .map(|line| {
            let task = ["- ", "* ", "+ "]
                .iter()
                .find_map(|bullet| line.strip_prefix(bullet))?;
            task.starts_with(['☐', '☑']).then_some(task)
        })
        .collect();
    // A hard break keeps them on their own lines, where they aren't list items any more.
    Some(tasks?.join("\\\n"))
}

/// A table as far as it is parsed.
#[derive(Default)]
struct TableParse {
    rows: Vec<Vec<String>>,
    longest_cell_chars: Vec<usize>,
    /// Rows past [`TABLE_ROWS`]: counted, to say they are cut.
    rows_cut: usize,
    /// The cell being parsed, if it is shown.
    cell: Option<ShownCell>,
}

struct ShownCell {
    column: usize,
    /// The characters it shows so far.
    chars: usize,
}

impl TableParse {
    fn start_row(&mut self) {
        if self.rows.len() < TABLE_ROWS {
            self.rows.push(Vec::new());
        } else {
            self.rows_cut += 1;
        }
    }

    /// A cell past the columns shown is left out; the first of them is shown as `…`.
    fn start_cell(&mut self, source: &str) {
        self.cell = None;
        // Once a row is cut, so are all after it.
        if self.rows_cut > 0 {
            return;
        }
        let Some(row) = self.rows.last_mut() else {
            return;
        };
        let column = row.len();
        if column < TABLE_COLUMNS {
            // The source starts at the space after the cell's `|`. A `\|` only keeps a pipe from
            // ending the cell: GFM takes the backslash away before parsing the cell, so that
            // inline code shows the pipe alone too, and the cell is parsed without the table.
            row.push(source.trim().replace("\\|", "|"));
            self.cell = Some(ShownCell { column, chars: 0 });
        } else if column == TABLE_COLUMNS {
            row.push("…".to_owned());
        } else {
            return;
        }
        if self.longest_cell_chars.len() <= column {
            self.longest_cell_chars.resize(column + 1, 1);
        }
    }

    /// Text the cell being parsed shows.
    fn count(&mut self, text: &str) {
        if let Some(cell) = self.cell.as_mut() {
            cell.chars += text.chars().count();
        }
    }

    fn end_cell(&mut self) {
        if let Some(cell) = self.cell.take()
            && let Some(longest) = self.longest_cell_chars.get_mut(cell.column)
        {
            *longest = (*longest).max(cell.chars);
        }
    }

    fn finish(mut self, source: String) -> Table {
        if self.rows_cut > 0 {
            self.rows
                .push(vec![format!("… {} more rows", self.rows_cut)]);
        }
        Table {
            rows: self.rows,
            longest_cell_chars: self.longest_cell_chars,
            source,
        }
    }
}

/// An ATX heading's text without its `#`s; a setext heading's first line.
fn heading_text(source: &str) -> String {
    let first = source.lines().next().unwrap_or("").trim();
    first
        .trim_start_matches('#')
        .trim_end_matches('#')
        .trim()
        .to_owned()
}

fn unquote(source: &str) -> String {
    source
        .lines()
        .map(|line| {
            let line = line.trim_start();
            let line = line.strip_prefix('>').unwrap_or(line);
            line.strip_prefix(' ').unwrap_or(line)
        })
        .collect::<Vec<_>>()
        .join("\n")
        .trim_end()
        .to_owned()
}

/// Whether the window's inline Markdown takes `source` (after [`sanitize`]): paragraphs and
/// lists with emphasis, strikethrough, links, code spans, `<u>` and `<font>`, and nothing else
/// (no headings, quotes, code blocks, tables, rules, images, other HTML). As Slint's own parser
/// takes it, which is pulldown-cmark's with strikethrough on (checked against it in `ui`): text
/// it refuses is shown plain.
pub(crate) fn accepted(source: &str) -> bool {
    use pulldown_cmark::{Event, Options, Parser, Tag};
    let source = sanitize(source);
    Parser::new_ext(&source, Options::ENABLE_STRIKETHROUGH).all(|event| match event {
        Event::Start(
            Tag::Paragraph
            | Tag::Item
            | Tag::List(_)
            | Tag::Strong
            | Tag::Emphasis
            | Tag::Strikethrough
            | Tag::Link { .. },
        )
        | Event::End(_)
        | Event::Text(_)
        | Event::Code(_)
        | Event::SoftBreak
        | Event::HardBreak => true,
        Event::InlineHtml(html) => {
            let html = html.trim();
            html == "<u>"
                || html == "</u>"
                || html == "</font>"
                || (html.starts_with("<font color=") && html.ends_with('>'))
        }
        Event::Start(_)
        | Event::Html(_)
        | Event::Rule
        | Event::TaskListMarker(_)
        | Event::FootnoteReference(_)
        | Event::InlineMath(_)
        | Event::DisplayMath(_) => false,
    })
}

/// Slint reserves U+E541 as its placeholder for arguments, and a carriage return would end a
/// line in the middle of one.
pub(crate) fn sanitize(text: &str) -> String {
    text.chars()
        .filter(|&c| c != '\r')
        .map(|c| if c == '\u{e541}' { '\u{fffd}' } else { c })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_blocks() {
        let src = "# Title\n\nSome *text* here.\n\n```rust\nfn main() {}\n```\n\n- a\n- b\n\n> quoted\n> more\n\n---\n";
        assert_eq!(
            blocks(src),
            vec![
                Block::Heading(1, "Title".into()),
                Block::Text("Some *text* here.".into()),
                Block::Code {
                    lang: "rust".into(),
                    code: "fn main() {}".into()
                },
                Block::Text("- a\n- b".into()),
                Block::Quote("quoted\nmore".into()),
                Block::Rule,
            ]
        );
    }

    /// A text's paragraphs are apart as Markdown has them, with no break Slint alone needs.
    #[test]
    fn paragraphs_are_apart_by_a_blank_line_alone() {
        let blocks = blocks("one\n\ntwo\n\n- x\n\nthree");
        assert_eq!(
            blocks,
            vec![Block::Text("one\n\ntwo\n\n- x\n\nthree".into())]
        );
        let Some(Block::Text(text)) = blocks.first() else {
            panic!("a text block");
        };
        assert!(!text.contains("\\\n"), "no hard break in {text:?}");
    }

    /// Slint draws a paragraph after the first with a hard break, to put space between them.
    #[test]
    fn slint_is_given_the_hard_breaks_it_needs() {
        assert_eq!(
            spaced_for_slint("one\n\ntwo\n\n- x\n\nthree"),
            "one\n\n\\\ntwo\n\n- x\n\n\\\nthree"
        );
        assert_eq!(spaced_for_slint("alone"), "alone");
        assert_eq!(spaced_for_slint(""), "");
    }

    #[test]
    fn unclosed_fence_while_streaming_is_code() {
        assert_eq!(
            blocks("Look:\n\n```py\nprint(1)\n"),
            vec![
                Block::Text("Look:".into()),
                Block::Code {
                    lang: "py".into(),
                    code: "print(1)".into()
                },
            ]
        );
    }

    #[test]
    fn plan_sections_split_lists_into_items() {
        let src = "## Steps\n\n1. First\n2. Second\n   - detail\n\nDone.\n";
        let sections: Vec<&str> = sections(src)
            .into_iter()
            .filter_map(|(_, range)| src.get(range))
            .collect();
        assert_eq!(
            sections,
            ["## Steps", "1. First", "2. Second\n   - detail", "Done."]
        );
    }

    /// Inline Markdown as the Slint window draws it.
    fn styled(source: &str) -> slint::StyledText {
        let source = sanitize(source);
        slint::StyledText::from_markdown(&source)
            .unwrap_or_else(|_| slint::StyledText::from_plain_text(&source))
    }

    /// The lines Slint draws a text in, as its paragraphs; copying joins them with `\n`.
    fn drawn_lines(source: &str) -> Vec<String> {
        let shown = format!("{:?}", styled(source));
        shown
            .split("StyledTextParagraph { text: ")
            .skip(1)
            .filter_map(|paragraph| paragraph.split(", formatting:").next())
            .map(|quoted| quoted.trim_matches('"').to_owned())
            .collect()
    }

    /// One text, so a selection spans it; as Slint draws it (`spaced_for_slint`), an empty line
    /// before each paragraph after the first, none before a list.
    #[test]
    fn a_run_of_paragraphs_and_lists_is_one_text() {
        let src =
            "First.\n\nSecond *one*.\n\n- a\n- b\n\nAfter the list.\n\n```\ncode\n```\n\nAlone.";
        let blocks = blocks(src);
        let [Block::Text(run), Block::Code { .. }, Block::Text(alone)] = blocks.as_slice() else {
            panic!("a run, code, a paragraph: not {blocks:?}")
        };
        assert_eq!(
            drawn_lines(&spaced_for_slint(run)),
            [
                "First.",
                "",
                "Second one.",
                "• a",
                "• b",
                "",
                "After the list."
            ]
        );
        assert_eq!(drawn_lines(alone), ["Alone."]);
    }

    /// Task list items show boxes, checked or not, not the brackets they are written with; a
    /// bracket elsewhere stays as written.
    #[test]
    fn task_list_items_show_boxes() {
        let src = "Steps:\n\n- [ ] a task\n- [x] a done task\n- [X] also done\n- a [ ] in the text";
        let blocks = blocks(src);
        let [Block::Text(run)] = blocks.as_slice() else {
            panic!("one run, not {blocks:?}");
        };
        assert_eq!(
            drawn_lines(run),
            [
                "Steps:",
                "• ☐ a task",
                "• ☑ a done task",
                "• ☑ also done",
                "• a [ ] in the text"
            ]
        );
        // In a plan, the item is shown with its box and quoted back as written.
        let plan = "- [x] Add the handle";
        let sections = sections(plan);
        let [(Block::Text(shown), range)] = sections.as_slice() else {
            panic!("one item, not {sections:?}");
        };
        assert_eq!(shown, "☑ Add the handle");
        assert_eq!(plan.get(range.clone()), Some("- [x] Add the handle"));
    }

    /// A box stands in for the bullet in a list of tasks; where anything else is in the list,
    /// the items keep their bullets and it stays a list.
    #[test]
    fn a_list_of_tasks_has_boxes_instead_of_bullets() {
        let drawn = |src: &str| {
            let blocks = blocks(src);
            let [Block::Text(run)] = blocks.as_slice() else {
                panic!("one run, not {blocks:?}");
            };
            drawn_lines(run)
        };
        assert_eq!(
            drawn("- [ ] a task\n- [x] **done**"),
            ["☐ a task", "☑ done"]
        );
        assert_eq!(
            drawn("- [ ] a task\n  - [x] nested"),
            ["• ☐ a task", "    ◦ ☑ nested"]
        );
    }

    /// In a run, text Slint refuses would turn all of it into plain text, its joins included.
    #[test]
    fn text_slint_refuses_stands_alone() {
        for refused in ["A stray <T> here.", "==="] {
            let src = format!("Before *this*.\n\n{refused}\n\nAfter *that*.");
            assert_eq!(
                blocks(&src),
                [
                    Block::Text("Before *this*.".into()),
                    Block::Text(refused.into()),
                    Block::Text("After *that*.".into()),
                ],
                "{refused}"
            );
        }
    }

    /// A plan's parts are commented on one by one.
    #[test]
    fn plan_sections_stay_apart() {
        let src = "First.\n\nSecond.";
        let sections: Vec<&str> = sections(src)
            .into_iter()
            .filter_map(|(_, range)| src.get(range))
            .collect();
        assert_eq!(sections, ["First.", "Second."]);
    }

    fn only_table(src: &str) -> Table {
        let blocks = blocks(src);
        let [Block::Table(table)] = blocks.as_slice() else {
            panic!("one table, not {blocks:?}")
        };
        table.clone()
    }

    #[test]
    fn tables_keep_their_cells_as_markdown() {
        let src = "Before.\n\n| Name | What |\n|---|:-:|\n| `main_window.rs` | **bold** and \\| pipe |\n| a.rs | [link](https://example.org/a/long/path) |\n\nAfter.";
        let blocks = blocks(src);
        let [Block::Text(_), Block::Table(table), Block::Text(_)] = blocks.as_slice() else {
            panic!("text, a table, text: not {blocks:?}")
        };
        assert_eq!(
            table.rows,
            [
                ["Name", "What"],
                ["`main_window.rs`", "**bold** and | pipe"],
                ["a.rs", "[link](https://example.org/a/long/path)"],
            ]
        );
        // What the cells show: the code's backticks, the emphasis and the link's target are not.
        assert_eq!(table.longest_cell_chars, [14, 15]);
        assert!(table.source.starts_with("| Name |"), "{}", table.source);
        assert!(table.source.ends_with("long/path) |"), "{}", table.source);
    }

    /// GFM: a `\|` in a cell is a pipe, in inline code too.
    #[test]
    fn an_escaped_pipe_in_a_cell_is_a_pipe() {
        let table = only_table("| Shell |\n|---|\n| `ls \\| wc -l`, a \\| b |\n");
        let cell = table.rows.get(1).and_then(|row| row.first());
        assert_eq!(
            cell.map(|cell| drawn_lines(cell)),
            Some(vec!["ls | wc -l, a | b".to_owned()])
        );
    }

    /// Every cell is an element of the window.
    #[test]
    fn tables_are_cut_to_rows_and_columns_and_say_so() {
        let header = "| h ".repeat(TABLE_COLUMNS + 6) + "|\n";
        let rule = "|---".repeat(TABLE_COLUMNS + 6) + "|\n";
        let mut src = header + &rule;
        for _ in 0..TABLE_ROWS + 41 {
            src.push_str("| y |\n");
        }
        let table = only_table(&src);
        assert_eq!(table.rows.len(), TABLE_ROWS + 1);
        let header = table.rows.first().expect("a header");
        assert_eq!(header.len(), TABLE_COLUMNS + 1);
        assert_eq!(header.last().map(String::as_str), Some("…"));
        assert_eq!(table.longest_cell_chars.len(), TABLE_COLUMNS + 1);
        assert_eq!(
            table.rows.last().map(Vec::as_slice),
            Some(["… 42 more rows".to_owned()].as_slice())
        );
    }

    /// A very wide cell is wrapped in the window, so it is kept whole.
    #[test]
    fn long_cells_are_kept_whole() {
        let long = "word ".repeat(2_000);
        let long = long.trim();
        let table = only_table(&format!("| a | b |\n|---|---|\n| {long} | c |\n"));
        let kept = table.rows.get(1).and_then(|row| row.first());
        assert!(
            kept.is_some_and(|cell| cell == long),
            "kept {} characters",
            kept.map_or(0, String::len)
        );
        assert_eq!(table.longest_cell_chars, [long.len(), 1]);
    }
}
