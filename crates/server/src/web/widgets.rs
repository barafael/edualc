//! The page's small controls, as the native window's (`ui/widgets.slint`): icons, buttons, a
//! menu of choices, a meter.

use dioxus::prelude::*;

mod icons {
    include!(concat!(env!("OUT_DIR"), "/icons.rs"));
}

pub use icons::Lucide;

/// A Lucide icon, drawn in the text's colour.
#[component]
pub fn Icon(icon: Lucide, #[props(default, into)] class: String) -> Element {
    rsx! {
        span { class: "icon {class}", "aria-hidden": "true", dangerous_inner_html: icon.svg() }
    }
}

/// A button showing only an icon, named for assistive technology and its tooltip.
#[component]
pub fn IconButton(
    icon: Lucide,
    label: String,
    onclick: EventHandler<MouseEvent>,
    #[props(default, into)] class: String,
    #[props(default)] disabled: bool,
    #[props(default)] pressed: Option<bool>,
) -> Element {
    rsx! {
        button {
            class: "icon-button {class}",
            title: "{label}",
            "aria-label": "{label}",
            "aria-pressed": pressed.map(|pressed| pressed.to_string()),
            disabled,
            onclick: move |event| onclick.call(event),
            Icon { icon }
        }
    }
}

/// A button with words: the primary one filled with the accent.
#[component]
pub fn Button(
    onclick: EventHandler<MouseEvent>,
    children: Element,
    #[props(default)] primary: bool,
    #[props(default)] danger: bool,
    #[props(default)] disabled: bool,
    #[props(default)] icon: Option<Lucide>,
    #[props(default)] title: String,
) -> Element {
    let kind = if primary {
        "primary"
    } else if danger {
        "danger"
    } else {
        ""
    };
    rsx! {
        button {
            class: "button {kind}",
            title: "{title}",
            disabled,
            onclick: move |event| onclick.call(event),
            if let Some(icon) = icon {
                Icon { icon }
            }
            {children}
        }
    }
}

/// A bar of how much of something is used (0 to 1; less than 0 for unknown, which shows
/// nothing), warming as it fills: a warning from 70%, danger from 90%.
#[component]
pub fn Meter(fraction: f32, #[props(default)] label: String) -> Element {
    if fraction < 0.0 {
        return rsx! {};
    }
    let level = if fraction >= 0.9 {
        "danger"
    } else if fraction >= 0.7 {
        "warning"
    } else {
        "info"
    };
    let percent = (fraction.clamp(0.0, 1.0) * 100.0).round();
    rsx! {
        div {
            class: "meter {level}",
            role: "meter",
            "aria-label": "{label}",
            "aria-valuemin": "0",
            "aria-valuemax": "100",
            "aria-valuenow": "{percent}",
            div { class: "meter-fill", style: "width: {percent}%" }
        }
    }
}

/// A menu of choices, each with what choosing it means; the chosen one marked. It opens from
/// `button`'s content, and closes as one is chosen or the page is clicked elsewhere.
#[component]
pub fn Menu(
    label: String,
    button: Element,
    items: Vec<(String, String)>,
    chosen: Option<usize>,
    onchoose: EventHandler<usize>,
    #[props(default, into)] class: String,
    #[props(default)] upward: bool,
) -> Element {
    let mut open = use_signal(|| false);
    let direction = if upward { "up" } else { "down" };
    rsx! {
        div { class: "menu {class}",
            button {
                class: "menu-button",
                title: "{label}",
                "aria-label": "{label}",
                "aria-haspopup": "menu",
                "aria-expanded": "{open}",
                onclick: move |_| open.toggle(),
                {button}
            }
            if open() {
                div { class: "menu-backdrop", onclick: move |_| open.set(false) }
                div { class: "menu-items popup {direction}", role: "menu",
                    for (index, (title, description)) in items.into_iter().enumerate() {
                        button {
                            class: if chosen == Some(index) { "menu-item chosen" } else { "menu-item" },
                            role: "menuitemradio",
                            "aria-checked": "{chosen == Some(index)}",
                            onclick: move |_| {
                                open.set(false);
                                onchoose.call(index);
                            },
                            div { class: "menu-title", "{title}" }
                            if !description.is_empty() {
                                div { class: "menu-description", "{description}" }
                            }
                        }
                    }
                }
            }
        }
    }
}
