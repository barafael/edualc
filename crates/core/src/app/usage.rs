//! The usage panel (the CLI's `/usage` and `/context`), and bringing a session that runs in
//! another window here.

use serde::de::DeserializeOwned;
use serde_json::Value;

use super::{Acting, App, LimitUse, Request};
use crate::claude::Resume;
use crate::desktop::Running;
use crate::external::wire::{ContextReport, ControlRequest, LimitKind, SessionId, UsageReport};
use crate::ipc::{Act, ActFailure};

impl App {
    pub(super) fn show_usage(&mut self) {
        self.usage.open = true;
        self.usage.error = None;
        self.redraw.usage = true;
        self.ask_usage();
    }

    /// Asks the CLI for the panel's numbers. A session watched from another window has no
    /// process here to ask.
    pub(super) fn ask_usage(&mut self) {
        if self.watch.is_some() {
            self.usage.error = Some(
                "This session runs in another window; bring it here (or open one of your own) for its numbers."
                    .into(),
            );
            return;
        }
        self.ensure_process();
        let asked = [
            self.request(&ControlRequest::GetUsage, Request::Usage),
            self.request(&ControlRequest::GetContextUsage, Request::ContextUsage),
        ];
        self.usage.waiting = asked.iter().filter(|sent| **sent).count();
        if self.usage.waiting == 0 {
            self.usage.error = Some("Claude Code isn't running to ask.".into());
        }
    }

    pub(super) fn usage_answered(&mut self, outcome: Result<Value, String>) {
        if let Some(report) = self.usage_answer::<UsageReport>(outcome) {
            self.take_plan_limits(&report);
            self.usage.report = Some(report);
        }
    }

    pub(super) fn context_answered(&mut self, outcome: Result<Value, String>) {
        if let Some(report) = self.usage_answer::<ContextReport>(outcome) {
            self.usage.context = Some(report);
        }
    }

    /// One of the panel's answers, or why there is none.
    fn usage_answer<T: DeserializeOwned>(&mut self, outcome: Result<Value, String>) -> Option<T> {
        self.usage.waiting = self.usage.waiting.saturating_sub(1);
        self.redraw.usage = true;
        match super::answer_as(outcome) {
            Ok(answer) => Some(answer),
            Err(error) => {
                tracing::warn!(%error, "no usage numbers");
                self.usage.error = Some(error.to_string());
                None
            }
        }
    }

    /// The sidebar's bars from the fuller answer: the session's five hours and the week.
    fn take_plan_limits(&mut self, report: &UsageReport) {
        for limit in &report.rate_limits.limits {
            let used = LimitUse::from(limit);
            match limit.kind {
                LimitKind::Session => self.status.limits.five_hour = Some(used),
                LimitKind::WeeklyAll => self.status.limits.seven_day = Some(used),
                LimitKind::WeeklyScoped | LimitKind::Other => {}
            }
        }
    }

    /// Stops the watched session in its window (in the background: it may take a moment).
    pub(super) fn bring_here(&mut self) {
        let Some(watch) = &self.watch else {
            return;
        };
        let stop = Act::StopSession(Running::from(&watch.session));
        let id = watch.session.id.clone();
        self.ask_desktop(stop, Acting::Bringing(id));
    }

    /// Session `id` stopped in its window: it continues here, from its whole transcript (it
    /// may have written more since the view last read it), unless another was opened
    /// meanwhile.
    pub(super) fn brought(&mut self, id: &SessionId, result: Result<(), ActFailure>) {
        let Some(session) = self
            .watch
            .as_ref()
            .map(|watch| watch.session.clone())
            .filter(|session| &session.id == id)
        else {
            if let Err(error) = result {
                self.chat
                    .error(format!("Couldn't bring the session here: {error}"));
            }
            return;
        };
        if let Err(error) = result {
            self.chat
                .error(format!("Couldn't bring {} here: {error}", session.name));
            return;
        }
        self.watch = None;
        self.live.retain(|running| running.pid != session.pid);
        self.cwd.clone_from(&session.cwd);
        self.status.session_id = Some(session.id.clone());
        self.status.title = Some(session.name.clone());
        self.status.worktree.clone_from(&session.worktree);
        self.load_session(session.id.clone(), session.cwd.clone());
        self.start(Some(Resume::Continue(session.id)));
        self.redraw.sessions = true;
        self.redraw.menus = true;
        self.view.focus_composer();
    }
}
