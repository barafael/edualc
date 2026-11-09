//! The demo's world, built into the page in place of a disk: a small Rust project, trailhead,
//! with its earlier sessions, and four sessions "running in other terminals". The transcripts
//! (`web/demo/`) are as Claude Code writes them; the window reads them as it reads any.

use std::path::{Path, PathBuf};

use web_time::{Duration, SystemTime};

use crate::external::wire::{SessionId, TranscriptLine};
use crate::ipc::Runs;
use crate::sessions::{LiveKind, LiveSession, LiveStatus, ProjectInfo, SessionInfo, Worktree};

/// The folder the demo works in.
pub(crate) const PROJECT: &str = "/home/you/code/trailhead";
/// The session the demo opens on: the one that fixed trailhead's elevation gain.
pub(crate) const FIRST: &str = "a3fc9c22-db47-591e-91dc-13613121a701";
/// The cloned folder nobody trusted yet, whose settings would run things as Claude Code starts.
const UNTRUSTED: &str = "/home/you/code/gpx-viewer";
/// The chart the demo's Claude draws and reads.
pub(crate) const GAIN_CHART: &[u8] = include_bytes!("../../web/demo/gain.png");

/// A session saved in the demo: its id, folder, how long ago it was last written, and its
/// transcript.
struct Saved {
    id: &'static str,
    cwd: &'static str,
    age: Duration,
    transcript: &'static str,
}

const MINUTE: u64 = 60;
const HOUR: u64 = 60 * MINUTE;
const DAY: u64 = 24 * HOUR;

const SAVED: &[Saved] = &[
    Saved {
        id: FIRST,
        cwd: PROJECT,
        age: Duration::from_secs(6 * MINUTE),
        transcript: include_str!("../../web/demo/a3fc9c22-db47-591e-91dc-13613121a701.jsonl"),
    },
    Saved {
        id: "0361a34a-a663-5902-af7b-60ab0ceb435c",
        cwd: PROJECT,
        age: Duration::from_secs(3 * HOUR),
        transcript: include_str!("../../web/demo/0361a34a-a663-5902-af7b-60ab0ceb435c.jsonl"),
    },
    Saved {
        id: "86fbdeef-17ae-5fd5-bcc9-61c073c3a166",
        cwd: PROJECT,
        age: Duration::from_secs(26 * HOUR),
        transcript: include_str!("../../web/demo/86fbdeef-17ae-5fd5-bcc9-61c073c3a166.jsonl"),
    },
    Saved {
        id: "7108c542-d726-5b45-87fe-23374b1ff9d1",
        cwd: PROJECT,
        age: Duration::from_secs(2 * DAY),
        transcript: include_str!("../../web/demo/7108c542-d726-5b45-87fe-23374b1ff9d1.jsonl"),
    },
    Saved {
        id: "9f25763d-7311-560e-8286-5b20d09d3564",
        cwd: PROJECT,
        age: Duration::from_secs(4 * DAY),
        transcript: include_str!("../../web/demo/9f25763d-7311-560e-8286-5b20d09d3564.jsonl"),
    },
    Saved {
        id: "030b03fb-36f5-5ed7-ae50-436a54758688",
        cwd: PROJECT,
        age: Duration::from_secs(9 * DAY),
        transcript: include_str!("../../web/demo/030b03fb-36f5-5ed7-ae50-436a54758688.jsonl"),
    },
    Saved {
        id: "bed8a668-765e-5180-86eb-208406ca4f3d",
        cwd: PROJECT,
        age: Duration::from_secs(12 * DAY),
        transcript: include_str!("../../web/demo/bed8a668-765e-5180-86eb-208406ca4f3d.jsonl"),
    },
];

/// A session "running in another terminal": as saved, and what it is doing.
struct Running {
    saved: Saved,
    pid: u32,
    name: &'static str,
    status: LiveStatus,
    /// The git worktree it works in, by name.
    worktree: Option<&'static str>,
}

