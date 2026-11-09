//! The Android app: the page's window (`web`), drawn by Dioxus in Android's WebView, and run by
//! native code rather than in a browser, as the Slint window is. It reaches an edualcd as the
//! native window does (`edualc_core::line`: TLS, edualcd's certificate pinned, reached again as
//! the network comes back, its name looked up anew), from the edualcds it was paired with
//! (`ipc::servers`, kept in the app's own files, out of Android's backups). Before the window
//! shows, its start screen lists them and pairs with another by the address edualcd prints
//! (`edualcd --pair`); a demo's address (`edualcd --demo`) is taken too, and the window says
//! its line is unencrypted. What the window does on the phone itself (a link opened, a
//! notification, the clipboard) is Android's (`java`).
//!
//! The window works where its edualcd's home folder is, or opens the session it last showed
//! there again (Android ends an app in the background, and starts it anew as it comes back);
//! it opens other folders from its list, as the page does. Closing it (the settings' "Choose
//! another edualcd") goes back to the start screen.

// Dioxus's `rsx!` keeps its templates in `OnceLock`s: its state, not ours (as in `web`).
#![allow(clippy::disallowed_types)]

use std::path::PathBuf;

use dioxus::prelude::*;
use edualc_core::app::{App as WindowApp, Desktop};
use edualc_core::external::wire::SessionId;
use edualc_core::ipc::pairing::Server;
use edualc_core::ipc::servers::Servers;
use edualc_core::line::{self, Lookup};
use edualc_core::link::{Greeting, Link};
use edualc_core::sessions::Saved;

use crate::web::view::PageFrontend;
use crate::web::widgets::{Button, Icon, IconButton, Lucide};
use crate::web::{Frame, Shell, View};

pub mod java;

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

/// What is being typed to pair with an edualcd: kept while the start screen goes and comes
/// back, so that a pairing that failed is mended, not pasted again.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct Form {
    address: String,
    name: String,
}

/// The session the window showed last, and on which edualcd: opened again as the app starts
/// anew there.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct LastShown {
    server: String,
    session: Option<SessionId>,
}

/// What the app keeps: the edualcds paired with, the session shown last, and the folder they
/// are kept in.
#[derive(Debug, Default)]
struct Kept {
    servers: Servers,
    last: Option<LastShown>,
    /// None if Android named no folder for the app's files: nothing is kept then.
    dir: Option<PathBuf>,
    /// The paired edualcds' file didn't read: it isn't written over, which would lose them all.
    unreadable: bool,
}

impl Kept {
    const SERVERS: &str = "servers.json";
    const LAST: &str = "last-shown.json";

    fn load() -> (Self, Option<String>) {
        let dir = match java::files_dir() {
            Ok(dir) => dir,
            Err(error) => return (Self::default(), Some(error.to_string())),
        };
        let last = std::fs::read(dir.join(Self::LAST))
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok());
        let (servers, problem) = match Servers::load(&dir.join(Self::SERVERS)) {
            Ok(servers) => (servers, None),
            Err(error) => (
                Servers::default(),
                Some(format!(
                    "{error}. Nothing paired now is kept, so that it isn't written over."
                )),
            ),
        };
        let kept = Self {
            servers,
            last,
            dir: Some(dir),
            unreadable: problem.is_some(),
        };
        (kept, problem)
    }

    /// Writes the paired edualcds; why they weren't, if they weren't.
    fn save(&self) -> Option<String> {
        if self.unreadable {
            return Some(
                "the paired edualcds' file doesn't read: it isn't written over".to_owned(),
            );
        }
        let dir = self.dir.as_ref()?;
        self.servers
            .save(&dir.join(Self::SERVERS))
            .err()
            .map(|error| error.to_string())
    }

    /// Keeps the session `last` shows (best effort: lost, a new session opens).
    fn keep_last(&mut self, last: LastShown) {
        if self.last.as_ref() == Some(&last) {
            return;
        }
        if let Some(dir) = &self.dir
            && let Ok(bytes) = serde_json::to_vec(&last)
            && let Err(error) = std::fs::write(dir.join(Self::LAST), bytes)
        {
            tracing::warn!(%error, "the session shown wasn't kept");
        }
        self.last = Some(last);
    }

    /// The session shown last on the edualcd paired as `name`, if any.
    fn last_on(&self, name: &str) -> Option<SessionId> {
        self.last
            .as_ref()
            .filter(|last| last.server == name)
            .and_then(|last| last.session.clone())
    }
}

/// What the app asks to reach an edualcd.
#[derive(Debug, Clone, PartialEq)]
struct Reach {
    name: String,
    server: Server,
    /// Paired anew: kept once reached.
    keep: bool,
}

