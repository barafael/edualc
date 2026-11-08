//! Text as the page shows it: plain, or inline Markdown made HTML. Whatever HTML the text itself
//! holds is shown as text, never taken as markup (`<u>` aside, as the native window takes it);
//! a link opens in a new tab, and only a web page's or mail's.

use dioxus::prelude::*;
use edualc_core::present::Styled;
use edualc_core::present::model::{BlockKind, CjkChars, MdBlock};
use pulldown_cmark::{CowStr, Event, Options, Parser, Tag, TagEnd};

/// Inline Markdown as HTML, safe to set as a node's inner HTML.
pub fn markdown_html(source: &str) -> String {
    let mut links_open = Vec::new();
    let events = Parser::new_ext(source, Options::ENABLE_STRIKETHROUGH).map(|event| match event {
        Event::Html(html) | Event::InlineHtml(html) => match html.trim() {
            "<u>" | "</u>" => Event::InlineHtml(html),
            _ => Event::Text(html),
        },
        Event::Start(Tag::Link { dest_url, .. }) => {
            let opens = opens(&dest_url);
            links_open.push(opens);
            if opens {
                Event::InlineHtml(CowStr::from(format!(
                    "<a href=\"{}\" target=\"_blank\" rel=\"noopener noreferrer\">",
                    escaped(&dest_url)
                )))
            } else {
                Event::InlineHtml(CowStr::from("<span class=\"link\">"))
            }
        }
        Event::End(TagEnd::Link) => {
            Event::InlineHtml(CowStr::from(if links_open.pop().unwrap_or(false) {
                "</a>"
            } else {
                "</span>"
            }))
        }
        // Images and the like aren't fetched: their text stands for them.
        Event::Start(Tag::Image { .. }) => Event::Text(CowStr::from("")),
        other @ (Event::Start(_)
        | Event::End(_)
        | Event::Text(_)
        | Event::Code(_)
        | Event::InlineMath(_)
        | Event::DisplayMath(_)
        | Event::FootnoteReference(_)
        | Event::SoftBreak
        | Event::HardBreak
        | Event::Rule
        | Event::TaskListMarker(_)) => other,
    });
    let mut html = String::new();
    pulldown_cmark::html::push_html(&mut html, events);
    html
}

/// Whether a link may open: a web page's, or mail.
fn opens(url: &str) -> bool {
    ["https://", "http://", "mailto:"]
        .iter()
        .any(|scheme| url.starts_with(scheme))
}

/// Text made safe inside HTML.
pub fn escaped(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for character in text.chars() {
        match character {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            other => out.push(other),
        }
    }
    out
}

/// The `lang` attribute a text's CJK characters ask for, so the browser picks their glyphs.
pub fn lang(cjk: &CjkChars) -> &'static str {
    use edualc_core::present::model::Cjk;
    match cjk.language {
        Cjk::None => "",
        Cjk::Japanese => "ja",
        Cjk::SimplifiedChinese => "zh-Hans",
        Cjk::TraditionalChinese => "zh-Hant",
        Cjk::Korean => "ko",
    }
}

/// Styled text: plain (whitespace kept), or Markdown.
#[component]
pub fn Text(text: Styled, #[props(default)] class: String) -> Element {
    match text {
        Styled::Empty => rsx! {},
        Styled::Plain(text) => rsx! {
            div { class: "plain {class}", "{text}" }
        },
        Styled::Markdown(source) => rsx! {
            div { class: "markdown {class}", dangerous_inner_html: markdown_html(&source) }
        },
    }
}

/// A Markdown block of a message, a plan or a side answer.
#[component]
pub fn Block(block: MdBlock) -> Element {
    let lang = lang(&block.cjk);
    match block.kind {
        BlockKind::Text => rsx! {
            div { class: "block text", lang, Text { text: block.body } }
        },
        BlockKind::Heading => {
            let level = block.level.clamp(1, 6);
            rsx! {
                div { class: "block heading h{level}", lang, role: "heading", "aria-level": "{level}",
                    Text { text: block.body }
                }
            }
        }
        BlockKind::Code => rsx! {
            div { class: "block code",
                if !block.lang.is_empty() {
                    div { class: "code-lang", "{block.lang}" }
                }
                button {
                    class: "copy",
                    title: "Copy",
                    onclick: move |_| super::view::copy(&block.raw),
                    "⧉"
                }
                pre { code { "{block.raw}" } }
            }
        },
        BlockKind::Quote => rsx! {
            blockquote { class: "block quote", lang, Text { text: block.body } }
        },
        BlockKind::Table => rsx! {
            div { class: "block table", lang,
                table {
                    for (index, row) in block.rows.iter().enumerate() {
                        tr {
                            for cell in row.cells.iter() {
                                if index == 0 {
                                    th { Text { text: cell.clone() } }
                                } else {
                                    td { Text { text: cell.clone() } }
                                }
                            }
                        }
                    }
                }
            }
        },
        BlockKind::Rule => rsx! {
            hr { class: "block rule" }
        },
    }
}

/// A message's blocks.
#[component]
pub fn Blocks(blocks: Vec<MdBlock>) -> Element {
    rsx! {
        div { class: "blocks",
            for block in blocks {
                Block { block }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// HTML in a message is shown, not taken; links open only to the web or mail.
    #[test]
    fn markdown_is_made_safe_html() {
        let html = markdown_html("a <script>alert(1)</script> and *this*");
        assert!(!html.contains("<script>"), "{html}");
        assert!(html.contains("&lt;script&gt;"), "{html}");
        assert!(html.contains("<em>this</em>"), "{html}");
        let html = markdown_html("[ok](https://example.com) [no](javascript:alert(1))");
        assert!(
            html.contains("<a href=\"https://example.com\" target=\"_blank\""),
            "{html}"
        );
        assert!(!html.contains("javascript:"), "{html}");
    }
}
