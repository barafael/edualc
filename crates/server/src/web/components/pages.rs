//! Where the window is before a session, as the native window's pages (`ui/start.slint`,
//! `ui/project.slint`, `ui/newproject.slint`): the start page of projects and of sessions that
//! wait for you, a project's page of its sessions, the dialogs to open a folder and to make a
//! new project (over a session too: the folder its trust bar opens), and the switcher that goes
//! from a session to another (`ui/app.slint`'s `SessionSwitcher`). A folder is typed, not
//! picked: the page isn't on the daemon's desktop.

use dioxus::prelude::*;
use edualc_core::app::Action;
use edualc_core::present::model::{LiveState, ProjectCard, SwitcherItem, WaitingItem};

use super::sessions::{FolderRow, LiveDot, SessionRows};
use crate::web::View;
use crate::web::view::use_bumped;
use crate::web::widgets::{Button, Icon, IconButton, Lucide, take_focus};

/// A page's filter: its home for the keyboard (Ctrl+K, F6).
pub(super) const FILTER: &str = "page-filter";

/// The switcher's list; each of its rows is named after it, with its place in the list.
const SWITCHER_LIST: &str = "switcher-list";

/// Whether the switcher is shown, over a session: the window's, which the bar's button and
/// Ctrl+K both set.
#[derive(Clone, Copy)]
pub struct SwitcherOpen(pub Signal<bool>);

impl SwitcherOpen {
    /// Shows the switcher, or puts it away.
    pub fn toggle(self, view: View) {
        if *self.0.peek() {
            self.close(view);
        } else {
            let mut shown = self.0;
            shown.set(true);
        }
    }

    /// Puts the switcher away, the focus that was in it going back to the writing.
    pub fn close(self, view: View) {
        let mut shown = self.0;
        shown.set(false);
        super::panels::back_home(view);
    }
}

/// A dialog over the window.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dialog {
    NewProject,
    OpenFolder,
}

/// The dialog shown, if one is: the window's, as the native window's is. The start page's
/// buttons, a session's trust bar and the app each open one, and Esc puts it away from
/// wherever the focus is.
#[derive(Clone, Copy)]
pub struct DialogOpen(pub Signal<Option<Dialog>>);

impl DialogOpen {
    /// Opens the dialog of folders, the recent ones asked for anew.
    pub fn open_folder(self, view: View) {
        view.act(Action::ListProjects);
        let mut shown = self.0;
        shown.set(Some(Dialog::OpenFolder));
    }
}

/// Whether `needle` (lower case, trimmed) is in any of `texts`; an empty one is in all.
fn matches(needle: &str, texts: &[&str]) -> bool {
    needle.is_empty()
        || texts
            .iter()
            .any(|text| text.to_lowercase().contains(needle))
}

/// The cards the filter leaves: by a project's name, where it is, or what was said last in it.
fn shown_cards<'a>(cards: &'a [ProjectCard], filter: &str) -> Vec<&'a ProjectCard> {
    let needle = filter.trim().to_lowercase();
    cards
        .iter()
        .filter(|card| matches(&needle, &[&card.name, &card.shown, &card.last_said]))
        .collect()
}

/// The sessions the switcher's query leaves, by where each is in `items`: by title, project,
/// or how they are doing.
fn shown_sessions(items: &[SwitcherItem], query: &str) -> Vec<usize> {
    let needle = query.trim().to_lowercase();
    items
        .iter()
        .enumerate()
        .filter(|(_, item)| matches(&needle, &[&item.title, &item.project_name, &item.detail]))
        .map(|(at, _)| at)
        .collect()
}

/// What a page's filter narrows.
#[derive(Clone, Copy, PartialEq)]
enum Filtered {
    Projects,
    Sessions,
}

impl Filtered {
    fn as_str(self) -> &'static str {
        match self {
            Self::Projects => "projects",
            Self::Sessions => "sessions",
        }
    }
}