/// The app.
#[component]
pub fn App() -> Element {
    let view = use_hook(View::new);
    use_context_provider(|| view);
    let mut kept = use_signal(Kept::default);
    let mut stage = use_signal(|| Stage::Choosing(None));
    let form = use_signal(Form::default);
    // Bumped by each reach and by giving one up: an answer for another is let go.
    let attempt = use_signal(|| 0_u32);
    // Run here, whoever asks (the start screen goes as the window comes): what it spawns is the
    // app's, as long as it runs.
    let reaching = use_callback(move |asked: Reach| reach(view, stage, kept, form, attempt, asked));
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
        let (loaded, problem) = Kept::load();
        let default = loaded
            .servers
            .chosen(None)
            .ok()
            .filter(|_| problem.is_none());
        kept.set(loaded);
        match default {
            Some((name, server)) => reaching(Reach {
                name,
                server,
                keep: false,
            }),
            None => stage.set(Stage::Choosing(problem)),
        }
    });
    // The session shown, kept as it changes, to be opened again.
    let shown = use_memo(move || view.page.read().session.clone());
    use_effect(move || {
        let session = shown();
        if let Stage::Running(server) = &*stage.peek() {
            kept.write().keep_last(LastShown {
                server: server.clone(),
                session,
            });
        }
    });
    match stage() {
        Stage::Running(_) => rsx! {
            Shell {}
        },
        Stage::Choosing(said) => rsx! {
            Start { reaching, kept, form, said }
        },
        Stage::Reaching(name) => rsx! {
            Screen {
                div { class: "start-reaching", role: "status",
                    span { class: "start-heron", style: "--heron: url('{HERON}');" }
                    "Reaching {name}…"
                    Button {
                        onclick: move |_| {
                            let mut attempt = attempt;
                            attempt += 1;
                            stage.set(Stage::Choosing(None));
                        },
                        "Cancel"
                    }
                }
            }
        },
    }
}

/// The start screen's frame: the window's look, a column in the middle.
#[component]
fn Screen(children: Element) -> Element {
    rsx! {
        Frame {
            document::Stylesheet { href: START_STYLE }
            main { class: "start", {children} }
        }
    }
}

/// The edualcds paired with, to reach one or forget it, and pairing with another.
#[component]
fn Start(
    reaching: Callback<Reach>,
    kept: Signal<Kept>,
    form: Signal<Form>,
    said: Option<String>,
) -> Element {
    let mut form = form;
    let mut problem = use_signal(|| None::<String>);
    // The edualcd whose forgetting waits for a yes: its token goes with it.
    let mut forgetting = use_signal(|| None::<String>);
    let listed: Vec<(String, Server, bool)> = {
        let kept = kept.read();
        let default = kept.servers.default_name();
        kept.servers
            .paired()
            .map(|(name, server)| (name.clone(), server.clone(), Some(name.as_str()) == default))
            .collect()
    };
    let mut pair = move || {
        let Form { address, name } = form.peek().clone();
        let server = match Server::parse(&address) {
            Ok(server) => server,
            Err(error) => return problem.set(Some(error.to_string())),
        };
        let named = name.trim().to_owned();
        let name = if named.is_empty() {
            kept.peek().servers.name_for(&server)
        } else {
            named
        };
        problem.set(None);
        reaching(Reach {
            name,
            server,
            keep: true,
        });
    };
    let shown_problem = problem().or(said);
    let typed = form.read().clone();
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
                        if forgetting().as_deref() == Some(named.as_str()) {
                            li { key: "{named}", class: "start-server start-forgetting", role: "alert",
                                span { class: "start-server-texts",
                                    span { class: "start-server-name", "Forget {named}?" }
                                    span { class: "caption",
                                        if server.is_demo() {
                                            "Its address goes."
                                        } else {
                                            "Its token goes with it: pairing again takes edualcd --pair on its machine."
                                        }
                                    }
                                }
                                Button {
                                    danger: true,
                                    onclick: move |_| {
                                        kept.write().servers.forget(&named);
                                        let unsaved = kept.peek().save();
                                        problem.set(unsaved);
                                        forgetting.set(None);
                                    },
                                    "Forget"
                                }
                                Button { onclick: move |_| forgetting.set(None), "Keep" }
                            }
                        } else {
                            li { key: "{named}", class: "start-server",
                                button {
                                    class: "start-server-reach",
                                    // Its children are made first: the handler takes the name and
                                    // address last.
                                    onclick: move |_| {
                                        reaching(Reach {
                                            name: named.clone(),
                                            server: server.clone(),
                                            keep: false,
                                        })
                                    },
                                    Icon { icon: if server.is_demo() { Lucide::Sparkle } else { Lucide::SquareTerminal } }
                                    span { class: "start-server-texts",
                                        span { class: "start-server-name", "{named}" }
                                        span { class: "caption",
                                            "{server.authority()}"
                                            if server.is_demo() { " · demo, unencrypted" }
                                            if default { " · last reached" }
                                        }
                                    }
                                }
                                IconButton {
                                    icon: Lucide::Trash,
                                    label: format!("Forget {named}"),
                                    onclick: {
                                        let named = named.clone();
                                        move |_| forgetting.set(Some(named.clone()))
                                    },
                                }
                            }
                        }
                    }
                }
            }
            section { class: "start-section",
                h2 { "Pair with an edualcd" }
                p { class: "caption",
                    "Start edualcd where Claude Code is to run (systemctl --user enable --now edualcd), and paste the address it prints there: edualcd --pair. It starts https:// and holds its token, which lets whoever has it run Claude Code as you: keep it as a password. A demo's address (edualcd --demo) starts http://; its line isn't encrypted."
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
                    // Set as it is drawn, not from here after: the WebView's text is the one
                    // typed, however fast.
                    initial_value: "{typed.address}",
                    oninput: move |event| form.write().address = event.value(),
                }
                label { class: "start-label", r#for: "start-name", "Name (its host's, if left empty)" }
                input {
                    id: "start-name",
                    class: "field",
                    autocapitalize: "off",
                    "autocorrect": "off",
                    initial_value: "{typed.name}",
                    oninput: move |event| form.write().name = event.value(),
                    onkeydown: move |event| {
                        if event.key() == Key::Enter {
                            pair();
                        }
                    },
                }
                div { class: "start-actions",
                    Button {
                        primary: true,
                        disabled: typed.address.trim().is_empty(),
                        onclick: move |_| pair(),
                        "Pair"
                    }
                }
            }
        }
    }
}

