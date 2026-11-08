//! Filled in by its port.
use dioxus::prelude::*;

#[component]
pub fn Composer() -> Element {
    rsx! { div { class: "composer" } }
}

#[component]
pub fn CommandList() -> Element {
    rsx! { div { class: "commands" } }
}
