//! The edualcds this window is paired with (`~/.config/edualc/servers.json`), each by a name:
//! its address, its certificate's fingerprint, and its token. The file is the user's alone, and
//! is to be kept as an SSH key is: a token lets whoever holds it run Claude Code as the user on
//! that edualcd's machine. Claude Code's sandbox doesn't reach it (see `sandbox`).
//!
//! The window reaches its edualcd the same way whether it runs on this machine or another; the
//! one on this machine, a user service, is paired with like any other
//! (`edualc --pair "$(edualcd --pair)"`).

use std::collections::BTreeMap;
use std::io::Write as _;
use std::os::unix::fs::{DirBuilderExt as _, OpenOptionsExt as _};
use std::path::PathBuf;

use crate::ipc::pairing::Pairing;

const FILE: &str = "servers.json";

#[derive(Debug, thiserror::Error)]
pub(crate) enum Error {
    #[error("there is no config folder to keep the paired edualcds in")]
    NoConfig,
    #[error("{path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("{path} doesn't read: {source}")]
    Unreadable {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },
    #[error(
        "edualc isn't paired with an edualcd yet. Start one (systemctl --user enable --now \
         edualcd), then pair with the address it prints: edualc --pair \"$(edualcd --pair)\""
    )]
    Unpaired,
    #[error("no edualcd is paired as {0:?}; paired: {1}")]
    Unknown(String, String),
}

/// What the file holds.
#[derive(Debug, Default, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Servers {
    /// The one reached unless another is named; the last paired.
    #[serde(default)]
    default: Option<String>,
    #[serde(default)]
    servers: BTreeMap<String, Pairing>,
}

impl Servers {
    fn path() -> Result<PathBuf, Error> {
        Ok(dirs::config_dir()
            .ok_or(Error::NoConfig)?
            .join("edualc")
            .join(FILE))
    }

    /// What is kept; nothing, if there is no file yet.
    pub(crate) fn load() -> Result<Self, Error> {
        let path = Self::path()?;
        match std::fs::read(&path) {
            Ok(bytes) => {
                serde_json::from_slice(&bytes).map_err(|source| Error::Unreadable { path, source })
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(source) => Err(Error::Io { path, source }),
        }
    }

    /// Pairs with `pairing` as `name`, which becomes the one reached unless another is named.
    pub(crate) fn pair(&mut self, name: String, pairing: Pairing) {
        self.servers.insert(name.clone(), pairing);
        self.default = Some(name);
    }

    /// The edualcd named, or the default one.
    pub(crate) fn chosen(&self, name: Option<&str>) -> Result<(String, Pairing), Error> {
        let name = name
            .map(str::to_owned)
            .or_else(|| self.default.clone())
            .or_else(|| self.servers.keys().next().cloned())
            .ok_or(Error::Unpaired)?;
        match self.servers.get(&name) {
            Some(pairing) => Ok((name, pairing.clone())),
            None if self.servers.is_empty() => Err(Error::Unpaired),
            None => Err(Error::Unknown(
                name,
                self.servers.keys().cloned().collect::<Vec<_>>().join(", "),
            )),
        }
    }

    /// Writes the file anew, the user's alone.
    pub(crate) fn save(&self) -> Result<PathBuf, Error> {
        let path = Self::path()?;
        let io = |source| Error::Io {
            path: path.clone(),
            source,
        };
        if let Some(folder) = path.parent() {
            std::fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(folder)
                .map_err(io)?;
        }
        let text = serde_json::to_vec_pretty(self).map_err(|source| Error::Unreadable {
            path: path.clone(),
            source,
        })?;
        // Written beside and moved over it, so that a failure leaves the old file whole.
        let fresh = path.with_extension("json.new");
        match std::fs::remove_file(&fresh) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(io(error)),
        }
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&fresh)
            .map_err(io)?;
        file.write_all(&text).map_err(io)?;
        file.sync_all().map_err(io)?;
        std::fs::rename(&fresh, &path).map_err(io)?;
        Ok(path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ipc::pairing::Fingerprint;

    fn pairing(host: &str) -> Pairing {
        Pairing {
            host: host.to_owned(),
            port: 7878,
            fingerprint: Fingerprint::of(host.as_bytes()),
            token: "t".to_owned(),
        }
    }

    /// The last paired is reached unless another is named; a name not paired is said, with
    /// those that are.
    #[test]
    fn the_last_paired_is_the_default() {
        let mut servers = Servers::default();
        assert!(matches!(servers.chosen(None), Err(Error::Unpaired)));
        servers.pair("here".to_owned(), pairing("127.0.0.1"));
        servers.pair("box".to_owned(), pairing("box"));
        assert_eq!(
            servers.chosen(None).map(|(name, _)| name).ok(),
            Some("box".to_owned())
        );
        assert_eq!(
            servers
                .chosen(Some("here"))
                .map(|(_, pairing)| pairing.host)
                .ok(),
            Some("127.0.0.1".to_owned())
        );
        assert!(matches!(
            servers.chosen(Some("elsewhere")),
            Err(Error::Unknown(name, paired)) if name == "elsewhere" && paired == "box, here"
        ));
    }

    #[test]
    fn the_file_reads_back_and_refuses_unknown_keys() {
        let mut servers = Servers::default();
        servers.pair("box".to_owned(), pairing("box"));
        let text = serde_json::to_string(&servers).expect("written");
        let back: Servers = serde_json::from_str(&text).expect("read back");
        assert_eq!(back.chosen(None).ok(), servers.chosen(None).ok());
        assert!(serde_json::from_str::<Servers>(r#"{"servers": {}, "extra": 1}"#).is_err());
    }
}
