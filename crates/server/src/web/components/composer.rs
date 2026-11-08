//! The composer, as the native window's (`ui/app.slint`'s `EditArea`, `SendButton`,
//! `ContextMeter` and `CommandList`): the bottom of the conversation, edge to edge, is the text
//! box. A line sets it apart from the conversation and takes the accent while writing, glows
//! while a turn runs, and stays lit while Claude waits for an answer; a click anywhere in it
//! writes. It grows with the draft, then scrolls; dragged by its top edge, it keeps that height.
//! Made taller, it lies over the conversation's column. Under the text: the mode and model, what
//! Claude is doing, the context, send. Above it, in the dock, the slash commands matching the
//! draft.

use dioxus::prelude::*;
use edualc_core::app::Action;
use edualc_core::present::model::{
    Activity, ComposerSize, LiveState, MenuItem, PermissionKind, PromptKind, SendKey,
};

use crate::web::View;
use crate::web::view::image_url;
use crate::web::widgets::{Icon, IconButton, Lucide, Meter};

const STYLE: Asset = asset!("/assets/composer.css");
/// The text box's id, for focusing it and reading its caret.
const INPUT: &str = "composer-input";
/// The command list's id, which the text box names as what it completes from.
const COMMANDS: &str = "composer-commands";
/// The most the text box grows to with its draft before it scrolls, in CSS pixels.
const TALLEST: f64 = 320.0;
/// The most a drag makes it without a column to measure.
const TALLEST_DRAGGED: f64 = 600.0;

/// Which of the commands matching the draft the arrow keys are on: the composer moves it, the
/// list shows it. Kept at the page's root, as both reach it and neither holds the other.
#[derive(Clone, Copy)]
struct CommandIndex(Signal<usize>);

/// The command the arrow keys are on, made the first time either part asks.
fn use_command_index() -> Signal<usize> {
    use_hook(|| {
        try_consume_context::<CommandIndex>()
            .unwrap_or_else(|| {
                dioxus::core::provide_root_context(CommandIndex(Signal::new_in_scope(
                    0,
                    ScopeId::APP,
                )))
            })
            .0
    })
}

/// A drag of the top edge under way: where the pointer and the text box started, and the most
/// it may grow to.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Drag {
    from_y: f64,
    from_height: f64,
    tallest: f64,
}

/// A height in whole logical pixels, as the settings keep it: `as` saturates (and makes NaN 0),
/// so nothing is lost a `u16` could hold.
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn whole_pixels(height: f64) -> u16 {
    height.round() as u16
}

/// Slash commands matching the draft; Up and Down in the text box choose one, Tab takes it,
/// Enter runs it. A click takes it too. The text box keeps the focus: the list is told to
/// assistive technology as what it completes from.
#[component]
pub fn CommandList() -> Element {
    let view = use_context::<View>();
    let index = use_command_index();
    let composer = view.composer.read();
    let highlighted = index().min(composer.commands.len().saturating_sub(1));
    rsx! {
        document::Stylesheet { href: STYLE }
        div { id: COMMANDS, class: "commands", role: "listbox", "aria-label": "Commands",
            for (at, command) in composer.commands.iter().enumerate() {
                div {
                    key: "{command.name}",
                    id: "{COMMANDS}-{at}",
                    class: if at == highlighted { "command highlighted" } else { "command" },
                    role: "option",
                    "aria-selected": "{at == highlighted}",
                    // The focus stays in the text box.
                    onmousedown: move |event| event.prevent_default(),
                    onclick: move |_| view.act(Action::PickCommand(at)),
                    span { class: "command-name",
                        if command.hint.is_empty() {
                            "/{command.name}"
                        } else {
                            "/{command.name} {command.hint}"
                        }
                    }
                    span { class: "command-description", "{command.description}" }
                    if at == highlighted {
                        span { class: "command-key", "aria-hidden": "true", "Tab" }
                    }
                }
            }
        }
    }
}

