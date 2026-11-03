//! Code in colour, by its language: a fenced block's tag (`rust`, `py`, `toml`), which syntect
//! looks up among bat's grammars by name or file extension. The colours are the theme's
//! (`SyntaxColours` in `ui/theme.slint`), not syntect's themes.
//!
//! Slint makes coloured text only from Markdown, so each line of code goes to it as Markdown
//! that shows the code as it is: every ASCII punctuation character escaped, spaces and control
//! characters as character references (which no Markdown rule strips or reads as indentation),
//! lines ended by hard breaks, and each coloured run in a `<font color>` tag.
//!
//! A block that streams in grows a line at a time; [`Progress`] keeps the lines already
//! coloured and the parser's state after them, so each update colours only what is new.

use std::str::FromStr;

use syntect::highlighting::{
    Color, HighlightIterator, HighlightState, Highlighter as Painter, ScopeSelectors,
    StyleModifier, Theme, ThemeItem, ThemeSettings,
};
use syntect::parsing::{ParseState, ScopeStack, SyntaxReference, SyntaxSet};
use syntect::util::LinesWithEndings;

use crate::generated::SyntaxColours;
use crate::markdown;

/// Longer code is shown plain: highlighting takes about 0.1 ms a line, on the UI thread.
const MAX_LINES: usize = 3000;
/// A line longer than this (minified code, data) is shown plain, as the whole block is: some
/// grammars' patterns backtrack on long lines.
const MAX_LINE_BYTES: usize = 2000;

/// What the theme gives text it doesn't colour; never a colour of its own (it is transparent).
const UNCOLOURED: Color = Color {
    r: 0,
    g: 0,
    b: 0,
    a: 0,
};

/// Ends a block whose last line is empty: a hard break at the end of a paragraph is dropped,
/// so the empty line gets an empty run to stand on.
const EMPTY_LINE: &str = "<font color=\"#000000\"></font>";

/// The grammars, and the theme's colours as a syntect theme.
pub(crate) struct Highlighter {
    syntaxes: SyntaxSet,
    theme: Theme,
}

/// A block's code as far as it is coloured: its complete lines, as Markdown, and the parser's
/// state after them.
pub(crate) struct Progress {
    lang: String,
    /// The complete lines coloured, each with its `\n`.
    code: String,
    lines: Vec<String>,
    parse: ParseState,
    highlight: HighlightState,
}

impl Highlighter {
    pub(crate) fn new(colours: &SyntaxColours) -> Self {
        Self {
            // Loads lazily: a grammar is read when a block in its language first shows.
            syntaxes: two_face::syntax::extra_newlines(),
            theme: theme(colours),
        }
    }

    /// The theme's colours changed; text already coloured has the old ones, and each
    /// [`Progress`] made with them is to be dropped.
    pub(crate) fn set_colours(&mut self, colours: &SyntaxColours) {
        self.theme = theme(colours);
    }

    /// `code` in `lang`'s colours, carrying on from `progress` when `code` goes on from what it
    /// coloured; plain if the language isn't known, or the code is too long to colour.
    pub(crate) fn code(
        &self,
        lang: &str,
        code: &str,
        progress: &mut Option<Progress>,
    ) -> slint::StyledText {
        self.coloured(lang, code, progress)
            .unwrap_or_else(|| markdown::plain(code))
    }

