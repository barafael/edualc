//! What Claude Code runs besides the turn's own steps: its list of background tasks, which it
//! sends whole on every change, and the subagents it waits for in the foreground, which no list
//! names. Those are known from their `task_started`, until they end or move to the background.

use crate::external::wire::{BackgroundTask, TaskId, TaskKind, TaskPatch, TaskStatus};

/// Where a task runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Place {
    Background,
    /// The turn waits for it.
    Foreground,
}

/// The tasks running now, for the header's chip.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct Tasks {
    background: Vec<BackgroundTask>,
    foreground: Vec<BackgroundTask>,
}

impl Tasks {
    /// Every task running, the background ones first; one listed in both counts once, as the
    /// list's.
    pub(crate) fn running(&self) -> impl Iterator<Item = (&BackgroundTask, Place)> {
        let background = self.background.iter().map(|task| (task, Place::Background));
        let foreground = self
            .foreground
            .iter()
            .filter(|task| !self.listed(&task.task_id))
            .map(|task| (task, Place::Foreground));
        background.chain(foreground)
    }

    fn listed(&self, id: &TaskId) -> bool {
        self.background.iter().any(|task| task.task_id == *id)
    }

    /// Claude Code's list of background tasks, all of them. Whether that changed what runs.
    pub(crate) fn list(&mut self, mut tasks: Vec<BackgroundTask>) -> bool {
        tasks.retain(|task| !task.ambient);
        if tasks == self.background {
            return false;
        }
        self.background = tasks;
        true
    }

    /// A task began. Only a subagent in the foreground is kept: one in the background is in the
    /// next list, and a foreground shell command shows as its own tool call.
    pub(crate) fn start(&mut self, task: BackgroundTask, backgrounded: Option<bool>) -> bool {
        let waited_for =
            backgrounded == Some(false) && task.task_type == TaskKind::LocalAgent && !task.ambient;
        if !waited_for
            || self
                .foreground
                .iter()
                .any(|kept| kept.task_id == task.task_id)
        {
            return false;
        }
        self.foreground.push(task);
        true
    }

    /// A task changed: it ended, or moved to the background, where the list names it.
    pub(crate) fn update(&mut self, id: &TaskId, patch: &TaskPatch) -> bool {
        let gone =
            patch.status.is_some_and(TaskStatus::ended) || patch.is_backgrounded == Some(true);
        gone && self.end(id)
    }

    /// A task ended.
    pub(crate) fn end(&mut self, id: &TaskId) -> bool {
        let before = self.foreground.len();
        self.foreground.retain(|task| task.task_id != *id);
        self.foreground.len() != before
    }

    /// The turn ended: no foreground task outlives it, even one whose end went unsaid.
    pub(crate) fn end_turn(&mut self) -> bool {
        let had = !self.foreground.is_empty();
        self.foreground.clear();
        had
    }

    /// The process is gone, and all it ran with it.
    pub(crate) fn clear(&mut self) {
        self.background.clear();
        self.foreground.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn task(id: &str, kind: &str) -> BackgroundTask {
        serde_json::from_value(json!({ "task_id": id, "task_type": kind, "description": id }))
            .expect("a task")
    }

    fn patch(value: serde_json::Value) -> TaskPatch {
        serde_json::from_value(value).expect("a patch")
    }

    fn ids(tasks: &Tasks) -> Vec<(String, Place)> {
        tasks
            .running()
            .map(|(task, place)| (task.description.clone(), place))
            .collect()
    }

    /// A subagent the turn waits for runs until it ends, beside the background list.
    #[test]
    fn a_foreground_agent_runs_until_it_ends() {
        let mut tasks = Tasks::default();
        assert!(tasks.list(vec![task("b1", "local_bash")]));
        assert!(tasks.start(task("a1", "local_agent"), Some(false)));
        assert_eq!(
            ids(&tasks),
            [
                ("b1".to_owned(), Place::Background),
                ("a1".to_owned(), Place::Foreground)
            ]
        );
        let a1 = task("a1", "local_agent").task_id;
        assert!(!tasks.update(&a1, &patch(json!({ "status": "running" }))));
        assert!(tasks.end(&a1));
        assert_eq!(ids(&tasks), [("b1".to_owned(), Place::Background)]);
    }

    /// One moved to the background leaves the foreground, and counts once while both name it.
    #[test]
    fn an_agent_moved_to_the_background_counts_once() {
        let mut tasks = Tasks::default();
        tasks.start(task("a1", "local_agent"), Some(false));
        assert!(tasks.list(vec![task("a1", "local_agent")]));
        assert_eq!(ids(&tasks), [("a1".to_owned(), Place::Background)]);
        let a1 = task("a1", "local_agent").task_id;
        assert!(tasks.update(&a1, &patch(json!({ "is_backgrounded": true }))));
        assert_eq!(ids(&tasks), [("a1".to_owned(), Place::Background)]);
    }

    /// Only foreground subagents are kept from their start: a background one comes in the list,
    /// a shell command shows as its call, and Claude Code's own tasks show nowhere.
    #[test]
    fn only_foreground_agents_are_kept_from_their_start() {
        let mut tasks = Tasks::default();
        assert!(!tasks.start(task("a1", "local_agent"), Some(true)));
        assert!(!tasks.start(task("b1", "local_bash"), Some(false)));
        assert!(!tasks.start(task("w1", "local_workflow"), None));
        let mut ambient = task("a2", "local_agent");
        ambient.ambient = true;
        assert!(!tasks.start(ambient, Some(false)));
        assert!(!tasks.list(vec![{
            let mut listed = task("m1", "monitor_ws");
            listed.ambient = true;
            listed
        }]));
        assert_eq!(ids(&tasks), []);
    }

    /// The turn's end ends what it waited for, though no end was said; the list stays.
    #[test]
    fn the_turns_end_ends_its_foreground_agents() {
        let mut tasks = Tasks::default();
        tasks.list(vec![task("b1", "local_bash")]);
        tasks.start(task("a1", "local_agent"), Some(false));
        assert!(tasks.end_turn());
        assert!(!tasks.end_turn());
        assert_eq!(ids(&tasks), [("b1".to_owned(), Place::Background)]);
    }
}
