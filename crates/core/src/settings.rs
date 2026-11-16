//! What the user set in the window and wants kept between runs: the text zoom, the folders
//! trusted to run Claude Code in, the look. Kept in `$XDG_CONFIG_HOME/edualc/settings.json`.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::cjk::CjkLanguage;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Settings {
    /// The text size relative to the desktop's, in percent.
    #[serde(default = "Settings::full_size")]
    pub zoom_percent: u16,
    /// Trusted here (see [`crate::trust`]), with what is under them.
    #[serde(default)]
    pub trusted_folders: Vec<PathBuf>,
    /// The composer's height as dragged, in logical pixels; fitted to its text if none.
    #[serde(default)]
    pub composer_height_px: Option<u16>,
    /// The sidebar's width as dragged, in logical pixels; the default (from the text size) if
    /// none.
    #[serde(default)]
    pub sidebar_width_px: Option<u16>,
    /// The window's colours (dark or light as the desktop asks).
    #[serde(default)]
    pub look: Look,
    /// The key that sends what is written in the composer.
    #[serde(default)]
    pub send_with: SendKey,
    /// The language of text with Han characters alone (see [`crate::cjk`]); none for the desktop's,
    /// or Japanese if that isn't a CJK one.
    #[serde(default)]
    pub han_text: Option<CjkLanguage>,
    /// Whether a message from another session comes in at once, or only once you let its
    /// sender in. Messages edualc doesn't carry follow the receiving program's own rules.
    #[serde(default)]
    pub peer_messages: PeerMessages,
}

/// How messages from other sessions come into a session edualc runs.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PeerMessages {
    /// You're asked before the first message from each sender, in each session.
    #[default]
    AskEachSender,
    /// Every message comes in without asking.
    LetAllIn,
}

/// What sends the draft: Enter (Shift+Enter or Ctrl+Enter start a new line), or Ctrl+Enter
/// (Enter starts a new line, so a stray Enter never sends), as many chat apps let one choose.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SendKey {
    #[default]
    Enter,
    CtrlEnter,
}

/// The looks to choose from: Gruvbox, or plain greys with a blue accent.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Look {
    #[default]
    Gruvbox,
    Plain,
}

impl Look {
    /// The look the palette button switches to.
    pub fn other(self) -> Self {
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
            peer_messages: PeerMessages::default(),
        }
    }
}

/// How far the text zooms, in percent, and in what steps.
const ZOOM_RANGE: std::ops::RangeInclusive<u16> = 70..=200;
const ZOOM_STEP: u16 = 10;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Zoom {
    In,
    Out,
    Reset,
}

impl Settings {
    fn full_size() -> u16 {
        100
    }

    pub fn zoom(&mut self, zoom: Zoom) {
        let wanted = match zoom {
            Zoom::In => self.zoom_percent.saturating_add(ZOOM_STEP),
            Zoom::Out => self.zoom_percent.saturating_sub(ZOOM_STEP),
            Zoom::Reset => Self::full_size(),
        };
        self.zoom_percent = wanted.clamp(*ZOOM_RANGE.start(), *ZOOM_RANGE.end());
    }

    pub fn scale(&self) -> f32 {
        f32::from(self.zoom_percent) / 100.0
    }