    fn coloured(
        &self,
        lang: &str,
        code: &str,
        progress: &mut Option<Progress>,
    ) -> Option<slint::StyledText> {
        if code.is_empty() || code.lines().count() > MAX_LINES {
            return None;
        }
        let syntax = self.syntax(lang)?;
        let painter = Painter::new(&self.theme);
        let carries_on = progress
            .as_ref()
            .is_some_and(|progress| progress.lang == lang && code.starts_with(&progress.code));
        if !carries_on {
            *progress = Some(Progress {
                lang: lang.to_owned(),
                code: String::new(),
                lines: Vec::new(),
                parse: ParseState::new(syntax),
                highlight: HighlightState::new(&painter, ScopeStack::new()),
            });
        }
        let progress = progress.as_mut()?;
        let new = code.get(progress.code.len()..)?;
        let complete = new.rfind('\n').map_or(0, |newline| newline + 1);
        let (complete, last) = new.split_at(complete);
        for line in LinesWithEndings::from(complete) {
            let shown = self.line(&painter, &mut progress.parse, &mut progress.highlight, line)?;
            progress.lines.push(shown);
            progress.code.push_str(line);
        }
        // The last line may grow yet: coloured from a copy of the state.
        let last = self.line(
            &painter,
            &mut progress.parse.clone(),
            &mut progress.highlight.clone(),
            last,
        )?;
        let mut source = progress.lines.join("\\\n");
        if !progress.lines.is_empty() {
            source.push_str("\\\n");
        }
        if last.is_empty() && !progress.lines.is_empty() {
            source.push_str(EMPTY_LINE);
        } else {
            source.push_str(&last);
        }
        slint::StyledText::from_markdown(&source).ok()
    }

    /// The grammar for a fence's tag: its name or a file extension (`rust`, `rs`), before any
    /// attributes (`rust,ignore`).
    fn syntax(&self, lang: &str) -> Option<&SyntaxReference> {
        let token = lang.split(',').next().unwrap_or(lang).trim();
        if token.is_empty() {
            return None;
        }
        self.syntaxes.find_syntax_by_token(token)
    }

    /// One line, its `\n` left off, as Markdown in its colours; `None` if it is too long or
    /// the grammar fails on it.
    fn line(
        &self,
        painter: &Painter<'_>,
        parse: &mut ParseState,
        highlight: &mut HighlightState,
        line: &str,
    ) -> Option<String> {
        if line.len() > MAX_LINE_BYTES {
            return None;
        }
        let ops = parse.parse_line(line, &self.syntaxes).ok()?;
        let mut out = String::new();
        for (style, text) in HighlightIterator::new(highlight, &ops, line, painter) {
            let text = text.strip_suffix('\n').unwrap_or(text);
            if text.is_empty() {
                continue;
            }
            let colour = style.foreground;
            if colour == UNCOLOURED {
                push_escaped(&mut out, text);
            } else {
                out.push_str("<font color=\"#");
                for channel in [colour.r, colour.g, colour.b] {
                    push_hex(&mut out, channel);
                }
                out.push_str("\">");
                push_escaped(&mut out, text);
                out.push_str("</font>");
            }
        }
        Some(out)
    }
}

/// `text` as Markdown that shows it as it is.
fn push_escaped(out: &mut String, text: &str) {
    for c in text.chars() {
        match c {
            // As `markdown::plain` shows them: tabs as four spaces, no carriage returns, and
            // Slint's placeholder for arguments (and NUL, which Markdown replaces) as U+FFFD.
            '\t' => out.push_str("&#32;&#32;&#32;&#32;"),
            '\r' => {}
            '\0' | '\u{e541}' => out.push('\u{fffd}'),
            c if c.is_ascii_punctuation() => {
                out.push('\\');
                out.push(c);
            }
            c if c == ' ' || c.is_ascii_control() => {
                out.push_str("&#");
                out.push_str(&u32::from(c).to_string());
                out.push(';');
            }
            c => out.push(c),
        }
    }
}

fn push_hex(out: &mut String, byte: u8) {
    for nibble in [byte >> 4, byte & 0xf] {
        out.extend(char::from_digit(u32::from(nibble), 16));
    }
}