/// The composer: what is written to Claude, and what goes with it.
#[component]
pub fn Composer() -> Element {
    let view = use_context::<View>();
    let mut draft = view.draft;
    let mut command_index = use_command_index();
    // Esc dismisses a command's hint, until the next edit.
    let mut hint_dismissed = use_signal(|| false);
    // The height dragged to (zero for none), and how tall the box is: fitted to its text, as
    // dragged, or made taller over the conversation.
    let kept_height = use_memo(move || view.page.read().composer_height);
    let mut chosen = use_signal(|| kept_height.peek().map_or(0.0, f64::from));
    let mut size = use_signal(|| {
        if kept_height.peek().is_some() {
            ComposerSize::Chosen
        } else {
            ComposerSize::Fit
        }
    });
    let mut drag = use_signal(|| None::<Drag>);
    // The app sets the kept height as the settings are read.
    use_effect(move || match kept_height() {
        Some(height_px) => {
            chosen.set(f64::from(height_px));
            size.set(ComposerSize::Chosen);
        }
        None => size.set(ComposerSize::Fit),
    });
    // A request for the focus, as a new session starts or one opens.
    let focus_serial = use_memo(move || view.page.read().focus_composer);
    let mut focus_seen = use_signal(|| *focus_serial.peek());
    use_effect(move || {
        let serial = focus_serial();
        if serial != *focus_seen.peek() {
            focus_seen.set(serial);
            dom::focus_end(&draft.peek());
        }
    });

    // How it was before it was made taller.
    let mut back = move || {
        size.set(if *chosen.peek() > 0.0 {
            ComposerSize::Chosen
        } else {
            ComposerSize::Fit
        });
    };
    let mut toggle = move |wanted: ComposerSize| {
        if *size.peek() == wanted {
            back();
        } else {
            size.set(wanted);
        }
        dom::focus_end(&draft.peek());
    };
    // Sent: a box made taller for writing goes back.
    let mut sent = move || {
        if matches!(*size.peek(), ComposerSize::Tall | ComposerSize::Full) {
            back();
        }
    };
    // Sends the draft, emptied at once (the app puts back what must wait), so an Esc right
    // after finds it empty and stops the turn.
    let mut send_draft = move || {
        let text = draft.peek().clone();
        view.act(Action::Send(text));
        draft.set(String::new());
        sent();
    };

    // The pasted images' thumbnails, encoded as they change, not as the draft does.
    let thumbnails = use_memo(move || {
        let images = view.images.read();
        view.composer
            .read()
            .images
            .iter()
            .map(|chip| image_url(&chip.thumbnail, &images))
            .collect::<Vec<String>>()
    });

    let session = view.session.read();
    let composer = view.composer.read();
    let prompt_kind = view.prompt.read().kind;
    let send_key = view.preferences.read().send_key;
    let page = view.page.read();
    let thumbnails = thumbnails.read();
    let text = draft.read();

    let size_now = size();
    let stretched = matches!(size_now, ComposerSize::Tall | ComposerSize::Full);
    let line = f64::from(page.text_size) * 1.4;
    let glowing = session.busy && prompt_kind == PromptKind::None;
    let waiting = prompt_kind != PromptKind::None;
    let commands = composer.commands.len();
    let highlighted = command_index().min(commands.saturating_sub(1));
    let active_command = (commands > 0).then(|| format!("{COMMANDS}-{highlighted}"));
    let hint_shown = !composer.argument_hint.is_empty() && !hint_dismissed();
    let stop = session.busy && text.is_empty();
    let ready = stop || !text.is_empty();

    let size_class = match size_now {
        ComposerSize::Fit => "fit",
        ComposerSize::Chosen => "chosen",
        ComposerSize::Tall => "tall",
        ComposerSize::Full => "full",
    };
    let state_class = if waiting {
        "waiting"
    } else if glowing {
        "glowing"
    } else {
        ""
    };
    let glow = match session.activity {
        Activity::Tool => "var(--info)",
        Activity::Idle | Activity::Model => "var(--claude)",
    };
    let scroll_style = if size_now == ComposerSize::Chosen {
        format!("height: {}px;", chosen().max(2.0 * line).round())
    } else {
        String::new()
    };
    let placeholder = if session.turn {
        "Write the next message; it waits until Claude is done"
    } else {
        match send_key {
            SendKey::CtrlEnter => {
                "Message Claude  ·  / for commands  ·  ! for a shell command  ·  Ctrl+Enter to send"
            }
            SendKey::Enter => {
                "Message Claude  ·  / for commands  ·  ! for a shell command  ·  Shift+Enter for a new line"
            }
        }
    };
    let send_title = if stop {
        "Stop (Esc)"
    } else {
        match send_key {
            SendKey::CtrlEnter => "Send (Ctrl+Enter)",
            SendKey::Enter => "Send (Enter)",
        }
    };
    let (mode_icon, mode_tint) = mode_look(session.mode);
    let (shell_label, shell_tint) = if composer.shell {
        ("Make it a message again", "tint-info")
    } else {
        ("Run a shell command (start with !)", "")
    };
    let (tall_icon, tall_label) = if size_now == ComposerSize::Tall {
        (Lucide::Minimize2, "Make the message box small again")
    } else {
        (Lucide::Maximize2, "Make the message box taller")
    };
    let (full_icon, full_label) = if size_now == ComposerSize::Full {
        (Lucide::Shrink, "Show the conversation again")
    } else {
        (
            Lucide::Expand,
            "Give the message the whole column, to see all of a long paste",
        )
    };
    let send_icon = if stop {
        Lucide::Square
    } else {
        Lucide::ArrowUp
    };
    let model_index = page.model_index;
    let mode_index = page.mode_index;

    // Enter sends; Shift+Enter and Ctrl+Enter start a new line. With Ctrl+Enter chosen to send
    // (the settings), Enter starts a new line instead. While commands match the draft, Up and
    // Down choose one, Enter runs it, and → (at the end) or Tab put it in the draft. Esc
    // dismisses a command's hint. Ctrl+V asks for the clipboard's image too, and pastes its
    // text as ever. What an input method still composes is its own.
    let onkeydown = move |event: KeyboardEvent| {
        if event.is_composing() {
            return;
        }
        let key = event.key();
        let modifiers = event.modifiers();
        let commands = view.composer.peek().commands.len();
        let at = (*command_index.peek()).min(commands.saturating_sub(1));
        let send_key = view.preferences.peek().send_key;
        if key == Key::Enter && modifiers.ctrl() {
            event.prevent_default();
            match send_key {
                SendKey::CtrlEnter => send_draft(),
                SendKey::Enter => dom::break_line(),
            }
            return;
        }
        if let Key::Character(typed) = &key
            && modifiers.ctrl()
            && typed.eq_ignore_ascii_case("v")
        {
            view.act(Action::PasteImage);
            return;
        }
        if commands > 0 && key == Key::ArrowUp {
            event.prevent_default();
            command_index.set(at.saturating_sub(1));
            return;
        }
        if commands > 0 && key == Key::ArrowDown {
            event.prevent_default();
            command_index.set((at + 1).min(commands - 1));
            return;
        }
        if commands > 0 && (key == Key::Tab || (key == Key::ArrowRight && dom::caret_at_end())) {
            event.prevent_default();
            view.act(Action::PickCommand(at));
            return;
        }
        if commands > 0 && key == Key::Enter && !modifiers.shift() {
            event.prevent_default();
            view.act(Action::RunCommand(at));
            draft.set(String::new());
            sent();
            return;
        }
        if key == Key::Escape
            && !view.composer.peek().argument_hint.is_empty()
            && !*hint_dismissed.peek()
        {
            event.prevent_default();
            event.stop_propagation();
            hint_dismissed.set(true);
            return;
        }
        if key == Key::Enter && !modifiers.shift() && send_key == SendKey::Enter {
            event.prevent_default();
            send_draft();
        }
    };

    // The top edge: dragged, the box takes that height and keeps it.
    let onpointerdown = move |event: PointerEvent| {
        event.prevent_default();
        let from_height = dom::scroll_height().unwrap_or_else(|| (*chosen.peek()).max(2.0 * line));
        let tallest = dom::column_height().map_or(TALLEST_DRAGGED, |height| height * 0.8);
        drag.set(Some(Drag {
            from_y: event.client_coordinates().y,
            from_height,
            tallest,
        }));
    };
    let onpointermove = move |event: PointerEvent| {
        if let Some(Drag {
            from_y,
            from_height,
            tallest,
        }) = *drag.peek()
        {
            let height = (from_height + from_y - event.client_coordinates().y)
                .min(tallest)
                .max(2.0 * line);
            chosen.set(height);
            size.set(ComposerSize::Chosen);
        }
    };
    let mut end_drag = move || {
        if drag.peek().is_some() {
            drag.set(None);
            if *size.peek() == ComposerSize::Chosen {
                view.act(Action::ComposerHeight(Some(whole_pixels(*chosen.peek()))));
            }
        }
    };

    rsx! {
        document::Stylesheet { href: STYLE }
        div {
            id: "composer",
            class: "composer {size_class} {state_class}",
            style: "--glow: {glow};",
            // A click anywhere but on its controls writes.
            onclick: move |event| {
                if !dom::on_control(&event) {
                    dom::focus_end(&draft.peek());
                }
            },
            // Made taller, Tab goes round within it.
            if stretched {
                div {
                    class: "composer-fence",
                    tabindex: "0",
                    onfocus: move |_| dom::focus_edge(false),
                }
            }
            div { class: "composer-edge", "aria-hidden": "true" }
            // While a turn runs: the top edge glows, tinted by what is at work (the model or a
            // tool), breathing slowly in and out.
            if glowing {
                div { class: "composer-glow", "aria-hidden": "true" }
            }
            // A double click fits it to its text again.
            if !stretched {
                div {
                    class: "composer-grip",
                    "aria-hidden": "true",
                    onpointerdown,
                    ondoubleclick: move |_| {
                        chosen.set(0.0);
                        size.set(ComposerSize::Fit);
                        view.act(Action::ComposerHeight(None));
                    },
                }
            }
            div { class: "composer-body",
                // The images pasted, going with the next message; each can be taken off.
                if !composer.images.is_empty() {
                    div { class: "composer-images",
                        for (index, chip) in composer.images.iter().enumerate() {
                            div { class: "image-chip",
                                img {
                                    src: thumbnails.get(index).cloned().unwrap_or_default(),
                                    alt: "A pasted image, {chip.caption}",
                                }
                                div { class: "image-chip-side",
                                    IconButton {
                                        icon: Lucide::X,
                                        label: "Take this image off the message",
                                        class: "chip-remove",
                                        onclick: move |_| view.act(Action::RemoveImage(index)),
                                    }
                                    span { class: "caption", "{chip.caption}" }
                                }
                            }
                        }
                    }
                }
                // What a shell command does, said while the draft is one, before Enter runs it.
                if composer.shell {
                    div { class: "composer-shell",
                        Icon { icon: Lucide::SquareTerminal, class: "small" }
                        span {
                            if session.sandboxed {
                                "Shell command: Enter runs it in the sandbox, beside the conversation."
                            } else {
                                "Shell command: Enter runs it beside the conversation, not sandboxed."
                            }
                            " Claude sees it, and what it printed, with your next message."
                        }
                    }
                }
                div {
                    id: "composer-scroll",
                    class: if composer.shell { "composer-scroll mono" } else { "composer-scroll" },
                    style: "{scroll_style}",
                    // An invisible copy that wraps as the text does: the box is as tall as it.
                    if size_now == ComposerSize::Fit {
                        div {
                            class: "composer-measure",
                            style: "max-height: {TALLEST}px;",
                            "aria-hidden": "true",
                            "{text} "
                        }
                    }
                    // Only drawn: the text box itself tells assistive technology.
                    if text.is_empty() {
                        div { class: "composer-placeholder", "aria-hidden": "true", "{placeholder}" }
                    }
                    // What the command in the draft takes, after it (the draft is one line then).
                    if hint_shown {
                        div { class: "composer-hint", "aria-hidden": "true",
                            span { class: "composer-typed", "{text}" }
                            "{composer.argument_hint}"
                        }
                    }
                    textarea {
                        id: INPUT,
                        class: "composer-input",
                        rows: "1",
                        value: "{text}",
                        "aria-label": "Message to Claude",
                        "aria-placeholder": "Message Claude, or start with ! for a shell command",
                        "aria-autocomplete": "list",
                        "aria-controls": (commands > 0).then_some(COMMANDS),
                        "aria-activedescendant": active_command,
                        oninput: move |event| {
                            let edited = event.value();
                            draft.set(edited.clone());
                            command_index.set(0);
                            hint_dismissed.set(false);
                            view.act(Action::DraftEdited(edited));
                        },
                        onkeydown,
                    }
                }
                div { class: "composer-tools",
                    MenuButton {
                        heading: "Permission mode",
                        items: session.modes.clone(),
                        chosen: mode_index,
                        icon: mode_icon,
                        tint: mode_tint,
                        class: "mode-menu",
                        onchoose: move |index: usize| {
                            if let Ok(index) = i32::try_from(index) {
                                let mut page = view.page;
                                page.write().mode_index = index;
                            }
                            view.act(Action::ChooseMode(index));
                        },
                    }
                    MenuButton {
                        heading: "Model",
                        items: session.models.clone(),
                        chosen: model_index,
                        icon: Lucide::Sparkle,
                        class: "model-menu",
                        onchoose: move |index: usize| {
                            if let Ok(index) = i32::try_from(index) {
                                let mut page = view.page;
                                page.write().model_index = index;
                            }
                            view.act(Action::ChooseModel(index));
                        },
                    }
                    if !session.tasks.is_empty() {
                        super::sidebar::TasksChip {}
                    }
                    // As the sidebar shows a session: a hand while Claude waits for the user, a
                    // dot while it works.
                    if session.busy {
                        super::sidebar::LiveDot {
                            state: if waiting { LiveState::Waiting } else { LiveState::Busy },
                        }
                        span { class: "composer-status", "{session.status}" }
                    }
                    span { class: "composer-spacer" }
                    // How full the context is: a short bar that warms as it fills, and the count.
                    if !session.context.is_empty() {
                        div { class: "context-meter",
                            Meter {
                                fraction: session.context_fraction,
                                label: "Context used",
                            }
                            span { class: "caption", "{session.context}" }
                        }
                    }
                    IconButton {
                        icon: Lucide::SquareTerminal,
                        label: shell_label,
                        class: shell_tint,
                        onclick: move |_| view.act(Action::ToggleShell),
                    }
                    IconButton {
                        icon: tall_icon,
                        label: tall_label,
                        class: "tint-faint",
                        onclick: move |_| toggle(ComposerSize::Tall),
                    }
                    IconButton {
                        icon: full_icon,
                        label: full_label,
                        class: "tint-faint",
                        onclick: move |_| toggle(ComposerSize::Full),
                    }
                    // While Claude works and a message is written (to send when it's done), Send
                    // stays Send, and Stop is here.
                    if session.busy && !text.is_empty() {
                        IconButton {
                            icon: Lucide::Square,
                            label: "Stop",
                            class: "tint-danger",
                            onclick: move |_| view.act(Action::Interrupt),
                        }
                    }
                    // Send, or stop while Claude works and nothing new is written (a message
                    // written meanwhile is sent and waits its turn).
                    button {
                        class: if stop { "send-button stop" } else if ready { "send-button ready" } else { "send-button" },
                        title: send_title,
                        "aria-label": if stop { "Stop" } else { "Send" },
                        disabled: !ready,
                        onclick: move |_| {
                            if stop {
                                view.act(Action::Interrupt);
                            } else {
                                send_draft();
                                dom::focus_end("");
                            }
                        },
                        Icon { icon: send_icon }
                    }
                }
            }
            if stretched {
                div {
                    class: "composer-fence",
                    tabindex: "0",
                    onfocus: move |_| dom::focus_edge(true),
                }
            }
            // While dragging, the pointer is followed over the whole page.
            if drag().is_some() {
                div {
                    class: "composer-drag",
                    onpointermove,
                    onpointerup: move |_| end_drag(),
                    onpointercancel: move |_| end_drag(),
                }
            }
        }
    }
}

