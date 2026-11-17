//! A project's list of sessions: those running now in it, this window's own among them, then its
//! earlier ones, each part under a heading. A running session is listed once, as running,
//! however it runs; the filter narrows both parts.

use std::path::Path;
use web_time::SystemTime;

use crate::claude::{Backend, Program, SignInWith};
use crate::external::wire::SessionId;
use crate::ipc::{Hosted, HostedState};
use crate::sessions::{self, LiveSession, LiveStatus, SessionInfo};

/// The session this window's own process runs.
#[derive(Debug)]
pub struct Own<'a> {
    pub id: &'a SessionId,
    pub title: &'a str,
    /// The name other sessions message it by.
    pub name: String,
    pub state: LiveStatus,
    pub worktree: Option<&'a str>,
    /// Its subagents running now.
    pub agents: usize,
    pub program: Program,
}

/// What the list is made from.
#[derive(Debug)]
pub struct Listing<'a> {
    pub own: Option<Own<'a>>,
    /// What other Claude Code processes run, in any folder.
    pub live: &'a [LiveSession],
    /// What edualc's daemon runs: it alone knows what each signed in with. Claude Code's are
    /// among `live` too; agy's adapter writes no record there.
    pub hosted: &'a [Hosted],
    /// The project's saved sessions, the newest first.
    pub saved: &'a [SessionInfo],
    /// The session the window shows: its own, or one it follows.
    pub shown: Option<&'a SessionId>,
    /// The project's folder.
    pub folder: &'a Path,
    pub filter: &'a str,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Row {
    /// Over the running sessions: how many are listed, and how many of those wait for their
    /// user.
    RunningHeading {
        running: usize,
        waiting: usize,
    },
    /// Over the project's earlier sessions.
    EarlierHeading {
        folder: String,
    },
    Running(Running),
    Earlier(Earlier),
}

/// A session some process runs now: this window's, another window's, a terminal's, or edualc's
/// daemon's with no window showing it.
#[derive(Debug, PartialEq, Eq)]
pub struct Running {
    pub id: SessionId,
    pub title: String,
    /// The name other sessions message it by.
    pub name: String,
    pub state: LiveStatus,
    pub worktree: Option<String>,
    pub agents: usize,
    pub shown: bool,
    pub program: Program,
}

impl Running {
    /// The name to show beside its title: none where the title is that name, as for a session
    /// that has no title yet, or one the user titled as it is named.
    pub fn name_beside_title(&self) -> Option<&str> {
        (self.title != self.name).then_some(self.name.as_str())
    }
}

/// A session of the project that no process runs: continued when opened.
#[derive(Debug, PartialEq, Eq)]
pub struct Earlier {
    pub id: SessionId,
    pub title: String,
    pub modified: SystemTime,
    pub worktree: Option<String>,
    pub shown: bool,
    /// Its backend; a saved transcript doesn't say what Claude Code signed in with.
    pub backend: Backend,
}

