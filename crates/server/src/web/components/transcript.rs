//! The conversation (`ui/transcript.slint`, and the transcript's list in `ui/app.slint`): the
//! user's messages, Claude's Markdown, thinking, tool calls with their input, output and diffs,
//! runs of calls folded into one line, messages from other sessions, notes and errors. It
//! follows new output only while its end is in view, so reading back isn't interrupted; a
//! session opened shows at its end. Over a new session's empty conversation, the heron and a
//! greeting; the heron flies off as the conversation begins.

use dioxus::prelude::*;
use edualc_core::app::Action;
use edualc_core::present::Styled;
use edualc_core::present::model::{
    BlockKind, CjkChars, DiffKind, DiffLine, Entry, EntryKind, GroupRole, ImageState, MdBlock,
    PromptKind, ShownImage, ToolState,
};

use crate::web::View;
use crate::web::text::{self, Block, Text};
use crate::web::view::{copy, image_url};
use crate::web::widgets::{Icon, IconButton, Lucide};

/// The scrolling list's id, by which the page finds it to scroll it.
const LIST: &str = "transcript";

/// How close to the end (in CSS pixels) still counts as at the end.
const END_SLACK: f64 = 8.0;

/// How long the rows' height must hold still before an opened transcript shows (ms), how often
/// that is looked at, and how many looks at most (a moment and a half in all).
const SETTLED_MS: f64 = 100.0;
const SETTLE_LOOK_MS: i32 = 30;
const SETTLE_LOOKS: u32 = 50;

/// The heron's flight: how many wing beats it takes, each how long (ms), and the wait before it
/// sets off to fly in, so that it starts from out of sight.
const BEATS: u32 = 10;
const BEAT_MS: i32 = 110;
const APPROACH_MS: i32 = 50;

/// The heron's drawings: standing, and its wing beat's frames (up, down, level).
const HERON: [Asset; 4] = [
    asset!("/assets/heron/heron.svg"),
    asset!("/assets/heron/heron-wings-up.svg"),
    asset!("/assets/heron/heron-wings-down.svg"),
    asset!("/assets/heron/heron-wings-level.svg"),
];

/// The list, the page's clock and its pixel ratio, through the browser.
#[cfg(feature = "web")]
mod dom {
    use super::LIST;

    fn list() -> Option<web_sys::Element> {
        web_sys::window()?.document()?.get_element_by_id(LIST)
    }

    /// Goes to the list's end.
    pub fn to_end() {
        if let Some(list) = list() {
            list.set_scroll_top(list.scroll_height());
        }
    }

    /// Goes to the list's start.
    pub fn to_start() {
        if let Some(list) = list() {
            list.set_scroll_top(0);
        }
    }

    /// Scrolls the list by `delta` CSS pixels, down for more than 0.
    pub fn by(delta: f64) {
        if let Some(list) = list() {
            list.scroll_by_with_x_and_y(0.0, delta);
        }
    }

    /// How high the list's view is.
    pub fn height() -> f64 {
        list().map_or(0.0, |list| f64::from(list.client_height()))
    }

    /// Whether the list itself has the focus, rather than something in it.
    pub fn focused() -> bool {
        web_sys::window()
            .and_then(|window| window.document())
            .and_then(|document| document.active_element())
            .is_some_and(|active| active.id() == LIST)
    }

    /// Now, in milliseconds.
    pub fn now() -> f64 {
        web_sys::js_sys::Date::now()
    }

    /// Device pixels to a CSS pixel.
    pub fn pixel_ratio() -> f64 {
        web_sys::window().map_or(1.0, |window| window.device_pixel_ratio())
    }

    /// Waits `ms` milliseconds.
    pub async fn sleep(ms: i32) {
        use web_sys::js_sys::{Function, Promise};
        let promise = Promise::new(&mut |resolve: Function, _reject: Function| {
            if let Some(window) = web_sys::window()
                && window
                    .set_timeout_with_callback_and_timeout_and_arguments_0(&resolve, ms)
                    .is_err()
            {
                tracing::debug!("the browser refused a timer");
            }
        });
        if wasm_bindgen_futures::JsFuture::from(promise).await.is_err() {
            tracing::debug!("a timer failed");
        }
    }
}