/// A page's filter, its home for the keyboard: of `filtered`. With a `value`, it shows that;
/// without, what was typed.
#[component]
fn PageFilter(
    filtered: Filtered,
    oninput: EventHandler<String>,
    #[props(default)] value: Option<String>,
    #[props(default)] wide: bool,
) -> Element {
    let noun = filtered.as_str();
    rsx! {
        input {
            class: if wide { "field page-filter wide" } else { "field page-filter" },
            id: FILTER,
            r#type: "text",
            placeholder: "Filter {noun} (Ctrl+K)",
            "aria-label": "Filter {noun}",
            "aria-keyshortcuts": "Control+K",
            value,
            oninput: move |event: FormEvent| oninput.call(event.value()),
        }
    }
}

/// The page's filter takes the focus as the page is shown, and when the app asks (Ctrl+K, F6):
/// not from a dialog over the page, which took it as it opened.
fn use_page_focus(view: View) {
    let dialog = use_context::<DialogOpen>().0;
    let asked = use_memo(move || view.page.read().focus_page);
    use_effect(move || {
        asked();
        if dialog.peek().is_none() {
            crate::web::dom::grab_focus(FILTER);
        }
    });
}

/// The start page: the sessions that wait for you, in any project, and the projects.
#[component]
pub fn StartPage() -> Element {
    let view = use_context::<View>();
    let dialogs = use_context::<DialogOpen>();
    let mut dialog = dialogs.0;
    let mut filter = use_signal(String::new);
    use_page_focus(view);
    // Before the line to edualcd opens (and if it never does) nothing is known of the projects:
    // the page doesn't say there are none.
    let reached = view.events.read().is_some();
    let nav = view.nav.read();
    let typed = filter.read();
    let cards = shown_cards(&nav.cards, &typed);
    let filtered = !typed.trim().is_empty();
    rsx! {
        main { class: "page", "aria-label": "Projects",
            div { class: "page-column",
                div { class: "page-head",
                    h2 { class: "page-title", "Projects" }
                    PageFilter {
                        filtered: Filtered::Projects,
                        value: typed.clone(),
                        oninput: move |typed| filter.set(typed),
                    }
                    Button {
                        icon: Lucide::FolderPlus,
                        onclick: move |_| dialog.set(Some(Dialog::NewProject)),
                        "New project…"
                    }
                    Button {
                        icon: Lucide::Folder,
                        onclick: move |_| dialogs.open_folder(view),
                        "Open a folder…"
                    }
                }
                if !nav.waiting.is_empty() && !filtered {
                    section { class: "needs-you", "aria-labelledby": "needs-you-title",
                        h2 { id: "needs-you-title", class: "page-section", "Needs you" }
                        // By its project too: a session the daemon runs may be listed in two.
                        for item in nav.waiting.iter() {
                            Waiting {
                                key: "{item.project_folder} {item.session_id}",
                                item: item.clone(),
                            }
                        }
                    }
                }
                if !cards.is_empty() {
                    div { class: "cards",
                        for card in cards {
                            Card { key: "{card.path}", card: card.clone() }
                        }
                    }
                } else if filtered {
                    p { class: "page-empty", "No project matches the filter." }
                } else if reached {
                    p { class: "page-empty",
                        "No recent projects yet. Create a new project or open a folder to begin."
                    }
                }
            }
        }
    }
}

/// The dialog over the window, while one is open. Put away, the focus that was in it goes back
/// to where the window is.
#[component]
pub fn Dialogs() -> Element {
    let view = use_context::<View>();
    let mut dialog = use_context::<DialogOpen>().0;
    let close = move |()| {
        dialog.set(None);
        super::panels::back_home(view);
    };
    match dialog() {
        None => rsx! {},
        Some(Dialog::NewProject) => rsx! {
            NewProject { onclose: close }
        },
        Some(Dialog::OpenFolder) => rsx! {
            OpenFolder { onclose: close }
        },
    }
}

