//! Writing a file in place of the one there, whole or not at all.

use std::io::Write as _;
use std::path::Path;

/// Writes `bytes` to `path`, in place of what is there: written beside it under a name of its
/// own, synced, and moved over it. A write cut short leaves the old file whole, and two
/// writing at once don't write into each other's (a file half one and half the other wouldn't
/// parse). Made with `mode` (less the umask), where files have one.
pub fn replace(path: &Path, bytes: &[u8], mode: u32) -> std::io::Result<()> {
    let name = path
        .file_name()
        .map_or_else(|| "file".into(), |name| name.to_string_lossy());
    let fresh = path.with_file_name(format!(".{name}.{}.new", uuid::Uuid::new_v4()));
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, mode);
    #[cfg(not(unix))]
    let _: u32 = mode;
    let written = options.open(&fresh).and_then(|mut file| {
        file.write_all(bytes)?;
        file.sync_all()?;
        std::fs::rename(&fresh, path)
    });
    if written.is_err()
        && let Err(error) = std::fs::remove_file(&fresh)
        && error.kind() != std::io::ErrorKind::NotFound
    {
        tracing::debug!(%error, path = %fresh.display(), "a file not kept stays beside");
    }
    written
}

#[cfg(test)]
mod tests {
    use super::*;

    /// What was there is replaced whole, with nothing left beside it.
    #[test]
    fn a_file_is_replaced_whole() {
        let dir = std::env::temp_dir().join(format!("edualc-replace-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("a scratch folder");
        let path = dir.join("kept.json");
        std::fs::write(&path, "old").expect("written");
        replace(&path, b"new", 0o600).expect("replaced");
        assert_eq!(std::fs::read_to_string(&path).expect("read"), "new");
        let left = std::fs::read_dir(&dir).expect("listed").count();
        assert_eq!(left, 1, "nothing beside it");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = std::fs::metadata(&path)
                .expect("its mode")
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        std::fs::remove_dir_all(&dir).expect("the scratch folder goes");
    }
}