/// Off the browser (the server renders the page once, before any of this could run): nothing
/// to scroll, no time passing.
#[cfg(not(feature = "web"))]
mod dom {
    pub fn to_end() {}
    pub fn to_start() {}
    pub fn by(_delta: f64) {}
    pub fn height() -> f64 {
        0.0
    }
    pub fn focused() -> bool {
        false
    }
    pub fn now() -> f64 {
        0.0
    }
    pub fn pixel_ratio() -> f64 {
        1.0
    }
    pub async fn sleep(_ms: i32) {}
}

/// Hands the app an action on the entry at `index`.
fn act_on(view: View, index: i32, action: fn(usize) -> Action) {
    if let Ok(index) = usize::try_from(index) {
        view.act(action(index));
    }
}

/// The class of text whose CJK characters are its main script: its lines further apart.
fn cjk_class(cjk: &CjkChars) -> &'static str {
    if cjk.main { "cjk" } else { "" }
}

/// The conversation's area: the list of rows, the greeting over an empty one, and a button to
/// the end while reading back. The arrow keys, Page Up and Down, Space, Home and End scroll it
/// while it has the focus.
#[component]
pub fn Transcript() -> Element {
    let view = use_context::<View>();
    // Follow new output only while the end is in view; only the user's own scrolling decides.
    let mut follow = use_signal(|| true);
    // A transcript just opened: hidden until its height holds still, then shown at its end.
    let mut settling = use_signal(|| false);
    let mut changed_at = use_signal(|| 0.0_f64);
    let mut opening = use_signal(|| 0_u32);
    let reviewing =
        use_memo(move || view.prompt.read().kind == PromptKind::Plan && view.page.read().reviewing);
    // Covered by the usage details or the plan, it doesn't move behind them.
    let covered = use_memo(move || view.usage.read().open || reviewing());
    let to_end_serial = use_memo(move || view.page.read().scroll_to_end);
    let opened_serial = use_memo(move || view.page.read().opened);

    let mut go_to_end = move || {
        if !*follow.peek() {
            follow.set(true);
        }
        dom::to_end();
    };

    use_effect(move || {
        if to_end_serial() != 0 {
            go_to_end();
        }
    });

    use_effect(move || {
        if opened_serial() == 0 {
            return;
        }
        let opened = opening.peek().wrapping_add(1);
        opening.set(opened);
        settling.set(true);
        changed_at.set(dom::now());
        go_to_end();
        spawn(async move {
            for _ in 0..SETTLE_LOOKS {
                dom::sleep(SETTLE_LOOK_MS).await;
                if *opening.peek() != opened {
                    return;
                }
                if dom::now() - *changed_at.peek() >= SETTLED_MS {
                    break;
                }
            }
            if *follow.peek() {
                dom::to_end();
            }
            settling.set(false);
        });
    });

    let onscroll = move |event: ScrollEvent| {
        let data = event.data();
        let end = f64::from(data.scroll_height() - data.client_height());
        let at_end = data.scroll_top() >= end - END_SLACK;
        if *follow.peek() != at_end {
            follow.set(at_end);
        }
    };
    // The rows grew or shrank (text streamed, a card opened, an image decoded), or the view
    // did (the prompt bar opening below): at the end it stays there.
    let onresize = move |_: ResizeEvent| {
        if *settling.peek() {
            changed_at.set(dom::now());
        }
        if *follow.peek() {
            dom::to_end();
        }
    };
    let onkeydown = move |event: KeyboardEvent| {
        if *covered.peek() || !dom::focused() {
            return;
        }
        let line = 4.0 * f64::from(view.page.peek().text_size);
        let key = event.key();
        if key == Key::ArrowDown {
            dom::by(line);
        } else if key == Key::ArrowUp {
            dom::by(-line);
        } else if key == Key::PageDown || key == Key::Character(" ".to_owned()) {
            dom::by(dom::height() * 0.9);
        } else if key == Key::PageUp {
            dom::by(-dom::height() * 0.9);
        } else if key == Key::Home {
            dom::to_start();
        } else if key == Key::End {
            go_to_end();
        } else {
            return;
        }
        event.prevent_default();
    };

    let entries = view.entries.read();
    let to_end_shown = !follow() && !reviewing();
    rsx! {
        document::Stylesheet { href: asset!("/assets/transcript.css") }
        div { class: "transcript-area",
            div {
                id: LIST,
                class: if settling() { "transcript settling" } else { "transcript" },
                tabindex: "0",
                role: "group",
                "aria-label": "Conversation",
                "aria-description": "The arrow keys, Page Up and Page Down scroll it",
                onscroll,
                onkeydown,
                onresize,
                // The rows shown: a folded run's later calls aren't.
                div { class: "rows", onresize,
                    for entry in entries.iter().filter(|entry| !entry.hidden) {
                        Row { key: "{entry.index}", entry: entry.clone() }
                    }
                }
            }
            Greeting {}
            if to_end_shown {
                IconButton {
                    icon: Lucide::ArrowDown,
                    label: "Go to the end",
                    class: "to-end",
                    onclick: move |_| go_to_end(),
                }
            }
        }
    }
}

