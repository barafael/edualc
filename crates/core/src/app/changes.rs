//! The Changes view: what the session changed in its folder, as the daemon's snapshots tell
//! (`Read::Changes`, `Read::Diff`), the files you marked reviewed, and your comments on lines,
//! sent to Claude together as one message.

use std::path::PathBuf;

use crate::changes::{ChangeSet, Diff, DiffLine, FileChange, LineKind, Review, Scope};
use crate::external::wire::SessionId;
use crate::ipc::{Act, Read};

use super::{Acting, App, Reading};
use crate::present::Frontend;

/// The view, while it is open.
#[derive(Debug)]
pub struct ChangesView {
    pub session: SessionId,
    pub scope: Scope,
    pub listed: Listed<ChangeSet>,
    /// The file whose diff is shown.
    pub picked: Option<Picked>,
    /// The diff row whose comment is being written.
    pub editing: Option<usize>,
    /// The session runs elsewhere, so edualc took no snapshots of its turns.
    pub elsewhere: bool,
}

/// What the daemon was asked to read.
#[derive(Debug)]
pub enum Listed<T> {
    Reading,
    Read(T),
    Failed(String),
}

#[derive(Debug)]
pub struct Picked {
    pub root: PathBuf,
    pub path: PathBuf,
    pub diff: Listed<Diff>,
}

/// A comment on a line of a changed file, not sent yet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LineComment {
    pub root: PathBuf,
    pub path: PathBuf,
    pub line: DiffLine,
    pub text: String,
}

impl LineComment {
    fn on(&self, root: &std::path::Path, path: &std::path::Path, line: &DiffLine) -> bool {
        self.root == root
            && self.path == path
            && self.line.old == line.old
            && self.line.new == line.new
    }
}

/// The view's rows of a diff: its lines, with a gap between hunks.
pub fn rows(diff: &Diff) -> Vec<Option<&DiffLine>> {
    let mut rows = Vec::new();
    for (index, hunk) in diff.hunks.iter().enumerate() {
        if index > 0 {
            rows.push(None);
        }
        rows.extend(hunk.lines.iter().map(Some));
    }
    rows
}

/// What the comments come to as a message to Claude: each with its file (relative to `cwd`,
/// the session's folder, if it is in it), its line's number and text, then what you said.
pub fn message(comments: &[LineComment], cwd: Option<&std::path::Path>) -> String {
    let each: String = comments
        .iter()
        .map(|comment| {
            let (side, number) = match (comment.line.kind, comment.line.new, comment.line.old) {
                (LineKind::Removed, _, Some(old)) => ("removed line", old),
                (_, Some(new), _) | (_, None, Some(new)) => ("line", new),
                (_, None, None) => ("line", 0),
            };
            let whole = comment.root.join(&comment.path);
            let file = cwd
                .and_then(|cwd| whole.strip_prefix(cwd).ok())
                .unwrap_or(&whole);
            format!(
                "\n{}, {side} {number}: {}\n{}\n",
                file.display(),
                comment.line.text.trim(),
                comment.text.trim()
            )
        })
        .collect();
    format!("Comments on your changes:\n{each}")
}

impl<F: Frontend> App<F> {
    /// Opens the view on the last turn's changes, of the session shown.
    pub(super) fn show_changes(&mut self) {
        let followed = self.watch.as_ref().map(|watch| watch.session.id.clone());
        let Some(session) = followed.clone().or_else(|| self.status.session_id.clone()) else {
            return;
        };
        self.changes = Some(ChangesView {
            session,
            scope: Scope::Turn,
            listed: Listed::Reading,
            picked: None,
            editing: None,
            elsewhere: followed.is_some(),
        });
        self.read_changes();
    }

    /// Closes the view: "since you last looked" starts from here.
    pub(super) fn close_changes(&mut self) {
        if let Some(view) = self.changes.take() {
            self.ask_desktop(
                Act::ChangesLooked {
                    session: view.session,
                },
                Acting::Desktop,
            );
        }
        self.redraw.changes = true;
    }

    pub(super) fn choose_changes_scope(&mut self, scope: Scope) {
        if let Some(view) = self.changes.as_mut() {
            view.scope = scope;
            view.listed = Listed::Reading;
            self.read_changes();
        }
    }

