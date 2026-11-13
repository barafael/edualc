//! The panels beside the conversation and over it, as the native window's (`ui/side.slint`,
//! `ui/peek.slint`, `ui/usage.slint`, `ui/settings.slint`): the side questions (`/btw`), a task
//! followed, the usage details, and the settings.
//!
//! The settings page is opened and closed by the window itself, not by the app: its state is a
//! page-level signal, [`SettingsOpen`], which `components::Window` provides with
//! `use_context_provider(|| SettingsOpen(Signal::new(false)))` and every part reads with
//! [`use_settings_open`]. What opens it does as the native sidebar's button does:
//! [`SettingsOpen::open`] closes the usage details first. Ctrl+, opens it from wherever the
//! focus is (the window's shortcuts, `components::Window`).

use dioxus::html::geometry::PixelsVector2D;
use dioxus::prelude::*;
use edualc_core::app::Action;
use edualc_core::cjk::CjkLanguage;
use edualc_core::present::model::{
    HanLanguage, PeekStep, SendKey, SideExchange, SideState, StatRow, StepKind, TaskKind,
};
use edualc_core::settings;

use crate::web::View;
use crate::web::text::Blocks;
use crate::web::widgets::{Button, Icon, IconButton, Lucide, Meter, focus, level};

/// Whether the settings are open, over the conversation: the window's, as the native window's
/// `Preferences.open` is.
#[derive(Clone, Copy)]
pub struct SettingsOpen(pub Signal<bool>);

impl SettingsOpen {
    /// Opens the settings, the usage details closed first (they take the same place).
    pub fn open(self, view: View) {
        view.act(Action::HideUsage);
        let mut open = self.0;
        open.set(true);
    }

    /// Closes the settings, the focus going back to the writing (it was in the settings).
    pub fn close(self, view: View) {
        let mut open = self.0;
        open.set(false);
        back_to_writing(view);
    }
}

/// Whether the settings are open (see [`SettingsOpen`]).
pub fn use_settings_open() -> Signal<bool> {
    use_context::<SettingsOpen>().0
}

/// The side panel's field: its home for the keyboard (F6).
pub(super) const SIDE_ASK: &str = "side-ask";

/// The last request for the side panel's field to take the focus that a panel took: a panel
/// just made takes one it missed, as the native window's `Side.focus-wanted` has it take it.
static SIDE_FOCUS_TAKEN: GlobalSignal<u32> = Signal::global(|| 0);

/// Scrolls a list to its end: the latest is at its foot.
fn scroll_to_end(list: Option<MountedEvent>) {
    if let Some(list) = list {
        spawn(async move {
            let end = match list.get_scroll_size().await {
                Ok(size) => size.height,
                Err(error) => return tracing::debug!(?error, "a list without a size"),
            };
            let to = PixelsVector2D::new(0.0, end);
            if let Err(error) = list.scroll(to, ScrollBehavior::Instant).await {
                tracing::debug!(?error, "a list didn't scroll");
            }
        });
    }
}

/// Sends the focus to the composer, as the native window's `go-to(FocusArea.writing)`.
fn back_to_writing(view: View) {
    let mut page = view.page;
    page.write().focus_composer += 1;
}

