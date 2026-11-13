//! The browser demo: the window as on Linux, in a page, with a script in place of the daemon
//! and Claude Code (`daemon`, `scenarios`), and a demo project's sessions in place of
//! `~/.claude` (`demo`). It opens on a session that fixed the project's elevation gain.

use slint::ComponentHandle;

use crate::app::{self, App, Desktop};
use crate::external::wire::SessionId;
use crate::settings::Settings;
use crate::{generated, ui};

mod daemon;
pub(crate) mod demo;
mod scenarios;

/// How many messages to the daemon may wait; the demo's answers them at once.
const QUEUE: usize = 1024;

pub(crate) fn main() {
    let window = match generated::AppWindow::new() {
        Ok(window) => window,
        Err(error) => {
            tracing::error!(%error, "no window");
            return;
        }
    };
    let (events, received) = async_channel::unbounded();
    let (link, requests) = async_channel::bounded(QUEUE);
    ui::wire(&window, &events);
    let frontend = ui::SlintFrontend::new(
        window.clone_strong(),
        crate::appearance::Fonts {
            sans: None,
            mono: None,
        },
        events.clone(),
    );
    let mut app = App::new(
        frontend,
        events.clone(),
        link,
        Desktop::in_the_browser(crate::appearance::DEFAULT_TEXT_SIZE),
        Ok(Settings::default()),
        app::Start {
            cwd: demo::Folder::Trailhead.path().to_path_buf(),
            backend: crate::claude::Backend::Claude,
            model: None,
        },
    );
    app.begin(
        Vec::new(),
        Vec::new(),
        Some(SessionId::from(demo::FIRST)),
        None,
        None,
    );
    let started = slint::spawn_local(app.run(received))
        .and_then(|_app| slint::spawn_local(daemon::run(requests, events)));
    if let Err(error) = started {
        tracing::error!(%error, "the demo couldn't start");
        return;
    }
    if let Err(error) = window.run() {
        tracing::error!(%error, "the window couldn't run");
    }
}
