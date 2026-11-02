//! The sidebar's list of sessions: those running now, in any folder and this window's own
//! among them, then this folder's earlier ones, each part under a heading. A running session is
//! listed once, as running, however it runs; the filter narrows both parts.

use std::path::Path;
use std::time::SystemTime;

use crate::external::wire::SessionId;
use crate::sessions::{self, LiveSession, LiveStatus, SessionInfo};

/// The session this window's own process runs.
#[derive(Debug)]
pub(crate) struct Own<'a> {
    pub(crate) id: &'a SessionId,
    pub(crate) title: &'a str,
    /// The name other sessions message it by.
    pub(crate) name: String,
    pub(crate) state: LiveStatus,
    pub(crate) worktree: Option<&'a str>,
    /// Its subagents running now.
    pub(crate) agents: usize,
}

/// What the list is made from.
#[derive(Debug)]
pub(crate) struct Listing<'a> {
    pub(crate) own: Option<Own<'a>>,
    /// What other Claude Code processes run, in any folder.
    pub(crate) live: &'a [LiveSession],
    /// This folder's saved sessions, the newest first.
    pub(crate) saved: &'a [SessionInfo],
    /// The session the window shows: its own, or one it follows.
    pub(crate) shown: Option<&'a SessionId>,
    pub(crate) folder: &'a Path,
    pub(crate) filter: &'a str,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Row {
    /// Over the running sessions: how many are listed, and how many of those wait for their
    /// user.
    RunningHeading {
        running: usize,
        waiting: usize,
    },
    /// Over this folder's earlier sessions.
    EarlierHeading {
        folder: String,
    },
    Running(Running),
    Earlier(Earlier),
}

/// A session some process runs now: this window's, another window's, a terminal's.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Running {
    pub(crate) id: SessionId,
    pub(crate) title: String,
    /// The name other sessions message it by.
    pub(crate) name: String,
    /// It runs in this folder.
    pub(crate) here: bool,
    /// The folder it runs in, when that is another and its name doesn't already say so.
    pub(crate) elsewhere: Option<String>,
    pub(crate) state: LiveStatus,
    pub(crate) worktree: Option<String>,
    pub(crate) agents: usize,
    pub(crate) shown: bool,
}

/// A session of this folder that no process runs: continued when opened.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Earlier {
    pub(crate) id: SessionId,
    pub(crate) title: String,
    pub(crate) modified: SystemTime,
    pub(crate) worktree: Option<String>,
    pub(crate) shown: bool,
}

/// The rows, in order: the running sessions (those waiting for their user first, then the
/// others, this folder's before others', by title), then the earlier ones as saved. A part with
/// nothing to list has no heading.
pub(crate) fn rows(listing: &Listing<'_>) -> Vec<Row> {
    let own_id = listing.own.as_ref().map(|own| own.id);
    let mut running: Vec<Running> = listing
        .own
        .iter()
        .map(|own| Running {
            id: own.id.clone(),
            title: own.title.to_owned(),
            name: own.name.clone(),
            here: true,
            elsewhere: None,
            state: own.state,
            worktree: own.worktree.map(str::to_owned),
            agents: own.agents,
            shown: listing.shown == Some(own.id),
        })
        .collect();
    running.extend(
        listing
            .live
            .iter()
            .filter(|session| Some(&session.id) != own_id)
            .map(|session| Running {
                id: session.id.clone(),
                title: session
                    .title
                    .clone()
                    .or_else(|| {
                        listing
                            .saved
                            .iter()
                            .find(|saved| saved.id == session.id)
                            .map(|saved| saved.title.clone())
                    })
                    .unwrap_or_else(|| session.name.clone()),
                name: session.name.clone(),
                here: session.cwd == listing.folder,
                elsewhere: elsewhere(&session.name, &session.cwd, listing.folder),
                state: session.status,
                worktree: session
                    .worktree
                    .as_ref()
                    .map(|worktree| worktree.name.clone()),
                agents: 0,
                shown: listing.shown == Some(&session.id),
            }),
    );
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
        })
        .collect();
    running.retain(|row| {
        matches(
            listing.filter,
            [
                row.title.as_str(),
                row.name.as_str(),
                row.elsewhere.as_deref().unwrap_or_default(),
            ],
        )
    });
    // Only waiting moves a session up: one that starts or ends a turn keeps its place, so the
    // rows don't move under the pointer every few seconds.
    running.sort_by_key(|row| {
        (
            row.state != LiveStatus::Waiting,
            !row.here,
            row.title.to_lowercase(),
        )
    });
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

/// The folder a session runs in, to show beside its name: none for this folder, nor where its
/// name starts with the folder's (as Claude Code names sessions: `claude-but-sane-6e`).
fn elsewhere(name: &str, cwd: &Path, folder: &Path) -> Option<String> {
    if cwd == folder {
        return None;
    }
    let shown = sessions::folder_name(cwd);
    let plain = |text: &str| -> String {
        text.chars()
            .filter(char::is_ascii_alphanumeric)
            .map(|character| character.to_ascii_lowercase())
            .collect()
    };
    (!plain(name).starts_with(&plain(&shown))).then_some(shown)
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
            id: id(number),
            title: title.to_owned(),
            modified: SystemTime::UNIX_EPOCH + Duration::from_secs(u64::from(number)),
            worktree: None,
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

    #[test]
    fn those_waiting_for_their_user_come_first_then_this_folders() {
        let live = [
            titled(live(1, "dotfiles-9c", "/code/dotfiles", Busy), "niri"),
            titled(live(2, "beep-2a", "/code/beep", Idle), "Threads"),
            titled(live(3, "inneneinrichtung-4f", "/x/inn", Waiting), "Room"),
            titled(live(4, "beep-7d", "/code/beep", Busy), "Attachments"),
        ];
        assert_eq!(
            titles(&rows(&listing(None, &live, &[], ""))),
            [
                "# running 4, 1 waiting",
                "> Room",
                "> Attachments",
                "> Threads",
                "> niri"
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

    #[test]
    fn another_folder_is_named_unless_the_session_name_says_it() {
        let beep = Path::new("/code/beep");
        let dotfiles = Path::new("/code/dotfiles");
        let edualc = Path::new("/code/claude_but_sane");
        let named = Some("dotfiles".to_owned());
        assert_eq!(elsewhere("proud-moth-dotfiles-27", dotfiles, beep), named);
        assert_eq!(elsewhere("claude-but-sane-6e", edualc, beep), None);
        assert_eq!(elsewhere("proud-moth-beep-27", beep, beep), None);
    }

    #[test]
    fn the_filter_narrows_both_parts_and_drops_empty_headings() {
        let saved = [saved(1, "Attach"), saved(2, "Threads"), saved(3, "Running")];
        let live = [
            titled(live(3, "beep-2a", "/code/beep", Idle), "Running"),
            titled(live(4, "proud-moth-27", "/x/dotfiles", Busy), "niri"),
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
        // Its folder's name finds a session elsewhere.
        assert_eq!(
            titles(&rows(&listing(None, &live, &saved, "dotfiles"))),
            ["# running 1, 0 waiting", "> niri"]
        );
    }
}