/// Side questions (the terminal's `/btw`), beside the conversation instead of over the input:
/// asked and answered with the conversation's context but never part of it, kept while the
/// panel is folded away, and taken up into the conversation or a branch of it when wanted.
#[component]
pub fn SidePanel() -> Element {
    let view = use_context::<View>();
    let side = view.side.read();
    let mut question = use_signal(String::new);
    let mut field = use_signal(|| None::<MountedEvent>);
    let mut list = use_signal(|| None::<MountedEvent>);
    // Asked to ask: the field takes the focus, a panel just made too if it missed the asking.
    use_effect(move || {
        let wanted = view.page.read().focus_side;
        if wanted != *SIDE_FOCUS_TAKEN.peek()
            && let Some(field) = field.read().clone()
        {
            *SIDE_FOCUS_TAKEN.write() = wanted;
            focus(Some(field));
        }
    });
    let placeholder = if !side.can_ask {
        "Continue this session here to ask on the side"
    } else if side.exchanges.is_empty() {
        "Ask on the side"
    } else {
        "Ask a follow-up"
    };
    let asks = side.can_ask && !side.pending;
    rsx! {
        aside { class: "side side-panel", "aria-labelledby": "side-title",
            div { class: "panel-head",
                Icon { icon: Lucide::MessageCircleQuestionMark, class: "claude" }
                div { class: "panel-heading",
                    h2 { id: "side-title", class: "panel-title", "By the way" }
                    div { class: "caption elide",
                        "Sees the conversation; doesn't join it or run tools"
                    }
                }
                IconButton {
                    icon: Lucide::PanelRightClose,
                    label: "Fold away (it waits in the header)",
                    onclick: move |_| view.act(Action::ToggleSide),
                }
                IconButton {
                    icon: Lucide::X,
                    label: "Dismiss the side questions",
                    onclick: move |_| view.act(Action::DismissSide),
                }
            }
            div {
                class: "panel-list",
                role: "log",
                "aria-label": "Side questions and their answers",
                onmounted: move |event| list.set(Some(event)),
                // As the transcript does: the latest stays in sight as the rows grow.
                div {
                    class: "panel-rows",
                    onresize: move |_| scroll_to_end(list.read().clone()),
                    if side.exchanges.is_empty() {
                        p { class: "small panel-empty",
                            "Ask something on the side while Claude works: about the code, the conversation, anything. The conversation goes on undisturbed."
                        }
                    }
                    for (index, exchange) in side.exchanges.iter().enumerate() {
                        Exchange { key: "{index}", index, exchange: exchange.clone() }
                    }
                }
            }
            div { class: "side-ask",
                input {
                    id: SIDE_ASK,
                    class: "field",
                    r#type: "text",
                    placeholder,
                    "aria-label": placeholder,
                    disabled: !asks,
                    value: "{question}",
                    onmounted: move |event| field.set(Some(event)),
                    oninput: move |event| question.set(event.value()),
                    onkeydown: move |event: KeyboardEvent| {
                        if event.key() == Key::Enter && !event.is_composing() {
                            event.prevent_default();
                            let asked = std::mem::take(&mut *question.write());
                            view.act(Action::AskSide(asked));
                        }
                    },
                }
                // One above the other: side by side, they don't fit a narrow panel.
                div { class: "side-actions",
                    Button {
                        icon: Lucide::CornerDownLeft,
                        disabled: !side.has_answers,
                        onclick: move |_| view.act(Action::SideToConversation),
                        "Use in the conversation"
                    }
                    Button {
                        icon: Lucide::GitBranch,
                        disabled: !side.can_branch,
                        onclick: move |_| view.act(Action::BranchSide),
                        "Branch into a session"
                    }
                }
            }
        }
    }
}

/// A side question and what came of it: an answer, still waited for, or why there is none.
#[component]
fn Exchange(index: usize, exchange: SideExchange) -> Element {
    let view = use_context::<View>();
    rsx! {
        div { class: "exchange",
            div { class: "bubble plain", "{exchange.question}" }
            match exchange.state {
                SideState::Pending => rsx! {
                    Waiting { text: "Answering" }
                },
                SideState::Retrying => rsx! {
                    Waiting { text: "Waiting for the API, then trying again" }
                },
                SideState::Answered => rsx! {
                    div { class: "answer",
                        Blocks { blocks: exchange.answer }
                        IconButton {
                            icon: Lucide::Copy,
                            label: "Copy the answer",
                            class: "copy-answer",
                            onclick: move |_| view.act(Action::CopySide(index)),
                        }
                    }
                },
                SideState::Failed => rsx! {
                    div { class: "failed plain", "{exchange.error}" }
                },
            }
        }
    }
}

/// An answer still waited for: a dot in Claude's colour, and why it is waited for.
#[component]
fn Waiting(#[props(into)] text: String) -> Element {
    rsx! {
        div { class: "waiting small", role: "status",
            span { class: "dot", "aria-hidden": "true" }
            "{text}"
        }
    }
}

