//! A folder to work in, as the user names it: checked before the window turns to it, so the
//! folder list can say what is wrong while the path is still there to correct.

use std::path::{Path, PathBuf};

/// A folder that exists, by its canonical path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Folder(PathBuf);

#[derive(Debug, thiserror::Error)]
pub(crate) enum FolderError {
    #[error("There is no folder {}", .0.display())]
    Missing(PathBuf),
    #[error("{} is a file, not a folder", .0.display())]
    NotAFolder(PathBuf),
    #[error("Couldn't open {}: {source}", .path.display())]
    Unreadable {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

impl Folder {
    /// The folder a path typed by the user names; `~` at its start is `home`.
    pub(crate) fn typed(text: &str, home: Option<&Path>) -> Result<Self, FolderError> {
        let text = text.trim();
        let path = match (text.strip_prefix('~'), home) {
            (Some(rest), Some(home)) if rest.is_empty() || rest.starts_with('/') => {
                home.join(rest.trim_start_matches('/'))
            }
            _ => PathBuf::from(text),
        };
        Self::at(path)
    }

    /// The folder at `path`, as the folder picker gives it.
    pub(crate) fn at(path: PathBuf) -> Result<Self, FolderError> {
        match path.canonicalize() {
            Ok(canonical) if canonical.is_dir() => Ok(Self(canonical)),
            Ok(_) => Err(FolderError::NotAFolder(path)),
            Err(source) if source.kind() == std::io::ErrorKind::NotFound => {
                Err(FolderError::Missing(path))
            }
            Err(source) => Err(FolderError::Unreadable { path, source }),
        }
    }

    /// A folder the daemon found to be one, and made whole (`ipc::Read::Folder`).
    pub(crate) fn checked(path: PathBuf) -> Self {
        Self(path)
    }

    pub(crate) fn into_path(self) -> PathBuf {
        self.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A typed path opens the folder it names, `~` as the home folder; a missing one, or a
    /// file, is told apart, with the path as typed, so the user can correct it.
    #[test]
    fn a_typed_path_names_a_folder_or_says_why_not() {
        let scratch = std::env::temp_dir().join(format!("edualc-folders-{}", std::process::id()));
        let inner = scratch.join("work");
        std::fs::create_dir_all(&inner).expect("a scratch folder");
        std::fs::write(scratch.join("notes.txt"), "").expect("a scratch file");
        let canonical = inner.canonicalize().expect("a canonical path");

        let opened = |text: &str| Folder::typed(text, Some(&scratch)).map(Folder::into_path);
        assert_eq!(opened("~/work").ok(), Some(canonical.clone()));
        assert_eq!(
            opened(&format!("  {}  ", inner.display())).ok(),
            Some(canonical)
        );
        assert_eq!(
            opened("~").ok(),
            Some(scratch.canonicalize().expect("a canonical path"))
        );
        assert!(
            matches!(opened("~/gone"), Err(FolderError::Missing(path)) if path == scratch.join("gone"))
        );
        assert!(matches!(
            opened("~/notes.txt"),
            Err(FolderError::NotAFolder(_))
        ));
        // `~name` is someone's home in a shell, not this one's: taken as written.
        assert!(
            matches!(opened("~work"), Err(FolderError::Missing(path)) if path == Path::new("~work"))
        );
        std::fs::remove_dir_all(&scratch).expect("the scratch folder goes");
    }
}
