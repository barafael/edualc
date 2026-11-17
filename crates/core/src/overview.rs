//! Start-page cards and the "Needs you" list, joined from known projects, running terminal
//! sessions, and sessions the daemon hosts.

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::path::{Path, PathBuf};

use web_time::SystemTime;

use crate::claude::{Backend, Program, SignInWith};
use crate::external::wire::SessionId;
use crate::ipc::{Hosted, HostedId, HostedState, ProjectOverview};
use crate::sessions::{self, LiveSession, LiveStatus, SessionInfo};

const CARD_SESSIONS: usize = 5;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Card {
    pub path: PathBuf,
    pub name: String,
    pub modified: SystemTime,
    pub needs_you: usize,
    pub running: usize,
    pub last_said: Option<String>,
    pub sessions: Vec<CardSession>,
}

/// Which session an item opens.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionRef {
    /// One edualc's daemon runs.
    Hosted {
        hosted: HostedId,
        session: SessionId,
    },
    /// One another program runs, or a saved one.
    Session(SessionId),
}

impl SessionRef {
    /// Claude Code's id for it.
    pub fn session(&self) -> &SessionId {
        match self {
            Self::Hosted { session, .. } | Self::Session(session) => session,
        }
    }

    pub fn hosted(&self) -> Option<HostedId> {
        match self {
            Self::Hosted { hosted, .. } => Some(*hosted),
            Self::Session(_) => None,
        }
    }

