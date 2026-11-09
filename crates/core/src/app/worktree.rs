//! The worktree a session works in, and opening it in this window or copying its path.

use super::App;
use crate::present::Frontend;
use crate::sessions::Worktree;

impl<F: Frontend> App<F> {
    /// The worktree of the session shown: the watched one's as its process reports it, or ours.
    pub(super) fn current_worktree(&self) -> Option<&Worktree> {
        match &self.watch {
            Some(watch) => watch
                .session
                .worktree
                .as_ref()
                .or(self.status.worktree.as_ref()),
            None => self.status.worktree.as_ref(),
        }
    }

    /// Does with the current worktree what the menu has at `index`: 0 opens it in this window,
    /// 1 copies its path (it is a folder on edualcd's machine, which an editor here may not
    /// reach).
    pub(super) fn open_worktree(&mut self, index: usize) {
        let Some(worktree) = self.current_worktree().cloned() else {
            return;
        };
        match index {
            // A worktree may be gone since it was read.
            0 => self.read_folder(crate::ipc::Read::Folder(worktree.path)),
            1 => self.view.copy(&worktree.path.to_string_lossy()),
            _ => {}
        }
    }

    /// Looks for the worktree the session entered, as its transcript says after a turn.
    pub(super) fn read_worktree(&mut self) {
        let Some(id) = self.status.session_id.clone() else {
            return;
        };
        let read = crate::ipc::Read::WorktreeOf {
            cwd: self.cwd.clone(),
            id: id.clone(),
        };
        self.read(read, super::Reading::WorktreeOf { id });
    }

    pub(super) fn worktree_read(
        &mut self,
        id: &crate::external::wire::SessionId,
        worktree: Option<Worktree>,
    ) {
        if self.status.session_id.as_ref() == Some(id) && worktree.is_some() {
            self.status.worktree = worktree;
        }
    }
}