/// A task followed beside the conversation, read only, the latest at the foot: a subagent's
/// work (what it was asked, what it says and each tool it calls), or what a background command
/// prints. The chip's list of what runs opens it.
#[component]
pub fn PeekPanel() -> Element {
    let view = use_context::<View>();
    let peek = view.peek.read();
    let conversation = peek.kind == TaskKind::Agent;
    let mut list = use_signal(|| None::<MountedEvent>);
    let (icon, close) = if conversation {
        (Lucide::Bot, "Stop following this agent")
    } else {
        (Lucide::SquareTerminal, "Stop following this command")
    };
    let state = if peek.running { "running" } else { "ended" };
    rsx! {
        aside { class: "side peek-panel", "aria-labelledby": "peek-title",
            div { class: "panel-head",
                Icon { icon, class: "claude" }
                div { class: "panel-heading",
                    h2 { id: "peek-title", class: "panel-title elide", "{peek.title}" }
                    div { class: "caption elide", "{peek.detail} · {state}" }
                }
                IconButton {
                    icon: Lucide::X,
                    label: close,
                    onclick: move |_| view.act(Action::ClosePeek),
                }
            }
            if !conversation && !peek.note.is_empty() {
                div { class: "caption peek-note", "{peek.note}" }
            }
            div {
                class: "panel-list",
                tabindex: "0",
                "aria-label": if conversation { "Its conversation" } else { "What it printed" },
                onmounted: move |event| list.set(Some(event)),
                // As the transcript does: the latest stays in sight as the rows grow.
                div {
                    class: "panel-rows peek-rows",
                    onresize: move |_| scroll_to_end(list.read().clone()),
                    if conversation {
                        if peek.steps.is_empty() {
                            p { class: "small panel-empty secondary",
                                "Nothing of its conversation is saved yet. It shows here as it goes."
                            }
                        }
                        for (index, step) in peek.steps.iter().enumerate() {
                            Step { key: "{index}", step: step.clone() }
                        }
                    } else {
                        if peek.output.is_empty() && peek.note.is_empty() {
                            p { class: "small panel-empty secondary",
                                if peek.running {
                                    "Nothing printed yet. It shows here as it goes."
                                } else {
                                    "It printed nothing."
                                }
                            }
                        }
                        for (index, line) in peek.output.iter().enumerate() {
                            div { key: "{index}", class: "output-line small", "{line}" }
                        }
                    }
                }
            }
        }
    }
}

/// A step of a subagent's conversation: what it was asked, what it says, or a tool it calls.
#[component]
fn Step(step: PeekStep) -> Element {
    match step.kind {
        StepKind::Asked => rsx! {
            div { class: "step bubble plain", "{step.text}" }
        },
        StepKind::Said => rsx! {
            div { class: "step plain", "{step.text}" }
        },
        StepKind::Called => rsx! {
            div { class: "step called small",
                span { class: "called-tool", "{step.title}" }
                span { class: "called-input elide", "{step.text}" }
            }
        },
    }
}

/// The usage details, over the conversation: the plan and its limits and when they start over,
/// this session's cost and time, what fills the context, and the last day and week as the CLI's
/// `/usage` sees them, with its advice; a tab for each. Esc, the cross or a click beside them
/// closes them.
#[component]
pub fn Usage() -> Element {
    let view = use_context::<View>();
    let open = view.usage.read().open;
    rsx! {
        if open {
            UsagePanel {}
        }
    }
}