/// One row of the conversation.
#[component]
fn Row(entry: Entry) -> Element {
    let class = if entry.nested {
        "entry nested"
    } else {
        "entry"
    };
    let body = match entry.kind {
        EntryKind::User => rsx! { UserBubble { entry } },
        // An image the user sent, where their messages are.
        EntryKind::Image => rsx! {
            div { class: "lane user-images",
                for shown in entry.images {
                    ImageView { shown }
                }
            }
        },
        EntryKind::Peer => rsx! { PeerBubble { entry } },
        EntryKind::Assistant => rsx! { AssistantText { entry } },
        EntryKind::Thinking => rsx! { Thinking { entry } },
        // An open run's calls, indented under its line; a folded run shows only the line.
        EntryKind::Tool => {
            let head = entry.group == GroupRole::Head;
            let card = entry.group == GroupRole::Alone || entry.group_open;
            rsx! {
                if head {
                    RunLine { entry: entry.clone() }
                }
                if card {
                    ToolRow { entry }
                }
            }
        }
        EntryKind::Info => rsx! {
            div {
                class: "lane info {cjk_class(&entry.cjk)}",
                lang: text::lang(&entry.cjk),
                Text { text: entry.body }
            }
        },
        EntryKind::Error => rsx! {
            div { class: "lane error",
                Icon { icon: Lucide::CircleAlert, class: "small" }
                div { class: "error-text {cjk_class(&entry.cjk)}", lang: text::lang(&entry.cjk),
                    Text { text: entry.body }
                }
            }
        },
    };
    rsx! {
        div { class, {body} }
    }
}

/// The user's message: a bubble on the right of the column.
#[component]
fn UserBubble(entry: Entry) -> Element {
    rsx! {
        div { class: "lane user-row",
            div {
                class: "bubble user {cjk_class(&entry.cjk)}",
                lang: text::lang(&entry.cjk),
                Text { text: entry.body }
            }
        }
    }
}

/// A message another session sent this one: on Claude's side, as Claude is the one it is for,
/// with who sent it above it, so it isn't taken for something the user wrote.
#[component]
fn PeerBubble(entry: Entry) -> Element {
    rsx! {
        div { class: "lane peer-row",
            div { class: "bubble peer",
                div { class: "peer-sender",
                    Icon { icon: Lucide::MessagesSquare, class: "small" }
                    span { "Message from {entry.sender}" }
                }
                div { class: "{cjk_class(&entry.cjk)}", lang: text::lang(&entry.cjk),
                    Text { text: entry.body }
                }
            }
        }
    }
}

/// Claude's message: its Markdown, and a button to copy it.
#[component]
fn AssistantText(entry: Entry) -> Element {
    let view = use_context::<View>();
    let index = entry.index;
    rsx! {
        div { class: "assistant",
            Markdown { blocks: entry.blocks }
            div { class: "lane",
                IconButton {
                    icon: Lucide::Copy,
                    label: "Copy message",
                    class: "tiny",
                    onclick: move |_| act_on(view, index, Action::CopyEntry),
                }
            }
        }
    }
}

/// A message's blocks. Prose holds to the column's measure; code and tables are never narrower,
/// and wider as they need, up to the wide measure or the room there is: they start where the
/// prose starts and grow to its right, moving left only where that runs out of room. `flat`
/// (inside a tool's card) lets every block fill the width.
#[component]
fn Markdown(blocks: Vec<MdBlock>, #[props(default)] flat: bool) -> Element {
    rsx! {
        div { class: if flat { "md flat" } else { "md" },
            for block in blocks {
                MarkdownBlock { block }
            }
        }
    }
}

