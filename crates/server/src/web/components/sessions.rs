//! The sessions as rows, for a project's page: those running now, in any folder, and the
//! folder's earlier ones (`ui/project.slint`); a recent folder's row; and what the composer
//! shows of a session: the chip of what runs beside the turn (`TasksChip`) and how a running
//! session is doing (`LiveDot`).

use dioxus::prelude::*;
use edualc_core::app::Action;
use edualc_core::external::wire::{SessionId, TaskId};
use edualc_core::present::model::{
    LiveState, ProjectItem, SessionRow, SessionRowKind, TaskItem, TaskKind,
};

use crate::web::View;
use crate::web::view::use_bumped;
use crate::web::widgets::{Button, Icon, IconButton, Lucide, focus};

/// Opens the session with this id, as the lists hold it.
fn open_session(view: View, id: &str) {
    match SessionId::try_from(id.to_owned()) {
        Ok(id) => view.act(Action::OpenSession(id)),
        Err(error) => tracing::warn!(%error, "a session the lists shouldn't hold"),
    }
}

/// A popup: over a transparent backdrop that closes it on a click, as Escape does. It takes
/// the focus as it opens.
#[component]
fn Popup(
    class: String,
    label: String,
    onclose: EventHandler<()>,
    children: Element,
    #[props(default)] style: String,
) -> Element {
    rsx! {
        div { class: "menu-backdrop", onclick: move |_| onclose.call(()) }
        div {
            class: "popup popup-anchored {class}",
            role: "dialog",
            "aria-label": "{label}",
            tabindex: "-1",
            style,
            onmounted: move |element: MountedEvent| focus(Some(element)),
            onkeydown: move |event: KeyboardEvent| {
                if event.key() == Key::Escape {
                    // Taken here, so that the window's own Esc doesn't stop the turn as well.
                    event.prevent_default();
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

/// What runs besides the turn's own steps, and how much ("2 agents, 1 command"), as a chip; a
/// click lists it, above the chip, as does a bump of the page's `show_tasks` (a session's
/// count of agents, a stopped turn that left tasks running). A subagent's row opens what it
/// does beside the conversation; each can be stopped.
#[component]
pub fn TasksChip() -> Element {
    let view = use_context::<View>();
    let mut open = use_signal(|| false);
    let mut chip = use_signal(|| None::<MountedEvent>);
    // How far above the chip the list may reach: up to the window's top margin.
    let mut room = use_signal(|| None::<f64>);
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
    use_bumped(|page| page.show_tasks, show);
    let close = move |()| {
        open.set(false);
        focus(chip.peek().clone());
    };
    let session = view.session.read();
    let style = room()
        .map(|top| format!("max-height: calc({top}px - var(--space-m));"))
        .unwrap_or_default();
    rsx! {
        div { class: "tasks",
            button {
                class: "press tasks-chip",
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
            class: if task.peekable { "press task-row peekable" } else { "press task-row" },
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
            class: "press session-row running {selected}",
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
            class: "press session-row earlier {selected}",
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

/// The sessions, one list: those running now, in any folder, then the folder's earlier ones.
#[component]
pub(super) fn SessionRows() -> Element {
    let view = use_context::<View>();
    let sessions = view.sessions.read();
    rsx! {
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
    }
}

/// One of the recent folders: its name, where it is, when it was last used. A folder may be
/// gone since: opened as a typed one is, so that the list says why it can't.
#[component]
pub(super) fn FolderRow(project: ProjectItem) -> Element {
    let view = use_context::<View>();
    let path = project.path.clone();
    rsx! {
        button {
            class: "press folder-row",
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
