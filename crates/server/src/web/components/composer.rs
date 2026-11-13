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
use edualc_core::present::Image;
use edualc_core::present::model::{
    Activity, ComposerSize, LiveState, MenuItem, PermissionKind, PromptKind, SendKey,
};

use crate::web::View;
use crate::web::view::image_url;
use crate::web::widgets::{Icon, IconButton, Lucide, Meter};

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

/// A drag of the top edge under way: where the pointer started, and, once measured, where the
/// text box started and the most it may grow to.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Drag {
    from_y: f64,
    measured: Option<Measured>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
struct Measured {
    from_height: f64,
    tallest: f64,
}

/// A height in whole logical pixels, as the settings keep it: within what a `u16` holds, and 0 for
/// what isn't a number.
fn whole_pixels(height: f64) -> u16 {
    height
        .round()
        .clamp(0.0, f64::from(u16::MAX))
        .to_string()
        .parse()
        .unwrap_or_default()
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
    // Where a press lands is the script's to see; it keeps it (see `dom::clicked`).
    use_hook(dom::watch_presses);
    // The text box holds what is typed itself: set from here as each keystroke comes back, it
    // would lose those typed meanwhile (the app's WebView answers later than a browser's page
    // does). The draft is put in it only as the app changes it otherwise (sent, a command
    // picked, a draft restored).
    let mut typed = use_signal(String::new);
    use_effect(move || {
        let text = view.draft.read();
        // Most often what was just typed: compared before it is copied.
        if *text != *typed.peek() {
            dom::set_text(&text);
            typed.set(text.clone());
        }
    });
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
    // Whether Enter sends: asked as the text box takes the focus, not with each key (a keyboard
    // with keys is attached; see `platform::keyboard_attached`).
    let mut keyboard = use_signal(|| true);
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
            // On a touch screen it waits to be tapped: the keyboard would cover what opened.
            crate::web::dom::grab_focus_at_end(INPUT);
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
    // after finds it empty and stops the turn. Kept while no app takes it (the line not open
    // yet).
    let mut send_draft = move || {
        let text = draft.peek().clone();
        if view.try_act(Action::Send(text)) {
            draft.set(String::new());
            sent();
        }
    };

    // The pasted images' thumbnails, encoded as they change: not as the rest of the composer
    // (the commands matching the draft) does, at every key.
    let chips = use_memo(move || {
        view.composer
            .read()
            .images
            .iter()
            .map(|chip| chip.thumbnail.clone())
            .collect::<Vec<Image>>()
    });
    let thumbnails = use_memo(move || {
        let images = view.images.read();
        chips
            .read()
            .iter()
            .map(|thumbnail| image_url(thumbnail, &images))
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
    // dismisses a command's hint. What an input method still composes is its own. An image
    // pasted comes with the paste (`onpaste`), from the browser's clipboard.
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
            if view.try_act(Action::RunCommand(at)) {
                draft.set(String::new());
                sent();
            }
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
        // A phone's own keyboard has no Shift+Enter: there Enter starts a new line, and the
        // send button sends.
        if key == Key::Enter && !modifiers.shift() && send_key == SendKey::Enter && *keyboard.peek()
        {
            event.prevent_default();
            send_draft();
        }
    };

    // The top edge: dragged, the box takes that height and keeps it. The drag begins at once,
    // so that a release is never missed; it moves the box once the heights are measured (in
    // the app, the WebView answers a moment later).
    let onpointerdown = move |event: PointerEvent| {
        event.prevent_default();
        let from_y = event.client_coordinates().y;
        drag.set(Some(Drag {
            from_y,
            measured: None,
        }));
        spawn(async move {
            let (scroll, column) = dom::heights().await;
            let measured = Measured {
                from_height: scroll.unwrap_or_else(|| (*chosen.peek()).max(2.0 * line)),
                tallest: column.map_or(TALLEST_DRAGGED, |height| height * 0.8),
            };
            // Released meanwhile, or another drag begun: this one is over.
            if *drag.peek()
                == Some(Drag {
                    from_y,
                    measured: None,
                })
            {
                drag.set(Some(Drag {
                    from_y,
                    measured: Some(measured),
                }));
            }
        });
    };
    let onpointermove = move |event: PointerEvent| {
        if let Some(Drag {
            from_y,
            measured:
                Some(Measured {
                    from_height,
                    tallest,
                }),
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

    let oninput = move |event: FormEvent| {
        let edited = event.value();
        typed.set(edited.clone());
        draft.set(edited.clone());
        command_index.set(0);
        hint_dismissed.set(false);
        view.act(Action::DraftEdited(edited));
    };
    // Images pasted go with the next message; text is pasted as ever.
    let onpaste = move |_event: ClipboardEvent| {
        #[cfg(feature = "web")]
        dom::paste_images(&_event, view);
    };

    rsx! {
        div {
            id: "composer",
            class: "composer {size_class} {state_class}",
            style: "--glow: {glow};",
            // A click anywhere but on its controls writes.
            onclick: move |_| dom::clicked(&draft.peek()),
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
                    // A touch keeps to where it began (the pointer's implicit capture): its
                    // moves and its end come here, not to the cover over the page.
                    onpointermove,
                    onpointerup: move |_| end_drag(),
                    onpointercancel: move |_| end_drag(),
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
                            if session.shell_commands {
                                "Shell command: Enter runs it beside the conversation. {session.agent} sees it, and what it printed, with your next message."
                            } else {
                                "Shell command: {session.agent} sessions don't run these. Ask {session.agent} to run it, or use a terminal."
                            }
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
                    // Not set from the draft: see `typed`.
                    textarea {
                        id: INPUT,
                        class: "composer-input",
                        rows: "1",
                        "aria-label": "Message to Claude",
                        "aria-placeholder": "Message Claude, or start with ! for a shell command",
                        "aria-autocomplete": "list",
                        "aria-controls": (commands > 0).then_some(COMMANDS),
                        "aria-activedescendant": active_command,
                        onfocus: move |_| {
                            spawn(async move {
                                keyboard.set(crate::web::platform::keyboard_attached().await);
                            });
                        },
                        oninput,
                        onkeydown,
                        onpaste,
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
                                dom::focus_end(&draft.peek());
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

/// The page's own parts the composer reaches, through its script: the same in a browser and in
/// the app's WebView, where what is read comes back a moment later (the heights). What must be
/// read at once, inside a keystroke, is the browser's alone (the caret, the images pasted); the
/// app leaves those to the WebView and to Android's clipboard. Rendered on the server, nothing
/// here runs: it all answers what the user does.
mod dom {
    use dioxus::prelude::document;

    use crate::web::dom::FOCUS_AT_END as FOCUS;

    /// What a click on doesn't write: the composer's controls and menus.
    const CONTROLS: &str =
        "button, a, textarea, input, [role='combobox'], [role='listbox'], [role='option']";
    /// What Tab reaches in the composer made taller.
    const FOCUSABLE: &str =
        "#composer .composer-body :is(button:not(:disabled), textarea, [tabindex='0'])";

    /// Runs `script` on the text box, as `element`, with `text` (the draft) as `text`.
    fn on_input(script: &str, text: &str) {
        let eval = document::eval(&format!(
            "const text = await dioxus.recv();
            const element = document.getElementById({:?});
            if (element) {{ {script} }}",
            super::INPUT
        ));
        if eval.send(text).is_err() {
            tracing::debug!("the text box wasn't reached");
        }
    }

    /// The draft in the text box, unless it is there, and the caret after it.
    const SET: &str = "if (element.value !== text) {
        element.value = text;
        element.setSelectionRange(text.length, text.length);
    }";

    /// Puts `text` in the text box, the caret after it, as the app set the draft.
    pub fn set_text(text: &str) {
        on_input(SET, text);
    }

    /// Focuses the text box, the caret at the end of `text`, the draft; set first if it isn't
    /// shown yet.
    pub fn focus_end(text: &str) {
        on_input(&format!("{SET} {FOCUS}"), text);
    }

    /// A click on the composer: on its ground, not one of its controls, it writes. Where it
    /// landed is the script's to see: the pointer's last press, which [`watch_presses`] keeps.
    pub fn clicked(draft: &str) {
        on_input(
            &format!("if (!window.edualcPressed?.closest({CONTROLS:?})) {{ {SET} {FOCUS} }}"),
            draft,
        );
    }

    /// Keeps where the pointer last pressed, for [`clicked`].
    pub fn watch_presses() {
        document::eval(
            "if (!window.edualcPressesWatched) {
                window.edualcPressesWatched = true;
                document.addEventListener('pointerdown', (event) => {
                    window.edualcPressed = event.target instanceof Element ? event.target : null;
                }, true);
            }",
        );
    }

    /// Ctrl+Enter, which the text box itself ignores: a line break in place of the selection,
    /// as typed (undone as typing is, and an edit like any other).
    pub fn break_line() {
        document::eval("document.execCommand('insertText', false, '\\n');");
    }

    /// The text's box's height and the conversation's column's, in CSS pixels.
    pub async fn heights() -> (Option<f64>, Option<f64>) {
        let asked = document::eval(
            "const scroll = document.getElementById('composer-scroll');
            const column = document.getElementById('composer')?.closest('.column');
            return [scroll?.offsetHeight ?? null, column?.clientHeight ?? null];",
        );
        asked
            .join::<(Option<f64>, Option<f64>)>()
            .await
            .unwrap_or((None, None))
    }

    /// Tab reached an end of the composer made taller: the focus goes round to the first of its
    /// controls (`first`) or the last.
    pub fn focus_edge(first: bool) {
        let pick = if first {
            "found[0]"
        } else {
            "found[found.length - 1]"
        };
        document::eval(&format!(
            "const found = document.querySelectorAll({FOCUSABLE:?}); {pick}?.focus();"
        ));
    }

    /// The caret is at the end of the draft, nothing selected; read in the keystroke, which the
    /// app's WebView answers too late for (→ takes a command there only from Tab and a tap).
    pub fn caret_at_end() -> bool {
        #[cfg(feature = "web")]
        {
            use web_sys::wasm_bindgen::JsCast as _;
            let input = web_sys::window()
                .and_then(|window| window.document())
                .and_then(|document| document.get_element_by_id(super::INPUT))
                .and_then(|element| element.dyn_into::<web_sys::HtmlTextAreaElement>().ok());
            input.is_some_and(|input| {
                let length =
                    u32::try_from(input.value().encode_utf16().count()).unwrap_or(u32::MAX);
                matches!(
                    (input.selection_start(), input.selection_end()),
                    (Ok(Some(start)), Ok(Some(end))) if start == end && start >= length
                )
            })
        }
        #[cfg(not(feature = "web"))]
        false
    }

    /// The images among what was pasted, each read and made ready to send in the page (the
    /// sandbox the decoder is on Linux), for the next message.
    #[cfg(feature = "web")]
    pub fn paste_images(event: &dioxus::prelude::ClipboardEvent, view: crate::web::View) {
        use dioxus::prelude::ReadableExt as _;
        use dioxus::web::WebEventExt as _;
        use edualc_core::app::Event;
        use edualc_core::decode;
        use edualc_core::ipc::{ActId, PasteFailure};
        use web_sys::wasm_bindgen::JsCast as _;
        let Ok(event) = event.as_web_event().dyn_into::<web_sys::ClipboardEvent>() else {
            return;
        };
        let Some(files) = event.clipboard_data().and_then(|data| data.files()) else {
            return;
        };
        for index in 0..files.length() {
            let Some(file) = files.item(index) else {
                continue;
            };
            if !file.type_().starts_with("image/") {
                continue;
            }
            let Some(events) = view.events.peek().clone() else {
                return;
            };
            wasm_bindgen_futures::spawn_local(async move {
                let read = wasm_bindgen_futures::JsFuture::from(file.array_buffer()).await;
                let pasted = match read {
                    Ok(buffer) => {
                        let bytes = js_sys::Uint8Array::new(&buffer).to_vec();
                        decode::to_send(&bytes).map_err(PasteFailure::Refused)
                    }
                    Err(_unread) => Err(PasteFailure::Clipboard(
                        "the browser didn't hand over the image".to_owned(),
                    )),
                };
                if events
                    .send(Event::Pasted {
                        request: ActId::default(),
                        pasted,
                    })
                    .await
                    .is_err()
                {
                    tracing::debug!("the app ended");
                }
            });
        }
    }
}
