//! [`write_atomic`] — replace a file whole. A temp file in the target's
//! directory, created with the final [`Mode`] (never wider for a moment),
//! is written, fsynced and renamed over the target; then the directory is
//! fsynced. A reader sees the old file or the new one, never a part; a crash
//! leaves at most a stray `.tengu-*.tmp`. A symlink at the target is
//! replaced, never followed.
//!
//! Users: the secrets vault, the builder's `builder.json` / `config.toml`,
//! the `tengu run` heartbeat, the workspace `vectors.bin`.

use std::io::Write;
use std::path::Path;

use anyhow::{Context, Result};

/// The new file's mode (Unix; the umask still applies).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Mode {
    /// 0600: secrets and state.
    OwnerOnly,
    /// 0644: files the operator edits or commits (a sandbox's config).
    Shared,
}

/// Replace `path` with `bytes` (module doc).
pub(crate) fn write_atomic(path: &Path, bytes: &[u8], mode: Mode) -> Result<()> {
    let dir = path
        .parent()
        .filter(|d| !d.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let mut builder = tempfile::Builder::new();
    builder.prefix(".tengu-").suffix(".tmp");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let bits = match mode {
            Mode::OwnerOnly => 0o600,
            Mode::Shared => 0o644,
        };
        builder.permissions(std::fs::Permissions::from_mode(bits));
    }
    #[cfg(not(unix))]
    let _ = mode;
    let mut tmp = builder
        .tempfile_in(dir)
        .with_context(|| format!("temp file in {}", dir.display()))?;
    tmp.write_all(bytes)
        .and_then(|()| tmp.as_file().sync_all())
        .with_context(|| format!("write {}", tmp.path().display()))?;
    // A failed persist drops the temp file with the error.
    tmp.persist(path)
        .map_err(|e| e.error)
        .with_context(|| format!("replace {}", path.display()))?;
    // The rename itself survives a power cut once the directory is synced.
    // Best effort: some filesystems refuse to fsync a directory.
    if let Ok(d) = std::fs::File::open(dir) {
        let _ = d.sync_all();
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    fn mode_of(p: &Path) -> u32 {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(p).unwrap().permissions().mode() & 0o777
    }

    #[test]
    fn replaces_the_file_and_leaves_no_temp() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("f.txt");
        write_atomic(&path, b"one", Mode::Shared).unwrap();
        write_atomic(&path, b"two", Mode::Shared).unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"two");
        let names: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(names, ["f.txt"]);
    }

    #[cfg(unix)]
    #[test]
    fn the_new_file_takes_the_mode_whatever_the_old_one_had() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("vault");
        std::fs::write(&path, b"old").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        write_atomic(&path, b"secret", Mode::OwnerOnly).unwrap();
        assert_eq!(mode_of(&path), 0o600);
        write_atomic(&path, b"config", Mode::Shared).unwrap();
        assert_eq!(mode_of(&path) & 0o600, 0o600);
        assert_eq!(mode_of(&path) & 0o033, 0);
    }

    #[cfg(unix)]
    #[test]
    fn a_symlink_at_the_target_is_replaced_not_followed() {
        let dir = tempfile::tempdir().unwrap();
        let outside = dir.path().join("outside");
        std::fs::write(&outside, b"keep").unwrap();
        let link = dir.path().join("link");
        std::os::unix::fs::symlink(&outside, &link).unwrap();
        write_atomic(&link, b"new", Mode::OwnerOnly).unwrap();
        assert_eq!(std::fs::read(&outside).unwrap(), b"keep");
        assert!(!std::fs::symlink_metadata(&link)
            .unwrap()
            .file_type()
            .is_symlink());
        assert_eq!(std::fs::read(&link).unwrap(), b"new");
    }

    #[test]
    fn a_missing_directory_is_an_error_naming_it() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nope/f.txt");
        let e = format!("{:#}", write_atomic(&path, b"x", Mode::Shared).unwrap_err());
        assert!(e.contains("nope"), "{e}");
    }
}
