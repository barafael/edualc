//! The sidebar, as the native window's (`ui/app.slint`'s `Sidebar`): edualc's heron and name
//! with the window's buttons; the folder worked in, and the others to turn to; the filter; the
//! sessions running now and this folder's earlier ones; at its foot the sandbox, the account and
//! the plan's limits used. Its right edge sets its width. Also the chips the header shows while
//! it is hidden (`SandboxChip`, `TasksChip`) and how a running session is doing (`LiveDot`).

use dioxus::html::input_data::MouseButton;
use dioxus::prelude::*;
use edualc_core::app::Action;
use edualc_core::external::wire::{SessionId, TaskId};
use edualc_core::present::Look;
use edualc_core::present::model::{
    LiveState, ProjectItem, SessionRow, SessionRowKind, SignInState, TaskItem, TaskKind,
};

use crate::web::View;
use crate::web::widgets::{Button, Icon, IconButton, Lucide, Meter, focus, percent};

const STYLE: Asset = asset!("/assets/sidebar.css");
/// The heron's drawings: standing, and the frames of what it does now and then.
const HERON: Asset = asset!("/assets/heron/heron.svg");
const HERON_BACK: Asset = asset!("/assets/heron/heron-looking-back.svg");
const HERON_PREEN_1: Asset = asset!("/assets/heron/heron-preening-1.svg");
const HERON_PREEN_2: Asset = asset!("/assets/heron/heron-preening-2.svg");
const HERON_HALF: Asset = asset!("/assets/heron/heron-flapping-half.svg");
const HERON_UP: Asset = asset!("/assets/heron/heron-flapping-up.svg");

/// The sessions' filter: the sidebar's home for the keyboard (Ctrl+K, F6).
pub(super) const FILTER: &str = "sidebar-filter";

/// The widest the sidebar is dragged to, in CSS pixels.
const WIDEST: f64 = 720.0;
/// How far an arrow key widens or narrows it.
const STEP: f64 = 32.0;

/// Opens the session with this id, as the lists hold it.
fn open_session(view: View, id: &str) {
    match SessionId::try_from(id.to_owned()) {
        Ok(id) => view.act(Action::OpenSession(id)),
        Err(error) => tracing::warn!(%error, "a session the lists shouldn't hold"),
    }
}

/// A width in whole CSS pixels, if it fits a `u16` (the kept width's type).
fn whole(width: f64) -> Option<u16> {
    format!("{:.0}", width.max(0.0)).parse().ok()
}

/// The sidebar's least width at this text size: its header's buttons and the heron, which grow
/// with the zoom, and no less than 200 pixels (the stylesheet's `min-width`).
fn least_width(text_size: f32) -> f64 {
    let button = f64::from(text_size * 2.2).max(32.0);
    (4.0 * button + 24.0 + 4.0 * 8.0 + 4.0 + 2.0 * 12.0).max(200.0)
}

/// The sidebar's width when none was chosen (the stylesheet's `--sidebar`).
fn default_width(text_size: f32) -> f64 {
    f64::from(text_size * 18.0).max(272.0)
}

/// A popup: over a transparent backdrop that closes it on a click, as Escape does. It takes
/// the focus as it opens, unless something in it does (`takes_focus: false`).
#[component]
fn Popup(
    class: String,
    label: String,
    onclose: EventHandler<()>,
    children: Element,
    #[props(default = true)] takes_focus: bool,
    #[props(default)] style: String,
) -> Element {
    rsx! {
        div { class: "sidebar-backdrop", onclick: move |_| onclose.call(()) }
        div {
            class: "popup sidebar-popup {class}",
            role: "dialog",
            "aria-label": "{label}",
            tabindex: "-1",
            style,
            onmounted: move |element: MountedEvent| {
                if takes_focus {
                    focus(Some(element));
                }
            },
            onkeydown: move |event: KeyboardEvent| {
                if event.key() == Key::Escape {
                    event.stop_propagation();
                    onclose.call(());
                }
            },
            {children}
        }
    }
}

