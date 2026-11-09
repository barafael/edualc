//! The window's areas for the keyboard, as the Slint window's (`ui/app.slint`'s `next-area`):
//! the sidebar, the conversation, what is written (what Claude asks, or the composer) and the
//! side panel. F6 goes on to the next shown, Shift+F6 back; each area has a home the focus goes
//! to. The conversation, focused, scrolls with the arrow keys, Page Up and Down, Space, Home and
//! End; Claude's question's options are gone through with the arrow keys.

use gpui::{App, Context, FocusHandle, KeyDownEvent, ListOffset, Window, px};

use super::Shell;
use crate::app::Action;
use crate::present::model::{FocusArea, PromptKind, SideView};

/// Each area's focus, which whatever in it is focused is within; and the question's options'.
pub(crate) struct Areas {
    pub(crate) sidebar: FocusHandle,
    pub(crate) conversation: FocusHandle,
    pub(crate) writing: FocusHandle,
    pub(crate) side: FocusHandle,
    pub(crate) options: FocusHandle,
}

impl Areas {
    pub(crate) fn new(cx: &App) -> Self {
        Self {
            sidebar: cx.focus_handle(),
            conversation: cx.focus_handle(),
            writing: cx.focus_handle(),
            side: cx.focus_handle(),
            options: cx.focus_handle(),
        }
    }
}

/// The area after `from` (or before, not `forward`), of those shown: the sidebar while it is,
/// the side panel while it is. From nowhere, the round starts at what is written.
pub(crate) fn next_area(
    from: Option<FocusArea>,
    forward: bool,
    sidebar: bool,
    side: bool,
) -> FocusArea {
    let shown: Vec<FocusArea> = [
        FocusArea::Sidebar,
        FocusArea::Conversation,
        FocusArea::Writing,
        FocusArea::Side,
    ]
    .into_iter()
    .filter(|area| match area {
        FocusArea::Sidebar => sidebar,
        FocusArea::Side => side,
        FocusArea::Conversation | FocusArea::Writing => true,
    })
    .collect();
    let at = from
        .and_then(|from| shown.iter().position(|area| *area == from))
        .or_else(|| shown.iter().position(|area| *area == FocusArea::Writing))
        .unwrap_or_default();
    let next = if forward {
        (at + 1) % shown.len()
    } else {
        (at + shown.len() - 1) % shown.len()
    };
    shown.get(next).copied().unwrap_or(FocusArea::Writing)
}

impl Shell {
    /// The area the focus is in, if any.
    fn area(&self, window: &Window, cx: &App) -> Option<FocusArea> {
        let areas = &self.areas;
        [
            (&areas.sidebar, FocusArea::Sidebar),
            (&areas.conversation, FocusArea::Conversation),
            (&areas.writing, FocusArea::Writing),
            (&areas.side, FocusArea::Side),
        ]
        .into_iter()
        .find(|(handle, _)| handle.contains_focused(window, cx))
        .map(|(_, area)| area)
    }

    /// F6 (`forward`) or Shift+F6: the focus to the next area's home.
    pub(crate) fn step_area(&self, forward: bool, window: &mut Window, cx: &mut Context<Self>) {
        let side = self.side.view == SideView::Shown && !self.peek.open;
        let area = next_area(self.area(window, cx), forward, self.sidebar_shown, side);
        self.go_to(area, window, cx);
    }

    /// Puts the focus where `area` starts: the sessions' filter, the conversation, what Claude
    /// asks for first or else the composer, the side panel's field.
    fn go_to(&self, area: FocusArea, window: &mut Window, cx: &mut Context<Self>) {
        let fields = &self.fields;
        match area {
            FocusArea::Sidebar => fields
                .filter
                .update(cx, |state, cx| state.focus(window, cx)),
            FocusArea::Conversation => window.focus(&self.areas.conversation, cx),
            FocusArea::Writing => match self.prompt.kind {
                PromptKind::Question => window.focus(&self.areas.options, cx),
                PromptKind::Tool => fields
                    .feedback
                    .update(cx, |state, cx| state.focus(window, cx)),
                PromptKind::Plan => fields
                    .overall
                    .update(cx, |state, cx| state.focus(window, cx)),
                PromptKind::None => fields.draft.update(cx, |state, cx| state.focus(window, cx)),
            },
            FocusArea::Side => fields.side.update(cx, |state, cx| state.focus(window, cx)),
        }
        cx.notify();
    }

