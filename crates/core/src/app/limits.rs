//! The plan's usage limits as time passes: the sidebar's bars asked for again when a limit
//! starts over, and a turn the limit stopped continued then, as Claude Code's terminal does.
//! Behind a pipe Claude Code does neither.

use jiff::{SignedDuration, Timestamp};

use super::{App, Event, LimitWait, Request};
use crate::external::wire::{ControlRequest, UsageReport};
use crate::present::Frontend;

/// What Claude Code's terminal sends, unseen, once the limit has reset (Claude Code 2.1.x).
const CONTINUE_PROMPT: &str = "Your claude.ai usage limit has reset. Continue the task you were working on when the limit was reached; do not repeat work that is already complete.";

/// How long after a limit's reset time it is asked about, or the turn goes on: the plan's
/// numbers take a moment to catch up.
const AFTER_RESET: SignedDuration = SignedDuration::from_secs(30);

impl<F: Frontend> App<F> {
    /// A turn ended on the plan's usage limit: Claude Code is asked when it resets.
    pub(super) fn limit_reached(&mut self) {
        self.status.limit_wait = LimitWait::Asking;
        if !self.ask_limits() {
            self.limits_reported(None);
        }
    }

    /// Asks Claude Code for the plan's limits, if it runs; `usage_answered` takes the answer.
    fn ask_limits(&mut self) -> bool {
        let asked =
            self.process.is_some() && self.request(ControlRequest::GetUsage, Request::Usage);
        if asked {
            self.usage.waiting += 1;
        }
        asked
    }

    /// Claude Code told the plan's limits, or couldn't: a turn waiting to hear when the limit
    /// resets goes on then.
    pub(super) fn limits_reported(&mut self, report: Option<&UsageReport>) {
        if self.status.limit_wait == LimitWait::Asking {
            match report.and_then(reset_of_reached) {
                Some(at) => {
                    let at = at.max(Timestamp::now() + AFTER_RESET);
                    self.status.limit_wait = LimitWait::Until(at);
                    self.chat.info(format!(
                        "Continuing automatically when the limit resets ({}). Esc or Stop cancels that.",
                        crate::present::clock(at)
                    ));
                }
                None => {
                    self.status.limit_wait = LimitWait::Idle;
                    self.chat.info(
                        "Claude Code didn't say when the limit resets: send a prompt to go on once it has.",
                    );
                }
            }
            self.view.scroll_to_end();
        }
        self.set_limit_reset();
    }

    /// The user doesn't want the turn to go on by itself.
    pub(super) fn cancel_limit_wait(&mut self) {
        self.status.limit_wait = LimitWait::Idle;
        self.chat
            .info("Not continuing automatically. Send a prompt to go on.");
        self.view.scroll_to_end();
        self.set_limit_reset();
    }

    /// Sets the timer for the next time a limit starts over: the bars' or the one a turn
    /// waits for.
    pub(super) fn set_limit_reset(&mut self) {
        let now = Timestamp::now();
        let limits = &self.status.limits;
        let waited = match self.status.limit_wait {
            LimitWait::Until(at) => Some(at),
            LimitWait::Idle | LimitWait::Asking => None,
        };
        let next = [limits.five_hour, limits.seven_day]
            .into_iter()
            .flatten()
            .filter_map(|limit| limit.resets_at)
            .map(|at| at + AFTER_RESET)
            .chain(waited)
            .filter(|at| *at > now)
            .min();
        self.limit_reset.stop();
        let Some(at) = next else {
            return;
        };
        let wait = std::time::Duration::try_from(now.duration_until(at)).unwrap_or_default();
        self.limit_reset
            .start(wait, self.events.clone(), || Event::LimitReset);
    }

    /// A limit started over: the bars are asked for again, and a turn that waited for it
    /// goes on. The timer fires once: it is set again for the next reset.
    pub(super) fn limit_reset(&mut self) {
        self.limit_reset.stop();
        if let LimitWait::Until(at) = self.status.limit_wait
            && at <= Timestamp::now()
        {
            self.continue_after_limit();
        }
        if !self.ask_limits() {
            self.set_limit_reset();
        }
    }

    /// Sends what Claude Code's terminal sends once the limit has reset. The conversation shows
    /// that it went on, not the prompt.
    fn continue_after_limit(&mut self) {
        self.status.limit_wait = LimitWait::Idle;
        if self.watch.is_some() || !self.ensure_process() {
            return;
        }
        self.chat.info("The usage limit has reset: continuing.");
        self.start_turn(&[], CONTINUE_PROMPT);
    }
}

/// When the reached limits of the plan reset: the latest, as the turn needs all of them;
/// none if no limit is reached, or none says when.
fn reset_of_reached(report: &UsageReport) -> Option<Timestamp> {
    report
        .rate_limits
        .limits
        .iter()
        .filter(|limit| limit.percent >= 100.0)
        .filter_map(|limit| limit.resets_at)
        .max()
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn report(limits: &serde_json::Value) -> UsageReport {
        serde_json::from_value(json!({ "rate_limits": { "limits": limits } }))
            .expect("a usage report")
    }

    /// The turn waits for the reached limit that resets last; limits not reached don't hold
    /// it up, and with none reached, or none saying when, there is nothing to wait for.
    #[test]
    fn a_turn_waits_for_the_last_reached_limit_to_reset() {
        let both = report(&json!([
            { "kind": "session", "percent": 100.0, "resets_at": "2026-11-13T18:30:00Z", "scope": null },
            { "kind": "weekly_all", "percent": 100.0, "resets_at": "2026-11-15T09:00:00Z", "scope": null },
            { "kind": "weekly_scoped", "percent": 40.0, "resets_at": "2026-11-16T09:00:00Z", "scope": null },
        ]));
        assert_eq!(
            reset_of_reached(&both),
            Some("2026-11-15T09:00:00Z".parse().expect("a time"))
        );
        let session = report(&json!([
            { "kind": "session", "percent": 100.0, "resets_at": "2026-11-13T18:30:00Z", "scope": null },
            { "kind": "weekly_all", "percent": 85.0, "resets_at": "2026-11-15T09:00:00Z", "scope": null },
        ]));
        assert_eq!(
            reset_of_reached(&session),
            Some("2026-11-13T18:30:00Z".parse().expect("a time"))
        );
        let none_reached = report(&json!([
            { "kind": "session", "percent": 99.0, "resets_at": "2026-11-13T18:30:00Z", "scope": null },
        ]));
        assert_eq!(reset_of_reached(&none_reached), None);
        let unsaid = report(&json!([
            { "kind": "session", "percent": 100.0, "resets_at": null, "scope": null },
        ]));
        assert_eq!(reset_of_reached(&unsaid), None);
    }
}