/// How a session running elsewhere is doing, told by shape, not colour alone: a hand while it
/// waits for its user, a dot while it works, a ring while idle. Still, as its work is another
/// window's to show. Only drawn: what it is in says it.
#[component]
pub fn LiveDot(state: LiveState) -> Element {
    let mark = match state {
        LiveState::Waiting => {
            return rsx! {
                span { class: "live-dot waiting", "aria-hidden": "true",
                    Icon { icon: Lucide::Hand, class: "small" }
                }
            };
        }
        LiveState::Busy => "live-mark busy",
        LiveState::Idle | LiveState::Other => "live-mark",
    };
    rsx! {
        span { class: "live-dot", "aria-hidden": "true",
            span { class: mark }
        }
    }
}

/// What keeps Claude Code in, as a chip; a click opens its menu, which says what that means and
/// changes it. In the sidebar's foot it `explains` under its label why it matters when Claude
/// Code isn't sandboxed, and its menu `opens_upward`, from its left edge; in the header (while
/// the sidebar is hidden) neither, and the menu opens below it, from its right edge. `class`
/// goes on its wrapper (`.sandbox`), for where it sits.
#[component]
pub fn SandboxChip(
    #[props(default)] opens_upward: bool,
    #[props(default)] explains: bool,
    #[props(default)] class: String,
) -> Element {
    let view = use_context::<View>();
    let mut open = use_signal(|| false);
    let mut chip = use_signal(|| None::<MountedEvent>);
    let session = view.session.read();
    let unasked = view.preferences.read().messages_unasked;
    // Checked only once the kernel is known to hold it.
    let (icon, ink) = if session.sandbox_pending {
        (Lucide::Shield, "ink-tertiary")
    } else if session.sandboxed {
        (Lucide::ShieldCheck, "ink-info")
    } else {
        (Lucide::ShieldOff, "ink-warning")
    };
    let state = if session.sandboxed { "" } else { "open" };
    let explaining = if explains { "explains" } else { "" };
    let direction = if opens_upward { "up" } else { "down" };
    let mut close = move |()| {
        open.set(false);
        focus(chip.peek().clone());
    };
    let mut act = move |action: Action| {
        view.act(action);
        close(());
    };
    rsx! {
        document::Stylesheet { href: STYLE }
        div { class: "sandbox {class}",
            button {
                class: "sidebar-press sandbox-chip {state} {explaining}",
                r#type: "button",
                "aria-label": "{session.sandbox_label}",
                "aria-haspopup": "dialog",
                "aria-expanded": "{open}",
                onmounted: move |element| chip.set(Some(element)),
                onclick: move |_| open.set(true),
                span { class: "{ink}",
                    Icon { icon: icon, class: "small" }
                }
                span { class: "sandbox-texts",
                    span { class: "sandbox-label", "{session.sandbox_label}" }
                    if explains && !session.sandboxed {
                        span { class: "caption", "{session.sandbox_detail}" }
                    }
                }
            }
            if open() {
                Popup {
                    class: "sandbox-menu {direction}",
                    label: session.sandbox_label.clone(),
                    onclose: close,
                    div { class: "menu-heading", "{session.sandbox_label}" }
                    div { class: "small", "{session.sandbox_detail}" }
                    Button {
                        danger: session.sandbox_on,
                        onclick: move |_| act(Action::ToggleSandbox),
                        if session.sandbox_on {
                            "Turn the sandbox off"
                        } else {
                            "Sandbox Claude Code"
                        }
                    }
                    if session.sandbox_on {
                        Button { onclick: move |_| act(Action::ToggleSandboxDesktop),
                            if session.sandbox_desktop {
                                "Close the display to Claude"
                            } else {
                                "Let Claude use the display (UI tests)"
                            }
                        }
                        Button { onclick: move |_| act(Action::ToggleSandboxHardware),
                            if session.sandbox_hardware {
                                "Close USB and serial ports to Claude here"
                            } else {
                                "Let Claude use USB and serial ports here"
                            }
                        }
                    }
                    if session.sandbox_on && !session.sandbox_hardware {
                        div { class: "caption",
                            "For this folder: debug probes and boards, but also any other USB device you can open, such as a phone."
                        }
                    }
                    // Said while it holds, as it changes what reaches other sessions unseen.
                    if session.sandbox_on && unasked {
                        div { class: "caption warning",
                            "Its messages to other sessions go without asking you (see the settings, Ctrl+,)."
                        }
                    }
                    div { class: "caption",
                        "Changing it starts Claude Code again; the conversation goes on."
                    }
                }
            }
        }
    }
}