/// An edualcd reached: its line, its greeting, and the session to open and where.
struct Reached {
    link: Link,
    greeting: Greeting,
    resume: Option<SessionId>,
    cwd: PathBuf,
}

/// Reaches `server`, opens its line, and finds where the window opens: the session shown last
/// there, if edualcd still keeps it, in the folder it was started in; else edualcd's home.
/// Blocking: on a thread of its own.
fn reached(
    server: &Server,
    last: Option<SessionId>,
    events: async_channel::Sender<edualc_core::app::Event>,
) -> Result<Reached, line::Error> {
    let mut connected = line::connect(server, Lookup::EachTime)?;
    let home = connected
        .greeting
        .home
        .clone()
        .unwrap_or_else(|| PathBuf::from("/"));
    let (resume, cwd) = match last {
        Some(id) => match connected.saved(&id) {
            Ok(Saved::StartedIn(folder)) => (Some(id), folder),
            Ok(Saved::Unplaced) => (Some(id), home),
            Ok(Saved::Nowhere) => (None, home),
            Err(error) => {
                tracing::info!(%error, "the session shown last wasn't found");
                (None, home)
            }
        },
        None => (None, home),
    };
    let (link, greeting) = line::run(connected, events);
    Ok(Reached {
        link,
        greeting,
        resume,
        cwd,
    })
}

/// Reaches the edualcd `asked` names on a thread (it blocks), and runs the window on its line
/// until it closes; then the start screen again. Paired anew, it is kept once reached, as the
/// one reached next time; reached from the list, it becomes that too. Given up meanwhile
/// (`attempt` moved on), its line is let go as it comes.
fn reach(
    view: View,
    mut stage: Signal<Stage>,
    mut kept: Signal<Kept>,
    mut form: Signal<Form>,
    mut attempt: Signal<u32>,
    asked: Reach,
) {
    let Reach { name, server, keep } = asked;
    attempt += 1;
    let this = *attempt.peek();
    stage.set(Stage::Reaching(name.clone()));
    spawn(async move {
        let (events, received) = async_channel::unbounded();
        let (answer, answered) = async_channel::bounded(1);
        let (to, last, line_events) = (server.clone(), kept.peek().last_on(&name), events.clone());
        // The line's runtime is made and ended on threads of its own, never in this task.
        std::thread::spawn(move || {
            if answer
                .send_blocking(reached(&to, last, line_events))
                .is_err()
            {
                tracing::debug!("the app ended as edualcd answered");
            }
        });
        let answer = answered.recv().await;
        if *attempt.peek() != this {
            // Given up on: dropped, its line ends.
            return;
        }
        let Reached {
            link,
            greeting,
            resume,
            cwd,
        } = match answer {
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
        if keep || kept.peek().servers.default_name() != Some(name.as_str()) {
            kept.write().servers.pair(name.clone(), server.clone());
            if let Some(problem) = kept.peek().save() {
                tracing::warn!(%problem, "the paired edualcds weren't kept");
            }
        }
        if keep {
            form.set(Form::default());
        }
        {
            let mut page = view.page;
            let mut page = page.write();
            page.closed = false;
            page.lost = None;
            page.session = None;
            page.unencrypted = server.is_demo();
        }
        // Another edualcd's draft isn't this one's.
        let mut draft = view.draft;
        draft.set(String::new());
        let mut shown_events = view.events;
        shown_events.set(Some(events.clone()));
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
        app.begin(greeting.hosted, greeting.live, resume, None, None);
        stage.set(Stage::Running(name));
        // Asked here, with the app in front: Android shows no question from the background.
        java::ask_to_notify();
        app.run(received).await;
        // The window closed: its line ends with it.
        shown_events.set(None);
        stage.set(Stage::Choosing(None));
    });
}