/// The permission mode's icon and colour, by how much it lets through: ask, edits, plan, auto,
/// bypass.
fn mode_look(mode: PermissionKind) -> (Lucide, &'static str) {
    match mode {
        PermissionKind::AcceptEdits => (Lucide::FilePenLine, "tint-warning"),
        PermissionKind::Plan => (Lucide::Map, "tint-link"),
        PermissionKind::Auto => (Lucide::Zap, "tint-info"),
        PermissionKind::Bypass => (Lucide::ShieldAlert, "tint-danger"),
        PermissionKind::Unknown => (Lucide::ShieldQuestionMark, ""),
        PermissionKind::Default => (Lucide::ShieldCheck, ""),
    }
}

/// A compact choice in the composer's toolbar: an icon and the current choice's name, no frame
/// until hovered. Its menu opens upwards and says what each choice means. Up and Down move
/// through the choices, Enter or Space takes one; Esc closes the menu. Where the composer is too
/// narrow, the mode's shows only its icon (its name is still told).
#[component]
fn MenuButton(
    #[props(into)] heading: String,
    items: Vec<MenuItem>,
    chosen: i32,
    icon: Lucide,
    #[props(default, into)] tint: String,
    #[props(into)] class: String,
    onchoose: EventHandler<usize>,
) -> Element {
    let mut open = use_signal(|| false);
    let mut highlighted = use_signal(|| 0_usize);
    let count = items.len();
    let chosen = usize::try_from(chosen).ok().filter(|index| *index < count);
    let current = chosen
        .and_then(|index| items.get(index))
        .map(|item| item.title.clone())
        .unwrap_or_default();
    let list = format!("{class}-choices");
    let mut show = move || {
        highlighted.set(chosen.unwrap_or(0));
        open.set(true);
    };
    let mut choose = move |index: usize| {
        open.set(false);
        onchoose.call(index);
    };
    let onkeydown = move |event: KeyboardEvent| {
        let key = event.key();
        let pressed = key == Key::Enter || key == Key::Character(" ".to_owned());
        if !open() {
            if pressed || key == Key::ArrowUp || key == Key::ArrowDown {
                event.prevent_default();
                show();
            }
            return;
        }
        let at = *highlighted.peek();
        if key == Key::ArrowUp {
            event.prevent_default();
            highlighted.set(at.saturating_sub(1));
        } else if key == Key::ArrowDown {
            event.prevent_default();
            highlighted.set((at + 1).min(count.saturating_sub(1)));
        } else if key == Key::Home {
            event.prevent_default();
            highlighted.set(0);
        } else if key == Key::End {
            event.prevent_default();
            highlighted.set(count.saturating_sub(1));
        } else if pressed {
            event.prevent_default();
            if at < count {
                choose(at);
            }
        } else if key == Key::Escape {
            event.prevent_default();
            event.stop_propagation();
            open.set(false);
        }
    };
    rsx! {
        div { class: "composer-menu-holder {class}",
            div {
                class: "composer-menu-button {tint}",
                role: "combobox",
                tabindex: "0",
                title: "{heading}: {current}",
                "aria-label": heading,
                "aria-haspopup": "listbox",
                "aria-expanded": "{open}",
                "aria-controls": list.clone(),
                "aria-activedescendant": open().then(|| format!("{list}-{highlighted}")),
                onclick: move |_| {
                    if open() {
                        open.set(false);
                    } else {
                        show();
                    }
                },
                onkeydown,
                onblur: move |_| open.set(false),
                Icon { icon: icon, class: "small" }
                span { class: "menu-current", "{current}" }
                Icon { icon: Lucide::ChevronUp, class: "small menu-chevron" }
            }
            if open() {
                div {
                    id: list.clone(),
                    class: "composer-menu",
                    role: "listbox",
                    "aria-label": "{heading}",
                    div { class: "composer-menu-heading caption", "aria-hidden": "true", "{heading}" }
                    for (index, item) in items.iter().enumerate() {
                        div {
                            key: "{index}",
                            id: "{list}-{index}",
                            class: if chosen == Some(index) { "composer-choice chosen" } else { "composer-choice" },
                            "data-highlighted": "{index == highlighted()}",
                            role: "option",
                            "aria-selected": "{index == highlighted()}",
                            "aria-checked": "{chosen == Some(index)}",
                            // The focus stays on the button.
                            onmousedown: move |event| event.prevent_default(),
                            onmouseenter: move |_| highlighted.set(index),
                            onclick: move |_| choose(index),
                            div { class: "composer-choice-text",
                                div { class: "composer-choice-title", "{item.title}" }
                                if !item.description.is_empty() {
                                    div { class: "composer-choice-description", "{item.description}" }
                                }
                            }
                            Icon { icon: Lucide::Check, class: "small composer-check" }
                        }
                    }
                }
            }
        }
    }
}

