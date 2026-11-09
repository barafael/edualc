//! The Android app: the page's window (`web`), drawn by Dioxus in Android's WebView, and run by
//! native code rather than in a browser, as the Slint window is. It reaches an edualcd as the
//! native window does (`edualc_core::line`: TLS, edualcd's certificate pinned, reached again as
//! the network comes back), from the edualcds it was paired with (`ipc::servers`, kept in the
//! app's own files). Before the window shows, its start screen lists them and pairs with
//! another by the address edualcd prints (`edualcd --pair`); a demo's address
//! (`edualcd --demo`) is taken too. What the window does on the phone itself (a link opened, a
//! notification, the clipboard) is Android's (`java`).
//!
//! The window works where its edualcd's home folder is; it opens others from its list of
//! folders, as the page does. Closing it (the settings' "Choose another edualcd") goes back to
//! the start screen.

// Dioxus's `rsx!` keeps its templates in `OnceLock`s: its state, not ours (as in `web`).
#![allow(clippy::disallowed_types)]

use std::path::PathBuf;

use dioxus::prelude::*;
use edualc_core::app::{App as WindowApp, Desktop};
use edualc_core::ipc::pairing::Server;
use edualc_core::ipc::servers::Servers;

use crate::web::view::PageFrontend;
use crate::web::widgets::{Button, Icon, IconButton, Lucide};
use crate::web::{Fonts, Shell, View};

pub mod java;

const STYLE: Asset = asset!("/assets/edualc.css");
const START_STYLE: Asset = asset!("/assets/start.css");
const HERON: Asset = asset!("/assets/heron/heron.svg");

/// Where the app is.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Stage {
    /// On the start screen; why the last edualcd wasn't reached or was left, if it was.
    Choosing(Option<String>),
    /// Reaching the edualcd paired as this name.
    Reaching(String),
    /// The window, its line open to the edualcd paired as this name.
    Running(String),
}

/// The edualcds paired with, and where they are kept.
#[derive(Debug, Default)]
struct Paired {
    servers: Servers,
    /// None if Android named no folder for the app's files: nothing is kept then.
    file: Option<PathBuf>,
}

impl Paired {
    fn load() -> (Self, Option<String>) {
        let file = match java::files_dir() {
            Ok(dir) => dir.join("servers.json"),
            Err(error) => return (Self::default(), Some(error.to_string())),
        };
        match Servers::load(&file) {
            Ok(servers) => (
                Self {
                    servers,
                    file: Some(file),
                },
                None,
            ),
            Err(error) => (
                Self {
                    servers: Servers::default(),
                    file: Some(file),
                },
                Some(error.to_string()),
            ),
        }
    }

    /// Writes what is kept; why it wasn't, if it wasn't.
    fn save(&self) -> Option<String> {
        let file = self.file.as_ref()?;
        self.servers.save(file).err().map(|error| error.to_string())
    }
}

/// The app.
#[component]
pub fn App() -> Element {
    let view = use_hook(View::new);
    use_context_provider(|| view);
    let mut paired = use_signal(Paired::default);
    let mut stage = use_signal(|| Stage::Choosing(None));
    // Run here, whoever asks (the start screen goes as the window comes): what it spawns is the
    // app's, as long as it runs.
    let reaching = use_callback(move |(name, server, keep): (String, Server, bool)| {
        reach(view, stage, paired, name, server, keep);
    });
    // Images are drawn at the screen's scale, which the WebView knows.
    use_hook(move || {
        spawn(async move {
            let asked = document::eval("return window.devicePixelRatio;");
            if let Ok(ratio) = asked.join::<f64>().await {
                let mut page = view.page;
                page.write().pixel_ratio = ratio;
            }
        });
    });
    // Read as the app starts; the edualcd reached last is reached again at once.
    use_hook(move || {
        let (loaded, problem) = Paired::load();
        let default = loaded
            .servers
            .chosen(None)
            .ok()
            .filter(|_| problem.is_none());
        paired.set(loaded);
        match default {
            Some((name, server)) => reaching((name, server, false)),
            None => stage.set(Stage::Choosing(problem)),
        }
    });
    match stage() {
        Stage::Running(_) => rsx! {
            Shell {}
        },
        Stage::Choosing(said) => rsx! {
            Start { reaching, paired, said }
        },
        Stage::Reaching(name) => rsx! {
            Screen {
                div { class: "start-reaching", role: "status",
                    span { class: "start-heron", style: "--heron: url('{HERON}');" }
                    "Reaching {name}…"
                }
            }
        },
    }
}

