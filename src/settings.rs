//! What the user set in the window and wants kept between runs: the text zoom, the folders
//! trusted to run Claude Code in, the look. Kept in `$XDG_CONFIG_HOME/edualc/settings.json`.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::cjk::CjkLanguage;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Settings {
    /// The text size relative to the desktop's, in percent.
    #[serde(default = "Settings::full_size")]
    pub(crate) zoom_percent: u16,
    /// Trusted here (see `trust`), with what is under them.
    #[serde(default)]
    pub(crate) trusted_folders: Vec<PathBuf>,
    /// The composer's height as dragged, in logical pixels; fitted to its text if none.
    #[serde(default)]
    pub(crate) composer_height_px: Option<u16>,
    /// The sidebar's width as dragged, in logical pixels; the default (from the text size) if
    /// none.
    #[serde(default)]
    pub(crate) sidebar_width_px: Option<u16>,
    /// The window's colours (dark or light as the desktop asks).
    #[serde(default)]
    pub(crate) look: Look,
    /// The key that sends what is written in the composer.
    #[serde(default)]
    pub(crate) send_with: SendKey,
    /// The language of text with Han characters alone (see `cjk`); none for the desktop's,
    /// or Japanese if that isn't a CJK one.
    #[serde(default)]
    pub(crate) han_text: Option<CjkLanguage>,
}

/// What sends the draft: Enter (Shift+Enter or Ctrl+Enter start a new line), or Ctrl+Enter
/// (Enter starts a new line, so a stray Enter never sends), as many chat apps let one choose.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum SendKey {
    #[default]
    Enter,
    CtrlEnter,
}

/// The looks to choose from: Gruvbox, or plain greys with a blue accent.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum Look {
    #[default]
    Gruvbox,
    Plain,
}

impl Look {
    /// The look the palette button switches to.
    pub(crate) fn other(self) -> Self {
        match self {
            Self::Gruvbox => Self::Plain,
            Self::Plain => Self::Gruvbox,
        }
    }
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            zoom_percent: Self::full_size(),
            trusted_folders: Vec::new(),
            composer_height_px: None,
            sidebar_width_px: None,
            look: Look::default(),
            send_with: SendKey::default(),
            han_text: None,
        }
    }
}

/// How far the text zooms, in percent, and in what steps.
const ZOOM_RANGE: std::ops::RangeInclusive<u16> = 70..=200;
const ZOOM_STEP: u16 = 10;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Zoom {
    In,
    Out,
    Reset,
}

impl Settings {
    fn full_size() -> u16 {
        100
    }

    pub(crate) fn zoom(&mut self, zoom: Zoom) {
        let wanted = match zoom {
            Zoom::In => self.zoom_percent.saturating_add(ZOOM_STEP),
            Zoom::Out => self.zoom_percent.saturating_sub(ZOOM_STEP),
            Zoom::Reset => Self::full_size(),
        };
        self.zoom_percent = wanted.clamp(*ZOOM_RANGE.start(), *ZOOM_RANGE.end());
    }

    pub(crate) fn scale(&self) -> f32 {
        f32::from(self.zoom_percent) / 100.0
    }

    /// The language text with Han characters alone is taken to be in: the one chosen, else
    /// the desktop's if CJK, else Japanese.
    pub(crate) fn han_language(&self) -> CjkLanguage {
        self.han_text
            .or_else(CjkLanguage::of_desktop)
            .unwrap_or(CjkLanguage::Japanese)
    }
}

/// The keys of edualc's sandbox, which went on 2026-11-13: a file that has them still loads,
/// without them, and is written without them.
#[derive(Debug, Clone, Copy, strum::IntoStaticStr, strum::EnumIter)]
#[strum(serialize_all = "snake_case")]
enum Retired {
    Sandbox,
    SandboxDesktop,
    SandboxHardware,
    SandboxMessagesUnasked,
    SandboxRead,
    SandboxWrite,
}

