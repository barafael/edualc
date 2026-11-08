//! The demo's world, built into the page in place of a disk: a small Rust project, trailhead,
//! with its earlier sessions, and four sessions "running in other terminals". The transcripts
//! (`web/demo/`) are as Claude Code writes them; the window reads them as it reads any.

use std::path::Path;

use uuid::{Uuid, uuid};
use web_time::{Duration, SystemTime};

use crate::external::wire::{SessionId, TranscriptLine};
use crate::ipc::Runs;
use crate::sessions::{LiveKind, LiveSession, LiveStatus, ProjectInfo, SessionInfo, Worktree};
use crate::words::Words;

/// The demo's folders.
#[derive(Debug, Clone, Copy, PartialEq, Eq, strum::IntoStaticStr)]
pub(crate) enum Folder {
    /// The folder the demo works in.
    #[strum(serialize = "/home/you/code/trailhead")]
    Trailhead,
    #[strum(serialize = "/home/you/code/trailhead/.claude/worktrees/geojson")]
    TrailheadGeojson,
    #[strum(serialize = "/home/you/code/dotfiles")]
    Dotfiles,
    #[strum(serialize = "/home/you/code/blog")]
    Blog,
    #[strum(serialize = "/home/you/code/photo-sort")]
    PhotoSort,
    /// The cloned folder nobody trusted yet, whose settings would run things as Claude Code
    /// starts.
    #[strum(serialize = "/home/you/code/gpx-viewer")]
    GpxViewer,
}

impl Folder {
    pub(crate) fn path(self) -> &'static Path {
        Path::new(<&'static str>::from(self))
    }
}

/// The session the demo opens on: the one that fixed trailhead's elevation gain.
pub(crate) const FIRST: Uuid = uuid!("a3fc9c22-db47-591e-91dc-13613121a701");
/// The chart the demo's Claude draws and reads.
pub(crate) const GAIN_CHART: &[u8] = include_bytes!("../../../../web/demo/gain.png");

/// A session saved in the demo: its id, folder, how long ago it was last written, and its
/// transcript.
struct Saved {
    id: Uuid,
    cwd: Folder,
    age: Duration,
    transcript: Words,
}

const MINUTE: u64 = 60;
const HOUR: u64 = 60 * MINUTE;
const DAY: u64 = 24 * HOUR;

const SAVED: &[Saved] = &[
    Saved {
        id: FIRST,
        cwd: Folder::Trailhead,
        age: Duration::from_secs(6 * MINUTE),
        transcript: Words(include_str!(
            "../../../../web/demo/a3fc9c22-db47-591e-91dc-13613121a701.jsonl"
        )),
    },
    Saved {
        id: uuid!("0361a34a-a663-5902-af7b-60ab0ceb435c"),
        cwd: Folder::Trailhead,
        age: Duration::from_secs(3 * HOUR),
        transcript: Words(include_str!(
            "../../../../web/demo/0361a34a-a663-5902-af7b-60ab0ceb435c.jsonl"
        )),
    },
    Saved {
        id: uuid!("86fbdeef-17ae-5fd5-bcc9-61c073c3a166"),
        cwd: Folder::Trailhead,
        age: Duration::from_secs(26 * HOUR),
        transcript: Words(include_str!(
            "../../../../web/demo/86fbdeef-17ae-5fd5-bcc9-61c073c3a166.jsonl"
        )),
    },
    Saved {
        id: uuid!("7108c542-d726-5b45-87fe-23374b1ff9d1"),
        cwd: Folder::Trailhead,
        age: Duration::from_secs(2 * DAY),
        transcript: Words(include_str!(
            "../../../../web/demo/7108c542-d726-5b45-87fe-23374b1ff9d1.jsonl"
        )),
    },
    Saved {
        id: uuid!("9f25763d-7311-560e-8286-5b20d09d3564"),
        cwd: Folder::Trailhead,
        age: Duration::from_secs(4 * DAY),
        transcript: Words(include_str!(
            "../../../../web/demo/9f25763d-7311-560e-8286-5b20d09d3564.jsonl"
        )),
    },
    Saved {
        id: uuid!("030b03fb-36f5-5ed7-ae50-436a54758688"),
        cwd: Folder::Trailhead,
        age: Duration::from_secs(9 * DAY),
        transcript: Words(include_str!(
            "../../../../web/demo/030b03fb-36f5-5ed7-ae50-436a54758688.jsonl"
        )),
    },
    Saved {
        id: uuid!("bed8a668-765e-5180-86eb-208406ca4f3d"),
        cwd: Folder::Trailhead,
        age: Duration::from_secs(12 * DAY),
        transcript: Words(include_str!(
            "../../../../web/demo/bed8a668-765e-5180-86eb-208406ca4f3d.jsonl"
        )),
    },
];