/// What runs besides the turn's own steps, and how much ("2 agents, 1 command"), as a chip; a
/// click lists it, above the chip, as does a bump of the page's `show_tasks` (the sidebar's
/// count of agents, a stopped turn that left tasks running). A subagent's row opens what it
/// does beside the conversation; each can be stopped. `class` goes on its wrapper (`.tasks`).
#[component]
pub fn TasksChip(#[props(default)] class: String) -> Element {
    let view = use_context::<View>();
    let mut open = use_signal(|| false);
    let mut chip = use_signal(|| None::<MountedEvent>);
    // How far above the chip the list may reach: up to the window's top margin.
    let mut room = use_signal(|| None::<f64>);
    let wanted = use_memo(move || view.page.read().show_tasks);
    let mut seen = use_signal(|| *wanted.peek());
    let mut show = move || {
        open.set(true);
        if let Some(chip) = chip.peek().clone() {
            spawn(async move {
                if let Ok(rect) = chip.get_client_rect().await {
                    room.set(Some(rect.min_y()));
                }
            });
        }
    };
    use_effect(move || {
        let serial = wanted();
        if serial != *seen.peek() {
            seen.set(serial);
            show();
        }
    });
    let close = move |()| {
        open.set(false);
        focus(chip.peek().clone());
    };
    let session = view.session.read();
    let style = room()
        .map(|top| format!("max-height: calc({top}px - var(--space-m));"))
        .unwrap_or_default();
    rsx! {
        document::Stylesheet { href: STYLE }
        div { class: "tasks {class}",
            button {
                class: "sidebar-press tasks-chip",
                r#type: "button",
                "aria-label": "Running now: {session.tasks_summary}",
                "aria-haspopup": "dialog",
                "aria-expanded": "{open}",
                onmounted: move |element| chip.set(Some(element)),
                onclick: move |_| show(),
                span { class: "ink-claude",
                    Icon { icon: Lucide::Layers, class: "small" }
                }
                span { class: "tasks-summary trimmed", "{session.tasks_summary}" }
            }
            if open() {
                Popup {
                    class: "tasks-menu",
                    label: if session.tasks_left_running { "Still running" } else { "Running now" },
                    onclose: close,
                    style,
                    div { class: "tasks-head",
                        div { class: "menu-heading",
                            if session.tasks_left_running {
                                "Still running"
                            } else {
                                "Running now"
                            }
                        }
                        if session.tasks.len() > 1 {
                            Button { onclick: move |_| view.act(Action::StopAllTasks), "Stop all" }
                        }
                    }
                    // Stopping a turn leaves what runs beside it running, as the user wanted:
                    // they decide here.
                    if session.tasks_left_running {
                        div { class: "tasks-note",
                            "The turn stopped; these go on. Stop them here, or leave them running."
                        }
                    }
                    for task in session.tasks.iter() {
                        TaskRow {
                            key: "{task.id}",
                            task: task.clone(),
                            onfollow: move |()| {
                                open.set(false);
                            },
                        }
                    }
                }
            }
        }
    }
}

