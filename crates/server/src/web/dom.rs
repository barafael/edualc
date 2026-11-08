//! Moving the focus on the page by an element's id, once the element is drawn: it may only now
//! be shown (a panel just opened, the sidebar shown again). By the browser's own script, which
//! the server's render passes over.

use dioxus::prelude::*;

/// Puts the focus on the element with `id` once it is drawn.
pub fn focus(id: &str) {
    document::eval(&format!(
        "requestAnimationFrame(() => document.getElementById({id:?})?.focus())"
    ));
}

/// Puts the focus on the text box with `id` once it is drawn, the cursor after its text.
pub fn focus_at_end(id: &str) {
    document::eval(&format!(
        "requestAnimationFrame(() => {{
            const input = document.getElementById({id:?});
            if (input) {{
                input.focus();
                input.setSelectionRange(input.value.length, input.value.length);
            }}
        }})"
    ));
}