/// A session "running in another terminal": as saved, and what it is doing.
struct Running {
    saved: Saved,
    pid: u32,
    name: Words,
    status: LiveStatus,
    /// The git worktree it works in, by name.
    worktree: Option<Words>,
}

const RUNNING: &[Running] = &[
    Running {
        saved: Saved {
            id: uuid!("0d7e412c-b84d-539e-80cd-bb6ce66adcbe"),
            cwd: Folder::Dotfiles,
            age: Duration::from_secs(3 * MINUTE),
            transcript: Words(include_str!(
                "../../../../web/demo/0d7e412c-b84d-539e-80cd-bb6ce66adcbe.jsonl"
            )),
        },
        pid: 41_207,
        name: Words("dotfiles"),
        status: LiveStatus::Waiting,
        worktree: None,
    },
    Running {
        saved: Saved {
            id: uuid!("d0a8ce2d-48a8-5d4e-a6a8-113188de71cd"),
            cwd: Folder::TrailheadGeojson,
            age: Duration::from_secs(20 * MINUTE),
            transcript: Words(include_str!(
                "../../../../web/demo/d0a8ce2d-48a8-5d4e-a6a8-113188de71cd.jsonl"
            )),
        },
        pid: 40_118,
        name: Words("trailhead-geojson"),
        status: LiveStatus::Busy,
        worktree: Some(Words("geojson")),
    },
    Running {
        saved: Saved {
            id: uuid!("c19a4a63-ea1b-5082-ab8c-799e7080e23f"),
            cwd: Folder::Blog,
            age: Duration::from_secs(8 * MINUTE),
            transcript: Words(include_str!(
                "../../../../web/demo/c19a4a63-ea1b-5082-ab8c-799e7080e23f.jsonl"
            )),
        },
        pid: 39_552,
        name: Words("blog"),
        status: LiveStatus::Busy,
        worktree: None,
    },
    Running {
        saved: Saved {
            id: uuid!("77b30e85-cf17-5e19-bd46-dedb46664dc8"),
            cwd: Folder::PhotoSort,
            age: Duration::from_secs(40 * MINUTE),
            transcript: Words(include_str!(
                "../../../../web/demo/77b30e85-cf17-5e19-bd46-dedb46664dc8.jsonl"
            )),
        },
        pid: 38_004,
        name: Words("photo-sort"),
        status: LiveStatus::Idle,
        worktree: None,
    },
];

/// The folders Claude Code "was used in", with how long ago.
const FOLDERS: &[(Folder, Duration)] = &[
    (Folder::Trailhead, Duration::from_secs(6 * MINUTE)),
    (Folder::Dotfiles, Duration::from_secs(3 * MINUTE)),
    (Folder::Blog, Duration::from_secs(8 * MINUTE)),
    (Folder::PhotoSort, Duration::from_secs(40 * MINUTE)),
    (Folder::GpxViewer, Duration::from_secs(2 * HOUR)),
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
        .filter(|saved| saved.cwd.path() == cwd)
        .filter_map(|saved| {
            Some(SessionInfo {
                backend: crate::claude::Backend::Claude,
                id: SessionId::from(saved.id),
                title: title(&saved.transcript)?,
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
        .map(|running| {
            let cwd = running.saved.cwd.path().to_path_buf();
            LiveSession {
                pid: running.pid,
                started: running.pid.to_string(),
                id: SessionId::from(running.saved.id),
                name: running.name.as_str().to_owned(),
                title: title(&running.saved.transcript),
                kind: LiveKind::Interactive,
                status: running.status,
                worktree: running.worktree.map(|name| Worktree {
                    path: cwd.clone(),
                    name: name.as_str().to_owned(),
                    branch: format!("worktree-{name}"),
                }),
                agents: 0,
                cwd,
            }
        })
        .collect()
}

/// The folders Claude Code was used in, most recent first.
pub(crate) fn projects() -> Vec<ProjectInfo> {
    let mut projects: Vec<ProjectInfo> = FOLDERS
        .iter()
        .map(|&(folder, age)| ProjectInfo {
            path: folder.path().to_path_buf(),
            modified: ago(age),
        })
        .collect();
    projects.sort_by_key(|project| std::cmp::Reverse(project.modified));
    projects
}

/// The transcript at `path`, found by the session id it is named after.
pub(crate) fn transcript(path: &Path) -> Option<&'static str> {
    let id = Uuid::try_parse(path.file_stem()?.to_str()?).ok()?;
    SAVED
        .iter()
        .chain(RUNNING.iter().map(|running| &running.saved))
        .find(|saved| saved.id == id)
        .map(|saved| saved.transcript.as_str())
}

/// What a folder would run as Claude Code starts there, unless it is trusted: only the cloned
/// one has anything to say.
pub(crate) fn untrusted(folder: &Path) -> Option<Vec<Runs>> {
    (folder == Folder::GpxViewer.path()).then(|| {
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