/// The start screen's frame: the window's look, a column in the middle.
#[component]
fn Screen(children: Element) -> Element {
    let view = use_context::<View>();
    let page = view.page.read();
    let theme = match page.look {
        edualc_core::present::Look::Gruvbox => "gruvbox",
        edualc_core::present::Look::Plain => "plain",
    };
    let style = format!("--base-size: {}px;", page.text_size);
    rsx! {
        document::Stylesheet { href: STYLE }
        document::Stylesheet { href: START_STYLE }
        Fonts {}
        document::Title { "edualc" }
        div { class: "window {theme}", style,
            main { class: "start", {children} }
        }
    }
}

/// The edualcds paired with, to reach one or forget it, and pairing with another.
#[component]
fn Start(
    reaching: Callback<(String, Server, bool)>,
    paired: Signal<Paired>,
    said: Option<String>,
) -> Element {
    let mut address = use_signal(String::new);
    let mut name = use_signal(String::new);
    let mut problem = use_signal(|| None::<String>);
    let listed: Vec<(String, Server, bool)> = {
        let paired = paired.read();
        let default = paired.servers.default_name();
        paired
            .servers
            .paired()
            .map(|(name, server)| (name.clone(), server.clone(), Some(name.as_str()) == default))
            .collect()
    };
    let mut pair = move || {
        let server = match Server::parse(&address.peek()) {
            Ok(server) => server,
            Err(error) => return problem.set(Some(error.to_string())),
        };
        let named = name.peek().trim().to_owned();
        let named = if named.is_empty() {
            paired.peek().servers.name_for(&server)
        } else {
            named
        };
        problem.set(None);
        reaching((named, server, true));
    };
    let shown_problem = problem().or(said);
    rsx! {
        Screen {
            header { class: "start-header",
                span { class: "start-heron", style: "--heron: url('{HERON}');" }
                h1 { "edualc" }
            }
            if let Some(problem) = shown_problem {
                p { class: "start-problem", role: "alert", "{problem}" }
            }
            section { class: "start-section",
                h2 { "Your edualcds" }
                if listed.is_empty() {
                    p { class: "caption",
                        "None yet: pair with one below. edualcd runs Claude Code, on your computer or another you reach."
                    }
                }
                ul { class: "start-servers",
                    for (named, server, default) in listed {
                        li { key: "{named}", class: "start-server",
                            button {
                                class: "start-server-reach",
                                // Its children are made first: the handler takes the name and
                                // address last.
                                onclick: move |_| reaching((named.clone(), server.clone(), false)),
                                Icon { icon: if server.is_demo() { Lucide::Sparkle } else { Lucide::SquareTerminal } }
                                span { class: "start-server-texts",
                                    span { class: "start-server-name", "{named}" }
                                    span { class: "caption",
                                        "{server.authority()}"
                                        if server.is_demo() { " · demo" }
                                        if default { " · last reached" }
                                    }
                                }
                            }
                            IconButton {
                                icon: Lucide::Trash,
                                label: format!("Forget {named}"),
                                onclick: {
                                    let named = named.clone();
                                    move |_| {
                                        paired.write().servers.forget(&named);
                                        let unsaved = paired.peek().save();
                                        problem.set(unsaved);
                                    }
                                },
                            }
                        }
                    }
                }
            }
            section { class: "start-section",
                h2 { "Pair with an edualcd" }
                p { class: "caption",
                    "Start edualcd where Claude Code is to run (systemctl --user enable --now edualcd), and paste the address it prints there: edualcd --pair. It starts https:// and holds its token, which lets whoever has it run Claude Code as you: keep it as a password. A demo's address (edualcd --demo) starts http://."
                }
                label { class: "start-label", r#for: "start-address", "Address" }
                textarea {
                    id: "start-address",
                    class: "field start-address",
                    rows: "3",
                    spellcheck: "false",
                    autocapitalize: "off",
                    autocomplete: "off",
                    "autocorrect": "off",
                    inputmode: "url",
                    placeholder: "https://…:7878/#token=…&fingerprint=…",
                    // Not set from here: the WebView's text is the one typed, however fast.
                    oninput: move |event| address.set(event.value()),
                }
                label { class: "start-label", r#for: "start-name", "Name (its host's, if left empty)" }
                input {
                    id: "start-name",
                    class: "field",
                    autocapitalize: "off",
                    "autocorrect": "off",
                    oninput: move |event| name.set(event.value()),
                    onkeydown: move |event| {
                        if event.key() == Key::Enter {
                            pair();
                        }
                    },
                }
                div { class: "start-actions",
                    Button {
                        primary: true,
                        disabled: address().trim().is_empty(),
                        onclick: move |_| pair(),
                        "Pair"
                    }
                }
            }
        }
    }
}

/// Reaches `server`, as `name`, on a thread (it blocks), and runs the window on its line until
/// it closes; then the start screen again. Paired anew (`keep`), it is kept once reached, as
/// the one reached next time; reached from the list, it becomes that too.
fn reach(
    view: View,
    mut stage: Signal<Stage>,
    mut paired: Signal<Paired>,
    name: String,
    server: Server,
    keep: bool,
) {
    stage.set(Stage::Reaching(name.clone()));
    spawn(async move {
        let (events, received) = async_channel::unbounded();
        let (answer, answered) = async_channel::bounded(1);
        let reached = server.clone();
        let line_events = events.clone();
        // The line's runtime is made and ended on threads of its own, never in this task.
        std::thread::spawn(move || {
            let reached = edualc_core::line::connect(&reached)
                .map(|connected| edualc_core::line::run(connected, line_events));
            if answer.send_blocking(reached).is_err() {
                tracing::debug!("the app ended as edualcd answered");
            }
        });
        let (link, greeting) = match answered.recv().await {
            Ok(Ok(reached)) => reached,
            Ok(Err(error)) => {
                stage.set(Stage::Choosing(Some(format!("{name}: {error}"))));
                return;
            }
            Err(_gone) => {
                stage.set(Stage::Choosing(Some(format!("{name} wasn't reached"))));
                return;
            }
        };
        if keep || paired.peek().servers.default_name() != Some(name.as_str()) {
            paired.write().servers.pair(name.clone(), server);
            if let Some(problem) = paired.peek().save() {
                tracing::warn!(%problem, "the paired edualcds weren't kept");
            }
        }
        {
            let mut page = view.page;
            let mut page = page.write();
            page.closed = false;
            page.lost = None;
        }
        let mut shown_events = view.events;
        shown_events.set(Some(events.clone()));
        let cwd = greeting.home.clone().unwrap_or_else(|| PathBuf::from("/"));
        let text_size = view.page.peek().text_size;
        let mut app = WindowApp::new(
            PageFrontend::new(view),
            events,
            link,
            Desktop::of(&greeting, text_size),
            greeting.settings.clone(),
            cwd,
            None,
        );
        app.begin(greeting.hosted, greeting.live, None, None, None);
        stage.set(Stage::Running(name));
        app.run(received).await;
        // The window closed: its line ends with it.
        shown_events.set(None);
        stage.set(Stage::Choosing(None));
    });
}