/// The usage details while open: made again each time, at the first tab.
#[component]
fn UsagePanel() -> Element {
    let view = use_context::<View>();
    let usage = view.usage.read();
    // The section shown: the one picked, or the last there is.
    let mut tab = use_signal(|| 0_usize);
    let mut tabs = use_signal(Vec::<Option<MountedEvent>>::new);
    let count = usage.sections.len();
    let shown = tab().min(count.saturating_sub(1));
    let close = move || {
        view.act(Action::HideUsage);
        back_to_writing(view);
    };
    rsx! {
        div {
            class: "overlay",
            onkeydown: move |event: KeyboardEvent| {
                if event.key() == Key::Escape {
                    event.prevent_default();
                    event.stop_propagation();
                    close();
                }
            },
            div { class: "overlay-dim", onclick: move |_| close() }
            div {
                class: "overlay-panel usage-panel",
                role: "dialog",
                tabindex: "-1",
                "aria-labelledby": "usage-title",
                "aria-busy": "{usage.loading}",
                onmounted: move |event| focus(Some(event)),
                div { class: "overlay-head",
                    Icon { icon: Lucide::Gauge, class: "claude large" }
                    div { class: "panel-heading",
                        h2 { id: "usage-title", class: "overlay-title", "Usage" }
                        div { class: "caption",
                            if usage.loading {
                                "Asking Claude Code"
                            } else {
                                "As Claude Code's /usage and /context report it"
                            }
                        }
                    }
                    IconButton {
                        icon: Lucide::RefreshCw,
                        label: "Ask again",
                        disabled: usage.loading,
                        onclick: move |_| view.act(Action::ShowUsage),
                    }
                    IconButton {
                        icon: Lucide::X,
                        label: "Back to the conversation (Esc)",
                        onclick: move |_| close(),
                    }
                }
                if !usage.note.is_empty() {
                    div { class: "small usage-note", role: "note", "{usage.note}" }
                }
                // A tab for each section, one shown at a time, instead of one long list.
                if count > 0 {
                    div { class: "usage-tabs", role: "tablist", "aria-label": "Usage",
                        for (index, section) in usage.sections.iter().enumerate() {
                            button {
                                key: "{index}",
                                id: "usage-tab-{index}",
                                class: if index == shown { "usage-tab selected" } else { "usage-tab" },
                                role: "tab",
                                "aria-selected": "{index == shown}",
                                "aria-controls": "usage-section",
                                tabindex: if index == shown { "0" } else { "-1" },
                                onmounted: move |event| {
                                    let mut tabs = tabs.write();
                                    if tabs.len() <= index {
                                        tabs.resize(index + 1, None);
                                    }
                                    if let Some(slot) = tabs.get_mut(index) {
                                        *slot = Some(event);
                                    }
                                },
                                onclick: move |_| tab.set(index),
                                // The arrow keys go from tab to tab, as a tab list's do.
                                onkeydown: move |event: KeyboardEvent| {
                                    let key = event.key();
                                    let to = if key == Key::ArrowRight {
                                        (index + 1) % count
                                    } else if key == Key::ArrowLeft {
                                        (index + count - 1) % count
                                    } else if key == Key::Home {
                                        0
                                    } else if key == Key::End {
                                        count - 1
                                    } else {
                                        return;
                                    };
                                    event.prevent_default();
                                    tab.set(to);
                                    focus(tabs.read().get(to).cloned().flatten());
                                },
                                "{section.title}"
                            }
                        }
                    }
                }
                match usage.sections.get(shown) {
                    None => rsx! {
                        div { class: "usage-empty small",
                            if !usage.loading {
                                "Claude Code has no numbers to report yet."
                            }
                        }
                    },
                    Some(section) => rsx! {
                        div {
                            id: "usage-section",
                            class: "usage-section",
                            role: "tabpanel",
                            tabindex: "0",
                            "aria-labelledby": "usage-tab-{shown}",
                            div { class: "usage-rows",
                                if !section.note.is_empty() {
                                    div { class: "small", "{section.note}" }
                                }
                                for (index, row) in section.rows.iter().enumerate() {
                                    StatLine { key: "{index}", row: row.clone() }
                                }
                            }
                        }
                    },
                }
            }
        }
    }
}

/// A number of the usage details: what it is, its value (warming as a limit fills), a meter
/// where it is a share of something, and what is said of it.
#[component]
fn StatLine(row: StatRow) -> Element {
    let level = level(row.fraction);
    rsx! {
        div { class: "stat",
            div { class: "stat-line",
                span { class: "stat-label elide", "{row.label}" }
                span { class: "stat-value {level}", "{row.value}" }
            }
            Meter { fraction: row.fraction, label: row.label.clone() }
            if !row.detail.is_empty() {
                div { class: "small", "{row.detail}" }
            }
        }
    }
}

