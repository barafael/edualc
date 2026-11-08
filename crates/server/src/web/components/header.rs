//! Filled in by its port.
use dioxus::prelude::*;

#[component]
pub fn Header(sidebar_shown: bool, ontoggle: EventHandler<()>) -> Element {
    rsx! { header { class: "header" } }
}