/// The rows, in order: the project's running sessions (those waiting for their user first, then
/// the others, by title), then its earlier ones as saved. A part with nothing to list has no
/// heading.
pub fn rows(listing: &Listing<'_>) -> Vec<Row> {
    let own_id = listing.own.as_ref().map(|own| own.id);
    let saved_title = |id: &SessionId| {
        listing
            .saved
            .iter()
            .find(|saved| &saved.id == id)
            .map(|saved| saved.title.clone())
    };
    // A session in one of the project's worktrees runs in a folder inside it, or in one its
    // saved sessions say it moved to.
    let in_project = |cwd: &Path, id: &SessionId| {
        cwd.starts_with(listing.folder) || listing.saved.iter().any(|saved| &saved.id == id)
    };
    let mut running: Vec<Running> = listing
        .own
        .iter()
        .map(|own| Running {
            id: own.id.clone(),
            title: own.title.to_owned(),
            name: own.name.clone(),
            state: own.state,
            worktree: own.worktree.map(str::to_owned),
            agents: own.agents,
            shown: listing.shown == Some(own.id),
            program: own.program,
        })
        .collect();
    running.extend(
        listing
            .live
            .iter()
            .filter(|session| Some(&session.id) != own_id && in_project(&session.cwd, &session.id))
            .map(|session| Running {
                id: session.id.clone(),
                title: session
                    .title
                    .clone()
                    .or_else(|| saved_title(&session.id))
                    .unwrap_or_else(|| session.name.clone()),
                name: session.name.clone(),
                state: session.status,
                worktree: session
                    .worktree
                    .as_ref()
                    .map(|worktree| worktree.name.clone()),
                agents: session.agents,
                shown: listing.shown == Some(&session.id),
                program: Program::of(
                    session.backend,
                    listing
                        .hosted
                        .iter()
                        .find(|hosted| hosted.session == session.id)
                        .map_or(SignInWith::Account, |hosted| hosted.sign_in),
                ),
            }),
    );
    // What the daemon runs that no record lists (agy's sessions), in another window or in none.
    let unlisted: Vec<Running> = listing
        .hosted
        .iter()
        .filter(|hosted| {
            let id = &hosted.session;
            let listed = Some(id) == own_id || running.iter().any(|row| &row.id == id);
            !listed && in_project(&hosted.cwd, id)
        })
        .map(|hosted| {
            let id = &hosted.session;
            let saved = listing.saved.iter().find(|saved| &saved.id == id);
            let name = sessions::MessagingName::of(id, &hosted.cwd).to_string();
            Running {
                id: id.clone(),
                title: saved.map_or_else(|| name.clone(), |saved| saved.title.clone()),
                name,
                state: match hosted.state {
                    HostedState::Idle => LiveStatus::Idle,
                    HostedState::Busy => LiveStatus::Busy,
                    HostedState::Waiting { .. } => LiveStatus::Waiting,
                },
                worktree: saved
                    .and_then(|saved| saved.worktree.as_ref())
                    .map(|worktree| worktree.name.clone()),
                agents: 0,
                shown: listing.shown == Some(id),
                program: Program::of(
                    saved.map_or(Backend::Claude, |saved| saved.backend),
                    hosted.sign_in,
                ),
            }
        })
        .collect();
    running.extend(unlisted);
    // Running, a session isn't among the earlier ones, whether or not the filter shows it.
    let earlier: Vec<Earlier> = listing
        .saved
        .iter()
        .filter(|saved| !running.iter().any(|row| row.id == saved.id))
        .filter(|saved| matches(listing.filter, [saved.title.as_str()]))
        .map(|saved| Earlier {
            id: saved.id.clone(),
            title: saved.title.clone(),
            modified: saved.modified,
            worktree: saved
                .worktree
                .as_ref()
                .map(|worktree| worktree.name.clone()),
            shown: listing.shown == Some(&saved.id),
            backend: saved.backend,
        })
        .collect();
    running.retain(|row| matches(listing.filter, [row.title.as_str(), row.name.as_str()]));
    // Only waiting moves a session up: one that starts or ends a turn keeps its place, so the
    // rows don't move under the pointer every few seconds.
    running.sort_by_key(|row| (row.state != LiveStatus::Waiting, row.title.to_lowercase()));
    let mut rows = Vec::with_capacity(running.len() + earlier.len() + 2);
    if !running.is_empty() {
        rows.push(Row::RunningHeading {
            running: running.len(),
            waiting: running
                .iter()
                .filter(|row| row.state == LiveStatus::Waiting)
                .count(),
        });
        rows.extend(running.into_iter().map(Row::Running));
    }
    if !earlier.is_empty() {
        rows.push(Row::EarlierHeading {
            folder: sessions::folder_name(listing.folder),
        });
        rows.extend(earlier.into_iter().map(Row::Earlier));
    }
    rows
}