/// One of what runs, in the tasks' list: what it is, how far it got, a button to stop it. A
/// subagent's row (`peekable`) opens what it does beside the conversation, and says so.
#[component]
fn TaskRow(task: TaskItem, onfollow: EventHandler<()>) -> Element {
    let view = use_context::<View>();
    let icon = match task.kind {
        TaskKind::Agent => Lucide::Bot,
        TaskKind::Command => Lucide::SquareTerminal,
        TaskKind::Workflow => Lucide::Workflow,
        TaskKind::Monitor => Lucide::Radar,
        TaskKind::Other => Lucide::CircleDashed,
    };
    let label = if task.peekable {
        format!("Follow {}", task.title)
    } else {
        task.title.clone()
    };
    let follow = task.id.clone();
    let stop = task.id.clone();
    rsx! {
        // The row's click is its button's: Enter on the button clicks it too.
        div {
            class: if task.peekable { "sidebar-press task-row peekable" } else { "sidebar-press task-row" },
            onclick: move |_| {
                if task.peekable {
                    onfollow.call(());
                    view.act(Action::OpenPeek(TaskId::from(follow.clone())));
                }
            },
            button {
                class: "task-follow",
                r#type: "button",
                "aria-label": "{label}",
                disabled: !task.peekable,
                span { class: "ink-secondary",
                    Icon { icon: icon, class: "small" }
                }
                span { class: "task-texts",
                    span { class: "task-title", "{task.title}" }
                    span { class: "caption", "{task.detail}" }
                    if !task.used.is_empty() {
                        span { class: "caption", "{task.used}" }
                    }
                    if !task.now.is_empty() {
                        span { class: "caption", "{task.now}" }
                    }
                }
            }
            // Not a bare square, which reads as a checkbox in a list.
            span {
                onclick: move |event| event.stop_propagation(),
                IconButton {
                    icon: Lucide::CircleStop,
                    label: format!("Stop {}", task.title),
                    onclick: move |_| view.act(Action::StopTask(TaskId::from(stop.clone()))),
                }
            }
            // The chevron's room either way, so that every row's stop button lines up.
            span { class: "task-end ink-tertiary",
                if task.peekable {
                    Icon { icon: Lucide::ChevronRight, class: "small" }
                }
            }
        }
    }
}

/// A heading over a part of the session list: the sessions running now, or this folder's
/// earlier ones, with how many need their user.
#[component]
fn SessionHeading(entry: SessionRow) -> Element {
    let label = if entry.detail.is_empty() {
        entry.title.clone()
    } else {
        format!("{}, {}", entry.title, entry.detail)
    };
    rsx! {
        h2 { class: "session-heading", "aria-label": "{label}",
            span { class: "heading-title trimmed", title: "{entry.title}", "{entry.title}" }
            if !entry.detail.is_empty() {
                span { class: "heading-detail", "{entry.detail}" }
            }
        }
    }
}

/// How many subagents a running session has: a bot and the number. Only drawn.
#[component]
fn AgentCount(agents: i32) -> Element {
    rsx! {
        span { class: "agent-count", "aria-hidden": "true",
            Icon { icon: Lucide::Bot, class: "small" }
            "{agents}"
        }
    }
}

/// A session running now (this window's, another's, a terminal's), on two lines: its title,
/// then what it does, its name to message it by and where it runs; its subagents counted. On
/// the session shown here, the count opens the list of what runs, where an agent's work can be
/// followed.
#[component]
fn RunningRow(entry: SessionRow) -> Element {
    let view = use_context::<View>();
    let agents = match entry.agents {
        1 => ", 1 agent running".to_owned(),
        count if count > 1 => format!(", {count} agents running"),
        _ => String::new(),
    };
    let label = format!("{}, {}{agents}", entry.title, entry.detail);
    let shows = if entry.agents == 1 {
        "1 agent running, show it".to_owned()
    } else {
        format!("{} agents running, show them", entry.agents)
    };
    let selected = if entry.selected { "selected" } else { "" };
    let waiting = if entry.state == LiveState::Waiting {
        "waiting"
    } else {
        ""
    };
    let id = entry.id.clone();
    rsx! {
        // The row's click is its button's: Enter on the button clicks it too.
        div {
            class: "sidebar-press session-row running {selected}",
            onclick: move |_| open_session(view, &id),
            button {
                class: "session-open",
                r#type: "button",
                "aria-label": "{label}",
                "aria-current": entry.selected.then_some("true"),
                LiveDot { state: entry.state }
                span { class: "session-texts",
                    // The whole title, where the row cuts it off.
                    span { class: "session-title trimmed", title: "{entry.title}", "{entry.title}" }
                    span { class: "session-detail trimmed {waiting}", "{entry.detail}" }
                }
                if entry.agents > 0 && !entry.selected {
                    AgentCount { agents: entry.agents }
                }
            }
            if entry.agents > 0 && entry.selected {
                button {
                    class: "agent-count-button",
                    r#type: "button",
                    "aria-label": "{shows}",
                    onclick: move |event| {
                        event.stop_propagation();
                        let mut page = view.page;
                        page.write().show_tasks += 1;
                    },
                    AgentCount { agents: entry.agents }
                }
            }
        }
    }
}

