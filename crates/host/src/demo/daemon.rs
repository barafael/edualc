//! The demo's daemon: it takes what a window sends, as the daemon does, and answers as `claude
//! -p` would (`demo::script`), from a scenario a word in the prompt picks, with pauses so the
//! window shows the answer come; what a window reads comes from the demo project
//! (`demo::world`). The window is on another machine, as any page's is.

use std::collections::HashMap;
use std::path::Path;

use crate::daemon::Line;
use crate::ipc::{
    Act, ActFailure, ActId, Answer, FromDaemon, Generation, PROTOCOL, PasteFailure, Place, Read,
    StartFailure, ToDaemon,
};
use crate::settings::Settings;
use edualc_core::demo::script::{Session, Sink};
use edualc_core::demo::world as demo;
use tokio::sync::mpsc::Sender;

/// Answers a window until it hangs up.
pub async fn run(line: Line) {
    let Line {
        mut requests,
        outbox,
    } = line;
    let mut sessions: HashMap<Generation, Session> = HashMap::new();
    while let Some(request) = requests.recv().await {
        match request {
            ToDaemon::Hello { .. } => greet(&outbox),
            ToDaemon::Start { key, launch } => {
                let session = Session::start(key, Sink::Line(outbox.clone()), &launch);
                sessions.insert(key, session);
            }
            ToDaemon::Input { key, text, last } => {
                if let Some(session) = sessions.get_mut(&key)
                    && let Some(task) = session.take(&text, last)
                {
                    tokio::spawn(task.run());
                }
            }
            ToDaemon::End { key } => {
                if sessions.remove(&key).is_some() {
                    Sink::Line(outbox.clone()).ended(key);
                }
            }
            ToDaemon::Attach { key, .. } => tell(
                &outbox,
                FromDaemon::Failed {
                    key,
                    failure: StartFailure::NotHosted,
                },
            ),
            ToDaemon::Act { request, act } => acted(&outbox, request, act),
            ToDaemon::Read { request, read } => read_for(&outbox, request, read),
            // Nothing is kept, and nothing changes on its own; the demo ends with edualcd.
            ToDaemon::ShutDown
            | ToDaemon::SaveSettings(_)
            | ToDaemon::Watch { .. }
            | ToDaemon::Unwatch(_)
            | ToDaemon::Detach { .. }
            | ToDaemon::Identify { .. } => {}
        }
    }
}

/// The hello, and the sessions the demo runs (none; four "run in other terminals").
fn greet(outbox: &Sender<FromDaemon>) {
    let settings = Settings {
        trusted_folders: vec![demo::Folder::Trailhead.path().to_path_buf()],
        ..Settings::default()
    };
    tell(
        outbox,
        FromDaemon::Hello {
            protocol: PROTOCOL,
            settings: Ok(settings),
            openers: Vec::new(),
            live: demo::running(),
            finds_windows: false,
            local: false,
            home: demo::Folder::Trailhead.path().to_path_buf(),
        },
    );
    tell(outbox, FromDaemon::Hosted(Vec::new()));
}

/// What a window reads, from the demo project.
fn read_for(outbox: &Sender<FromDaemon>, request: ActId, read: Read) {
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
                    for piece in crate::ipc::chunks(rest).filter(|piece| !piece.is_empty()) {
                        tell(
                            outbox,
                            FromDaemon::Lines {
                                request,
                                text: piece.to_owned(),
                            },
                        );
                    }
                    Ok(u64::try_from(text.len()).unwrap_or(u64::MAX))
                }
                None => Err("not in the demo".to_owned()),
            };
            Answer::Transcript { path, read }
        }
        Read::WorktreeOf { .. } | Read::LinkedWorktree { .. } => Answer::Worktree(None),
        Read::TypedFolder(typed) => Answer::Folder(demo_folder(Path::new(typed.trim()))),
        Read::Folder(path) => Answer::Folder(demo_folder(&path)),
        // The demo keeps nothing between its runs: a session shown before isn't there.
        Read::Saved(_) => Answer::Saved(crate::sessions::Saved::Nowhere),
    };
    tell(outbox, FromDaemon::Answer { request, answer });
}

/// One of the demo's folders, or why a path isn't one.
fn demo_folder(path: &Path) -> Result<std::path::PathBuf, String> {
    demo::projects()
        .into_iter()
        .map(|project| project.path)
        .find(|project| project == path)
        .ok_or_else(|| format!("{} isn't one of the demo's folders", path.display()))
}

fn tell(outbox: &Sender<FromDaemon>, message: FromDaemon) {
    if outbox.try_send(message).is_err() {
        tracing::debug!("the window is gone");
    }
}

/// What the desktop is asked, answered as the demo can.
fn acted(outbox: &Sender<FromDaemon>, request: ActId, act: Act) {
    let message = match act {
        Act::CheckTrust { folder } => match demo::untrusted(&folder) {
            Some(runs) => FromDaemon::Trusted {
                request,
                accepted: false,
                runs,
            },
            None => FromDaemon::Trusted {
                request,
                accepted: true,
                runs: Vec::new(),
            },
        },
        Act::PickFolder => FromDaemon::Picked {
            request,
            folder: None,
        },
        Act::PasteImage => FromDaemon::Pasted {
            request,
            piece: postcard::to_stdvec(&Err::<crate::decode::Picture, _>(PasteFailure::NoImage))
                .unwrap_or_default(),
            last: true,
        },
        Act::OpenIn { .. }
        | Act::OpenUrl(_)
        | Act::Notify { .. }
        | Act::FocusWindowOf { .. }
        | Act::StopSession(_) => FromDaemon::Done {
            request,
            result: Err(ActFailure::NotHere),
        },
    };
    tell(outbox, message);
}
