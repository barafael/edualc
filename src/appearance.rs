//! How text looks: Fira where it is installed, at the size the desktop configures. Both are
//! asked of the desktop's own tools (fontconfig, gsettings, KDE's config), as nothing else here
//! links to them.

use std::collections::HashSet;

use crate::desktop::{self, Program};

/// The interface's and the code's families, in order of preference.
const SANS: &[&str] = &["Fira Sans"];
const MONO: &[&str] = &["Fira Code", "Fira Mono"];

/// Points to logical pixels: the window scales those for the screen (1.5 on a HiDPI laptop).
const PIXELS_PER_POINT: f32 = 96.0 / 72.0;
/// Where the desktop says nothing: 11pt, the usual desktop default.
const DEFAULT_POINTS: f32 = 11.0;

#[derive(Debug, Default)]
pub(crate) struct Fonts {
    pub(crate) sans: Option<String>,
    pub(crate) mono: Option<String>,
}

/// Fira if installed; otherwise what fontconfig takes for sans and monospace.
pub(crate) fn fonts() -> Fonts {
    let installed = installed_families();
    let pick = |preferred: &[&str], generic: &str| {
        preferred
            .iter()
            .find(|family| installed.contains(**family))
            .map(|family| (*family).to_owned())
            .or_else(|| fc_match(generic))
    };
    Fonts {
        sans: pick(SANS, "sans-serif"),
        mono: pick(MONO, "monospace"),
    }
}

/// The desktop's text size in logical pixels: GNOME's (and GTK's) interface font and text
/// scaling, or KDE's general font, or 11pt.
pub(crate) fn desktop_text_size() -> f32 {
    gnome_points().or_else(kde_points).unwrap_or(DEFAULT_POINTS) * PIXELS_PER_POINT
}

/// What `program` prints, if it is there and succeeds.
fn ask(program: Program, args: &[&str]) -> Option<String> {
    match desktop::run(program, args) {
        Ok(stdout) => Some(String::from_utf8_lossy(&stdout).trim().to_owned()),
        Err(error) => {
            tracing::debug!(%error, "no answer from the desktop");
            None
        }
    }
}

fn installed_families() -> HashSet<String> {
    ask(Program::FcList, &[":", "family"])
        .unwrap_or_default()
        .lines()
        .flat_map(|line| line.split(','))
        .map(|family| family.trim().to_owned())
        .collect()
}

fn fc_match(generic: &str) -> Option<String> {
    ask(Program::FcMatch, &["-f", "%{family[0]}", generic]).filter(|family| !family.is_empty())
}

/// GNOME's interface font ("'Adwaita Sans 11'") times its text scaling factor.
fn gnome_points() -> Option<f32> {
    let setting = |key: &str| {
        ask(
            Program::Gsettings,
            &["get", "org.gnome.desktop.interface", key],
        )
    };
    let points = font_points(setting("font-name")?.trim_matches('\''))?;
    let scaling = setting("text-scaling-factor")
        .and_then(|factor| factor.parse::<f32>().ok())
        .filter(|factor| *factor > 0.0)
        .unwrap_or(1.0);
    Some(points * scaling)
}

/// The size at the end of a font name, as GNOME writes it: "Adwaita Sans 11".
fn font_points(font: &str) -> Option<f32> {
    font.rsplit(' ')
        .next()?
        .parse()
        .ok()
        .filter(|points: &f32| *points > 0.0)
}

/// KDE's general font: `font=Noto Sans,10,-1,5,…` under `[General]` in `kdeglobals`.
fn kde_points() -> Option<f32> {
    let config = std::fs::read_to_string(dirs::config_dir()?.join("kdeglobals")).ok()?;
    let mut in_general = false;
    for line in config.lines() {
        let line = line.trim();
        if line.starts_with('[') {
            in_general = line == "[General]";
        } else if in_general && let Some(font) = line.strip_prefix("font=") {
            return font.split(',').nth(1)?.parse().ok();
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gnome_font_names_end_in_their_size() {
        assert_eq!(font_points("Adwaita Sans 11"), Some(11.0));
        assert_eq!(font_points("Cantarell 10.5"), Some(10.5));
        assert_eq!(font_points("Sans"), None);
    }
}