/// One of this folder's earlier sessions: continued when opened.
#[component]
fn EarlierRow(entry: SessionRow) -> Element {
    let view = use_context::<View>();
    let worktree = if entry.worktree.is_empty() {
        String::new()
    } else {
        format!(", in the worktree {}", entry.worktree)
    };
    let selected = if entry.selected { "selected" } else { "" };
    let id = entry.id.clone();
    rsx! {
        button {
            class: "sidebar-press session-row earlier {selected}",
            r#type: "button",
            "aria-label": "{entry.title}, {entry.age}{worktree}",
            "aria-current": entry.selected.then_some("true"),
            onclick: move |_| open_session(view, &id),
            if !entry.worktree.is_empty() {
                span { class: "ink-tertiary",
                    Icon { icon: Lucide::GitBranch, class: "small" }
                }
            }
            // The whole title, where the row cuts it off.
            span { class: "session-title trimmed", title: "{entry.title}", "{entry.title}" }
            span { class: "session-age", "{entry.age}" }
        }
    }
}

/// One of the recent folders: its name, where it is, when it was last used. A folder may be
/// gone since: opened as a typed one is, so that the list says why it can't.
#[component]
fn FolderRow(project: ProjectItem) -> Element {
    let view = use_context::<View>();
    let path = project.path.clone();
    rsx! {
        button {
            class: "sidebar-press folder-row",
            r#type: "button",
            "aria-label": "{project.name}",
            "aria-description": "{project.path}, {project.age}",
            title: "{project.path}",
            onclick: move |_| view.act(Action::OpenProject(path.clone())),
            span { class: "folder-texts",
                span { class: "trimmed", "{project.name}" }
                span { class: "caption trimmed", "{project.shown}" }
            }
            span { class: "caption", "{project.age}" }
        }
    }
}

/// The folders to work in instead: a path typed (the keys go there as it opens: a path typed
/// at once is taken, and Tab leads on to the recent folders), one picked in the desktop's
/// dialog, or one of the recent ones. Why the folder last asked for didn't open is said here,
/// the path still in its field to correct; made anew, so empty, each time it opens.
#[component]
fn Folders(onclose: EventHandler<()>) -> Element {
    let view = use_context::<View>();
    let mut path = use_signal(String::new);
    let sessions = view.sessions.read();
    rsx! {
        Popup {
            class: "folders",
            label: "Folders to work in",
            takes_focus: false,
            onclose,
            div { class: "folders-path",
                input {
                    class: "field",
                    r#type: "text",
                    placeholder: "Path to a folder",
                    "aria-label": "Path to a folder",
                    value: "{path}",
                    onmounted: move |element| focus(Some(element)),
                    oninput: move |event| path.set(event.value()),
                    onkeydown: move |event| {
                        if event.key() == Key::Enter {
                            view.act(Action::OpenProject(path()));
                        }
                    },
                }
            }
            if !sessions.problem.is_empty() {
                div { class: "folders-problem", role: "alert", "{sessions.problem}" }
            }
            div { class: "caption", "Recent" }
            div { class: "folders-list",
                for project in sessions.projects.iter() {
                    FolderRow { key: "{project.path}", project: project.clone() }
                }
            }
        }
    }
}