/// One block of Markdown, laid out by its kind (see `Markdown`).
#[component]
fn MarkdownBlock(block: MdBlock) -> Element {
    match block.kind {
        BlockKind::Text | BlockKind::Heading | BlockKind::Quote | BlockKind::Rule => rsx! {
            div { class: "lane", Block { block } }
        },
        BlockKind::Code => rsx! {
            div { class: "wide-row",
                div { class: "inset" }
                div { class: "wide", Block { block } }
            }
        },
        // A table as wide as its cells need; narrower, its cells wrap; past that it scrolls
        // sideways. Copying takes its Markdown.
        BlockKind::Table => {
            let raw = block.raw.clone();
            rsx! {
                div { class: "wide-row",
                    div { class: "inset" }
                    div { class: "wide table-wide",
                        Block { block }
                        IconButton {
                            icon: Lucide::Copy,
                            label: "Copy table",
                            class: "tiny",
                            onclick: move |_| copy(&raw),
                        }
                    }
                }
            }
        }
    }
}

/// Claude's thinking: folded to a line, opened to its text.
#[component]
fn Thinking(entry: Entry) -> Element {
    let view = use_context::<View>();
    let index = entry.index;
    let chevron = if entry.expanded {
        Lucide::ChevronDown
    } else {
        Lucide::ChevronRight
    };
    rsx! {
        div { class: "lane thinking",
            button {
                class: "thinking-head",
                "aria-expanded": "{entry.expanded}",
                onclick: move |_| act_on(view, index, Action::Toggle),
                Icon { icon: Lucide::Brain, class: "small" }
                span { class: "small", "Thinking" }
                Icon { icon: chevron, class: "small" }
            }
            if entry.expanded {
                div { class: "thinking-body",
                    span { class: "bar" }
                    div {
                        class: "thinking-text {cjk_class(&entry.cjk)}",
                        lang: text::lang(&entry.cjk),
                        Text { text: entry.body }
                    }
                }
            }
        }
    }
}

/// Before a tool call's name: how far it got, by shape as well as colour (Gruvbox's green and
/// yellow are close): a dot while it runs, a hand while it waits for the user, a check, a
/// cross, a slashed circle.
#[component]
fn StateMark(state: ToolState) -> Element {
    let (class, icon) = match state {
        ToolState::Streaming | ToolState::Running => ("working", None),
        ToolState::Awaiting => ("awaiting", Some(Lucide::Hand)),
        ToolState::Done => ("done", Some(Lucide::Check)),
        ToolState::Failed => ("failed", Some(Lucide::X)),
        ToolState::Denied => ("faint", Some(Lucide::Ban)),
        ToolState::Cancelled => ("faint", Some(Lucide::CircleSlash)),
    };
    rsx! {
        span { class: "state-mark {class}", "aria-hidden": "true",
            if let Some(name) = icon {
                Icon { icon: name, class: "small" }
            } else {
                span { class: "dot" }
            }
        }
    }
}

/// A run of tool calls in one line: open or folded, how it went, what it did.
#[component]
fn RunLine(entry: Entry) -> Element {
    let view = use_context::<View>();
    let index = entry.index;
    let chevron = if entry.group_open {
        Lucide::ChevronDown
    } else {
        Lucide::ChevronRight
    };
    let failed = if entry.group_state == ToolState::Failed {
        "failed"
    } else {
        ""
    };
    rsx! {
        button {
            class: "lane run-line",
            "aria-label": "{entry.group_summary}",
            "aria-expanded": "{entry.group_open}",
            onclick: move |_| act_on(view, index, Action::ToggleRun),
            Icon { icon: chevron, class: "small faint" }
            StateMark { state: entry.group_state }
            span { class: "run-summary {failed}", "{entry.group_summary}" }
        }
    }
}

/// A tool call's card in its row: as wide as the prose, or as its content needs where there is
/// room; it starts where the prose starts, moving left only where it runs out of room to the
/// right, as code does. A run's calls are indented under its line.
#[component]
fn ToolRow(entry: Entry) -> Element {
    let class = match entry.group {
        GroupRole::Alone => "tool-row",
        GroupRole::Head => "tool-row in-run head",
        GroupRole::Member => "tool-row in-run",
    };
    rsx! {
        div { class,
            div { class: "inset" }
            ToolCard { entry }
        }
    }
}

