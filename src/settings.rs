//! What the user set in the window and wants kept between runs: the text zoom, the folders
//! trusted to run Claude Code in, and how it is sandboxed. Kept in `$XDG_CONFIG_HOME/edualc/settings.json`.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::cjk::CjkLanguage;
use crate::ipc::SandboxAccess;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Settings {
    /// The text size relative to the desktop's, in percent.
    #[serde(default = "Settings::full_size")]
    pub(crate) zoom_percent: u16,
    /// Trusted here (see `trust`), with what is under them.
    #[serde(default)]
    pub(crate) trusted_folders: Vec<PathBuf>,
    /// Claude Code runs in a Landlock sandbox (see `sandbox`).
    #[serde(default = "Settings::on")]
    pub(crate) sandbox: bool,
    /// The sandbox lets Claude use the display and the GPU, for testing user interfaces.
    #[serde(default)]
    pub(crate) sandbox_desktop: bool,
    /// Folders, with what is under them, whose sandbox lets Claude reach USB devices and serial
    /// ports: debug probes, boards, serial monitors.
    #[serde(default)]
    pub(crate) sandbox_hardware: Vec<PathBuf>,
    /// More the sandbox lets Claude read (a toolchain's home, say), and write.
    #[serde(default)]
    pub(crate) sandbox_read: Vec<PathBuf>,
    #[serde(default)]
    pub(crate) sandbox_write: Vec<PathBuf>,
    /// The composer's height as dragged, in logical pixels; fitted to its text if none.
    #[serde(default)]
    pub(crate) composer_height_px: Option<u16>,
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
            sandbox: true,
            sandbox_desktop: false,
            sandbox_hardware: Vec::new(),
            sandbox_read: Vec::new(),
            sandbox_write: Vec::new(),
            composer_height_px: None,
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

    fn on() -> bool {
        true
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

    /// The folders, with what is under them, the sandbox lets Claude into so, beyond its own.
    pub(crate) fn sandbox_folders(&mut self, access: SandboxAccess) -> &mut Vec<PathBuf> {
        match access {
            SandboxAccess::Read => &mut self.sandbox_read,
            SandboxAccess::Write => &mut self.sandbox_write,
        }
    }

    /// The language text with Han characters alone is taken to be in: the one chosen, else
    /// the desktop's if CJK, else Japanese.
    pub(crate) fn han_language(&self) -> CjkLanguage {
        self.han_text
            .or_else(CjkLanguage::of_desktop)
            .unwrap_or(CjkLanguage::Japanese)
    }

    /// Whether the sandbox lets Claude reach USB devices and serial ports in `folder`: it, or a
    /// folder it is in, was let.
    pub(crate) fn hardware_in(&self, folder: &Path) -> bool {
        folder
            .ancestors()
            .any(|place| self.sandbox_hardware.iter().any(|let_in| let_in == place))
    }

    /// Lets the sandbox reach USB devices and serial ports in `folder`, or stops it: the folder
    /// and every folder above it it was let in by.
    pub(crate) fn toggle_hardware(&mut self, folder: &Path) {
        if self.hardware_in(folder) {
            self.sandbox_hardware
                .retain(|let_in| !folder.starts_with(let_in));
        } else {
            self.sandbox_hardware.push(folder.to_owned());
        }
    }
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
    serde_json::from_str(&text).map_err(|source| Error::Parse { path, source })
}

/// Writes the settings, unless the file there doesn't parse: what it holds (a hand edit with a
/// typo, a later version's keys) would be lost, and it stays until it is put right.
pub(crate) fn save(settings: &Settings) -> Result<(), Error> {
    save_to(path()?, settings)
}

fn save_to(path: PathBuf, settings: &Settings) -> Result<(), Error> {
    match std::fs::read_to_string(&path) {
        Ok(text) => {
            if let Err(source) = serde_json::from_str::<Settings>(&text) {
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

    /// USB and serial ports are let in per folder, with what is under it; turning them off in a
    /// folder takes back what let them in there, from above too.
    #[test]
    fn hardware_is_let_in_per_folder() {
        let mut settings = Settings::default();
        let project = Path::new("/home/someone/firmware");
        let board = project.join("board");
        assert!(!settings.hardware_in(&board));
        settings.toggle_hardware(project);
        assert!(settings.hardware_in(project) && settings.hardware_in(&board));
        assert!(!settings.hardware_in(Path::new("/home/someone/other")));
        settings.toggle_hardware(&board);
        assert!(!settings.hardware_in(&board) && !settings.hardware_in(project));
        assert!(settings.sandbox_hardware.is_empty());
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
        let typo = r#"{"trusted_folders": ["/kept"], "sandbox_wirte": []}"#;
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
        assert!(serde_json::from_str::<Settings>(r#"{"sandbox_wirte": ["/tmp"]}"#).is_err());
        assert!(serde_json::from_str::<Settings>(r#"{"sandbox_write": ["/tmp"]}"#).is_ok());
    }
}