/// A session that waits for you: whose it is, what it asks, and for how long.
#[component]
fn Waiting(item: WaitingItem) -> Element {
    let view = use_context::<View>();
    rsx! {
        button {
            class: "waiting-row",
            r#type: "button",
            "aria-label": "{item.project_name}, {item.session_title}: {item.question}",
            onclick: move |_| view.jump(&item.project_folder, &item.session_id, &item.hosted_id),
            LiveDot { state: LiveState::Waiting }
            span { class: "waiting-texts",
                span { class: "waiting-line",
                    span { class: "waiting-title trimmed", "{item.project_name} — {item.session_title}" }
                    span { class: "caption", "{item.waited}" }
                }
                span { class: "waiting-question", "{item.question}" }
            }
            span { class: "waiting-open", "Open session" }
        }
    }
}

/// A project: its name and folder, how many of its sessions wait or run, what was said last,
/// and its latest sessions to go on with.
#[component]
fn Card(card: ProjectCard) -> Element {
    let view = use_context::<View>();
    let path = card.path.clone();
    rsx! {
        article { class: "card",
            button {
                class: "card-head",
                r#type: "button",
                "aria-label": "Open project {card.name}",
                title: "{card.path}",
                onclick: move |_| view.act(Action::EnterProject(path.clone().into())),
                span { class: "card-texts",
                    span { class: "card-name trimmed", "{card.name}" }
                    span { class: "caption trimmed", "{card.shown}" }
                }
                span { class: "card-marks",
                    if card.needs_you > 0 {
                        span { class: "card-chip waiting", "{card.needs_you} waiting" }
                    }
                    if card.running > 0 {
                        span { class: "card-chip", "{card.running} running" }
                    }
                    if !card.age.is_empty() {
                        span { class: "caption", "{card.age}" }
                    }
                }
            }
            if !card.last_said.is_empty() {
                p { class: "card-said", "{card.last_said}" }
            }
            if card.has_sessions {
                div { class: "card-sessions",
                    for session in card.sessions.iter().cloned() {
                        button {
                            key: "{session.id}",
                            class: "card-session",
                            r#type: "button",
                            "aria-label": "{session.title}, {session.age}",
                            onclick: {
                                let folder = card.path.clone();
                                move |_| view.jump(&folder, &session.id, &session.hosted_id)
                            },
                            LiveDot { state: session.state }
                            span { class: "trimmed card-session-title", "{session.title}" }
                            span { class: "caption", "{session.age}" }
                        }
                    }
                }
            } else {
                p { class: "caption card-none", "No sessions yet — open to start one" }
            }
        }
    }
}

/// What a dialog and the switcher are drawn in, over the window: dimmed behind, closed by Esc
/// or a click beside it. A click on the panel leaves the focus in it, so that Esc still
/// reaches it.
#[component]
fn Overlay(
    #[props(into)] label: String,
    onclose: EventHandler<()>,
    children: Element,
    #[props(default, into)] class: String,
) -> Element {
    rsx! {
        div {
            class: "overlay page-overlay",
            onkeydown: move |event: KeyboardEvent| {
                if event.key() == Key::Escape {
                    event.prevent_default();
                    event.stop_propagation();
                    onclose.call(());
                }
            },
            div { class: "overlay-dim", onclick: move |_| onclose.call(()) }
            div {
                class: "overlay-panel page-dialog {class}",
                role: "dialog",
                "aria-modal": "true",
                "aria-label": "{label}",
                tabindex: "-1",
                {children}
            }
        }
    }
}

