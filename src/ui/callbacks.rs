//! The window's callbacks: each sends what was done to the app as an [`Action`], and holds
//! nothing but the way there.

use async_channel::Sender;
use slint::ComponentHandle;

use crate::app::{Action, Event, PermissionMode};
use crate::cjk::CjkLanguage;
use crate::external::wire::{RequestId, SessionId};
use crate::generated::{
    AppWindow, Clipboard, Composer, Focus, HanLanguage, LineBreak, Peek, Preferences, Prompt,
    SandboxAccess, SendKey, Session, Sessions, Side, Transcript, Usage,
};
use crate::ipc;
use crate::settings::{self, Zoom};

/// A way to the app, for a callback to keep.
#[derive(Clone)]
struct Actions(Sender<Event>);

impl Actions {
    fn send(&self, action: Action) {
        if self.0.try_send(Event::Action(action)).is_err() {
            tracing::debug!("an action after the app stopped");
        }
    }

    /// A callback that sends `action`.
    fn on(&self, action: fn() -> Action) -> impl Fn() + 'static {
        let actions = self.clone();
        move || actions.send(action())
    }

    /// A callback taking a row's index from Slint, which counts in `i32`.
    fn on_index(&self, action: fn(usize) -> Action) -> impl Fn(i32) + 'static {
        let actions = self.clone();
        move |index| match usize::try_from(index) {
            Ok(index) => actions.send(action(index)),
            Err(error) => tracing::debug!(%error, index, "a negative row index"),
        }
    }

    /// A callback taking a row's index and a text: what was typed for it.
    fn on_index_text(
        &self,
        action: fn(usize, String) -> Action,
    ) -> impl Fn(i32, slint::SharedString) + 'static {
        let actions = self.clone();
        move |index, text| match usize::try_from(index) {
            Ok(index) => actions.send(action(index, text.into())),
            Err(error) => tracing::debug!(%error, index, "a negative row index"),
        }
    }

    /// A callback taking two indexes from Slint: a row's, and one within it.
    fn on_indexes(&self, action: fn(usize, usize) -> Action) -> impl Fn(i32, i32) + 'static {
        let actions = self.clone();
        move |outer, inner| match (usize::try_from(outer), usize::try_from(inner)) {
            (Ok(outer), Ok(inner)) => actions.send(action(outer, inner)),
            (Err(error), _) | (_, Err(error)) => {
                tracing::debug!(%error, outer, inner, "a negative row index");
            }
        }
    }

    fn on_text(&self, action: fn(String) -> Action) -> impl Fn(slint::SharedString) + 'static {
        let actions = self.clone();
        move |text| actions.send(action(text.into()))
    }

    /// A callback taking the id of the request it answers.
    fn on_request(
        &self,
        action: fn(RequestId) -> Action,
    ) -> impl Fn(slint::SharedString) + 'static {
        let actions = self.clone();
        move |request| actions.send(action(RequestId::from(String::from(request))))
    }

    /// A callback taking a session's id, as the lists hold it.
    fn on_session(
        &self,
        action: fn(SessionId) -> Action,
    ) -> impl Fn(slint::SharedString) + 'static {
        let actions = self.clone();
        move |id| match SessionId::try_from(String::from(id)) {
            Ok(id) => actions.send(action(id)),
            Err(error) => tracing::warn!(%error, "a session the lists shouldn't hold"),
        }
    }
}