/// edualc's settings, over the conversation as the usage details are; Esc, the cross or a click
/// beside them closes them. Each takes effect at once and is kept in `settings.json`.
#[component]
pub fn Settings() -> Element {
    let open = use_settings_open();
    rsx! {
        if open() {
            SettingsPanel {}
        }
    }
}

/// The settings while open. The focus comes in as they open, and Tab goes round within them:
/// what is typed must not go to the composer they cover.
#[component]
fn SettingsPanel() -> Element {
    let view = use_context::<View>();
    let settings_open = use_context::<SettingsOpen>();
    let preferences = view.preferences.read();
    let mut first = use_signal(|| None::<MountedEvent>);
    let mut last = use_signal(|| None::<MountedEvent>);
    let send_key = preferences.send_key;
    let han = preferences.han_language;
    rsx! {
        div {
            class: "overlay",
            onkeydown: move |event: KeyboardEvent| {
                if event.key() == Key::Escape {
                    event.prevent_default();
                    event.stop_propagation();
                    settings_open.close(view);
                }
            },
            div {
                class: "overlay-dim",
                onclick: move |_| settings_open.close(view),
            }
            div {
                class: "overlay-panel settings-panel",
                role: "dialog",
                "aria-modal": "true",
                "aria-labelledby": "settings-title",
                // Tab goes round: from before the first control to the last, and back.
                div {
                    class: "fence",
                    tabindex: "0",
                    onfocus: move |_| focus(last.read().clone()),
                }
                div { class: "overlay-head",
                    Icon { icon: Lucide::Settings, class: "claude large" }
                    div { class: "panel-heading",
                        h2 { id: "settings-title", class: "overlay-title", "Settings" }
                        div { class: "caption",
                            "Kept in edualc's settings.json; each takes effect at once"
                        }
                    }
                    button {
                        class: "icon-button",
                        title: "Back to the conversation (Esc)",
                        "aria-label": "Back to the conversation (Esc)",
                        onmounted: move |event| {
                            first.set(Some(event.clone()));
                            focus(Some(event));
                        },
                        onclick: move |_| settings_open.close(view),
                        Icon { icon: Lucide::X }
                    }
                }
                div { class: "settings-scroll",
                    div { class: "settings-page",
                        h3 { id: "send-key", class: "settings-heading", "Sending" }
                        div { class: "choices", role: "radiogroup", "aria-labelledby": "send-key",
                            Choice {
                                id: "send-enter",
                                title: "Enter sends",
                                detail: "Shift+Enter or Ctrl+Enter start a new line.",
                                chosen: send_key == SendKey::Enter,
                                onclick: move |()| view.act(Action::SendWith(settings::SendKey::Enter)),
                            }
                            Choice {
                                id: "send-ctrl-enter",
                                title: "Ctrl+Enter sends",
                                detail: "Enter starts a new line, so a stray Enter never sends half a message.",
                                chosen: send_key == SendKey::CtrlEnter,
                                onclick: move |()| view.act(Action::SendWith(settings::SendKey::CtrlEnter)),
                            }
                        }
                        DaemonSettings { running: preferences.running_sessions }
                        // The app reaches one edualcd at a time; the start screen lists the others.
                        if cfg!(feature = "mobile") {
                            h3 { class: "settings-heading spaced", "edualcd" }
                            p { class: "small",
                                "edualcd runs Claude Code, and keeps its sessions going while the app is away. Choosing another closes this window; its sessions go on."
                            }
                            div { class: "settings-actions",
                                Button {
                                    icon: Lucide::RefreshCw,
                                    onclick: move |_| view.act(Action::Quit),
                                    "Choose another edualcd"
                                }
                            }
                        }
                        // Kana and Hangul tell a text's language themselves; Han characters
                        // alone don't.
                        h3 { id: "han-language", class: "settings-heading spaced",
                            "Text in Han characters alone"
                        }
                        p { class: "small",
                            "Chinese, Japanese and Korean draw many Han characters differently. A message with kana is set as Japanese and one with Hangul as Korean; one in Han characters alone as chosen here."
                        }
                        div {
                            class: "choices",
                            role: "radiogroup",
                            "aria-labelledby": "han-language",
                            Choice {
                                id: "han-japanese",
                                title: "Japanese",
                                detail: "Noto Sans CJK JP",
                                chosen: han == HanLanguage::Japanese,
                                onclick: move |()| view.act(Action::HanLanguage(CjkLanguage::Japanese)),
                            }
                            Choice {
                                id: "han-simplified-chinese",
                                title: "Simplified Chinese",
                                detail: "Noto Sans CJK SC",
                                chosen: han == HanLanguage::SimplifiedChinese,
                                onclick: move |()| view.act(Action::HanLanguage(CjkLanguage::SimplifiedChinese)),
                            }
                            Choice {
                                id: "han-traditional-chinese",
                                title: "Traditional Chinese",
                                detail: "Noto Sans CJK TC",
                                chosen: han == HanLanguage::TraditionalChinese,
                                onclick: move |()| view.act(Action::HanLanguage(CjkLanguage::TraditionalChinese)),
                            }
                            Choice {
                                id: "han-korean",
                                title: "Korean",
                                detail: "Noto Sans CJK KR",
                                chosen: han == HanLanguage::Korean,
                                onmounted: move |event| last.set(Some(event)),
                                onclick: move |()| view.act(Action::HanLanguage(CjkLanguage::Korean)),
                            }
                        }
                    }
                }
                div {
                    class: "fence",
                    tabindex: "0",
                    onfocus: move |_| focus(first.read().clone()),
                }
            }
        }
    }
}