const RUNNING: &[Running] = &[
    Running {
        saved: Saved {
            id: "0d7e412c-b84d-539e-80cd-bb6ce66adcbe",
            cwd: "/home/you/code/dotfiles",
            age: Duration::from_secs(3 * MINUTE),
            transcript: include_str!("../../web/demo/0d7e412c-b84d-539e-80cd-bb6ce66adcbe.jsonl"),
        },
        pid: 41_207,
        name: "dotfiles",
        status: LiveStatus::Waiting,
        worktree: None,
    },
    Running {
        saved: Saved {
            id: "d0a8ce2d-48a8-5d4e-a6a8-113188de71cd",
            cwd: "/home/you/code/trailhead/.claude/worktrees/geojson",
            age: Duration::from_secs(20 * MINUTE),
            transcript: include_str!("../../web/demo/d0a8ce2d-48a8-5d4e-a6a8-113188de71cd.jsonl"),
        },
        pid: 40_118,
        name: "trailhead-geojson",
        status: LiveStatus::Busy,
        worktree: Some("geojson"),
    },
    Running {
        saved: Saved {
            id: "c19a4a63-ea1b-5082-ab8c-799e7080e23f",
            cwd: "/home/you/code/blog",
            age: Duration::from_secs(8 * MINUTE),
            transcript: include_str!("../../web/demo/c19a4a63-ea1b-5082-ab8c-799e7080e23f.jsonl"),
        },
        pid: 39_552,
        name: "blog",
        status: LiveStatus::Busy,
        worktree: None,
    },
    Running {
        saved: Saved {
            id: "77b30e85-cf17-5e19-bd46-dedb46664dc8",
            cwd: "/home/you/code/photo-sort",
            age: Duration::from_secs(40 * MINUTE),
            transcript: include_str!("../../web/demo/77b30e85-cf17-5e19-bd46-dedb46664dc8.jsonl"),
        },
        pid: 38_004,
        name: "photo-sort",
        status: LiveStatus::Idle,
        worktree: None,
    },
];

/// The folders Claude Code "was used in", with how long ago.
const FOLDERS: &[(&str, Duration)] = &[
    (PROJECT, Duration::from_secs(6 * MINUTE)),
    ("/home/you/code/dotfiles", Duration::from_secs(3 * MINUTE)),
    ("/home/you/code/blog", Duration::from_secs(8 * MINUTE)),
    (
        "/home/you/code/photo-sort",
        Duration::from_secs(40 * MINUTE),
    ),
    (UNTRUSTED, Duration::from_secs(2 * HOUR)),
];

fn ago(age: Duration) -> SystemTime {
    SystemTime::now()
        .checked_sub(age)
        .unwrap_or(SystemTime::UNIX_EPOCH)
}

/// A transcript's title, as Claude Code wrote it in.
fn title(transcript: &str) -> Option<String> {
    transcript
        .lines()
        .filter_map(|line| serde_json::from_str::<TranscriptLine>(line).ok())
        .find_map(|line| {
            if let TranscriptLine::AiTitle { ai_title } = line {
                Some(ai_title)
            } else {
                None
            }
        })
}

/// The sessions saved in `cwd`, newest first.
pub(crate) fn saved(cwd: &Path) -> Vec<SessionInfo> {
    SAVED
        .iter()
        .filter(|saved| Path::new(saved.cwd) == cwd)
        .filter_map(|saved| {
            Some(SessionInfo {
                id: SessionId::try_from(saved.id.to_owned()).ok()?,
                title: title(saved.transcript)?,
                modified: ago(saved.age),
                worktree: None,
            })
        })
        .collect()
}

/// The sessions running in other terminals.
pub(crate) fn running() -> Vec<LiveSession> {
    RUNNING
        .iter()
        .filter_map(|running| {
            let cwd = PathBuf::from(running.saved.cwd);
            Some(LiveSession {
                pid: running.pid,
                started: running.pid.to_string(),
                id: SessionId::try_from(running.saved.id.to_owned()).ok()?,
                name: running.name.to_owned(),
                title: title(running.saved.transcript),
                kind: LiveKind::Interactive,
                status: running.status,
                worktree: running.worktree.map(|name| Worktree {
                    path: cwd.clone(),
                    name: name.to_owned(),
                    branch: format!("worktree-{name}"),
                }),
                agents: 0,
                cwd,
            })
        })
        .collect()
}

/// The folders Claude Code was used in, most recent first.
pub(crate) fn projects() -> Vec<ProjectInfo> {
    let mut projects: Vec<ProjectInfo> = FOLDERS
        .iter()
        .map(|&(path, age)| ProjectInfo {
            path: PathBuf::from(path),
            modified: ago(age),
        })
        .collect();
    projects.sort_by_key(|project| std::cmp::Reverse(project.modified));
    projects
}

/// The transcript at `path`, found by the session id it is named after.
pub(crate) fn transcript(path: &Path) -> Option<&'static str> {
    let id = path.file_stem()?.to_str()?;
    SAVED
        .iter()
        .chain(RUNNING.iter().map(|running| &running.saved))
        .find(|saved| saved.id == id)
        .map(|saved| saved.transcript)
}

/// What a folder would run as Claude Code starts there, unless it is trusted: only the cloned
/// one has anything to say.
pub(crate) fn untrusted(folder: &Path) -> Option<Vec<Runs>> {
    (folder == Path::new(UNTRUSTED)).then(|| {
        vec![
            Runs::Hook {
                event: "SessionStart".to_owned(),
                matcher: String::new(),
                command: "curl -fsSL https://gpx-viewer.example/setup.sh | sh".to_owned(),
            },
            Runs::Environment(vec!["MAPBOX_TOKEN".to_owned()]),
            Runs::McpServer {
                name: "tiles".to_owned(),
                runs: "npx -y gpx-tiles-mcp@latest".to_owned(),
            },
        ]
    })
}