pub(crate) fn wire(window: &AppWindow, events: &Sender<Event>) {
    let actions = Actions(events.clone());

    let composer = window.global::<Composer>();
    composer.on_send(actions.on_text(Action::Send));
    composer.on_draft_edited(actions.on_text(Action::DraftEdited));
    composer.on_pick_command(actions.on_index(Action::PickCommand));
    composer.on_run_command(actions.on_index(Action::RunCommand));
    composer.on_height_chosen({
        let actions = actions.clone();
        // Zero (or what doesn't fit a `u16`) goes back to fitting the text.
        move |height_px| {
            let kept = u16::try_from(height_px)
                .ok()
                .filter(|height_px| *height_px > 0);
            actions.send(Action::ComposerHeight(kept));
        }
    });
    composer.on_paste_image(actions.on(|| Action::PasteImage));
    composer.on_remove_image(actions.on_index(Action::RemoveImage));
    composer.on_break_line(|text, from, to| break_line(&text, from, to));
    composer.on_toggle_shell(actions.on(|| Action::ToggleShell));

    let session = window.global::<Session>();
    session.on_interrupt(actions.on(|| Action::Interrupt));
    session.on_choose_model(actions.on_index(Action::ChooseModel));
    session.on_choose_mode(actions.on_index(Action::ChooseMode));
    session.on_open_url(actions.on_text(Action::OpenUrl));
    session.on_continue_here(actions.on(|| Action::ContinueHere));
    session.on_go_to_window(actions.on(|| Action::GoToWindow));
    session.on_bring_here(actions.on(|| Action::BringHere));
    session.on_trust_folder(actions.on(|| Action::TrustFolder));
    session.on_start_without_folder_settings(actions.on(|| Action::StartWithoutFolderSettings));
    session.on_toggle_sandbox(actions.on(|| Action::ToggleSandbox));
    session.on_toggle_sandbox_desktop(actions.on(|| Action::ToggleSandboxDesktop));
    session.on_toggle_sandbox_hardware(actions.on(|| Action::ToggleSandboxHardware));
    session.on_show_usage(actions.on(|| Action::ShowUsage));

    let usage = window.global::<Usage>();
    usage.on_close(actions.on(|| Action::HideUsage));
    usage.on_refresh(actions.on(|| Action::ShowUsage));
    session.on_open_worktree(actions.on_index(Action::OpenWorktree));
    session.on_switch_look(actions.on(|| Action::SwitchLook));
    window.global::<Preferences>().on_choose_send_key({
        let actions = actions.clone();
        move |key| {
            actions.send(Action::SendWith(match key {
                SendKey::Enter => settings::SendKey::Enter,
                SendKey::CtrlEnter => settings::SendKey::CtrlEnter,
            }));
        }
    });
    let access_of = |access| match access {
        SandboxAccess::Read => ipc::SandboxAccess::Read,
        SandboxAccess::Write => ipc::SandboxAccess::Write,
    };
    window.global::<Preferences>().on_add_sandbox_folder({
        let actions = actions.clone();
        move |access| actions.send(Action::AddSandboxFolder(access_of(access)))
    });
    window.global::<Preferences>().on_forget_sandbox_folder({
        let actions = actions.clone();
        move |access, folder| {
            actions.send(Action::ForgetSandboxFolder {
                access: access_of(access),
                folder: folder.into(),
            });
        }
    });
    window.global::<Preferences>().on_choose_han_language({
        let actions = actions.clone();
        move |language| {
            actions.send(Action::HanLanguage(match language {
                HanLanguage::Japanese => CjkLanguage::Japanese,
                HanLanguage::SimplifiedChinese => CjkLanguage::SimplifiedChinese,
                HanLanguage::TraditionalChinese => CjkLanguage::TraditionalChinese,
                HanLanguage::Korean => CjkLanguage::Korean,
            }));
        }
    });
    session.on_zoom({
        let actions = actions.clone();
        move |direction| {
            let zoom = match direction.signum() {
                1 => Zoom::In,
                -1 => Zoom::Out,
                _ => Zoom::Reset,
            };
            actions.send(Action::Zoom(zoom));
        }
    });

    let side = window.global::<Side>();
    side.on_ask(actions.on_text(Action::AskSide));
    side.on_toggle(actions.on(|| Action::ToggleSide));
    side.on_dismiss(actions.on(|| Action::DismissSide));
    side.on_copy(actions.on_index(Action::CopySide));
    side.on_to_conversation(actions.on(|| Action::SideToConversation));
    side.on_branch(actions.on(|| Action::BranchSide));

    let peek = window.global::<Peek>();
    peek.on_follow(actions.on_text(|task| Action::OpenPeek(task.into())));
    peek.on_close(actions.on(|| Action::ClosePeek));

    let sessions = window.global::<Sessions>();
    sessions.on_new_session(actions.on(|| Action::NewSession));
    sessions.on_open(actions.on_session(Action::OpenSession));
    sessions.on_watch(actions.on_session(Action::Watch));
    sessions.on_open_project(actions.on_text(Action::OpenProject));
    sessions.on_browse_project(actions.on(|| Action::BrowseProject));
    sessions.on_refresh_projects(actions.on(|| Action::ListProjects));
    sessions.on_filter_edited(actions.on_text(Action::FilterSessions));

    let transcript = window.global::<Transcript>();
    transcript.on_toggle(actions.on_index(Action::Toggle));
    transcript.on_toggle_group(actions.on_index(Action::ToggleRun));
    transcript.on_copy_entry(actions.on_index(Action::CopyEntry));

    let prompt = window.global::<Prompt>();
    prompt.on_allow(actions.on_request(Action::Allow));
    prompt.on_allow_always(actions.on_request(Action::AllowAlways));
    prompt.on_deny({
        let actions = actions.clone();
        move |request, feedback| {
            actions.send(Action::Deny {
                request: RequestId::from(String::from(request)),
                feedback: feedback.into(),
            });
        }
    });
    prompt.on_answer(actions.on_request(Action::AnswerQuestions));
    prompt.on_edit_comment(actions.on_index(Action::EditComment));
    prompt.on_cancel_comment(actions.on(|| Action::CancelComment));
    prompt.on_toggle_option(
        actions.on_indexes(|question, option| Action::ToggleOption { question, option }),
    );
    prompt
        .on_set_other(actions.on_index_text(|question, text| Action::SetOther { question, text }));
    prompt
        .on_set_notes(actions.on_index_text(|question, text| Action::SetNotes { question, text }));
    prompt.on_set_comment(
        actions.on_index_text(|section, text| Action::SetComment { section, text }),
    );
    prompt.on_approve_plan({
        let actions = actions.clone();
        move |request, accept_edits, overall| {
            let mode = if accept_edits {
                PermissionMode::AcceptEdits
            } else {
                PermissionMode::Default
            };
            actions.send(Action::ApprovePlan {
                request: RequestId::from(String::from(request)),
                mode,
                overall: overall.into(),
            });
        }
    });

    window
        .global::<Clipboard>()
        .on_copy(actions.on_text(Action::Copy));

    // An area fence sends the focus round (see `AreaFence`): from the fence it moved to, a Tab
    // (or Shift+Tab) goes on into the area, once the focus event at hand is done.
    let weak = window.as_weak();
    window.global::<Focus>().on_step(move |forward| {
        let weak = weak.clone();
        slint::Timer::single_shot(std::time::Duration::ZERO, move || {
            let Some(window) = weak.upgrade() else {
                return;
            };
            let key = if forward {
                slint::platform::Key::Tab
            } else {
                slint::platform::Key::Backtab
            };
            let text = slint::SharedString::from(key);
            window
                .window()
                .dispatch_event(slint::platform::WindowEvent::KeyPressed { text: text.clone() });
            window
                .window()
                .dispatch_event(slint::platform::WindowEvent::KeyReleased { text });
            window.global::<Focus>().set_going_round(false);
        });
    });

    // Closing ends the process first (it gets a moment to tidy up), then the event loop.
    window.window().on_close_requested(move || {
        actions.send(Action::Quit);
        slint::CloseRequestResponse::HideWindow
    });
}