    fn of(hosted: &Hosted) -> Self {
        Self::Hosted {
            hosted: hosted.id,
            session: hosted.session.clone(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CardSession {
    pub opens: SessionRef,
    pub title: String,
    pub state: Option<LiveStatus>,
    pub modified: SystemTime,
    pub last_said: Option<String>,
    pub program: Program,
    pub worktree: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WaitingItem {
    pub opens: SessionRef,
    pub project_path: PathBuf,
    pub project_name: String,
    pub session_title: String,
    pub question: String,
    pub since: Option<SystemTime>,
    pub program: Program,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Built {
    pub cards: Vec<Card>,
    pub waiting: Vec<WaitingItem>,
}

pub fn cards(
    projects: &[ProjectOverview],
    live: &[LiveSession],
    hosted: &[Hosted],
    home: Option<&Path>,
) -> Vec<Card> {
    build(projects, live, hosted, home).cards
}

pub fn build(
    projects: &[ProjectOverview],
    live: &[LiveSession],
    hosted: &[Hosted],
    home: Option<&Path>,
) -> Built {
    let entries = collect_projects(projects, live, hosted, home);
    let paths: Vec<&Path> = entries.iter().map(|entry| entry.path.as_path()).collect();
    let names = distinct_names(&paths);

    let mut cards = Vec::with_capacity(entries.len());
    let mut waiting = Vec::new();
    let mut matched_hosted: HashSet<HostedId> = HashSet::new();
    let mut matched_live: HashSet<SessionId> = HashSet::new();

    for (entry, name) in entries.into_iter().zip(names) {
        let card = build_card(
            &entry,
            &name,
            live,
            hosted,
            &mut matched_hosted,
            &mut matched_live,
            &mut waiting,
        );
        cards.push(card);
    }

    // Waiting sessions whose folder wasn't turned into a card (for instance `$HOME`) keep
    // showing under "Needs you" so nothing waiting for an answer goes unseen.
    for item in hosted {
        if matched_hosted.contains(&item.id) {
            continue;
        }
        if let HostedState::Waiting { question, since } = &item.state {
            // Claude Code's record of the same process isn't listed again below.
            matched_live.insert(item.session.clone());
            let matching_live = live.iter().find(|live| live.id == item.session);
            let session_title = matching_live.map_or_else(
                || "Running session".to_owned(),
                |live| live.title.clone().unwrap_or_else(|| live.name.clone()),
            );
            let backend = matching_live.map_or(Backend::Claude, |live| live.backend);
            waiting.push(WaitingItem {
                opens: SessionRef::of(item),
                project_path: item.cwd.clone(),
                project_name: sessions::folder_name(&item.cwd),
                session_title,
                question: question.clone(),
                since: Some(*since),
                program: Program::of(backend, item.sign_in),
            });
        }
    }
    for item in live {
        if matched_live.contains(&item.id) {
            continue;
        }
        if item.status == LiveStatus::Waiting {
            waiting.push(WaitingItem {
                opens: SessionRef::Session(item.id.clone()),
                project_path: item.cwd.clone(),
                project_name: sessions::folder_name(&item.cwd),
                session_title: item.title.clone().unwrap_or_else(|| item.name.clone()),
                question: "Waiting for your answer".to_owned(),
                since: None,
                program: Program::of(item.backend, SignInWith::Account),
            });
        }
    }

    cards.sort_by(|left, right| {
        (
            std::cmp::Reverse(left.needs_you > 0),
            std::cmp::Reverse(left.needs_you),
            std::cmp::Reverse(left.running > 0),
            std::cmp::Reverse(left.modified),
            &left.path,
        )
            .cmp(&(
                std::cmp::Reverse(right.needs_you > 0),
                std::cmp::Reverse(right.needs_you),
                std::cmp::Reverse(right.running > 0),
                std::cmp::Reverse(right.modified),
                &right.path,
            ))
    });

    waiting.sort_by(|left, right| {
        let time_key = |since: Option<SystemTime>| match since {
            Some(at) => (0_u8, at),
            None => (1_u8, SystemTime::UNIX_EPOCH),
        };
        (
            time_key(left.since),
            &left.project_name,
            &left.session_title,
        )
            .cmp(&(
                time_key(right.since),
                &right.project_name,
                &right.session_title,
            ))
    });

    Built { cards, waiting }
}

fn collect_projects(
    projects: &[ProjectOverview],
    live: &[LiveSession],
    hosted: &[Hosted],
    home: Option<&Path>,
) -> Vec<ProjectOverview> {
    let mut seen: BTreeSet<PathBuf> = BTreeSet::new();
    let mut entries = Vec::new();
    for project in projects {
        if home == Some(project.path.as_path()) {
            continue;
        }
        if seen.insert(project.path.clone()) {
            entries.push(project.clone());
        }
    }
    let now = SystemTime::now();
    for item in hosted {
        if home == Some(item.cwd.as_path()) || matches_known_project(projects, item) {
            continue;
        }
        if seen.insert(item.cwd.clone()) {
            entries.push(ProjectOverview {
                path: item.cwd.clone(),
                modified: now,
                sessions: Vec::new(),
            });
        }
    }
    for item in live {
        if home == Some(item.cwd.as_path()) {
            continue;
        }
        if seen.insert(item.cwd.clone()) {
            entries.push(ProjectOverview {
                path: item.cwd.clone(),
                modified: now,
                sessions: Vec::new(),
            });
        }
    }
    entries
}

fn matches_known_project(projects: &[ProjectOverview], hosted: &Hosted) -> bool {
    projects.iter().any(|project| {
        project
            .sessions
            .iter()
            .any(|saved| saved.id == hosted.session)
    })
}

fn build_card(
    project: &ProjectOverview,
    project_name: &str,
    live: &[LiveSession],
    hosted: &[Hosted],
    matched_hosted: &mut HashSet<HostedId>,
    matched_live: &mut HashSet<SessionId>,
    waiting: &mut Vec<WaitingItem>,
) -> Card {
    let saved_by_id: BTreeMap<&SessionId, &SessionInfo> = project
        .sessions
        .iter()
        .map(|session| (&session.id, session))
        .collect();
    let live_by_id: BTreeMap<&SessionId, &LiveSession> =
        live.iter().map(|session| (&session.id, session)).collect();

    let mut sessions = Vec::new();
    let mut active_ids: HashSet<SessionId> = HashSet::new();
    let mut needs_you = 0_usize;
    let mut running = 0_usize;

    let project_hosted: Vec<&Hosted> = hosted
        .iter()
        .filter(|item| item.cwd == project.path || saved_by_id.contains_key(&item.session))
        .collect();

    for item in project_hosted {
        matched_hosted.insert(item.id);
        running += 1;
        let matching_live = live_by_id.get(&item.session).copied();
        let matching_saved = saved_by_id.get(&item.session).copied();
        // Claude Code's record of the same process isn't listed again below.
        active_ids.insert(item.session.clone());
        matched_live.insert(item.session.clone());
        let state = match &item.state {
            HostedState::Waiting { .. } => LiveStatus::Waiting,
            HostedState::Busy => LiveStatus::Busy,
            HostedState::Idle => matching_live.map_or(LiveStatus::Idle, |live| live.status),
        };
        let title = matching_live
            .and_then(|live| live.title.clone())
            .or_else(|| matching_saved.map(|saved| saved.title.clone()))
            .or_else(|| matching_live.map(|live| live.name.clone()))
            .unwrap_or_else(|| "Running session".to_owned());
        let modified = matching_saved.map_or(project.modified, |saved| saved.modified);
        let backend = matching_saved
            .map(|saved| saved.backend)
            .or_else(|| matching_live.map(|live| live.backend))
            .unwrap_or(Backend::Claude);
        let program = Program::of(backend, item.sign_in);
        let worktree = matching_live
            .and_then(|live| live.worktree.as_ref().map(|wt| wt.name.clone()))
            .or_else(|| {
                matching_saved.and_then(|saved| saved.worktree.as_ref().map(|wt| wt.name.clone()))
            });
        let last_said = matching_saved.and_then(|saved| saved.last_said.clone());

        if let HostedState::Waiting { question, since } = &item.state {
            needs_you += 1;
            waiting.push(WaitingItem {
                opens: SessionRef::of(item),
                project_path: project.path.clone(),
                project_name: project_name.to_owned(),
                session_title: title.clone(),
                question: question.clone(),
                since: Some(*since),
                program,
            });
        } else if state == LiveStatus::Waiting {
            needs_you += 1;
            waiting.push(WaitingItem {
                opens: SessionRef::of(item),
                project_path: project.path.clone(),
                project_name: project_name.to_owned(),
                session_title: title.clone(),
                question: "Waiting for your answer".to_owned(),
                since: None,
                program,
            });
        }

        sessions.push(CardSession {
            opens: SessionRef::of(item),
            title,
            state: Some(state),
            modified,
            last_said,
            program,
            worktree,
        });
    }

    for item in live {
        if item.cwd != project.path || active_ids.contains(&item.id) {
            continue;
        }
        matched_live.insert(item.id.clone());
        active_ids.insert(item.id.clone());
        running += 1;
        let matching_saved = saved_by_id.get(&item.id).copied();
        let title = item
            .title
            .clone()
            .or_else(|| matching_saved.map(|saved| saved.title.clone()))
            .unwrap_or_else(|| item.name.clone());
        let program = Program::of(item.backend, SignInWith::Account);
        let worktree = item
            .worktree
            .as_ref()
            .map(|wt| wt.name.clone())
            .or_else(|| {
                matching_saved.and_then(|saved| saved.worktree.as_ref().map(|wt| wt.name.clone()))
            });
        let last_said = matching_saved.and_then(|saved| saved.last_said.clone());
        let modified = matching_saved.map_or(project.modified, |saved| saved.modified);

        if item.status == LiveStatus::Waiting {
            needs_you += 1;
            waiting.push(WaitingItem {
                opens: SessionRef::Session(item.id.clone()),
                project_path: project.path.clone(),
                project_name: project_name.to_owned(),
                session_title: title.clone(),
                question: "Waiting for your answer".to_owned(),
                since: None,
                program,
            });
        }

        sessions.push(CardSession {
            opens: SessionRef::Session(item.id.clone()),
            title,
            state: Some(item.status),
            modified,
            last_said,
            program,
            worktree,
        });
    }

    for saved in &project.sessions {
        if active_ids.contains(&saved.id) {
            continue;
        }
        sessions.push(CardSession {
            opens: SessionRef::Session(saved.id.clone()),
            title: saved.title.clone(),
            state: None,
            modified: saved.modified,
            last_said: saved.last_said.clone(),
            program: Program::of(saved.backend, SignInWith::Account),
            worktree: saved.worktree.as_ref().map(|wt| wt.name.clone()),
        });
    }

    sessions.sort_by(|left, right| {
        (
            state_rank(left.state),
            std::cmp::Reverse(left.modified),
            &left.title,
        )
            .cmp(&(
                state_rank(right.state),
                std::cmp::Reverse(right.modified),
                &right.title,
            ))
    });

    let last_said = project
        .sessions
        .iter()
        .filter_map(|saved| saved.last_said.as_ref().map(|said| (saved.modified, said)))
        .max_by_key(|(modified, _)| *modified)
        .map(|(_, said)| said.clone());

    let modified = project
        .sessions
        .iter()
        .map(|saved| saved.modified)
        .chain(std::iter::once(project.modified))
        .max()
        .unwrap_or(project.modified);

    sessions.truncate(CARD_SESSIONS);

    Card {
        path: project.path.clone(),
        name: project_name.to_owned(),
        modified,
        needs_you,
        running,
        last_said,
        sessions,
    }
}

fn state_rank(state: Option<LiveStatus>) -> u8 {
    match state {
        Some(LiveStatus::Waiting) => 0,
        Some(LiveStatus::Busy) => 1,
        Some(LiveStatus::Idle) => 2,
        Some(LiveStatus::Other) => 3,
        None => 4,
    }
}

/// Names each path by its folder's name; folders with the same name get the nearest parent that
/// tells them apart beside it: `project (ux)`, `project (work)`.
fn distinct_names(paths: &[&Path]) -> Vec<String> {
    let parts: Vec<Vec<String>> = paths
        .iter()
        .map(|path| {
            path.components()
                .filter_map(|component| match component {
                    std::path::Component::Normal(part) => Some(part.to_string_lossy().into_owned()),
                    std::path::Component::RootDir
                    | std::path::Component::Prefix(_)
                    | std::path::Component::CurDir
                    | std::path::Component::ParentDir => None,
                })
                .collect()
        })
        .collect();

    let mut depth: Vec<usize> = vec![1; paths.len()];
    loop {
        let names: Vec<String> = parts
            .iter()
            .zip(&depth)
            .map(|(segments, take)| trailing_name(segments, *take))
            .collect();
        let mut counts: BTreeMap<&str, usize> = BTreeMap::new();
        for name in &names {
            *counts.entry(name.as_str()).or_insert(0) += 1;
        }
        let mut widened = false;
        for (idx, name) in names.iter().enumerate() {
            let duplicate = counts.get(name.as_str()).is_some_and(|count| *count > 1);
            let available = parts.get(idx).map_or(0, Vec::len);
            let current = depth.get_mut(idx);
            if let Some(current) = current
                && duplicate
                && *current < available
            {
                *current += 1;
                widened = true;
            }
        }
        if !widened {
            return parts
                .iter()
                .zip(&depth)
                .map(|(segments, take)| told_apart(segments, *take))
                .collect();
        }
    }
}

fn trailing_name(segments: &[String], take: usize) -> String {
    let skip = segments.len().saturating_sub(take);
    let shown: Vec<&str> = segments.iter().skip(skip).map(String::as_str).collect();
    if shown.is_empty() {
        "/".to_owned()
    } else {
        shown.join("/")
    }
}

/// The folder's name, and the parent `take` components up from it beside it when that is what
/// tells it apart. A parent named by a UUID (a session's scratch folder) shows its first part.
fn told_apart(segments: &[String], take: usize) -> String {
    let Some(folder) = segments.last() else {
        return "/".to_owned();
    };
    let parent = take
        .checked_sub(1)
        .filter(|up| *up > 0)
        .and_then(|up| segments.len().checked_sub(up + 1))
        .and_then(|index| segments.get(index));
    match parent {
        Some(parent) => {
            let short = uuid_head(parent).unwrap_or(parent);
            format!("{folder} ({short})")
        }
        None => folder.clone(),
    }
}

/// The first group of a UUID's hex digits, if `text` is one.
fn uuid_head(text: &str) -> Option<&str> {
    uuid::Uuid::try_parse(text)
        .ok()
        .and_then(|_| text.split('-').next())
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    use crate::ipc::HostedId;
    use crate::sessions::LiveKind;

    fn session_id(number: u128) -> SessionId {
        SessionId::from(uuid::Uuid::from_u128(number))
    }

    fn time(seconds: u64) -> SystemTime {
        SystemTime::UNIX_EPOCH + Duration::from_secs(seconds)
    }

    fn saved(number: u128, title: &str, modified: u64, last_said: Option<&str>) -> SessionInfo {
        SessionInfo {
            backend: Backend::Claude,
            id: session_id(number),
            title: title.to_owned(),
            modified: time(modified),
            worktree: None,
            last_said: last_said.map(ToOwned::to_owned),
        }
    }

    fn project(path: &str, modified: u64, sessions: Vec<SessionInfo>) -> ProjectOverview {
        ProjectOverview {
            path: PathBuf::from(path),
            modified: time(modified),
            sessions,
        }
    }

    fn hosted_session(id_num: u64, session: u128, cwd: &str, state: HostedState) -> Hosted {
        let mut id = HostedId::default();
        for _ in 0..id_num {
            id = id.next();
        }
        Hosted {
            id,
            session: session_id(session),
            cwd: PathBuf::from(cwd),
            sign_in: SignInWith::Account,
            attached: false,
            state,
        }
    }

    fn live_session(number: u128, cwd: &str, status: LiveStatus) -> LiveSession {
        LiveSession {
            pid: u32::try_from(number).unwrap_or(1),
            started: "1".to_owned(),
            id: session_id(number),
            cwd: PathBuf::from(cwd),
            name: format!("session-{number}"),
            title: Some(format!("Title {number}")),
            kind: LiveKind::Interactive,
            status,
            worktree: None,
            agents: 0,
            backend: Backend::Claude,
        }
    }

    #[test]
    fn needs_you_comes_first_and_orders_projects_by_count_and_recency() {
        let projects = [
            project(
                "/code/idle-new",
                500,
                vec![saved(1, "Refactor", 500, Some("Done."))],
            ),
            project(
                "/code/one-wait",
                200,
                vec![saved(2, "Tests", 200, Some("Running cargo test."))],
            ),
            project(
                "/code/two-wait",
                100,
                vec![saved(3, "Lint", 100, None), saved(4, "Docs", 90, None)],
            ),
        ];
        let hosted = [
            hosted_session(
                1,
                2,
                "/code/one-wait",
                HostedState::Waiting {
                    question: "Allow Bash: cargo test".to_owned(),
                    since: time(190),
                },
            ),
            hosted_session(
                2,
                3,
                "/code/two-wait",
                HostedState::Waiting {
                    question: "Allow Edit: lib.rs".to_owned(),
                    since: time(95),
                },
            ),
            hosted_session(
                3,
                4,
                "/code/two-wait",
                HostedState::Waiting {
                    question: "Approve plan".to_owned(),
                    since: time(85),
                },
            ),
        ];
        let built = build(&projects, &[], &hosted, Some(Path::new("/home/shen")));
        let order: Vec<(&str, usize, usize)> = built
            .cards
            .iter()
            .map(|card| (card.name.as_str(), card.needs_you, card.running))
            .collect();
        assert_eq!(
            order,
            [("two-wait", 2, 2), ("one-wait", 1, 1), ("idle-new", 0, 0),]
        );
        assert_eq!(built.waiting.len(), 3);
        assert_eq!(
            built.waiting.first().map(|w| w.question.as_str()),
            Some("Approve plan")
        );
    }

    /// The review's finding: a new session the daemon runs, with no transcript yet, was listed
    /// twice, as "Running session" and by the name in Claude Code's record of it.
    #[test]
    fn a_session_the_daemon_runs_is_listed_once_by_its_name() {
        let projects = [project("/code/beep", 100, Vec::new())];
        let live = [LiveSession {
            title: None,
            ..live_session(7, "/code/beep", LiveStatus::Idle)
        }];
        let hosted = [hosted_session(1, 7, "/code/beep", HostedState::Idle)];
        let built = build(&projects, &live, &hosted, None);
        let beep = built.cards.first().expect("a card");
        let listed: Vec<(&str, Option<HostedId>)> = beep
            .sessions
            .iter()
            .map(|session| (session.title.as_str(), session.opens.hosted()))
            .collect();
        assert_eq!(listed, [("session-7", hosted.first().map(|item| item.id))]);
        assert_eq!(beep.running, 1);
    }

    #[test]
    fn projects_without_sessions_stay_listed_and_home_is_left_out() {
        let projects = [
            project(
                "/home/shen",
                999,
                vec![saved(10, "Launched in home", 999, None)],
            ),
            project("/code/fresh", 100, Vec::new()),
            project(
                "/code/active",
                200,
                vec![saved(11, "Work", 200, Some("Ready."))],
            ),
        ];
        let live = [live_session(12, "/code/home-sibling", LiveStatus::Busy)];
        let cards = cards(&projects, &live, &[], Some(Path::new("/home/shen")));
        let names: Vec<&str> = cards.iter().map(|card| card.name.as_str()).collect();
        assert!(!names.contains(&"shen"));
        assert!(names.contains(&"fresh"));
        assert!(names.contains(&"home-sibling"));
        let fresh = cards
            .iter()
            .find(|card| card.name == "fresh")
            .expect("empty project card");
        assert!(fresh.sessions.is_empty());
        assert_eq!(fresh.last_said, None);
    }

    #[test]
    fn identical_folder_names_show_the_parent_that_tells_them_apart() {
        let names = distinct_names(&[
            Path::new("/home/shen/ux/project"),
            Path::new("/home/shen/work/project"),
            Path::new("/home/shen/code/edualc"),
        ]);
        assert_eq!(names, ["project (ux)", "project (work)", "edualc"]);
    }

    /// The review's finding: two sessions' scratch folders were named by their whole paths from
    /// the session ids down, cut off on the card.
    #[test]
    fn a_parent_named_by_a_uuid_shows_its_first_part() {
        let names = distinct_names(&[
            Path::new("/tmp/d9469fb1-1248-4f2a-8e77-d042059022a0/scratchpad/ux/project"),
            Path::new("/tmp/c82dcb2c-03e4-426e-974f-f916d0ae6278/scratchpad/ux/project"),
            Path::new("/project"),
        ]);
        assert_eq!(
            names,
            ["project (d9469fb1)", "project (c82dcb2c)", "project"]
        );
    }
}
