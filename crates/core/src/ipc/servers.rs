//! The edualcds a window is paired with, each by a name: its address, its certificate's
//! fingerprint, and its token (or a demo's address). The native window keeps them in
//! `~/.config/edualc/servers.json`, the Android app in its own files. The file is the user's
//! alone, and is to be kept as an SSH key is: a token lets whoever holds it run Claude Code as
//! the user on that edualcd's machine. Claude Code's sandbox doesn't reach it (see `sandbox`).
//!
//! A window reaches its edualcd the same way whether it runs on this machine or another; the
//! one on this machine, a user service, is paired with like any other
//! (`edualc --pair "$(edualcd --pair)"`).

use std::collections::BTreeMap;
use std::io::Write as _;
use std::os::unix::fs::{DirBuilderExt as _, OpenOptionsExt as _};
use std::path::{Path, PathBuf};

use super::pairing::Server;

#[derive(Debug, thiserror::Error)]
pub enum Error {
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
pub struct Servers {
    /// The one reached unless another is named; the last paired.
    #[serde(default)]
    default: Option<String>,
    #[serde(default)]
    servers: BTreeMap<String, Server>,
}

impl Servers {
    /// What is kept at `path`; nothing, if there is no file yet.
    pub fn load(path: &Path) -> Result<Self, Error> {
        let path = path.to_owned();
        match std::fs::read(&path) {
            Ok(bytes) => {
                serde_json::from_slice(&bytes).map_err(|source| Error::Unreadable { path, source })
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(source) => Err(Error::Io { path, source }),
        }
    }

    /// Pairs with `server` as `name`, which becomes the one reached unless another is named.
    pub fn pair(&mut self, name: String, server: Server) {
        self.servers.insert(name.clone(), server);
        self.default = Some(name);
    }

    /// The name to pair `server` as when none is given: its host's, unless another is paired
    /// by that name already (another port there, or a demo where an edualcd listened), which
    /// `host:port` then tells apart, and `host:port demo` a demo from an edualcd there. Pairing
    /// again with what is reached the same way takes its name: a token made anew replaces the
    /// old, and a demo never replaces an edualcd's token.
    pub fn name_for(&self, server: &Server) -> String {
        let authority = server.authority();
        let names = [
            server.host().to_owned(),
            authority.clone(),
            format!("{authority} demo"),
            format!("{authority} paired"),
        ];
        let free = names.iter().find(|name| {
            self.servers
                .get(name.as_str())
                .is_none_or(|paired| paired.same_place(server))
        });
        free.cloned().unwrap_or(authority)
    }

    /// Forgets the edualcd paired as `name`; the default, if it was, becomes another.
    pub fn forget(&mut self, name: &str) {
        self.servers.remove(name);
        if self.default.as_deref() == Some(name) {
            self.default = self.servers.keys().next().cloned();
        }
    }

    /// Those paired, by name.
    pub fn paired(&self) -> impl Iterator<Item = (&String, &Server)> {
        self.servers.iter()
    }

    /// The one reached unless another is named.
    pub fn default_name(&self) -> Option<&str> {
        self.default.as_deref()
    }

    /// The edualcd named, or the default one.
    pub fn chosen(&self, name: Option<&str>) -> Result<(String, Server), Error> {
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

    /// Writes the file at `path` anew, the user's alone.
    pub fn save(&self, path: &Path) -> Result<(), Error> {
        let path = path.to_owned();
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
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ipc::pairing::{Demo, Fingerprint, Pairing};

    fn pairing(host: &str) -> Server {
        Server::Paired(Pairing {
            host: host.to_owned(),
            port: 7878,
            fingerprint: Fingerprint::of(host.as_bytes()),
            token: "t".to_owned(),
        })
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
                .map(|(_, server)| server.host().to_owned())
                .ok(),
            Some("127.0.0.1".to_owned())
        );
        assert!(matches!(
            servers.chosen(Some("elsewhere")),
            Err(Error::Unknown(name, paired)) if name == "elsewhere" && paired == "box, here"
        ));
    }

    /// A server is named by its host, or by its address where another is paired by its host.
    #[test]
    fn a_name_taken_by_another_address_is_told_apart() {
        let mut servers = Servers::default();
        let demo = Server::Demo(Demo {
            host: "box".to_owned(),
            port: 7979,
        });
        assert_eq!(servers.name_for(&demo), "box");
        servers.pair(servers.name_for(&demo), demo.clone());
        assert_eq!(servers.name_for(&pairing("box")), "box:7878");
        assert_eq!(servers.name_for(&demo), "box");
        // A demo where an edualcd was paired doesn't take its name, nor its token's place.
        let mut servers = Servers::default();
        servers.pair("box".to_owned(), pairing("box"));
        let there = Server::Demo(Demo {
            host: "box".to_owned(),
            port: 7878,
        });
        assert_eq!(servers.name_for(&there), "box:7878");
        servers.pair(servers.name_for(&there), there.clone());
        assert_eq!(servers.name_for(&pairing("box")), "box");
        assert_eq!(servers.name_for(&there), "box:7878");
    }

    /// Forgetting the default makes another the default; forgetting the last leaves none.
    #[test]
    fn forgetting_the_default_moves_it() {
        let mut servers = Servers::default();
        servers.pair("here".to_owned(), pairing("127.0.0.1"));
        servers.pair("box".to_owned(), pairing("box"));
        servers.forget("box");
        assert_eq!(servers.default_name(), Some("here"));
        servers.forget("here");
        assert_eq!(servers.default_name(), None);
        assert!(matches!(servers.chosen(None), Err(Error::Unpaired)));
    }

    #[test]
    fn the_file_reads_back_and_refuses_unknown_keys() {
        let mut servers = Servers::default();
        servers.pair("box".to_owned(), pairing("box"));
        servers.pair(
            "demo".to_owned(),
            Server::Demo(Demo {
                host: "10.0.2.2".to_owned(),
                port: 7878,
            }),
        );
        let text = serde_json::to_string(&servers).expect("written");
        let back: Servers = serde_json::from_str(&text).expect("read back");
        assert_eq!(back.chosen(None).ok(), servers.chosen(None).ok());
        assert!(serde_json::from_str::<Servers>(r#"{"servers": {}, "extra": 1}"#).is_err());
    }
}