/// Whether any of `texts` holds `filter`, letter case aside; an empty filter lets all through.
fn matches<'a>(filter: &str, texts: impl IntoIterator<Item = &'a str>) -> bool {
    let filter = filter.trim().to_lowercase();
    filter.is_empty()
        || texts
            .into_iter()
            .any(|text| text.to_lowercase().contains(&filter))
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::time::Duration;

    use super::*;
    use crate::sessions::LiveKind;
    use crate::sessions::LiveStatus::{Busy, Idle, Waiting};

    fn id(number: u8) -> SessionId {
        SessionId::try_from(format!("00000000-0000-4000-8000-0000000000{number:02}"))
            .expect("a session id")
    }

    fn saved(number: u8, title: &str) -> SessionInfo {
        SessionInfo {
            backend: crate::claude::Backend::Claude,
            id: id(number),
            title: title.to_owned(),
            modified: SystemTime::UNIX_EPOCH + Duration::from_secs(u64::from(number)),
            worktree: None,
            last_said: None,
        }
    }

    /// A session another process runs, with no title yet.
    fn live(number: u8, name: &str, cwd: &str, status: LiveStatus) -> LiveSession {
        LiveSession {
            pid: u32::from(number),
            started: "1".to_owned(),
            id: id(number),
            cwd: PathBuf::from(cwd),
            name: name.to_owned(),
            title: None,
            kind: LiveKind::Interactive,
            status,
            worktree: None,
            agents: 0,
            backend: Backend::Claude,
        }
    }

    fn titled(session: LiveSession, title: &str) -> LiveSession {
        LiveSession {
            title: Some(title.to_owned()),
            ..session
        }
    }

    fn listing<'a>(
        own: Option<Own<'a>>,
        live: &'a [LiveSession],
        saved: &'a [SessionInfo],
        filter: &'a str,
    ) -> Listing<'a> {
        Listing {
            own,
            live,
            hosted: &[],
            saved,
            shown: None,
            folder: Path::new("/code/beep"),
            filter,
        }
    }

    fn titles(rows: &[Row]) -> Vec<String> {
        rows.iter()
            .map(|row| match row {
                Row::RunningHeading { running, waiting } => {
                    format!("# running {running}, {waiting} waiting")
                }
                Row::EarlierHeading { folder } => format!("# earlier in {folder}"),
                Row::Running(row) => format!("> {}", row.title),
                Row::Earlier(row) => row.title.clone(),
            })
            .collect()
    }

    /// The user's complaint: running and earlier sessions in one list, told apart only by a dot.
    #[test]
    fn running_sessions_are_listed_apart_from_earlier_ones_and_once() {
        let saved = [saved(1, "Attach"), saved(2, "React"), saved(3, "Thread")];
        let live = [titled(live(2, "beep-2a", "/code/beep", Idle), "React")];
        assert_eq!(
            titles(&rows(&listing(None, &live, &saved, ""))),
            [
                "# running 1, 0 waiting",
                "> React",
                "# earlier in beep",
                "Attach",
                "Thread"
            ]
        );
    }

    #[test]
    fn the_windows_own_session_is_running_with_its_state() {
        let saved = [saved(1, "Attachments"), saved(2, "Reactions")];
        let own_id = id(1);
        let own = Own {
            id: &own_id,
            title: "Attachments",
            name: "proud-moth-beep-27".to_owned(),
            state: Busy,
            worktree: None,
            agents: 2,
            program: Program::Claude,
        };
        let rows = rows(&Listing {
            shown: Some(&own_id),
            ..listing(Some(own), &[], &saved, "")
        });
        let Some(Row::Running(row)) = rows.get(1) else {
            panic!("the own session runs: {rows:?}");
        };
        assert_eq!((row.state, row.agents, row.shown), (Busy, 2, true));
        assert_eq!(
            titles(&rows),
            [
                "# running 1, 0 waiting",
                "> Attachments",
                "# earlier in beep",
                "Reactions"
            ]
        );
    }

    /// The review's finding: the project page listed every project's running sessions.
    #[test]
    fn only_the_projects_running_sessions_are_listed() {
        let saved = [saved(5, "Moved to a worktree")];
        let live = [
            titled(live(1, "dotfiles-9c", "/code/dotfiles", Waiting), "niri"),
            titled(live(2, "beep-2a", "/code/beep", Idle), "Threads"),
            titled(live(3, "beep-old-4f", "/code/beep-old", Busy), "Old"),
            titled(
                live(4, "beep-7d", "/code/beep/.claude/worktrees/w", Busy),
                "Attachments",
            ),
            live(5, "beep-1e", "/elsewhere/w", Idle),
        ];
        assert_eq!(
            titles(&rows(&listing(None, &live, &saved, ""))),
            [
                "# running 3, 0 waiting",
                "> Attachments",
                "> Moved to a worktree",
                "> Threads",
            ]
        );
    }

    #[test]
    fn those_waiting_for_their_user_come_first() {
        let live = [
            titled(live(1, "beep-1c", "/code/beep", Busy), "Attachments"),
            titled(live(2, "beep-2a", "/code/beep", Waiting), "Threads"),
        ];
        assert_eq!(
            titles(&rows(&listing(None, &live, &[], ""))),
            ["# running 2, 1 waiting", "> Threads", "> Attachments"]
        );
    }

    /// A session edualc's daemon runs that no record lists, as agy's, is listed from what the
    /// daemon says, in another window or in none.
    #[test]
    fn a_session_the_daemon_runs_is_running() {
        let saved = [
            saved(1, "Left running"),
            saved(2, "In another window"),
            saved(5, "Not running"),
        ];
        let hosted = |number: u8, cwd: &str, attached: bool, state| Hosted {
            id: crate::ipc::HostedId::default(),
            session: id(number),
            cwd: PathBuf::from(cwd),
            sign_in: SignInWith::Account,
            attached,
            state,
        };
        // A new one, with no transcript yet, that Claude Code's record lists too: once.
        let live = [live(4, "keen-owl-beep-04", "/code/beep", Idle)];
        let hosted = [
            hosted(
                1,
                "/code/beep",
                false,
                crate::ipc::HostedState::Waiting {
                    question: "Run cargo test?".to_owned(),
                    since: SystemTime::UNIX_EPOCH,
                },
            ),
            hosted(2, "/code/beep", true, crate::ipc::HostedState::Busy),
            hosted(3, "/code/dotfiles", false, crate::ipc::HostedState::Idle),
            hosted(4, "/code/beep", false, crate::ipc::HostedState::Idle),
        ];
        assert_eq!(
            titles(&rows(&Listing {
                hosted: &hosted,
                ..listing(None, &live, &saved, "")
            })),
            [
                "# running 3, 1 waiting",
                "> Left running",
                "> In another window",
                "> keen-owl-beep-04",
                "# earlier in beep",
                "Not running"
            ]
        );
    }

    /// A session that starts or ends a turn keeps its place; only waiting moves it up.
    #[test]
    fn working_or_idle_a_session_keeps_its_place() {
        let order = |states: [LiveStatus; 2]| {
            let live = [
                titled(live(1, "beep-2a", "/code/beep", states[0]), "Attachments"),
                titled(live(2, "beep-7d", "/code/beep", states[1]), "Threads"),
            ];
            titles(&rows(&listing(None, &live, &[], "")))
        };
        assert_eq!(order([Idle, Busy]), order([Busy, Idle]));
        let waiting_first = order([Idle, Waiting]);
        assert_eq!(waiting_first.get(1).map(String::as_str), Some("> Threads"));
    }

    #[test]
    fn a_session_without_a_title_yet_goes_by_its_saved_title_or_its_name() {
        let saved = [saved(1, "From the folder's list")];
        let live = [
            live(1, "beep-2a", "/code/beep", Idle),
            live(2, "beep-7d", "/code/beep", Idle),
        ];
        assert_eq!(
            titles(&rows(&listing(None, &live, &saved, ""))),
            [
                "# running 2, 0 waiting",
                "> beep-7d",
                "> From the folder's list"
            ]
        );
    }

    /// A running session is marked with what it runs: agy as its transcript's place says, and
    /// Claude Code with the API key as the daemon that started it says; others are on the
    /// user's own sign-in.
    #[test]
    fn a_running_session_is_marked_with_its_program() {
        let live = [
            LiveSession {
                backend: Backend::Agy,
                ..live(1, "ripe-lynx-beep-3", "/code/beep", Idle)
            },
            live(2, "keyed-owl-beep-4", "/code/beep", Idle),
            live(3, "plain-elk-beep-5", "/code/beep", Idle),
        ];
        let hosted = [Hosted {
            id: crate::ipc::HostedId::default(),
            session: id(2),
            cwd: PathBuf::from("/code/beep"),
            sign_in: SignInWith::ApiKey,
            attached: false,
            state: crate::ipc::HostedState::Idle,
        }];
        let rows = rows(&Listing {
            hosted: &hosted,
            ..listing(None, &live, &[], "")
        });
        let programs: Vec<(SessionId, Program)> = rows
            .iter()
            .filter_map(|row| match row {
                Row::Running(running) => Some((running.id.clone(), running.program)),
                Row::RunningHeading { .. } | Row::EarlierHeading { .. } | Row::Earlier(_) => None,
            })
            .collect();
        assert!(programs.contains(&(id(1), Program::Agy)));
        assert!(programs.contains(&(id(2), Program::ClaudeApiKey)));
        assert!(programs.contains(&(id(3), Program::Claude)));
    }

    /// A session that goes by its name has it once, as its title; one with a title of its own
    /// shows its name beside it, to message it by.
    #[test]
    fn a_name_shows_once() {
        let saved = [saved(1, "From the folder's list")];
        let live = [
            live(1, "beep-2a", "/code/beep", Idle),
            live(2, "beep-7d", "/code/beep", Idle),
        ];
        let beside: Vec<Option<String>> = rows(&listing(None, &live, &saved, ""))
            .iter()
            .filter_map(|row| match row {
                Row::Running(running) => Some(running.name_beside_title().map(str::to_owned)),
                Row::RunningHeading { .. } | Row::EarlierHeading { .. } | Row::Earlier(_) => None,
            })
            .collect();
        assert_eq!(beside, [None, Some("beep-2a".to_owned())]);
    }

    #[test]
    fn the_filter_narrows_both_parts_and_drops_empty_headings() {
        let saved = [saved(1, "Attach"), saved(2, "Threads"), saved(3, "Running")];
        let live = [
            titled(live(3, "beep-2a", "/code/beep", Idle), "Running"),
            titled(live(4, "proud-moth-beep-27", "/code/beep", Busy), "niri"),
        ];
        assert_eq!(
            titles(&rows(&listing(None, &live, &saved, "THREAD"))),
            ["# earlier in beep", "Threads"]
        );
        // A running session filtered out doesn't come back among the earlier ones.
        assert_eq!(
            titles(&rows(&listing(None, &live, &saved, "running"))),
            ["# running 1, 0 waiting", "> Running"]
        );
        // The name to message it by finds a session.
        assert_eq!(
            titles(&rows(&listing(None, &live, &saved, "moth"))),
            ["# running 1, 0 waiting", "> niri"]
        );
    }
}