/// A tool call: its name, what it was given, how it went; opened, its diff, input, Markdown,
/// output and images.
#[component]
fn ToolCard(entry: Entry) -> Element {
    let view = use_context::<View>();
    let index = entry.index;
    let state = entry.tool_state;
    let state_label = match state {
        ToolState::Awaiting => "Waiting for you",
        ToolState::Failed => "Failed",
        ToolState::Denied => "Denied",
        ToolState::Cancelled => "Cancelled",
        ToolState::Streaming | ToolState::Running | ToolState::Done => "",
    };
    let failed = state == ToolState::Failed;
    let ink = if failed { "danger" } else { "" };
    let card = if state == ToolState::Awaiting {
        "tool-card awaiting"
    } else {
        "tool-card"
    };
    let summary = if entry.tool_summary_prose {
        "tool-summary"
    } else {
        "tool-summary mono"
    };
    let chevron = if entry.expanded {
        Lucide::ChevronDown
    } else {
        Lucide::ChevronRight
    };
    rsx! {
        div { class: card,
            button {
                class: "tool-head",
                "aria-expanded": "{entry.expanded}",
                onclick: move |_| act_on(view, index, Action::Toggle),
                StateMark { state }
                span { class: "tool-title", "{entry.tool_title}" }
                span { class: summary, "{entry.tool_summary}" }
                if !state_label.is_empty() {
                    span { class: if failed { "small tool-state failed" } else { "small tool-state" },
                        "{state_label}"
                    }
                }
                Icon { icon: chevron, class: "small" }
            }
            if entry.expanded {
                div { class: "tool-body",
                    if !entry.diff.is_empty() {
                        DiffView { lines: entry.diff }
                    }
                    if entry.has_detail {
                        CodeBox { body: entry.detail, ink: "secondary" }
                    }
                    if !entry.blocks.is_empty() {
                        div { class: "tool-md",
                            Markdown { blocks: entry.blocks, flat: true }
                        }
                    }
                    if entry.has_output {
                        CodeBox {
                            body: entry.output,
                            ink,
                            note: entry.output_note,
                            oncopy: move |()| act_on(view, index, Action::CopyEntry),
                        }
                    }
                    for shown in entry.images {
                        ImageView { shown, origin: entry.tool_summary.clone() }
                    }
                }
            }
        }
    }
}

/// A tool's input or output, in code's type: it wraps. Below it, a note (what was left out);
/// with `oncopy`, a button to copy it.
#[component]
fn CodeBox(
    body: Styled,
    #[props(into)] ink: String,
    #[props(default)] note: String,
    #[props(default)] oncopy: Option<EventHandler<()>>,
) -> Element {
    let copyable = if oncopy.is_some() { "copyable" } else { "" };
    rsx! {
        div { class: "codebox {ink} {copyable}",
            Text { text: body }
            if !note.is_empty() {
                div { class: "caption", "{note}" }
            }
            if let Some(oncopy) = oncopy {
                IconButton {
                    icon: Lucide::Copy,
                    label: "Copy",
                    class: "tiny codebox-copy",
                    onclick: move |_| oncopy.call(()),
                }
            }
        }
    }
}

/// A diff's lines, added and removed ones marked by colour and sign.
#[component]
fn DiffView(lines: Vec<DiffLine>) -> Element {
    rsx! {
        div { class: "diff",
            for line in lines {
                DiffRow { line }
            }
        }
    }
}

/// One line of a diff.
#[component]
fn DiffRow(line: DiffLine) -> Element {
    let (class, sign) = match line.kind {
        DiffKind::Added => ("added", "+"),
        DiffKind::Removed => ("removed", "−"),
        DiffKind::Context => ("", ""),
        DiffKind::Hunk => ("hunk", ""),
    };
    rsx! {
        div { class: "diff-line {class}",
            span { class: "diff-sign", "{sign}" }
            span { class: "diff-text", "{line.text}" }
        }
    }
}

