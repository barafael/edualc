//! Filled in by its port.
use dioxus::prelude::*;

#[component]
pub fn SidePanel() -> Element {
    rsx! { aside { class: "side" } }
}

#[component]
pub fn PeekPanel() -> Element {
    rsx! { aside { class: "side" } }
}

#[component]
pub fn Usage() -> Element {
    rsx! {}
}

#[component]
pub fn Settings() -> Element {
    rsx! {}
}
