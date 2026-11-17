//! What a session changed in its folder, as the daemon's snapshots before and after each turn
//! tell it (see `edualc-host`'s `changes`): which files, who changed them, and how.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// Which changes are shown.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum Scope {
    /// The last turn's.
    #[default]
    Turn,
    /// Those since you last looked at the changes.
    SinceLooked,
    /// The whole session's.
    Session,
}

/// A file's contents, by their SHA-256: how the daemon keeps them, and what "reviewed" names.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct ContentId(pub [u8; 32]);

impl std::fmt::Display for ContentId {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0
            .iter()
            .try_for_each(|byte| write!(formatter, "{byte:02x}"))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Kind {
    Added,
    Changed,
    Deleted,
}

/// Who changed a file, as far as the turns tell.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Who {
    /// Claude's Edit, Write or NotebookEdit named it in the turn that changed it.
    Claude,
    /// It changed in a turn without Claude's editing tools naming it: a command it ran.
    Command,
    /// Both, in the turns shown.
    Both,
    /// Outside any turn: you, or another program.
    BetweenTurns,
}

/// Whether you marked a file reviewed, as it is now.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Review {
    NotReviewed,
    Reviewed,
    /// Marked, and changed since.
    ChangedSinceReviewed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FileChange {
    /// The folder the snapshots were taken in: the session's, or a worktree it works in.
    pub root: PathBuf,
    /// Relative to `root`.
    pub path: PathBuf,
    pub kind: Kind,
    pub who: Who,
    pub added: u32,
    pub removed: u32,
    pub review: Review,
    /// What it holds now, which marking it reviewed names; none once deleted.
    pub now: Option<ContentId>,
}

/// What the snapshots leave out, so the view can say so.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LeftOut {
    /// Files over [`LARGEST_FILE`] bytes.
    pub large: u32,
    pub binary: u32,
    /// The folder holds more than [`MOST_FILES`] files; those after were not looked at.
    pub beyond_limit: bool,
}

/// The largest file a snapshot keeps.
pub const LARGEST_FILE: u64 = 1024 * 1024;
/// The most files a snapshot looks at.
pub const MOST_FILES: usize = 20_000;

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ChangeSet {
    pub files: Vec<FileChange>,
    pub left_out: LeftOut,
}

impl ChangeSet {
    /// Lines added and removed, over all its files.
    pub fn lines(&self) -> (u32, u32) {
        self.files.iter().fold((0, 0), |(added, removed), file| {
            (
                added.saturating_add(file.added),
                removed.saturating_add(file.removed),
            )
        })
    }
}

/// A turn's changes in short, for its footer and the top bar.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Summary {
    pub files: u32,
    pub added: u32,
    pub removed: u32,
}

impl From<&ChangeSet> for Summary {
    fn from(changes: &ChangeSet) -> Self {
        let (added, removed) = changes.lines();
        Self {
            files: u32::try_from(changes.files.len()).unwrap_or(u32::MAX),
            added,
            removed,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum LineKind {
    Same,
    Removed,
    Added,
}

/// A line of a diff, with its numbers in the old file and the new, from 1.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DiffLine {
    pub kind: LineKind,
    pub old: Option<u32>,
    pub new: Option<u32>,
    pub text: String,
}

/// Changed lines with three lines of context around them.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Hunk {
    pub lines: Vec<DiffLine>,
}

/// The most lines of a diff sent to a window, so its message stays under
/// [`crate::ipc::MAX_MESSAGE`]...
pub const MOST_DIFF_LINES: usize = 4000;
/// ...each of at most this many characters.
pub const LONGEST_DIFF_LINE: usize = 200;

/// A file's hunks as a window gets them: cut short if they are many or long.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Diff {
    pub hunks: Vec<Hunk>,
    /// Lines were left out, or cut, to fit.
    pub cut: bool,
}

impl Diff {
    /// `hunks`, at most [`MOST_DIFF_LINES`] lines of at most [`LONGEST_DIFF_LINE`] characters.
    pub fn fitted(hunks: Vec<Hunk>) -> Self {
        let mut left = MOST_DIFF_LINES;
        let mut cut = false;
        let mut fitted = Vec::new();
        for hunk in hunks {
            if left == 0 {
                cut = true;
                break;
            }
            if hunk.lines.len() > left {
                cut = true;
            }
            let lines: Vec<DiffLine> = hunk
                .lines
                .into_iter()
                .take(left)
                .map(|mut line| {
                    if line.text.chars().count() > LONGEST_DIFF_LINE {
                        cut = true;
                        line.text = line.text.chars().take(LONGEST_DIFF_LINE).collect();
                        line.text.push('…');
                    }
                    line
                })
                .collect();
            left = left.saturating_sub(lines.len());
            fitted.push(Hunk { lines });
        }
        Self { hunks: fitted, cut }
    }
}

