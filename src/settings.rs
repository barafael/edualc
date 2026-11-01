//! What the user set in the window and wants kept between runs: the text zoom, the folders
//! trusted to run Claude Code in, and how it is sandboxed. Kept in `$XDG_CONFIG_HOME/edualc/settings.json`.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
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
    /// More the sandbox lets Claude read (a toolchain's home, say), and write.
    #[serde(default)]
    pub(crate) sandbox_read: Vec<PathBuf>,
    #[serde(default)]
    pub(crate) sandbox_write: Vec<PathBuf>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            zoom_percent: Self::full_size(),
            trusted_folders: Vec::new(),
            sandbox: true,
            sandbox_desktop: false,
            sandbox_read: Vec::new(),
            sandbox_write: Vec::new(),
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

pub(crate) fn save(settings: &Settings) -> Result<(), Error> {
    let path = path()?;
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
}
