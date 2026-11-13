//! The worktree a session works in, and opening it: in this window, an editor, a terminal or
//! the file manager.

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

    /// Opens the current worktree with the opener at `index` in the menu; 0 is this window.
    pub(super) fn open_worktree(&mut self, index: usize) {
        let Some(worktree) = self.current_worktree().cloned() else {
            return;
        };
        let Some(opener_index) = index.checked_sub(1) else {
            // A worktree may be gone since it was read.
            self.read_folder(crate::ipc::Read::Folder(worktree.path), false);
            return;
        };
        let Some(&opener) = self.openers.get(opener_index) else {
            return;
        };
        self.ask_desktop(
            crate::ipc::Act::OpenIn {
                opener,
                folder: worktree.path,
            },
            super::Acting::Desktop,
        );
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
        self.read(read, super::Reading::Worktree { id });
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