/// Lines added and removed from `old` to `new`.
pub fn counts(old: &str, new: &str) -> (u32, u32) {
    let diff = similar::TextDiff::from_lines(old, new);
    diff.iter_all_changes()
        .fold((0_u32, 0_u32), |(added, removed), change| {
            match change.tag() {
                similar::ChangeTag::Insert => (added.saturating_add(1), removed),
                similar::ChangeTag::Delete => (added, removed.saturating_add(1)),
                similar::ChangeTag::Equal => (added, removed),
            }
        })
}

/// The hunks from `old` to `new`.
pub fn hunks(old: &str, new: &str) -> Vec<Hunk> {
    let diff = similar::TextDiff::from_lines(old, new);
    let number = |index: Option<usize>| {
        index.map(|index| u32::try_from(index.saturating_add(1)).unwrap_or(u32::MAX))
    };
    diff.grouped_ops(3)
        .iter()
        .map(|group| Hunk {
            lines: group
                .iter()
                .flat_map(|op| diff.iter_changes(op))
                .map(|change| {
                    let text = change.value();
                    DiffLine {
                        kind: match change.tag() {
                            similar::ChangeTag::Equal => LineKind::Same,
                            similar::ChangeTag::Delete => LineKind::Removed,
                            similar::ChangeTag::Insert => LineKind::Added,
                        },
                        old: number(change.old_index()),
                        new: number(change.new_index()),
                        text: text.strip_suffix('\n').unwrap_or(text).to_owned(),
                    }
                })
                .collect(),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lines_are_counted_and_numbered_in_both_files() {
        let old = "a\nb\nc\n";
        let new = "a\nB\nc\nd\n";
        assert_eq!(counts(old, new), (2, 1));
        let hunks = hunks(old, new);
        let numbered: Vec<(LineKind, Option<u32>, Option<u32>, &str)> = hunks
            .iter()
            .flat_map(|hunk| &hunk.lines)
            .map(|line| (line.kind, line.old, line.new, line.text.as_str()))
            .collect();
        assert_eq!(
            numbered,
            [
                (LineKind::Same, Some(1), Some(1), "a"),
                (LineKind::Removed, Some(2), None, "b"),
                (LineKind::Added, None, Some(2), "B"),
                (LineKind::Same, Some(3), Some(3), "c"),
                (LineKind::Added, None, Some(4), "d"),
            ]
        );
    }

    #[test]
    fn far_apart_changes_are_hunks_of_their_own() {
        let old: String = (1..=20).map(|line| format!("{line}\n")).collect();
        let new: String = (1..=20)
            .map(|line| match line {
                2 => "two\n".to_owned(),
                19 => "nineteen\n".to_owned(),
                _ => format!("{line}\n"),
            })
            .collect();
        assert_eq!(hunks(&old, &new).len(), 2);
        assert!(hunks(&old, &old).is_empty());
    }

    /// A diff a window gets fits one message: a long file's lines, and a long line, are cut.
    #[test]
    fn a_diff_is_cut_to_fit_a_message() {
        let old = "same\n".repeat(10);
        let new = format!("{}\n", "x".repeat(LONGEST_DIFF_LINE * 3)).repeat(MOST_DIFF_LINES);
        let diff = Diff::fitted(hunks(&old, &new));
        assert!(diff.cut);
        let lines: Vec<&DiffLine> = diff.hunks.iter().flat_map(|hunk| &hunk.lines).collect();
        assert_eq!(lines.len(), MOST_DIFF_LINES);
        assert!(
            lines
                .iter()
                .all(|line| line.text.chars().count() <= LONGEST_DIFF_LINE + 1)
        );
        let encoded = postcard::to_stdvec(&diff).expect("encoded");
        assert!(encoded.len() < crate::ipc::MAX_MESSAGE);
        assert!(!Diff::fitted(hunks("a\n", "b\n")).cut);
    }

    #[test]
    fn a_content_id_reads_as_hex() {
        let mut bytes = [0_u8; 32];
        bytes[0] = 0xab;
        bytes[31] = 0x01;
        let shown = ContentId(bytes).to_string();
        assert_eq!(shown.len(), 64);
        assert!(shown.starts_with("ab00"));
        assert!(shown.ends_with("01"));
    }
}