    fn read_changes(&mut self) {
        let Some(view) = self.changes.as_ref() else {
            return;
        };
        let read = Read::Changes {
            session: view.session.clone(),
            scope: view.scope,
        };
        let reading = Reading::Changes {
            session: view.session.clone(),
            scope: view.scope,
        };
        self.read(read, reading);
        self.redraw.changes = true;
    }

    /// The changes the daemon read; the file shown stays if it is among them, else the first.
    pub(super) fn changes_read(
        &mut self,
        session: &SessionId,
        scope: Scope,
        result: Result<ChangeSet, String>,
    ) {
        let Some(view) = self
            .changes
            .as_mut()
            .filter(|view| &view.session == session && view.scope == scope)
        else {
            return;
        };
        let keep = match (&result, &view.picked) {
            (Ok(changes), Some(picked)) => changes
                .files
                .iter()
                .position(|file| file.root == picked.root && file.path == picked.path),
            (Ok(_) | Err(_), _) => None,
        };
        let first = match &result {
            Ok(changes) if !changes.files.is_empty() => Some(0),
            Ok(_) | Err(_) => None,
        };
        view.listed = match result {
            Ok(changes) => Listed::Read(changes),
            Err(error) => Listed::Failed(error),
        };
        self.redraw.changes = true;
        match keep.or(first) {
            Some(index) => self.select_changed_file(index),
            None => {
                if let Some(view) = self.changes.as_mut() {
                    view.picked = None;
                }
            }
        }
    }

    /// Shows the diff of the file at `index` in the list.
    pub(super) fn select_changed_file(&mut self, index: usize) {
        let Some(view) = self.changes.as_mut() else {
            return;
        };
        let Listed::Read(changes) = &view.listed else {
            return;
        };
        let Some(file) = changes.files.get(index) else {
            return;
        };
        let (root, path) = (file.root.clone(), file.path.clone());
        view.picked = Some(Picked {
            root: root.clone(),
            path: path.clone(),
            diff: Listed::Reading,
        });
        view.editing = None;
        let read = Read::Diff {
            session: view.session.clone(),
            scope: view.scope,
            root: root.clone(),
            path: path.clone(),
        };
        let reading = Reading::Diff {
            session: view.session.clone(),
            root,
            path,
        };
        self.read(read, reading);
        self.redraw.changes = true;
    }

    pub(super) fn diff_read(
        &mut self,
        session: &SessionId,
        root: &std::path::Path,
        path: &std::path::Path,
        result: Result<Diff, String>,
    ) {
        let Some(picked) = self
            .changes
            .as_mut()
            .filter(|view| &view.session == session)
            .and_then(|view| view.picked.as_mut())
            .filter(|picked| picked.root == root && picked.path == path)
        else {
            return;
        };
        picked.diff = match result {
            Ok(diff) => Listed::Read(diff),
            Err(error) => Listed::Failed(error),
        };
        self.redraw.changes = true;
    }

    /// Marks the file shown reviewed, as it is now; or, marked so, takes the mark off.
    pub(super) fn toggle_reviewed(&mut self) {
        let Some(view) = self.changes.as_mut() else {
            return;
        };
        let (Listed::Read(changes), Some(picked)) = (&mut view.listed, &view.picked) else {
            return;
        };
        let Some(file) = changes
            .files
            .iter_mut()
            .find(|file| file.root == picked.root && file.path == picked.path)
        else {
            return;
        };
        let session = view.session.clone();
        let path = file.root.join(&file.path);
        let act = if file.review == Review::Reviewed {
            file.review = Review::NotReviewed;
            Act::UnmarkReviewed { session, path }
        } else {
            file.review = Review::Reviewed;
            Act::MarkReviewed {
                session,
                path,
                now: file.now,
            }
        };
        self.ask_desktop(act, Acting::Desktop);
        self.redraw.changes = true;
    }