/// The page's own parts the composer reaches: the text box's focus and caret, and sizes.
#[cfg(feature = "web")]
mod dom {
    use dioxus::prelude::MouseEvent;
    use dioxus::web::WebEventExt as _;
    use web_sys::wasm_bindgen::JsCast as _;
    use web_sys::{HtmlElement, HtmlTextAreaElement};

    /// What a click on doesn't write: the composer's controls and menus.
    const CONTROLS: &str =
        "button, a, textarea, input, [role='combobox'], [role='listbox'], [role='option']";
    /// What Tab reaches in the composer made taller.
    const FOCUSABLE: &str =
        "#composer .composer-body :is(button:not(:disabled), textarea, [tabindex='0'])";

    fn document() -> Option<web_sys::Document> {
        web_sys::window()?.document()
    }

    fn text_box() -> Option<HtmlTextAreaElement> {
        document()?.get_element_by_id(super::INPUT)?.dyn_into().ok()
    }

    /// Focuses the text box with the caret at the end of `text`, the draft: shown at once, as
    /// the draft just set is drawn only after this.
    pub fn focus_end(text: &str) {
        let Some(input) = text_box() else {
            return;
        };
        if input.value() != text {
            input.set_value(text);
        }
        let end = u32::try_from(text.encode_utf16().count()).unwrap_or(u32::MAX);
        if input.focus().is_err() || input.set_selection_range(end, end).is_err() {
            tracing::debug!("the text box took no focus");
        }
    }

