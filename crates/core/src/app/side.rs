//! Side questions (the CLI's `/btw`): asked beside the conversation and answered with its
//! context, without joining it or running tools. They wait in a panel that can be folded away
//! while something else needs the user, and brought back.

use serde_json::Value;

use super::{App, Request};
use crate::claude::Resume;
use crate::external::wire::{ControlRequest, SideAnswer, SideTurn};

#[derive(Debug, Default)]
pub struct SideChat {
    pub exchanges: Vec<Exchange>,
    pub view: SideView,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum SideView {
    #[default]
    Hidden,
    Shown,
    /// Out of sight, but kept: a button brings it back, marked when an answer came meanwhile.
    Folded {
        unseen: bool,
    },
}

#[derive(Debug)]
pub struct Exchange {
    pub question: String,
    pub answer: Answer,
}

#[derive(Debug)]
pub enum Answer {
    Pending,
    /// The API asked to wait; the CLI tries again.
    Retrying,
    Answered(String),
    Failed(String),
}

impl SideChat {
    /// The answered questions, as the CLI takes them for a follow-up.
    fn history(&self) -> Vec<SideTurn> {
        self.answered()
            .map(|(question, response)| SideTurn {
                question: question.to_owned(),
                response: response.to_owned(),
            })
            .collect()
    }

    fn answered(&self) -> impl Iterator<Item = (&str, &str)> {
        self.exchanges
            .iter()
            .filter_map(|exchange| match &exchange.answer {
                Answer::Answered(response) => Some((exchange.question.as_str(), response.as_str())),
                Answer::Pending | Answer::Retrying | Answer::Failed(_) => None,
            })
    }

    /// What was asked and answered, for the conversation (or a branch of it) to take up.
    fn transcript(&self) -> String {
        let exchanges: Vec<String> = self
            .answered()
            .map(|(question, response)| format!("Q: {question}\nA: {response}"))
            .collect();
        format!(
            "I asked on the side while we worked:\n\n{}",
            exchanges.join("\n\n")
        )
    }

    pub fn has_answers(&self) -> bool {
        self.answered().next().is_some()
    }

    pub fn pending(&self) -> bool {
        self.exchanges
            .iter()
            .any(|exchange| matches!(exchange.answer, Answer::Pending | Answer::Retrying))
    }
}

impl App {
    /// Asks `question` on the side; an empty one only opens the panel to type it there.
    pub(super) fn ask_side(&mut self, question: &str) {
        let question = question.trim();
        self.side.view = SideView::Shown;
        self.redraw.side = true;
        if question.is_empty() {
            self.view.focus_side();
            return;
        }
        if self.watch.is_some() {
            self.side.exchanges.push(Exchange {
                question: question.to_owned(),
                answer: Answer::Failed(
                    "This session runs in another window. Continue it here as a copy to ask on the side."
                        .into(),
                ),
            });
            return;
        }
        self.ensure_process();
        let index = self.side.exchanges.len();
        let request = ControlRequest::SideQuestion {
            question: question.to_owned(),
            history: self.side.history(),
        };
        let answer = if self.request(&request, Request::Side(index)) {
            Answer::Pending
        } else {
            Answer::Failed("Claude Code isn't running; send a message to start it.".into())
        };
        self.side.exchanges.push(Exchange {
            question: question.to_owned(),
            answer,
        });
    }

    pub(super) fn side_answered(&mut self, index: usize, outcome: Result<Value, String>) {
        let answer = match super::answer_as::<SideAnswer>(outcome) {
            // The CLI's own notice ("No answer available"), not the model's answer.
            Ok(answer) if answer.synthetic => Answer::Failed(answer.response),
            Ok(answer) => Answer::Answered(answer.response),
            Err(error) => Answer::Failed(error.to_string()),
        };
        if let Some(exchange) = self.side.exchanges.get_mut(index) {
            exchange.answer = answer;
        }
        if let SideView::Folded { unseen } = &mut self.side.view {
            *unseen = true;
        }
        self.redraw.side = true;
    }

    pub(super) fn side_retrying(&mut self, index: usize) {
        if let Some(exchange) = self.side.exchanges.get_mut(index) {
            exchange.answer = Answer::Retrying;
            self.redraw.side = true;
        }
    }

    /// Folds the panel away, or brings it back (an empty one, if there was none).
    pub(super) fn toggle_side(&mut self) {
        self.side.view = match self.side.view {
            SideView::Shown => SideView::Folded { unseen: false },
            SideView::Hidden | SideView::Folded { .. } => SideView::Shown,
        };
        if self.side.view == SideView::Shown {
            self.view.focus_side();
        }
        self.redraw.side = true;
    }

    pub(super) fn dismiss_side(&mut self) {
        self.side = SideChat::default();
        self.redraw.side = true;
    }

    pub(super) fn copy_side(&mut self, index: usize) {
        if let Some(Exchange {
            answer: Answer::Answered(text),
            ..
        }) = self.side.exchanges.get(index)
        {
            self.view.copy(text);
        }
    }

    /// Puts the side questions and their answers into the draft, to send on with the
    /// conversation.
    pub(super) fn side_to_conversation(&mut self) {
        if !self.side.has_answers() {
            return;
        }
        self.view.append_draft(&self.side.transcript());
        self.view.focus_composer();
    }

    /// Branches the conversation into a new session that takes the side questions up, as the
    /// terminal's `/btw` does: the session so far stays as it was.
    pub(super) fn branch_side(&mut self) {
        let Some(id) = self.status.session_id.clone() else {
            self.chat
                .error("There is no conversation to branch yet; send a first message.");
            return;
        };
        if self.status.turns > 0 || !self.side.has_answers() {
            return;
        }
        let first = self
            .side
            .exchanges
            .first()
            .map_or("", |exchange| exchange.question.as_str());
        let title = format!("btw: {}", crate::tools::truncate_chars(first, 72));
        let prompt = format!("{}\n\nLet's continue from there.", self.side.transcript());
        self.stop();
        self.status.session_id = None;
        self.status.title = Some(title);
        self.start(Some(Resume::Fork(id)));
        self.side = SideChat::default();
        self.redraw.side = true;
        self.redraw.sessions = true;
        self.send(&prompt);
    }

    /// The process stopped: answers it owed won't come.
    pub(super) fn side_lost(&mut self) {
        for exchange in &mut self.side.exchanges {
            if matches!(exchange.answer, Answer::Pending | Answer::Retrying) {
                exchange.answer = Answer::Failed("Claude Code stopped before answering.".into());
                self.redraw.side = true;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn follow_ups_carry_the_answered_questions() {
        let side = SideChat {
            exchanges: vec![
                Exchange {
                    question: "Which number?".into(),
                    answer: Answer::Answered("42".into()),
                },
                Exchange {
                    question: "Lost?".into(),
                    answer: Answer::Failed("stopped".into()),
                },
            ],
            ..SideChat::default()
        };
        assert_eq!(
            side.history(),
            vec![SideTurn {
                question: "Which number?".into(),
                response: "42".into(),
            }]
        );
        assert!(side.transcript().ends_with("Q: Which number?\nA: 42"));
        assert!(!side.pending());
    }
}
