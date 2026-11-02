//! What Claude Code runs besides the turn's own steps: its list of background tasks, which it
//! sends whole on every change, and the subagents it waits for in the foreground, which no list
//! names. Those are known from their `task_started`, until they end or move to the background.

use std::collections::BTreeMap;

use crate::external::wire::{
    BackgroundTask, ControlRequest, TaskId, TaskKind, TaskPatch, TaskStatus, TaskUsage, ToolName,
};

use super::{App, Request};

impl App {
    /// The user stopped the turn, which ends only the turn: if tasks run on, their list opens
    /// and says so, for the user to stop them or leave them running.
    pub(super) fn tasks_after_stop(&mut self) {
        if self.status.tasks.running().next().is_some() {
            self.status.tasks_left_running = true;
            self.redraw.tasks = true;
            self.view.show_tasks();
        }
    }

    pub(super) fn stop_task(&mut self, task: TaskId) {
        if self
            .status
            .tasks
            .running()
            .any(|running| running.task.task_id == task)
        {
            let stop = ControlRequest::StopTask {
                task_id: task.clone(),
            };
            self.request(&stop, Request::StopTask(task));
        }
    }

    pub(super) fn stop_all_tasks(&mut self) {
        let tasks: Vec<TaskId> = self
            .status
            .tasks
            .running()
            .map(|running| running.task.task_id.clone())
            .collect();
        for task in tasks {
            self.stop_task(task);
        }
    }
}

/// Where a task runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Place {
    Background,
    /// The turn waits for it.
    Foreground,
}

/// How far a task got, as it last said.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TaskProgress {
    pub(crate) usage: TaskUsage,
    pub(crate) last_tool: Option<ToolName>,
    /// What it does now, in a line, where Claude Code makes one.
    pub(crate) summary: Option<String>,
}

/// A task that runs, with where and how far.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Running<'a> {
    pub(crate) task: &'a BackgroundTask,
    pub(crate) place: Place,
    pub(crate) progress: Option<&'a TaskProgress>,
}

/// The tasks running now, for the header's chip.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct Tasks {
    background: Vec<BackgroundTask>,
    foreground: Vec<BackgroundTask>,
    progress: BTreeMap<TaskId, TaskProgress>,
}

impl Tasks {
    /// Every task running, the background ones first; one listed in both counts once, as the
    /// list's.
    pub(crate) fn running(&self) -> impl Iterator<Item = Running<'_>> {
        let background = self.background.iter().map(|task| (task, Place::Background));
        let foreground = self
            .foreground
            .iter()
            .filter(|task| !self.listed(&task.task_id))
            .map(|task| (task, Place::Foreground));
        background.chain(foreground).map(|(task, place)| Running {
            task,
            place,
            progress: self.progress.get(&task.task_id),
        })
    }

    fn listed(&self, id: &TaskId) -> bool {
        self.background.iter().any(|task| task.task_id == *id)
    }

    fn runs(&self, id: &TaskId) -> bool {
        self.listed(id) || self.foreground.iter().any(|task| task.task_id == *id)
    }

    /// Forgets the progress of tasks no longer running.
    fn prune(&mut self) {
        let running: Vec<TaskId> = self
            .progress
            .keys()
            .filter(|id| self.runs(id))
            .cloned()
            .collect();
        self.progress.retain(|id, _| running.contains(id));
    }

    /// Claude Code's list of background tasks, all of them. Whether that changed what runs.
    pub(crate) fn list(&mut self, mut tasks: Vec<BackgroundTask>) -> bool {
        tasks.retain(|task| !task.ambient);
        if tasks == self.background {
            return false;
        }
        self.background = tasks;
        self.prune();
        true
    }

    /// How far task `id` got. Kept even before the task shows, as its start or listing may come
    /// after; whether a running task shows something new.
    pub(crate) fn progress(&mut self, id: TaskId, progress: TaskProgress) -> bool {
        let shown = self.runs(&id);
        let changed = self.progress.get(&id) != Some(&progress);
        self.progress.insert(id, progress);
        shown && changed
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
        if patch.status.is_some_and(TaskStatus::ended) {
            self.end(id)
        } else if patch.is_backgrounded == Some(true) {
            self.leave_foreground(id)
        } else {
            false
        }
    }

    fn leave_foreground(&mut self, id: &TaskId) -> bool {
        let before = self.foreground.len();
        self.foreground.retain(|task| task.task_id != *id);
        self.foreground.len() != before
    }

    /// A task ended.
    pub(crate) fn end(&mut self, id: &TaskId) -> bool {
        let left = self.leave_foreground(id);
        let had_progress = self.progress.remove(id).is_some();
        left || had_progress
    }

    /// The turn ended: no foreground task outlives it, even one whose end went unsaid.
    pub(crate) fn end_turn(&mut self) -> bool {
        let had = !self.foreground.is_empty();
        self.foreground.clear();
        self.prune();
        had
    }

    /// The process is gone, and all it ran with it.
    pub(crate) fn clear(&mut self) {
        self.background.clear();
        self.foreground.clear();
        self.progress.clear();
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
            .map(|running| (running.task.description.clone(), running.place))
            .collect()
    }

    fn used(tokens: u64) -> TaskProgress {
        TaskProgress {
            usage: TaskUsage {
                total_tokens: tokens,
                tool_uses: 1,
                duration_ms: 1000,
            },
            last_tool: Some(ToolName::Grep),
            summary: None,
        }
    }

    fn tokens(tasks: &Tasks, id: &str) -> Option<u64> {
        let id = task(id, "local_agent").task_id;
        tasks
            .running()
            .find(|running| running.task.task_id == id)
            .and_then(|running| running.progress)
            .map(|progress| progress.usage.total_tokens)
    }

    /// A task's progress shows while it runs, also after a move to the background and when it
    /// came before the task's start; its end takes it away.
    #[test]
    fn progress_shows_while_its_task_runs() {
        let mut tasks = Tasks::default();
        let a1 = task("a1", "local_agent").task_id;
        assert!(
            !tasks.progress(a1.clone(), used(10)),
            "nothing shows it yet"
        );
        tasks.start(task("a1", "local_agent"), Some(false));
        assert_eq!(tokens(&tasks, "a1"), Some(10));
        assert!(tasks.progress(a1.clone(), used(20)));
        assert!(!tasks.progress(a1.clone(), used(20)), "nothing new");
        tasks.update(&a1, &patch(json!({ "is_backgrounded": true })));
        tasks.list(vec![task("a1", "local_agent")]);
        tasks.end_turn();
        assert_eq!(
            tokens(&tasks, "a1"),
            Some(20),
            "it runs on in the background"
        );
        assert!(tasks.end(&a1));
        tasks.list(Vec::new());
        assert_eq!(tokens(&tasks, "a1"), None);
        assert!(!tasks.progress(a1, used(30)), "an ended task shows nothing");
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