    /// The caret is at the end of the draft, nothing selected.
    pub fn caret_at_end() -> bool {
        text_box().is_some_and(|input| {
            let length = u32::try_from(input.value().encode_utf16().count()).unwrap_or(u32::MAX);
            matches!(
                (input.selection_start(), input.selection_end()),
                (Ok(Some(start)), Ok(Some(end))) if start == end && start >= length
            )
        })
    }

    /// Ctrl+Enter, which the text box itself ignores: a line break in place of the selection,
    /// as typed (undone as typing is, and an edit like any other).
    pub fn break_line() {
        let inserted = document()
            .and_then(|document| document.dyn_into::<web_sys::HtmlDocument>().ok())
            .and_then(|document| {
                document
                    .exec_command_with_show_ui_and_value("insertText", false, "\n")
                    .ok()
            });
        if inserted != Some(true) {
            tracing::debug!("the browser put no line break in");
        }
    }

    /// The click landed on one of the composer's controls, not on its ground.
    pub fn on_control(event: &MouseEvent) -> bool {
        event
            .data()
            .try_as_web_event()
            .and_then(|event| event.target())
            .and_then(|target| target.dyn_into::<web_sys::Element>().ok())
            .and_then(|element| element.closest(CONTROLS).ok().flatten())
            .is_some()
    }

