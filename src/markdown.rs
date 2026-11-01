//! Assistant text as blocks. Slint's `StyledText` takes only inline Markdown (emphasis, code,
//! links) and lists, so the blocks it can't show (headings, code, quotes, tables, rules) are
//! split off here and drawn by the UI; the rest goes to Slint as Markdown source.

use std::ops::Range;

use pulldown_cmark::{CodeBlockKind, Event, Options, Parser, Tag, TagEnd};

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Block {
    /// A paragraph or a list, as Markdown source for `StyledText`.
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

pub(crate) fn blocks(src: &str) -> Vec<Block> {
    split(src, false)
        .into_iter()
        .map(|(block, _)| block)
        .collect()
}

/// The parts of a plan someone can comment on: [`blocks`], with every item of a list on its
/// own, and each with where its Markdown source is in `src` (to quote it back, or note
/// something under it). Text Markdown shows nothing of (a link's definition, say) is a part
/// too, shown as it is written: what is approved is all seen.
pub(crate) fn sections(src: &str) -> Vec<(Block, Range<usize>)> {
    let parts = split(src, true);
    let mut out = Vec::with_capacity(parts.len());
    let mut shown_to = 0;
    for (block, range) in parts {
        if let Some(hidden) = hidden_between(src, shown_to..range.start) {
            out.push(hidden);
        }
        shown_to = shown_to.max(range.end);
        out.push((block, range));
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

fn split(src: &str, items: bool) -> Vec<(Block, Range<usize>)> {
    let mut options = Options::empty();
    options.insert(Options::ENABLE_TABLES);
    options.insert(Options::ENABLE_STRIKETHROUGH);
    let mut out = Vec::new();
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
                            Block::Heading(*level as u8, heading_text(&source())),
                            range.clone(),
                        )),
                        Tag::BlockQuote(_) => {
                            out.push((Block::Quote(unquote(&source())), range.clone()));
                        }
                        Tag::List(_) if items => in_list = true,
                        // In a plan, HTML (a comment, say) is shown as it is written: as
                        // Markdown it would show nothing of what is approved.
                        Tag::HtmlBlock if items => out.push((
                            Block::Code {
                                lang: "html".to_owned(),
                                code: source(),
                            },
                            range.clone(),
                        )),
                        // Paragraphs, lists and HTML; inline tags never stand at the top.
                        Tag::Paragraph
                        | Tag::HtmlBlock
                        | Tag::List(_)
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
                        | Tag::MetadataBlock(_) => out.push((Block::Text(source()), range.clone())),
                    }
                } else if depth == 1 && in_list && matches!(tag, Tag::Item) {
                    out.push((Block::Text(source()), range.clone()));
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
                        out.push((Block::Code { lang, code }, whole.clone()));
                    }
                    if matches!(end, TagEnd::Table)
                        && let Some(table) = table.take()
                    {
                        let source = src.get(whole.clone()).unwrap_or("").to_owned();
                        out.push((Block::Table(table.finish(source)), whole));
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
            Event::Rule if depth == 0 => out.push((Block::Rule, range)),
            Event::Html(_) | Event::InlineHtml(_) if depth == 0 => {
                out.push((Block::Text(source()), range));
            }
            Event::Rule
            | Event::Html(_)
            | Event::InlineHtml(_)
            | Event::InlineMath(_)
            | Event::DisplayMath(_)
            | Event::FootnoteReference(_)
            | Event::SoftBreak
            | Event::HardBreak
            | Event::TaskListMarker(_) => {}
        }
    }
    out
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
            // The source starts at the space after the cell's `|`.
            row.push(source.trim().to_owned());
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

/// Inline Markdown as Slint styled text; as plain text if Slint's parser refuses it (an HTML
/// tag it doesn't know, say a stray `<T>` outside backticks).
pub(crate) fn styled(source: &str) -> slint::StyledText {
    let source = sanitize(source);

    slint::StyledText::from_markdown(&source)
        .unwrap_or_else(|_| slint::StyledText::from_plain_text(&source))
}

/// Text shown as it is (code, tool output): tabs as spaces, as the text layout draws no tab
/// stops.
pub(crate) fn plain(text: &str) -> slint::StyledText {
    slint::StyledText::from_plain_text(&sanitize(text).replace('\t', "    "))
}

/// Slint reserves U+E541 as its placeholder for arguments, and a carriage return would end a
/// line in the middle of one.
fn sanitize(text: &str) -> String {
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
                ["`main_window.rs`", "**bold** and \\| pipe"],
                ["a.rs", "[link](https://example.org/a/long/path)"],
            ]
        );
        // What the cells show: the code's backticks, the emphasis and the link's target are not.
        assert_eq!(table.longest_cell_chars, [14, 15]);
        assert!(table.source.starts_with("| Name |"), "{}", table.source);
        assert!(table.source.ends_with("long/path) |"), "{}", table.source);
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