/// The theme's colours by TextMate scope. The most specific selector that matches a word wins,
/// so operators are left uncoloured inside the keywords, and escapes coloured inside strings.
fn theme(colours: &SyntaxColours) -> Theme {
    let rules = [
        (
            "comment, punctuation.definition.comment",
            colour(colours.comment),
        ),
        (
            "keyword, storage.type, storage.modifier, keyword.operator.word, markup.heading",
            colour(colours.keyword),
        ),
        ("keyword.operator", UNCOLOURED),
        (
            "string, punctuation.definition.string, markup.raw",
            colour(colours.string),
        ),
        (
            "constant, constant.character.escape, support.constant, variable.other.constant, \
             variable.language",
            colour(colours.constant),
        ),
        (
            "entity.name.type, entity.name.class, entity.name.struct, entity.name.enum, \
             entity.name.trait, entity.name.union, entity.name.interface, \
             entity.other.inherited-class, support.type, support.class, storage.type.primitive, \
             entity.other.attribute-name",
            colour(colours.type_name),
        ),
        (
            // Tags (HTML's, and TOML's keys) as Gruvbox's Vim theme has them.
            "entity.name.function, support.function, variable.function, entity.name.macro, \
             support.macro, entity.name.tag",
            colour(colours.function),
        ),
        ("markup.inserted", colour(colours.inserted)),
        ("markup.deleted", colour(colours.deleted)),
    ];
    Theme {
        settings: ThemeSettings {
            foreground: Some(UNCOLOURED),
            ..ThemeSettings::default()
        },
        scopes: rules
            .into_iter()
            .map(|(selectors, foreground)| ThemeItem {
                scope: ScopeSelectors::from_str(selectors)
                    .expect("the selectors above are written to parse"),
                style: StyleModifier {
                    foreground: Some(foreground),
                    background: None,
                    font_style: None,
                },
            })
            .collect(),
        ..Theme::default()
    }
}