    /// The line at diff row `row` of the file shown, with its file.
    fn line_at(&self, row: usize) -> Option<(PathBuf, PathBuf, DiffLine)> {
        let picked = self.changes.as_ref()?.picked.as_ref()?;
        let Listed::Read(diff) = &picked.diff else {
            return None;
        };
        let line = rows(diff).get(row).copied().flatten()?.clone();
        Some((picked.root.clone(), picked.path.clone(), line))
    }

    pub(super) fn edit_line_comment(&mut self, row: usize) {
        if self.line_at(row).is_some()
            && let Some(view) = self.changes.as_mut()
        {
            view.editing = Some(row);
            self.redraw.changes = true;
        }
    }

    pub(super) fn cancel_line_comment(&mut self) {
        if let Some(view) = self.changes.as_mut() {
            view.editing = None;
            self.redraw.changes = true;
        }
    }

    /// Keeps `text` as the comment on diff row `row`; nothing removes it.
    pub(super) fn set_line_comment(&mut self, row: usize, text: &str) {
        let Some((root, path, line)) = self.line_at(row) else {
            return;
        };
        self.line_comments
            .retain(|comment| !comment.on(&root, &path, &line));
        if !text.trim().is_empty() {
            self.line_comments.push(LineComment {
                root,
                path,
                line,
                text: text.to_owned(),
            });
        }
        if let Some(view) = self.changes.as_mut() {
            view.editing = None;
        }
        self.redraw.changes = true;
    }

    /// Sends the comments to Claude as one message, and goes back to the conversation.
    pub(super) fn send_line_comments(&mut self) {
        if self.line_comments.is_empty() {
            return;
        }
        let text = message(&self.line_comments, self.location.cwd());
        self.line_comments.clear();
        self.close_changes();
        self.send(&text);
    }

    /// A turn of the session shown changed its folder: the view, if open, reads again.
    pub(super) fn turn_changed(&mut self) {
        self.redraw.changes = true;
        if self.changes.is_some() {
            self.read_changes();
        }
    }
}

/// How many comments there are on `file`.
pub fn comments_on(comments: &[LineComment], file: &FileChange) -> usize {
    comments
        .iter()
        .filter(|comment| comment.root == file.root && comment.path == file.path)
        .count()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::changes::Hunk;

    fn line(kind: LineKind, old: Option<u32>, new: Option<u32>, text: &str) -> DiffLine {
        DiffLine {
            kind,
            old,
            new,
            text: text.to_owned(),
        }
    }

    #[test]
    fn hunks_are_rows_with_a_gap_between() {
        let diff = Diff {
            hunks: vec![
                Hunk {
                    lines: vec![line(LineKind::Same, Some(1), Some(1), "a")],
                },
                Hunk {
                    lines: vec![line(LineKind::Added, None, Some(9), "b")],
                },
            ],
            cut: false,
        };
        let texts: Vec<Option<&str>> = rows(&diff)
            .into_iter()
            .map(|row| row.map(|line| line.text.as_str()))
            .collect();
        assert_eq!(texts, [Some("a"), None, Some("b")]);
    }

    /// Each comment goes with its file, its line's number in the file as it is (the old
    /// file's for a removed line) and the line's text.
    #[test]
    fn comments_go_as_one_message_with_their_lines() {
        let comment = |kind, old, new, text: &str, said: &str| LineComment {
            root: PathBuf::from("/code/beep"),
            path: PathBuf::from("src/lib.rs"),
            line: line(kind, old, new, text),
            text: said.to_owned(),
        };
        let comments = [
            comment(
                LineKind::Added,
                None,
                Some(12),
                "fn b() {}",
                "Name it better.",
            ),
            comment(
                LineKind::Removed,
                Some(4),
                None,
                "  old();",
                "Keep this call.",
            ),
        ];
        assert_eq!(
            message(&comments, Some(std::path::Path::new("/code/beep"))),
            "Comments on your changes:\n\
             \nsrc/lib.rs, line 12: fn b() {}\nName it better.\n\
             \nsrc/lib.rs, removed line 4: old();\nKeep this call.\n"
        );
        // A file outside the session's folder (a worktree elsewhere) goes by its whole path.
        assert!(
            message(&comments, Some(std::path::Path::new("/elsewhere")))
                .contains("/code/beep/src/lib.rs, line 12")
        );
    }
}