/// A limit's use: its label, its bar, and the share.
#[component]
fn UsageBar(#[props(into)] label: String, used: f32) -> Element {
    let share = percent(used);
    rsx! {
        div { class: "usage-bar",
            span { class: "caption usage-label", "{label}" }
            Meter { fraction: used, label }
            span { class: "caption usage-share", "{share}%" }
        }
    }
}

/// The sidebar's right edge being dragged: where the pointer started, the width then, and the
/// width now.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Drag {
    from_x: f64,
    from_width: f64,
    width: Option<f64>,
}

/// The sidebar. Narrow windows give up its width first, then the side panel's; the
/// conversation keeps the rest. Its right edge sets its width: dragged, or with the arrow keys;
/// a double click gives the default back. `onhide` hides it (the header then shows a button to
/// show it again).
#[component]
pub fn Sidebar(onhide: EventHandler<()>) -> Element {
    let view = use_context::<View>();
    let settings = use_context::<super::panels::SettingsOpen>();
    let mut projects_open = use_signal(|| false);
    let mut project_button = use_signal(|| None::<MountedEvent>);
    let mut account_open = use_signal(|| false);
    let mut info_button = use_signal(|| None::<MountedEvent>);
    // The width it is drawn at, and the drag of its edge, if one goes on.
    let mut drawn_width = use_signal(|| None::<f64>);
    let mut drag = use_signal(|| None::<Drag>);
    // The folder typed was opened: the list closes.
    // Ctrl+K: the filter takes the focus, once the sidebar is drawn (it may just be shown).
    let filter_wanted = use_memo(move || view.page.read().filter_wanted);
    use_effect(move || {
        if filter_wanted() {
            let mut page = view.page;
            page.write().filter_wanted = false;
            crate::web::dom::focus(FILTER);
        }
    });
    let projects_opened = use_memo(move || view.page.read().projects_opened);
    use_effect(move || {
        projects_opened();
        projects_open.set(false);
    });
    // Asked for elsewhere (the trust bar): the list opens.
    let projects_asked = use_memo(move || view.page.read().projects_asked);
    use_effect(move || {
        if projects_asked() > 0 {
            view.act(Action::ListProjects);
            projects_open.set(true);
        }
    });

    let page = view.page.read();
    let text_size = page.text_size;
    let least = least_width(text_size);
    let chosen = drag()
        .and_then(|drag| drag.width)
        .or(page.sidebar_width.map(f64::from));
    let width = drawn_width()
        .or(chosen)
        .unwrap_or_else(|| default_width(text_size));
    let look = page.look;
    drop(page);
    let style = chosen
        .map(|chosen| format!("width: {chosen}px;"))
        .unwrap_or_default();
    // A width kept between runs: zero (none) gives the default back.
    let keep = move |width: Option<u16>| {
        let mut page = view.page;
        page.write().sidebar_width = width;
        view.act(Action::SidebarWidth(width));
    };
    // The drag ends: the width dragged to is kept; without a move, the one kept before.
    let mut finish = move || {
        if let Some(dragged) = drag() {
            drag.set(None);
            let kept = dragged
                .width
                .and_then(whole)
                .or(view.page.peek().sidebar_width);
            keep(kept);
        }
    };
    let mut close_projects = move |()| {
        projects_open.set(false);
        focus(project_button.peek().clone());
    };
    let close_account = move |()| {
        account_open.set(false);
        focus(info_button.peek().clone());
    };
    let session = view.session.read();
    let sessions = view.sessions.read();
    let account = [
        ("Email", &session.email),
        ("Organization", &session.organization),
        ("Provider", &session.provider),
        ("Claude Code", &session.version),
        ("This session", &session.cost),
    ];
    let signed_in = match session.sign_in {
        SignInState::SignedIn => "Signed in",
        SignInState::SignedOut => "Not signed in",
        SignInState::Unknown => "Account",
    };
    let dragging = if drag().is_some() { "dragging" } else { "" };
    let heron = format!(
        "--heron: url('{HERON}'); --heron-back: url('{HERON_BACK}'); --heron-preen-1: url('{HERON_PREEN_1}'); --heron-preen-2: url('{HERON_PREEN_2}'); --heron-half: url('{HERON_HALF}'); --heron-up: url('{HERON_UP}');"
    );
    rsx! {
        document::Stylesheet { href: STYLE }
        aside {
            class: "sidebar",
            "aria-label": "Sessions",
            style,
            onresize: move |event| {
                if let Ok(size) = event.get_border_box_size() {
                    drawn_width.set(Some(size.width));
                }
            },
            div { class: "sidebar-body",
                div { class: "sidebar-head",
                    // edualc's heron, at the size of the grid it is drawn on.
                    span { class: "heron-mark", "aria-hidden": "true", style: heron }
                    // Gives way first: the buttons beside it keep their size.
                    div { class: "sidebar-name trimmed", "edualc" }
                    IconButton {
                        icon: Lucide::Palette,
                        label: if look == Look::Gruvbox { "Plain colours" } else { "Gruvbox colours" },
                        onclick: move |_| view.act(Action::SwitchLook),
                    }
                    IconButton {
                        icon: Lucide::Settings,
                        label: "Settings (Ctrl+,)",
                        onclick: move |_| settings.open(view),
                    }
                    IconButton {
                        icon: Lucide::SquarePen,
                        label: "New session",
                        onclick: move |_| view.act(Action::NewSession),
                    }
                    // Where the header's button to show it again then appears.
                    IconButton {
                        icon: Lucide::PanelLeftClose,
                        label: "Hide the sessions (Ctrl+B)",
                        onclick: move |_| onhide.call(()),
                    }
                }
                // The working directory; a click offers the others Claude Code was used in.
                div { class: "project",
                    button {
                        class: "sidebar-press project-button",
                        r#type: "button",
                        "aria-label": "Folder {session.project}",
                        "aria-description": "{session.cwd}. Opens the folders to work in instead.",
                        "aria-haspopup": "dialog",
                        "aria-expanded": "{projects_open}",
                        onmounted: move |element| project_button.set(Some(element)),
                        onclick: move |_| {
                            view.act(Action::ListProjects);
                            projects_open.set(true);
                        },
                        span { class: "ink-secondary",
                            Icon { icon: Lucide::Folder }
                        }
                        span { class: "project-texts",
                            span { class: "project-name trimmed", title: "{session.project}", "{session.project}" }
                            span { class: "caption trimmed", title: "{session.cwd}", "{session.cwd}" }
                        }
                        span { class: "ink-secondary",
                            Icon { icon: Lucide::ChevronsUpDown, class: "small" }
                        }
                    }
                    if projects_open() {
                        Folders { onclose: move |()| close_projects(()) }
                    }
                }
                // One list: the sessions running now, in any folder, then this folder's earlier
                // ones. The filter is the sidebar's home for the keyboard (Ctrl+K, F6).
                input {
                    class: "field sidebar-filter",
                    id: FILTER,
                    r#type: "text",
                    placeholder: "Filter sessions",
                    "aria-label": "Filter sessions",
                    "aria-keyshortcuts": "Control+K",
                    title: "Filter sessions (Ctrl+K)",
                    oninput: move |event| view.act(Action::FilterSessions(event.value())),
                }
                if !sessions.empty.is_empty() {
                    div { class: "sessions-empty", "{sessions.empty}" }
                }
                div { class: "session-list",
                    for row in sessions.rows.iter() {
                        match row.kind {
                            SessionRowKind::Heading => rsx! {
                                SessionHeading { key: "heading {row.title}", entry: row.clone() }
                            },
                            SessionRowKind::Running => rsx! {
                                RunningRow { key: "running {row.id}", entry: row.clone() }
                            },
                            SessionRowKind::Earlier => rsx! {
                                EarlierRow { key: "earlier {row.id}", entry: row.clone() }
                            },
                        }
                    }
                }
                // The plan and how much of it is used (a click shows the details); who is
                // signed in waits behind the info button.
                div { class: "sidebar-foot",
                    div { class: "sidebar-rule" }
                    // Beside what the session may spend: what it may do.
                    SandboxChip { opens_upward: true, explains: true }
                    div { class: "account-row",
                        // The plan is in the usage details.
                        span { class: "product trimmed", "Claude Code" }
                        // As an `IconButton`, held here to take the focus back.
                        button {
                            class: "icon-button",
                            r#type: "button",
                            title: "Account",
                            "aria-label": "Account",
                            "aria-haspopup": "dialog",
                            "aria-expanded": "{account_open}",
                            onmounted: move |element| info_button.set(Some(element)),
                            onclick: move |_| account_open.set(true),
                            Icon { icon: Lucide::Info }
                        }
                        // Above the row its button is on, from the sidebar's left margin.
                        if account_open() {
                            Popup {
                                class: "account",
                                label: "Account",
                                onclose: close_account,
                                div { class: "caption", "{signed_in}" }
                                for (label , value) in account.into_iter().filter(|(_, value)| !value.is_empty()) {
                                    div { key: "{label}", class: "account-line",
                                        span { class: "label", "{label}" }
                                        span { class: "value trimmed", title: "{value}", "{value}" }
                                    }
                                }
                            }
                        }
                    }
                    button {
                        class: "sidebar-press usage-button",
                        r#type: "button",
                        "aria-label": "Usage details",
                        onclick: move |_| view.act(Action::ShowUsage),
                        if session.limit_five_hour >= 0.0 {
                            UsageBar { label: "5h", used: session.limit_five_hour }
                        }
                        if !session.limit_five_hour_resets.is_empty() {
                            div { class: "caption usage-resets", "{session.limit_five_hour_resets}" }
                        }
                        if session.limit_seven_day >= 0.0 {
                            UsageBar { label: "7d", used: session.limit_seven_day }
                        }
                        if session.limit_five_hour < 0.0 && session.limit_seven_day < 0.0 {
                            div { class: "caption", "Usage" }
                        }
                    }
                }
            }
            // The right edge: dragged, it sets the width, kept between runs; a double click
            // gives the default back. Reached with Tab, the arrow keys widen and narrow it.
            div {
                class: "sidebar-edge {dragging}",
                role: "slider",
                tabindex: "0",
                "aria-label": "Width of the session list",
                "aria-orientation": "horizontal",
                "aria-valuenow": "{width:.0}",
                "aria-valuemin": "{least:.0}",
                "aria-valuemax": "{WIDEST:.0}",
                // A click doesn't take the focus, so writing goes on where it was.
                onmousedown: move |event| event.prevent_default(),
                onpointerdown: move |event| {
                    if event.trigger_button() == Some(MouseButton::Primary) {
                        drag.set(
                            Some(Drag {
                                from_x: event.client_coordinates().x,
                                from_width: width,
                                width: None,
                            }),
                        );
                    }
                },
                onpointermove: move |event| {
                    let Some(dragged) = drag() else {
                        return;
                    };
                    // Let go of outside the page: the drag ends as the pointer comes back.
                    if !event.held_buttons().contains(MouseButton::Primary) {
                        finish();
                        return;
                    }
                    let moved = dragged.from_width + event.client_coordinates().x - dragged.from_x;
                    drag.set(
                        Some(Drag {
                            width: Some(moved.min(WIDEST).max(least)),
                            ..dragged
                        }),
                    );
                },
                onpointerup: move |event| {
                    if event.trigger_button() != Some(MouseButton::Primary) {
                        return;
                    }
                    finish();
                },
                ondoubleclick: move |_| keep(None),
                onkeydown: move |event| {
                    let step = if event.key() == Key::ArrowRight {
                        STEP
                    } else if event.key() == Key::ArrowLeft {
                        -STEP
                    } else {
                        return;
                    };
                    event.prevent_default();
                    keep(whole((width + step).min(WIDEST).max(least)));
                },
                div { class: "edge-line" }
                // While dragged, over the whole window, so that the pointer stays with it.
                if drag().is_some() {
                    div { class: "edge-cover" }
                }
            }
        }
    }
}
