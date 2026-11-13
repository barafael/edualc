//! The browser demo's daemon: Claude Code, played by a script. It takes what the window sends,
//! as the daemon on Linux does, and answers as `claude -p` would in stream-json, from a
//! scenario a word in the prompt picks (see `scenarios`), with pauses so the window shows the
//! answer come. Nothing leaves the page.

use std::collections::HashMap;
use std::path::Path;

use async_channel::{Receiver, Sender};

use crate::app::Event;
use crate::demo::script::{Session, Sink};
use crate::demo::world as demo;
use crate::ipc::{Act, ActId, Answer, Generation, PasteFailure, Place, Read, ToDaemon};

/// Answers the window until it closes.
pub(crate) async fn run(requests: Receiver<ToDaemon>, events: Sender<Event>) {
    let mut sessions: HashMap<Generation, Session> = HashMap::new();
    while let Ok(request) = requests.recv().await {
        match request {
            ToDaemon::Start { key, launch } => {
                let session = Session::start(key, Sink::Events(events.clone()), &launch);
                sessions.insert(key, session);
            }
            ToDaemon::Input { key, text, last } => {
                if let Some(session) = sessions.get_mut(&key)
                    && let Some(task) = session.take(&text, last)
                    && let Err(error) = slint::spawn_local(task.run())
                {
                    tracing::warn!(%error, "what the script does couldn't start");
                }
            }
            ToDaemon::End { key } => {
                if sessions.remove(&key).is_some() {
                    Sink::Events(events.clone()).ended(key);
                }
            }
            ToDaemon::Act { request, act } => acted(&events, request, act),
            ToDaemon::Read { request, read } => read_for(&events, request, read),
            // Nothing is written to a disk, and nothing in the demo changes but by the script;
            // the page's daemon serves this window alone, and ends with the page.
            ToDaemon::Hello { .. }
            | ToDaemon::Attach { .. }
            | ToDaemon::Detach { .. }
            | ToDaemon::Identify { .. }
            | ToDaemon::ShutDown
            | ToDaemon::SaveSettings(_)
            | ToDaemon::Watch { .. }
            | ToDaemon::Unwatch(_) => {}
        }
    }
}

/// What a window reads, from the demo project.
fn read_for(events: &Sender<Event>, request: ActId, read: Read) {
    let mut lines = Vec::new();
    let answer = match read {
        Read::Live => Answer::Live(demo::running()),
        Read::Projects => Answer::Projects(demo::projects()),
        Read::Sessions { cwd, limit, .. } => {
            let mut sessions = demo::saved(&cwd);
            sessions.truncate(limit);
            Answer::Sessions(sessions)
        }
        Read::Transcript { place, from } => {
            let path = match place {
                Place::Path(path) => path,
                Place::Session { cwd, id } => cwd.join(format!("{}.jsonl", id.as_str())),
                // Beside its session's, as Claude Code keeps it: not its session's own.
                Place::Subagent { cwd, id, task, .. } => crate::sessions::transcript_of(
                    &cwd.join(format!("{}.jsonl", id.as_str())),
                    &task,
                ),
            };
            let read = match demo::transcript(&path) {
                Some(text) => {
                    let rest = usize::try_from(from)
                        .ok()
                        .and_then(|at| text.get(at..))
                        .unwrap_or_default();
                    lines = rest.lines().map(str::to_owned).collect();
                    Ok(u64::try_from(text.len()).unwrap_or(u64::MAX))
                }
                None => Err("not in the demo".to_owned()),
            };
            Answer::Transcript { path, read }
        }
        Read::WorktreeOf { .. } | Read::LinkedWorktree { .. } => Answer::Worktree(None),
        Read::TypedFolder(typed) => Answer::Folder(demo_folder(Path::new(typed.trim()))),
        Read::Folder(path) => Answer::Folder(demo_folder(&path)),
    };
    let event = Event::Answered {
        request,
        answer,
        lines: crate::sessions::parse_lines(&lines),
    };
    if events.try_send(event).is_err() {
        tracing::debug!("the window is gone");
    }
}

/// One of the demo's folders, or why a path isn't one.
fn demo_folder(path: &Path) -> Result<std::path::PathBuf, String> {
    demo::projects()
        .into_iter()
        .map(|project| project.path)
        .find(|project| project == path)
        .ok_or_else(|| format!("{} isn't one of the demo's folders", path.display()))
}

/// What the desktop is asked, answered as a browser can.
fn acted(events: &Sender<Event>, request: ActId, act: Act) {
    let event = match act {
        Act::CheckTrust { folder } => match demo::untrusted(&folder) {
            Some(runs) => Event::Trusted {
                request,
                accepted: false,
                runs,
            },
            None => Event::Trusted {
                request,
                accepted: true,
                runs: Vec::new(),
            },
        },
        Act::PickFolder => Event::Picked {
            request,
            folder: None,
        },
        Act::PasteImage => Event::Pasted {
            request,
            pasted: Err(PasteFailure::NoImage),
        },
        Act::OpenIn { .. }
        | Act::OpenUrl(_)
        | Act::Notify { .. }
        | Act::FocusWindowOf { .. }
        | Act::StopSession(_) => Event::Acted {
            request,
            result: Ok(()),
        },
    };
    if events.try_send(event).is_err() {
        tracing::debug!("the window is gone");
    }
}