/// A dialog over the window: under its name, with a cross to put it away.
#[component]
fn Modal(
    #[props(into)] title: String,
    icon: Lucide,
    onclose: EventHandler<()>,
    children: Element,
    #[props(default, into)] class: String,
) -> Element {
    rsx! {
        Overlay { label: title.clone(), class, onclose,
            div { class: "overlay-head",
                span { class: "dialog-icon",
                    Icon { icon }
                }
                h2 { class: "overlay-title", "{title}" }
                IconButton {
                    icon: Lucide::X,
                    label: "Cancel (Esc)",
                    onclick: move |_| onclose.call(()),
                }
            }
            {children}
        }
    }
}

/// A new project: a folder made by its name inside another, a git repository started in it or
/// not. What is wrong with it is said here, the dialog still open; made, it closes.
#[component]
fn NewProject(onclose: EventHandler<()>) -> Element {
    let view = use_context::<View>();
    let mut name = use_signal(String::new);
    let mut parent = use_signal(|| view.nav.peek().new_project_parent.clone());
    let mut git = use_signal(|| true);
    // Made since the dialog opened: it closes.
    use_bumped(|page| page.project_created, move || onclose.call(()));
    let submit = move || {
        if !name().is_empty() && !parent().is_empty() {
            view.act(Action::CreateProject {
                parent: parent(),
                name: name(),
                git: git(),
            });
        }
    };
    // What was wrong is taken back as either field changes: asked of the app only if something
    // was, not at every key.
    let problem = use_memo(move || view.nav.read().new_project_error.clone());
    let clear_problem = move || {
        if !problem.peek().is_empty() {
            view.act(Action::ClearNewProjectError);
        }
    };
    // Enter in either field makes it: not one that takes an input method's candidate.
    let entered = move |event: KeyboardEvent| {
        if event.key() == Key::Enter && !event.is_composing() {
            submit();
        }
    };
    let problem = problem();
    let inside = parent();
    let slash = if inside.is_empty() || inside == "/" {
        ""
    } else {
        "/"
    };
    let named = if name().is_empty() {
        "<name>".to_owned()
    } else {
        name()
    };
    let ready = !name().is_empty() && !inside.is_empty();
    rsx! {
        Modal { title: "New project", icon: Lucide::FolderPlus, onclose,
            div { class: "dialog-body",
                label { class: "dialog-label", r#for: "new-project-name", "Name" }
                input {
                    class: "field",
                    id: "new-project-name",
                    r#type: "text",
                    placeholder: "my-project",
                    value: "{name}",
                    onmounted: take_focus,
                    oninput: move |event| {
                        name.set(event.value());
                        clear_problem();
                    },
                    onkeydown: entered,
                }
                if !problem.is_empty() {
                    div { class: "dialog-problem", role: "alert", "{problem}" }
                }
                label { class: "dialog-label", r#for: "new-project-parent", "Inside" }
                input {
                    class: "field",
                    id: "new-project-parent",
                    r#type: "text",
                    placeholder: "Parent folder",
                    value: "{parent}",
                    oninput: move |event| {
                        parent.set(event.value());
                        clear_problem();
                    },
                    onkeydown: entered,
                }
                p { class: "caption dialog-hint", "edualc creates {inside}{slash}{named}" }
                label { class: "dialog-check",
                    input {
                        r#type: "checkbox",
                        checked: git(),
                        onchange: move |event| git.set(event.checked()),
                    }
                    "Start a git repository"
                }
                div { class: "dialog-actions",
                    Button { onclick: move |_| onclose.call(()), "Cancel" }
                    Button { primary: true, disabled: !ready, onclick: move |_| submit(), "Create project" }
                }
            }
        }
    }
}

/// A folder to work in: a path typed, or one of the recent ones. Why the folder asked for
/// didn't open is said here, the path still in its field to correct; opened, the dialog closes.
#[component]
fn OpenFolder(onclose: EventHandler<()>) -> Element {
    let view = use_context::<View>();
    let mut path = use_signal(String::new);
    // Opened since the dialog was: it closes.
    use_bumped(|page| page.projects_opened, move || onclose.call(()));
    let sessions = view.sessions.read();
    rsx! {
        Modal { title: "Open a folder", icon: Lucide::Folder, onclose, class: "tall",
            div { class: "dialog-body",
                div { class: "dialog-row",
                    input {
                        class: "field",
                        r#type: "text",
                        placeholder: "Path to a folder",
                        "aria-label": "Path to a folder",
                        value: "{path}",
                        onmounted: take_focus,
                        oninput: move |event| path.set(event.value()),
                        // An Enter that takes an input method's candidate isn't the path's.
                        onkeydown: move |event| {
                            if event.key() == Key::Enter && !event.is_composing() {
                                view.act(Action::OpenProject(path()));
                            }
                        },
                    }
                    Button {
                        primary: true,
                        disabled: path().trim().is_empty(),
                        onclick: move |_| view.act(Action::OpenProject(path())),
                        "Open"
                    }
                }
                if !sessions.problem.is_empty() {
                    div { class: "dialog-problem", role: "alert", "{sessions.problem}" }
                }
                div { class: "caption", "Recent" }
                div { class: "dialog-list",
                    for project in sessions.projects.iter() {
                        FolderRow { key: "{project.path}", project: project.clone() }
                    }
                }
            }
        }
    }
}

/// A project's page: its sessions, running and earlier, and a new one begun here.
#[component]
pub fn ProjectPage() -> Element {
    let view = use_context::<View>();
    use_page_focus(view);
    // Of the session and the sessions, only these: their other changes don't draw the page.
    let cwd = use_memo(move || view.session.read().cwd.clone());
    let empty = use_memo(move || view.sessions.read().empty.clone());
    let nav = view.nav.read();
    let folder = if cwd.read().is_empty() {
        nav.project_folder.clone()
    } else {
        cwd()
    };
    rsx! {
        main { class: "page", "aria-label": "Project {nav.project_name}",
            div { class: "page-column narrow",
                div { class: "page-head",
                    div { class: "page-names",
                        h2 { class: "page-title trimmed", "{nav.project_name}" }
                        div { class: "caption trimmed", title: "{folder}", "{folder}" }
                    }
                    Button {
                        primary: true,
                        icon: Lucide::SquarePen,
                        onclick: move |_| view.new_session(),
                        "New session here"
                    }
                }
                PageFilter {
                    filtered: Filtered::Sessions,
                    wide: true,
                    oninput: move |typed| view.act(Action::FilterSessions(typed)),
                }
                if !empty.read().is_empty() {
                    p { class: "page-empty", "{empty}" }
                }
                SessionRows {}
            }
        }
    }
}

/// The switcher (Ctrl+K in a session): each project's latest sessions, by what is typed. Up
/// and Down choose one, Enter opens it.
#[component]
pub fn Switcher(onclose: EventHandler<()>) -> Element {
    let view = use_context::<View>();
    let mut query = use_signal(String::new);
    let mut chosen = use_signal(|| 0_usize);
    // What is typed leaves these; choosing among them with the arrows doesn't filter again.
    let shown = use_memo(move || shown_sessions(&view.nav.read().all_sessions, &query.read()));
    let nav = view.nav.read();
    let shown = shown.read();
    let count = shown.len();
    let at = chosen().min(count.saturating_sub(1));
    let picked = shown.get(at).copied();
    // Opens the session at `source` in the list of all, and puts the switcher away.
    let open = move |source: usize| {
        if let Some(item) = view.nav.peek().all_sessions.get(source) {
            view.jump(&item.project_folder, &item.id, &item.hosted_id);
            onclose.call(());
        }
    };
    // The row chosen is brought into view: the focus stays in the field, so the list doesn't
    // follow by itself.
    let mut choose = move |row: usize| {
        chosen.set(row);
        crate::web::dom::reveal(&format!("{SWITCHER_LIST}-{row}"));
    };
    rsx! {
        Overlay { label: "Switch session", class: "tall switcher", onclose,
            div { class: "dialog-row switcher-head",
                input {
                    class: "field",
                    r#type: "text",
                    role: "combobox",
                    "aria-expanded": "true",
                    "aria-controls": SWITCHER_LIST,
                    "aria-activedescendant": (count > 0).then(|| format!("{SWITCHER_LIST}-{at}")),
                    placeholder: "Switch session… (type project or title)",
                    "aria-label": "Switch session",
                    value: "{query}",
                    onmounted: take_focus,
                    oninput: move |event| {
                        query.set(event.value());
                        chosen.set(0);
                    },
                    // The field's keys only: Enter on the cross closes. Those an input method
                    // takes (its candidates chosen, one taken) are left to it.
                    onkeydown: move |event: KeyboardEvent| {
                        if event.is_composing() {
                            return;
                        }
                        let key = event.key();
                        if key == Key::ArrowDown {
                            event.prevent_default();
                            choose((at + 1).min(count.saturating_sub(1)));
                        } else if key == Key::ArrowUp {
                            event.prevent_default();
                            choose(at.saturating_sub(1));
                        } else if key == Key::Enter {
                            event.prevent_default();
                            if let Some(source) = picked {
                                open(source);
                            }
                        }
                    },
                }
                IconButton {
                    icon: Lucide::X,
                    label: "Close (Esc)",
                    onclick: move |_| onclose.call(()),
                }
            }
            if shown.is_empty() {
                p { class: "page-empty",
                    if query().trim().is_empty() {
                        "No sessions available yet."
                    } else {
                        "No sessions match the filter."
                    }
                }
            }
            div { class: "dialog-list", id: SWITCHER_LIST, role: "listbox",
                for (index , source) in shown.iter().copied().enumerate() {
                    if let Some(item) = nav.all_sessions.get(source) {
                        div {
                            key: "{item.project_folder} {item.id}",
                            id: "{SWITCHER_LIST}-{index}",
                            class: "switcher-item",
                            class: if index == at { "chosen" },
                            class: if item.selected { "selected" },
                            role: "option",
                            "aria-selected": "{index == at}",
                            "aria-label": "{item.project_name}, {item.title}, {item.detail}",
                            onclick: move |_| open(source),
                            LiveDot { state: item.state }
                            span { class: "switcher-texts",
                                span { class: "trimmed", "{item.title}" }
                                span { class: "caption trimmed",
                                    if item.detail.is_empty() {
                                        "{item.project_name}"
                                    } else {
                                        "{item.project_name} · {item.detail}"
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The start page's filter reads a project's name, its folder and what was said last in
    /// it, whatever the case; the switcher's a session's title, project and state.
    #[test]
    fn filters_read_names_folders_and_what_was_said() {
        let card = |name: &str, shown: &str, said: &str| ProjectCard {
            name: name.to_owned(),
            shown: shown.to_owned(),
            last_said: said.to_owned(),
            ..ProjectCard::default()
        };
        let cards = [
            card("trailhead", "~/code/trailhead", "Fixed the gain."),
            card("notes", "~/notes", "Nothing yet."),
        ];
        assert_eq!(shown_cards(&cards, "").len(), 2);
        assert_eq!(shown_cards(&cards, " TRAIL ").len(), 1);
        assert_eq!(shown_cards(&cards, "nothing").len(), 1);
        assert!(shown_cards(&cards, "absent").is_empty());

        let items = [SwitcherItem {
            project_name: "trailhead".to_owned(),
            title: "Elevation gain".to_owned(),
            detail: "waiting".to_owned(),
            ..SwitcherItem::default()
        }];
        assert_eq!(shown_sessions(&items, "gain").len(), 1);
        assert_eq!(shown_sessions(&items, "Waiting").len(), 1);
        assert!(shown_sessions(&items, "notes").is_empty());
    }
}
