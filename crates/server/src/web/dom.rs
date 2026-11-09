//! Moving the focus on the page by an element's id, once the element is drawn: it may only now
//! be shown (a panel just opened, the sidebar shown again). By the browser's own script, which
//! the server's render passes over, and which the Android app's WebView runs alike. And waiting,
//! on whatever clock the page has.

use dioxus::prelude::*;

/// Runs `script` on the element with `id`, as `element`, once it is drawn; if it is there.
fn on_drawn(id: &str, script: &str) {
    document::eval(&format!(
        "requestAnimationFrame(() => {{
            const element = document.getElementById({id:?});
            if (element) {{ {script} }}
        }})"
    ));
}

/// The focus, the caret after the text if it is a text box.
const FOCUS_AT_END: &str = "element.focus();
    if (element.setSelectionRange) {
        element.setSelectionRange(element.value.length, element.value.length);
    }";

/// On a touch screen a text field the user didn't tap takes no focus: it would bring the
/// keyboard up over what just opened. It takes it as it is tapped.
const UNLESS_TYPED_ON_TOUCH: &str = "if (element.matches('input, textarea, [contenteditable]')
        && matchMedia('(pointer: coarse)').matches) { return; }";

/// Puts the focus on the element with `id` once it is drawn.
pub fn focus(id: &str) {
    on_drawn(id, "element.focus();");
}

/// Puts the focus on the text box with `id` once it is drawn, the cursor after its text.
pub fn focus_at_end(id: &str) {
    on_drawn(id, FOCUS_AT_END);
}

/// Puts the focus on the element with `id` as something opens by itself (a prompt): not on a
/// text field on a touch screen (see [`UNLESS_TYPED_ON_TOUCH`]).
pub fn grab_focus(id: &str) {
    on_drawn(id, &format!("{UNLESS_TYPED_ON_TOUCH} element.focus();"));
}

/// As [`grab_focus`], the caret after the text (a session opened, its draft in the composer).
pub fn grab_focus_at_end(id: &str) {
    on_drawn(id, &format!("{UNLESS_TYPED_ON_TOUCH} {FOCUS_AT_END}"));
}

/// Moves the focus to the next of a menu's items (`forward`) or the one before, staying at the
/// ends.
pub fn focus_next(forward: bool) {
    let step = if forward {
        "nextElementSibling"
    } else {
        "previousElementSibling"
    };
    document::eval(&format!(
        "const next = document.activeElement?.{step};
        if (next && next.getAttribute('role') === 'menuitem') {{ next.focus(); }}"
    ));
}

/// Waits `ms` milliseconds: on the browser's timers, or the app's runtime's. Rendered on the
/// server, nothing waits.
pub async fn sleep(ms: u32) {
    #[cfg(feature = "web")]
    gloo_timers::future::TimeoutFuture::new(ms).await;
    #[cfg(feature = "mobile")]
    tokio::time::sleep(std::time::Duration::from_millis(u64::from(ms))).await;
    #[cfg(not(any(feature = "web", feature = "mobile")))]
    let _: u32 = ms;
}