/// The settings in `text`, as a file holds them; retired keys are left out.
fn parse(text: &str) -> Result<Settings, serde_json::Error> {
    let mut value: serde_json::Value = serde_json::from_str(text)?;
    if let Some(keys) = value.as_object_mut() {
        for retired in <Retired as strum::IntoEnumIterator>::iter() {
            keys.remove(<&str>::from(retired));
        }
    }
    serde_json::from_value(value)
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum Error {
    #[error("no configuration directory to keep settings in")]
    NoConfigDir,
    #[error("couldn't read {path}: {source}")]
    Read {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("{path} isn't settings this version reads: {source}")]
    Parse {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },
    #[error("couldn't put the settings into JSON: {0}")]
    Encode(#[source] serde_json::Error),
    #[error("couldn't write {path}: {source}")]
    Write {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

fn path() -> Result<PathBuf, Error> {
    Ok(dirs::config_dir()
        .ok_or(Error::NoConfigDir)?
        .join("edualc")
        .join("settings.json"))
}

/// The kept settings; the defaults if none were kept yet.
pub(crate) fn load() -> Result<Settings, Error> {
    let path = path()?;
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => {
            return Ok(Settings::default());
        }
        Err(source) => return Err(Error::Read { path, source }),
    };
    parse(&text).map_err(|source| Error::Parse { path, source })
}

/// Writes the settings, unless the file there doesn't parse: what it holds (a hand edit with a
/// typo, a later version's keys) would be lost, and it stays until it is put right.
pub(crate) fn save(settings: &Settings) -> Result<(), Error> {
    save_to(path()?, settings)
}

fn save_to(path: PathBuf, settings: &Settings) -> Result<(), Error> {
    match std::fs::read_to_string(&path) {
        Ok(text) => {
            if let Err(source) = parse(&text) {
                return Err(Error::Parse { path, source });
            }
        }
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => {}
        Err(source) => return Err(Error::Read { path, source }),
    }
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|source| Error::Write {
            path: dir.to_owned(),
            source,
        })?;
    }
    let text = serde_json::to_string_pretty(settings).map_err(Error::Encode)?;
    std::fs::write(&path, text).map_err(|source| Error::Write { path, source })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zoom_steps_within_its_range() {
        let mut settings = Settings::default();
        settings.zoom(Zoom::In);
        assert_eq!(settings.zoom_percent, 110);
        for _ in 0..20 {
            settings.zoom(Zoom::In);
        }
        assert_eq!(settings.zoom_percent, 200);
        settings.zoom(Zoom::Reset);
        for _ in 0..20 {
            settings.zoom(Zoom::Out);
        }
        assert_eq!(settings.zoom_percent, 70);
    }

    #[test]
    fn older_files_without_a_zoom_read_as_full_size() {
        let settings: Settings = serde_json::from_str("{}").expect("parses");
        assert_eq!(settings, Settings::default());
    }

    /// A file written while edualc had its sandbox loads, the sandbox's keys dropped and the
    /// rest kept; saved, it holds none of them.
    #[test]
    fn an_older_file_with_the_sandboxs_keys_loads() {
        let older = r#"{
            "zoom_percent": 120,
            "trusted_folders": ["/work/app"],
            "sandbox": false,
            "sandbox_desktop": true,
            "sandbox_hardware": ["/work/firmware"],
            "sandbox_messages_unasked": true,
            "sandbox_read": ["/opt/toolchain"],
            "sandbox_write": ["/srv/shared"],
            "look": "plain"
        }"#;
        let read = parse(older).expect("an older file loads");
        assert_eq!(read.zoom_percent, 120);
        assert_eq!(read.trusted_folders, [PathBuf::from("/work/app")]);
        assert_eq!(read.look, Look::Plain);

        let dir = std::env::temp_dir().join(format!("edualc-older-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("a scratch folder");
        let path = dir.join("settings.json");
        std::fs::write(&path, older).expect("written");
        save_to(path.clone(), &read).expect("an older file is written over");
        let saved = std::fs::read_to_string(&path).expect("read back");
        assert!(!saved.contains("sandbox"), "{saved}");
        assert_eq!(parse(&saved).expect("parses"), read);
        std::fs::remove_dir_all(&dir).expect("cleaned up");
    }

    /// Enter sends unless the file says Ctrl+Enter, as it is written.
    #[test]
    fn the_key_that_sends_is_kept() {
        let older: Settings = serde_json::from_str(r#"{ "look": "plain" }"#).expect("parses");
        assert_eq!(older.send_with, SendKey::Enter);
        let chosen = Settings {
            send_with: SendKey::CtrlEnter,
            ..Settings::default()
        };
        let written = serde_json::to_string(&chosen).expect("written");
        assert!(written.contains(r#""send_with":"ctrl_enter""#), "{written}");
        let read: Settings = serde_json::from_str(&written).expect("read back");
        assert_eq!(read.send_with, SendKey::CtrlEnter);
    }

    /// A file that doesn't parse stays as it is; one that does is written over, as is a missing
    /// one.
    #[test]
    fn a_file_that_doesnt_parse_isnt_written_over() {
        let dir = std::env::temp_dir().join(format!("edualc-settings-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("a scratch folder");
        let path = dir.join("settings.json");
        let typo = r#"{"trusted_folders": ["/kept"], "trusted_foldres": []}"#;
        std::fs::write(&path, typo).expect("written");
        assert!(matches!(
            save_to(path.clone(), &Settings::default()),
            Err(Error::Parse { .. })
        ));
        assert_eq!(std::fs::read_to_string(&path).ok().as_deref(), Some(typo));
        std::fs::remove_file(&path).expect("removed");
        save_to(path.clone(), &Settings::default()).expect("a missing file is written");
        save_to(path, &Settings::default()).expect("a file that parses is written over");
        std::fs::remove_dir_all(&dir).expect("cleaned up");
    }

    /// A key written by hand with a typo would otherwise be dropped, and its setting quietly
    /// left at the default.
    #[test]
    fn a_misspelled_key_is_refused() {
        assert!(parse(r#"{"trusted_foldres": ["/tmp"]}"#).is_err());
        assert!(parse(r#"{"trusted_folders": ["/tmp"]}"#).is_ok());
    }
}
