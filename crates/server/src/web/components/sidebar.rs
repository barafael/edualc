//! Filled in by its port.
use dioxus::prelude::*;

#[component]
pub fn Sidebar(onhide: EventHandler<()>) -> Element {
    rsx! { aside { class: "sidebar", "Sessions" } }
}