    /// The conversation's keys, while it has the focus itself: the arrows a few lines, Page Up
    /// and Down (and Space) most of a view, Home and End to either end.
    pub(crate) fn conversation_key(
        &mut self,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.areas.conversation.is_focused(window) {
            return;
        }
        let keys = &event.keystroke;
        if keys.modifiers.control || keys.modifiers.alt {
            return;
        }
        let line = px(4.0 * self.page.text_size);
        let page = self.list.viewport_bounds().size.height * 0.9;
        match keys.key.as_str() {
            "down" => self.list.scroll_by(line),
            "up" => self.list.scroll_by(-line),
            "pagedown" | "space" => self.list.scroll_by(page),
            "pageup" => self.list.scroll_by(-page),
            "home" => self.list.scroll_to(ListOffset::default()),
            "end" => self.scroll_to_end(),
            _ => return,
        }
        cx.stop_propagation();
        cx.notify();
    }

    /// The question's options' keys: Up and Down go through them, Space picks one, Enter picks
    /// it (of one to pick, the one at hand, and goes on to the next question), Left and Right
    /// go to the question before or after.
    pub(crate) fn options_key(
        &mut self,
        event: &KeyDownEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let keys = &event.keystroke;
        if keys.modifiers.control || keys.modifiers.alt {
            return;
        }
        let count = self.prompt.questions.len();
        let shown = self.question_shown.min(count.saturating_sub(1));
        let Some(question) = self.prompt.questions.get(shown) else {
            return;
        };
        let options = question.options.len();
        let at = self.option_at.min(options.saturating_sub(1));
        let pick = |shell: &Self| {
            shell.actions.send(Action::ToggleOption {
                question: shown,
                option: at,
            });
        };
        match keys.key.as_str() {
            "down" => self.option_at = (at + 1).min(options.saturating_sub(1)),
            "up" => self.option_at = at.saturating_sub(1),
            "space" => pick(self),
            "enter" if question.multi => pick(self),
            "enter" => {
                if !question
                    .options
                    .get(at)
                    .is_some_and(|option| option.selected)
                {
                    pick(self);
                }
                self.show_question(shown + 1);
            }
            "right" => self.show_question(shown + 1),
            "left" => self.show_question(shown.saturating_sub(1)),
            _ => return,
        }
        cx.stop_propagation();
        cx.notify();
    }

    /// Question `at` shown (the last, past it), from its first option.
    pub(crate) fn show_question(&mut self, at: usize) {
        self.question_shown = at.min(self.prompt.questions.len().saturating_sub(1));
        self.option_at = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// F6 goes round the areas shown, as in the Slint window; Shift+F6 the other way.
    #[test]
    fn f6_goes_round_the_areas_shown() {
        use FocusArea::{Conversation, Side, Sidebar, Writing};
        assert_eq!(next_area(Some(Sidebar), true, true, false), Conversation);
        assert_eq!(next_area(Some(Writing), true, true, false), Sidebar);
        assert_eq!(next_area(Some(Writing), true, true, true), Side);
        assert_eq!(next_area(Some(Side), true, true, true), Sidebar);
        assert_eq!(next_area(Some(Writing), true, false, false), Conversation);
        assert_eq!(next_area(Some(Conversation), false, false, false), Writing);
        assert_eq!(next_area(Some(Sidebar), false, true, true), Side);
        assert_eq!(next_area(None, true, true, false), Sidebar);
        assert_eq!(next_area(Some(Side), true, true, false), Sidebar);
    }
}
