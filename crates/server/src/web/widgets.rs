//! The page's small controls, as the native window's (`ui/widgets.slint`): icons, buttons, a
//! meter; and moving the focus to one.

use dioxus::prelude::*;

mod icons {
    include!(concat!(env!("OUT_DIR"), "/icons.rs"));
}

pub use icons::Lucide;

/// Puts the focus on a control, as it is made (an `onmounted` handler).
pub async fn take_focus(element: MountedEvent) {
    if let Err(error) = element.set_focus(true).await {
        tracing::debug!(?error, "an element didn't take the focus");
    }
}

/// Moves the focus to `element`, if there is one.
pub fn focus(element: Option<MountedEvent>) {
    if let Some(element) = element {
        spawn(take_focus(element));
    }
}

/// How near its limit a share is, as a class (`theme.slint`'s thresholds): `danger` from nine
/// tenths, `warning` from seven, otherwise none.
pub fn level(fraction: f32) -> &'static str {
    if fraction >= 0.9 {
        "danger"
    } else if fraction >= 0.7 {
        "warning"
    } else {
        ""
    }
}

/// A share in whole percent, from 0 to 100.
pub fn percent(fraction: f32) -> f32 {
    (fraction.clamp(0.0, 1.0) * 100.0).round()
}

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
    let level = level(fraction);
    let percent = percent(fraction);
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