    /// The language text with Han characters alone is taken to be in: the one chosen, else
    /// the desktop's if CJK, else Japanese.
    pub fn han_language(&self) -> CjkLanguage {
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
pub enum Error {
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
pub fn load() -> Result<Settings, Error> {
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

/// One thing the user changed. A window sends the change, not its whole copy of the settings,
/// so what another window changed meanwhile is kept.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Change {
    ZoomPercent(u16),
    Look(Look),
    SendWith(SendKey),
    HanText(CjkLanguage),
    ComposerHeight(Option<u16>),
    SidebarWidth(Option<u16>),
    /// A folder trusted here, with what is under it.
    Trust(PathBuf),
    PeerMessages(PeerMessages),
}

impl Settings {
    pub fn apply(&mut self, change: Change) {
        match change {
            Change::ZoomPercent(percent) => {
                self.zoom_percent = percent.clamp(*ZOOM_RANGE.start(), *ZOOM_RANGE.end());
            }
            Change::Look(look) => self.look = look,
            Change::SendWith(key) => self.send_with = key,
            Change::HanText(language) => self.han_text = Some(language),
            Change::ComposerHeight(height) => self.composer_height_px = height,
            Change::SidebarWidth(width) => self.sidebar_width_px = width,
            Change::Trust(folder) => {
                if !self.trusted_folders.contains(&folder) {
                    self.trusted_folders.push(folder);
                }
            }
            Change::PeerMessages(peer_messages) => self.peer_messages = peer_messages,
        }
    }
}

/// Applies `change` to the settings as the file holds them now and writes them back, unless
/// the file doesn't parse: what it holds (a hand edit with a typo, a later version's keys)
/// would be lost, and it stays until it is put right. Changes are to be applied one at a time,
/// by one owner (edualcd).
pub fn change(change: Change) -> Result<Settings, Error> {
    change_at(path()?, change)
}

fn change_at(path: PathBuf, change: Change) -> Result<Settings, Error> {
    let mut settings = match std::fs::read_to_string(&path) {
        Ok(text) => parse(&text).map_err(|source| Error::Parse {
            path: path.clone(),
            source,
        })?,
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => Settings::default(),
        Err(source) => return Err(Error::Read { path, source }),
    };
    settings.apply(change);
    if let Some(dir) = path.parent() {
        private_folder(dir).map_err(|source| Error::Write {
            path: dir.to_owned(),
            source,
        })?;
    }
    let text = serde_json::to_string_pretty(&settings).map_err(Error::Encode)?;
    // Written beside the file and renamed over it, so a crash leaves the old file or the new
    // one, never half of one that no longer parses.
    let fresh = path.with_extension("json.new");
    private_file(&fresh, text.as_bytes()).map_err(|source| Error::Write {
        path: fresh.clone(),
        source,
    })?;
    std::fs::rename(&fresh, &path).map_err(|source| Error::Write { path, source })?;
    Ok(settings)
}

/// `dir` and the folders above it that aren't there yet, made readable by the user alone.
fn private_folder(dir: &std::path::Path) -> std::io::Result<()> {
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
    builder.create(dir)
}

/// `path` written anew with `bytes`, readable by the user alone.
fn private_file(path: &std::path::Path, bytes: &[u8]) -> std::io::Result<()> {
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    std::io::Write::write_all(&mut options.open(path)?, bytes)
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
        change_at(path.clone(), Change::Look(Look::Plain)).expect("an older file is written over");
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
            change_at(path.clone(), Change::Look(Look::Plain)),
            Err(Error::Parse { .. })
        ));
        assert_eq!(std::fs::read_to_string(&path).ok().as_deref(), Some(typo));
        std::fs::remove_file(&path).expect("removed");
        change_at(path.clone(), Change::Look(Look::Plain)).expect("a missing file is written");
        change_at(path, Change::Look(Look::Gruvbox)).expect("a file that parses is written over");
        std::fs::remove_dir_all(&dir).expect("cleaned up");
    }

    /// A change is made to the file as it is now: one window's trusted folder stays when
    /// another changes the zoom, and a folder trusted twice is listed once.
    #[test]
    fn changes_from_two_windows_are_both_kept() {
        let dir = std::env::temp_dir().join(format!("edualc-change-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("a scratch folder");
        let path = dir.join("settings.json");
        change_at(path.clone(), Change::Trust(PathBuf::from("/work/app"))).expect("trusted");
        change_at(path.clone(), Change::ZoomPercent(120)).expect("zoomed");
        change_at(path.clone(), Change::Trust(PathBuf::from("/work/app"))).expect("again");
        let kept = parse(&std::fs::read_to_string(&path).expect("read back")).expect("parses");
        assert_eq!(kept.trusted_folders, [PathBuf::from("/work/app")]);
        assert_eq!(kept.zoom_percent, 120);
        assert!(!dir.join("settings.json.new").exists());
        let mode = std::os::unix::fs::PermissionsExt::mode(
            &std::fs::metadata(&path).expect("the file").permissions(),
        );
        assert_eq!(mode & 0o777, 0o600, "readable by the user alone");
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