fn colour(colour: slint::Color) -> Color {
    Color {
        r: colour.red(),
        g: colour.green(),
        b: colour.blue(),
        a: 0xff,
    }
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    fn colours() -> SyntaxColours {
        SyntaxColours {
            comment: slint::Color::from_rgb_u8(0x95, 0x86, 0x76),
            keyword: slint::Color::from_rgb_u8(0xfb, 0x49, 0x34),
            string: slint::Color::from_rgb_u8(0xb8, 0xbb, 0x26),
            constant: slint::Color::from_rgb_u8(0xd3, 0x86, 0x9b),
            type_name: slint::Color::from_rgb_u8(0xfa, 0xbd, 0x2f),
            function: slint::Color::from_rgb_u8(0x8e, 0xc0, 0x7c),
            inserted: slint::Color::from_rgb_u8(0xb8, 0xbb, 0x26),
            deleted: slint::Color::from_rgb_u8(0xfb, 0x58, 0x41),
        }
    }

    /// The text Slint shows, as copying joins its paragraphs; read from the debug output, as
    /// `StyledText` keeps its paragraphs to itself.
    fn shown(text: &slint::StyledText) -> String {
        let debug = format!("{text:?}");
        debug
            .split("StyledTextParagraph { text: \"")
            .skip(1)
            .map(unquote)
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// A Rust string's debug form, from after its opening quote, back to the string.
    fn unquote(debug: &str) -> String {
        let mut out = String::new();
        let mut chars = debug.chars();
        while let Some(c) = chars.next() {
            match c {
                '"' => break,
                '\\' => match chars.next() {
                    Some('n') => out.push('\n'),
                    Some('t') => out.push('\t'),
                    Some('r') => out.push('\r'),
                    Some('0') => out.push('\0'),
                    Some('u') => {
                        let hex: String =
                            chars.by_ref().skip(1).take_while(|&c| c != '}').collect();
                        let code = u32::from_str_radix(&hex, 16).expect("debug escapes are hex");
                        out.push(char::from_u32(code).expect("debug escapes are characters"));
                    }
                    Some(other) => out.push(other),
                    None => {}
                },
                c => out.push(c),
            }
        }
        out
    }

    /// What `markdown::plain` shows of `code`.
    fn as_plain(code: &str) -> String {
        shown(&markdown::plain(code))
    }

    #[test]
    fn rust_is_coloured() {
        let highlighter = Highlighter::new(&colours());
        let code = "// a comment\nfn main() {\n    let x = \"hi\";\n}";
        let text = highlighter.code("rust", code, &mut None);
        assert_eq!(shown(&text), code);
        let debug = format!("{text:?}");
        // The comment's grey, the keyword's red, the string's green.
        for argb in ["0xff958676", "0xfffb4934", "0xffb8bb26"] {
            let argb = u32::from_str_radix(argb.trim_start_matches("0x"), 16).expect("hex");
            assert!(
                debug.contains(&format!("Color({argb})")),
                "no {argb:#x} in {debug}"
            );
        }
    }

    #[test]
    fn a_fence_tag_finds_its_grammar_by_name_or_extension() {
        let highlighter = Highlighter::new(&colours());
        for lang in [
            "rust",
            "rs",
            "rust,ignore",
            "py",
            "toml",
            "tsx",
            "sh",
            "Dockerfile",
        ] {
            assert!(highlighter.syntax(lang).is_some(), "{lang}");
        }
        for lang in ["", "text", "nonsense"] {
            assert!(highlighter.syntax(lang).is_none(), "{lang}");
        }
    }

    #[test]
    fn unknown_languages_are_plain() {
        let highlighter = Highlighter::new(&colours());
        let text = highlighter.code("nonsense", "a  *b*", &mut None);
        assert_eq!(
            format!("{text:?}"),
            format!("{:?}", markdown::plain("a  *b*"))
        );
    }

    #[test]
    fn markdown_in_code_shows_as_written() {
        let highlighter = Highlighter::new(&colours());
        let code = "    # not a heading\n1. not a list\n- nor this\n> nor a quote\n\n\n<b>&amp;</b> `x` \\ *y* _z_ ~~s~~ [l](u)\n\tend  \n\n";
        for lang in ["python", "markdown", "sh"] {
            let text = highlighter.code(lang, code, &mut None);
            assert_eq!(shown(&text), as_plain(code), "{lang}");
        }
    }

    /// A block streamed in a piece at a time ends as it would if coloured at once.
    #[test]
    fn a_streamed_block_ends_as_a_whole_one() {
        let highlighter = Highlighter::new(&colours());
        let code = "/* a\n   long comment */\nfn f() -> u8 {\n    1\n}\n";
        let mut progress = None;
        let mut streamed = None;
        for end in 1..=code.len() {
            let piece = code.get(..end).expect("the code is ASCII");
            streamed = Some(highlighter.code("rust", piece, &mut progress));
        }
        let whole = highlighter.code("rust", code, &mut None);
        assert_eq!(format!("{streamed:?}"), format!("{:?}", Some(whole)));
    }

    /// The grammars' licenses, as the license report ends with them: cargo-about sees only
    /// the crate that bundles them.
    fn grammar_licenses() -> String {
        let mut section = String::from(
            "\n## Grammars\n\nThe grammars code is coloured by come with two-face, from bat, under \
             their own licenses:\n\n",
        );
        for license in two_face::acknowledgement::listing().for_syntaxes() {
            license.write_md(&mut section);
        }
        section
    }

    #[test]
    fn the_license_report_ends_with_the_grammars_licenses() {
        let report = include_str!("../THIRD_PARTY_LICENSES.md");
        assert!(
            report.ends_with(&grammar_licenses()),
            "THIRD_PARTY_LICENSES.md is out of date: regenerate it as about.toml says"
        );
    }

    /// Adds the grammars' licenses to a report cargo-about has just made (see about.toml).
    #[test]
    #[ignore = "writes THIRD_PARTY_LICENSES.md, when it is regenerated"]
    fn add_the_grammars_licenses_to_the_report() {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/THIRD_PARTY_LICENSES.md");
        let mut report = std::fs::read_to_string(path).expect("cargo-about has made the report");
        assert!(
            !report.contains("\n## Grammars\n"),
            "the report has them already"
        );
        report.push_str(&grammar_licenses());
        std::fs::write(path, report).expect("the report can be written");
    }

    /// Whatever the code, the text shown is the code, as plain text would show it. One
    /// highlighter for every case: each grammar's patterns are compiled as it is first used.
    #[test]
    fn the_text_is_the_code() {
        let highlighter = Highlighter::new(&colours());
        proptest!(ProptestConfig::with_cases(128), |(
            code in "[ \t\n\r!-~a-zé\u{3000}\u{e541}\u{7}]{1,200}",
            lang in prop::sample::select(vec!["rust", "python", "markdown", "html", "sh"]),
        )| {
            let text = highlighter.code(lang, &code, &mut None);
            prop_assert_eq!(shown(&text), as_plain(&code));
        });
    }
}
