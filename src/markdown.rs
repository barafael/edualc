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
    /// Laid out as aligned plain text, to be shown in a monospace font.
    Table(String),
    Rule,
}

/// Tables are cut to this many rows and columns, and their cells to this many characters: a
/// table is padded to its widest cell, so a few long cells in many rows would grow it without
/// bound.
const TABLE_ROWS: usize = 500;
const TABLE_COLUMNS: usize = 24;
const TABLE_CELL: usize = 120;

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
    let mut table: Option<Vec<Vec<String>>> = None;
    // The table has all the rows shown of it; the rest are counted, and said to be cut.
    let mut table_full = false;
    let mut rows_cut = 0usize;
    // Whether text goes to the table's last cell, or to a column past those shown.
    let mut cell_shown = false;
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
                        Tag::Table(_) => {
                            table = Some(Vec::new());
                            table_full = false;
                            rows_cut = 0;
                        }
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
                if let Some(rows) = table.as_mut() {
                    match tag {
                        Tag::TableHead | Tag::TableRow => {
                            table_full = rows.len() >= TABLE_ROWS;
                            if table_full {
                                rows_cut += 1;
                            } else {
                                rows.push(Vec::new());
                            }
                        }
                        Tag::TableCell => {
                            cell_shown = false;
                            if let Some(row) = rows.last_mut().filter(|_| !table_full) {
                                if row.len() < TABLE_COLUMNS {
                                    row.push(String::new());
                                    cell_shown = true;
                                } else if row.len() == TABLE_COLUMNS {
                                    row.push("…".to_owned());
                                }
                            }
                        }
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
                if depth == 0 {
                    in_list = false;
                    let whole = trimmed(src, &open);
                    if let Some((lang, text)) = code.take() {
                        let code = text.strip_suffix('\n').unwrap_or(&text).to_owned();
                        out.push((Block::Code { lang, code }, whole.clone()));
                    }
                    if matches!(end, TagEnd::Table)
                        && let Some(mut rows) = table.take()
                    {
                        if rows_cut > 0 {
                            rows.push(vec![format!("… {rows_cut} more rows")]);
                        }
                        out.push((Block::Table(lay_out_table(&rows)), whole));
                    }
                }
            }
            Event::Text(text) => {
                if let Some((_, code)) = code.as_mut() {
                    code.push_str(&text);
                } else if cell_shown {
                    add_to_cell(&mut table, &text);
                }
            }
            Event::Code(text) if cell_shown => add_to_cell(&mut table, &text),
            Event::Rule if depth == 0 => out.push((Block::Rule, range)),
            Event::Html(_) | Event::InlineHtml(_) if depth == 0 => {
                out.push((Block::Text(source()), range));
            }
            Event::Code(_)
            | Event::Rule
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

/// Adds text to the table's last cell, as far as the table is shown.
fn add_to_cell(table: &mut Option<Vec<Vec<String>>>, text: &str) {
    let Some(cell) = table
        .as_mut()
        .and_then(|rows| rows.last_mut())
        .and_then(|row| row.last_mut())
    else {
        return;
    };
    let held = cell.chars().count();
    if held >= TABLE_CELL {
        return;
    }
    let mut chars = text.chars();
    cell.extend(chars.by_ref().take(TABLE_CELL - held));
    // Cut: said so, in the cell's last place.
    if chars.next().is_some() {
        cell.pop();
        cell.push('…');
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

/// Columns padded to their widest cell, the header ruled off: readable in a monospace font.
fn lay_out_table(rows: &[Vec<String>]) -> String {
    let columns = rows.iter().map(Vec::len).max().unwrap_or(0);
    let width = |c: usize| {
        rows.iter()
            .filter_map(|row| row.get(c))
            .map(|cell| cell.chars().count())
            .max()
            .unwrap_or(0)
    };
    let widths: Vec<usize> = (0..columns).map(width).collect();
    let mut lines = Vec::new();
    for (i, row) in rows.iter().enumerate() {
        let cells: Vec<String> = widths
            .iter()
            .enumerate()
            .map(|(c, &w)| {
                let cell = row.get(c).map_or("", String::as_str);
                format!("{cell}{}", " ".repeat(w - cell.chars().count()))
            })
            .collect();
        lines.push(cells.join(" │ ").trim_end().to_owned());
        if i == 0 {
            let rule: Vec<String> = widths.iter().map(|&w| "─".repeat(w)).collect();
            lines.push(rule.join("─┼─"));
        }
    }
    lines.join("\n")
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

    /// One very wide cell over many rows would otherwise be padded into every row.
    #[test]
    fn tables_stay_bounded() {
        let mut src = format!("| a |\n|---|\n| {} |\n", "x".repeat(30_000));
        for _ in 0..30_000 {
            src.push_str("| y |\n");
        }
        let Some(Block::Table(text)) = blocks(&src).into_iter().next() else {
            panic!("a table");
        };
        assert!(text.len() < 1_000_000, "{} bytes", text.len());
        assert!(text.contains('…'), "the cut cell says so");
        assert!(text.ends_with("more rows"), "so does the cut table");
    }

    #[test]
    fn tables_are_aligned() {
        let src = "| Name | Size |\n|---|---|\n| `a.rs` | 10 |\n| longer.rs | 2 |\n";
        let blocks = blocks(src);
        let [Block::Table(t)] = blocks.as_slice() else {
            panic!("one table")
        };
        assert_eq!(
            t,
            "Name      │ Size\n──────────┼─────\na.rs      │ 10\nlonger.rs │ 2"
        );
    }
}