/// `text` with a line break in place of what lies between the byte offsets `from` and `to`, as
/// the text box gives them (one outside the text, or inside a character, is moved to the
/// boundary before it); and the caret's offset after the break.
fn break_line(text: &str, from: i32, to: i32) -> LineBreak {
    let at = |offset: i32| text.floor_char_boundary(usize::try_from(offset).unwrap_or(0));
    let (start, end) = (at(from.min(to)), at(from.max(to)));
    let mut broken = String::with_capacity(text.len() + 1);
    broken.push_str(text.get(..start).unwrap_or(text));
    broken.push('\n');
    broken.push_str(text.get(end..).unwrap_or(""));
    LineBreak {
        text: broken.into(),
        caret: i32::try_from(start + 1).unwrap_or(i32::MAX),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn broken(text: &str, from: i32, to: i32) -> (String, i32) {
        let LineBreak { text, caret } = break_line(text, from, to);
        (text.into(), caret)
    }

    /// A break goes in at the caret, or in place of the selection; the caret after it. Offsets
    /// are bytes: one inside a character, or past the end, goes to the boundary before.
    #[test]
    fn a_line_break_replaces_the_selection_at_the_caret() {
        assert_eq!(broken("ab", 1, 1), ("a\nb".to_owned(), 2));
        assert_eq!(broken("abcd", 3, 1), ("a\nd".to_owned(), 2));
        assert_eq!(broken("", 0, 0), ("\n".to_owned(), 1));
        // 今 is three bytes: an offset inside it goes before it.
        assert_eq!(broken("今日", 4, 4), ("今\n日".to_owned(), 4));
        assert_eq!(broken("今日", 2, 2), ("\n今日".to_owned(), 1));
        assert_eq!(broken("ab", 99, -5), ("\n".to_owned(), 1));
    }
}