/// An image in the conversation: its pixels at the screen's scale, at most 480px wide and the
/// room there is, its shape kept; a frame while it is decoded; why, if it isn't shown. `origin`
/// (a tool call's summary, the file read, say) names it for assistive technology; empty for
/// one the user sent.
#[component]
fn ImageView(shown: ShownImage, #[props(default)] origin: String) -> Element {
    let view = use_context::<View>();
    let url = match shown.state {
        ImageState::Shown => image_url(&shown.image, &view.images.read()),
        ImageState::Pending | ImageState::Failed => String::new(),
    };
    if shown.state == ImageState::Failed {
        return rsx! {
            div { class: "small image-failed", "Image not shown: {shown.note}" }
        };
    }
    if url.is_empty() {
        return rsx! {
            div { class: "image-pending",
                span { class: "caption", "Image" }
            }
        };
    }
    let width = f64::from(shown.width_px) / dom::pixel_ratio();
    let height = shown.height_px.max(1);
    let named = if origin.is_empty() {
        "Image".to_owned()
    } else {
        format!("Image from {origin}")
    };
    rsx! {
        img {
            class: "shown-image",
            src: url,
            alt: "{named}, {shown.width_px} × {shown.height_px} pixels",
            style: "width: {width}px; aspect-ratio: {shown.width_px.max(1)} / {height};",
        }
    }
}

/// A new session's greeting. Kept while the conversation runs, its words faded out, so that the
/// heron can fly off as the conversation begins.
#[component]
fn Greeting() -> Element {
    let view = use_context::<View>();
    let empty = use_memo(move || view.entries.read().is_empty());
    let cwd = view.session.read().cwd.clone();
    let words = if empty() {
        "greeting-words"
    } else {
        "greeting-words gone"
    };
    rsx! {
        div { class: "greeting",
            Heron {}
            div { class: words,
                div { class: "greeting-title", "What should we work on?" }
                div { class: "greeting-place small", "in {cwd}" }
            }
        }
    }
}

/// Where the heron is: standing over the empty conversation, flying off, out of sight, or
/// flying in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Flight {
    Standing,
    Leaving,
    Away,
    Arriving,
}

/// edualc's heron, over a new session's empty conversation. It stands there while the
/// conversation is empty, flies off when it begins, and flies in when a new one opens. It faces
/// left, so it leaves to the upper left and comes in from the upper right.
#[component]
fn Heron() -> Element {
    let view = use_context::<View>();
    let wanted = use_memo(move || view.entries.read().is_empty());
    let mut flight = use_signal(|| {
        if *wanted.peek() {
            Flight::Standing
        } else {
            Flight::Away
        }
    });
    // The wing beat's frame: up, level, down, level.
    let mut beat = use_signal(|| 0_u8);
    // Which flight is under way, so that one cut short stops.
    let mut flown = use_signal(|| 0_u32);
    let mut last = use_signal(|| *wanted.peek());

    use_effect(move || {
        let wanted = wanted();
        if *last.peek() == wanted {
            return;
        }
        last.set(wanted);
        let this = flown.peek().wrapping_add(1);
        flown.set(this);
        beat.set(0);
        spawn(async move {
            if wanted {
                // Out of sight first, unanimated, so that it flies in from there.
                flight.set(Flight::Away);
                dom::sleep(APPROACH_MS).await;
                if *flown.peek() != this {
                    return;
                }
                flight.set(Flight::Arriving);
            } else {
                flight.set(Flight::Leaving);
            }
            for _ in 0..BEATS {
                dom::sleep(BEAT_MS).await;
                if *flown.peek() != this {
                    return;
                }
                let next = (*beat.peek() + 1) % 4;
                beat.set(next);
            }
            flight.set(if wanted {
                Flight::Standing
            } else {
                Flight::Away
            });
        });
    });

    let flight = flight();
    let class = match flight {
        Flight::Standing => "heron standing",
        Flight::Leaving => "heron leaving",
        Flight::Away => "heron away",
        Flight::Arriving => "heron arriving",
    };
    let frame = if flight == Flight::Standing {
        0
    } else {
        match beat() {
            0 => 1,
            2 => 2,
            _ => 3,
        }
    };
    rsx! {
        div { class, "aria-hidden": "true",
            div { class: "heron-x",
                div { class: "heron-y",
                    for (at, drawing) in HERON.iter().enumerate() {
                        div {
                            class: if at == frame { "heron-bird shown" } else { "heron-bird" },
                            style: "mask-image: url({drawing}); -webkit-mask-image: url({drawing});",
                        }
                    }
                }
            }
        }
    }
}