/// One of a setting's choices: its name, what it means, and whether it is the one in use.
#[component]
fn Choice(
    #[props(into)] id: String,
    #[props(into)] title: String,
    #[props(into)] detail: String,
    chosen: bool,
    onclick: EventHandler,
    #[props(default)] onmounted: Option<EventHandler<MountedEvent>>,
) -> Element {
    let icon = if chosen {
        Lucide::CircleCheck
    } else {
        Lucide::Circle
    };
    rsx! {
        button {
            class: if chosen { "setting choice selected" } else { "setting choice" },
            role: "radio",
            "aria-checked": "{chosen}",
            "aria-describedby": "{id}-detail",
            onmounted: move |event| {
                if let Some(onmounted) = onmounted {
                    onmounted.call(event);
                }
            },
            onclick: move |_| onclick.call(()),
            Icon { icon, class: "small mark" }
            span { class: "setting-text",
                span { class: "setting-title", "{title}" }
                span { id: "{id}-detail", class: "small", "{detail}" }
            }
        }
    }
}

/// edualc's daemon, which runs every window's sessions: shut down, for a newly installed edualc
/// to take over. The first press asks again, saying how many sessions end.
#[component]
fn DaemonSettings(running: i32) -> Element {
    let view = use_context::<View>();
    let mut confirming = use_signal(|| false);
    let label = match (confirming(), running) {
        (false, _) => "Shut down edualc…".to_owned(),
        (true, 1) => "End 1 session and shut down".to_owned(),
        (true, count) => format!("End {count} sessions and shut down"),
    };
    rsx! {
        h3 { class: "settings-heading spaced", "edualc's daemon" }
        p { class: "small",
            "It runs the sessions of every window, and goes on while one runs, so a newly installed edualc takes over only once it is shut down. Shutting down ends every session, in every window; the sidebar continues them in the next window you open."
        }
        div { class: "daemon-shutdown",
            Button {
                danger: true,
                onclick: move |_| {
                    if confirming() {
                        confirming.set(false);
                        view.act(Action::ShutDown);
                    } else {
                        confirming.set(true);
                    }
                },
                "{label}"
            }
            if confirming() {
                Button { onclick: move |_| confirming.set(false), "Cancel" }
            }
        }
    }
}
