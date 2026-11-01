//! The worktree a session works in, and opening it: in this window, an editor, a terminal or
//! the file manager.

use super::{App, Event};
use crate::sessions::{self, Worktree};

impl App {
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
            self.open_project(&worktree.path.to_string_lossy());
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
    pub(super) fn read_worktree(&self) {
        let Some(id) = self.status.session_id.clone() else {
            return;
        };
        let cwd = self.cwd.clone();
        self.in_background(
            move || {
                let worktree = sessions::find_transcript(&cwd, &id)
                    .and_then(|transcript| sessions::worktree_of(&transcript));
                (id, worktree)
            },
            |(id, worktree)| Event::WorktreeRead { id, worktree },
        );
    }

    pub(super) fn worktree_read(
        &mut self,
        id: &crate::wire::SessionId,
        worktree: Option<Worktree>,
    ) {
        if self.status.session_id.as_ref() == Some(id) && worktree.is_some() {
            self.status.worktree = worktree;
        }
    }
}