    /// The text's box's height now, in CSS pixels.
    pub fn scroll_height() -> Option<f64> {
        let scroll: HtmlElement = document()?
            .get_element_by_id("composer-scroll")?
            .dyn_into()
            .ok()?;
        Some(f64::from(scroll.offset_height()))
    }

    /// The conversation's column's height, in CSS pixels.
    pub fn column_height() -> Option<f64> {
        let column = document()?
            .get_element_by_id("composer")?
            .closest(".column")
            .ok()??;
        Some(f64::from(column.client_height()))
    }

    /// Tab reached an end of the composer made taller: the focus goes round to the first of its
    /// controls (`first`) or the last.
    pub fn focus_edge(first: bool) {
        let Some(found) =
            document().and_then(|document| document.query_selector_all(FOCUSABLE).ok())
        else {
            return;
        };
        let at = if first {
            0
        } else {
            found.length().saturating_sub(1)
        };
        if let Some(element) = found
            .item(at)
            .and_then(|node| node.dyn_into::<HtmlElement>().ok())
            && element.focus().is_err()
        {
            tracing::debug!("the composer's control took no focus");
        }
    }
}

/// Rendered on the server, the page has no parts to reach yet.
#[cfg(not(feature = "web"))]
mod dom {
    use dioxus::prelude::MouseEvent;

    pub fn focus_end(_text: &str) {}

    pub fn caret_at_end() -> bool {
        false
    }

    pub fn break_line() {}

    pub fn on_control(_event: &MouseEvent) -> bool {
        false
    }

    pub fn scroll_height() -> Option<f64> {
        None
    }

    pub fn column_height() -> Option<f64> {
        None
    }

    pub fn focus_edge(_first: bool) {}
}
