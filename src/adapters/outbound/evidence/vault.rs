//! `FsVault` — `ports::evidence::Vault` over `<TENGU_HOME>/state/evidence/<vault>/`
//! (`docs/lineage-2026-10-06.md` § 3). The vault dir is created once
//! (`create_dir`, never over an existing one); files are copied with
//! `std::fs::copy` (APFS: `clonefile`, no extra disk), each copy hashed and
//! checked equal to its source's hash; `seal` clears every write bit.
//! Sources are only opened for reading.
//!
//! | Rule | Value |
//! |---|---|
//! | Source | `~/…` or absolute; a regular file, or a dir holding only dirs and regular files (a symlink, socket or fifo anywhere ⇒ refused before anything is created); reported: its non-empty SQLite WALs (a FILE with the SQLite header → its `<db>-wal`; a DIR → every `*-wal` under it) — `snapshot` refuses them |
//! | Copy | never over an existing vault file; the source is hashed before the copy, the copy after; different ⇒ `Err` (the source changed) |
//! | Paths | vault-relative, `/`-separated, sorted bytewise |
//! | `MANIFEST.json` | at the vault root, created new, not listed in itself |

use std::fs;
use std::io::Read;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Context, Result};
use sha2::{Digest, Sha256};

use super::live_wal;
use crate::config::paths::expand_tilde;
use crate::domain::evidence::{ItemKind, ManifestEntry};
use crate::ports::evidence::{SourceInfo, Vault};

pub(crate) const MANIFEST_FILE: &str = "MANIFEST.json";

pub(crate) struct FsVault {
    root: PathBuf,
}

impl FsVault {
    /// `<tengu_home>/state/evidence/<vault>`.
    pub(crate) fn new(tengu_home: &Path, vault: &str) -> Self {
        Self::at(tengu_home.join("state").join("evidence").join(vault))
    }

    pub(crate) fn at(root: PathBuf) -> Self {
        Self { root }
    }
}

/// The file starts with the SQLite header (`SQLite format 3\0`).
fn is_sqlite(path: &Path) -> bool {
    let mut head = [0u8; 16];
    fs::File::open(path)
        .and_then(|mut f| f.read_exact(&mut head))
        .is_ok_and(|()| &head == b"SQLite format 3\0")
}

/// sha256 hex + length of a file, streamed.
pub(crate) fn hash_file(path: &Path) -> Result<(String, u64)> {
    let mut f = fs::File::open(path).with_context(|| format!("open {}", path.display()))?;
    let mut h = Sha256::new();
    let mut buf = vec![0u8; 1 << 20];
    let mut n = 0u64;
    loop {
        let k = f
            .read(&mut buf)
            .with_context(|| format!("read {}", path.display()))?;
        if k == 0 {
            break;
        }
        h.update(&buf[..k]);
        n += k as u64;
    }
    Ok((format!("{:x}", h.finalize()), n))
}

/// Every regular file under `dir`, relative, `/`-separated, sorted; `Err`
/// on anything that is neither a dir nor a regular file.
fn walk(dir: &Path) -> Result<Vec<(String, u64)>> {
    fn go(base: &Path, dir: &Path, out: &mut Vec<(String, u64)>) -> Result<()> {
        for entry in fs::read_dir(dir).with_context(|| format!("read dir {}", dir.display()))? {
            let entry = entry?;
            let path = entry.path();
            let meta = fs::symlink_metadata(&path)?;
            let ft = meta.file_type();
            if ft.is_dir() {
                go(base, &path, out)?;
            } else if ft.is_file() {
                let rel = path
                    .strip_prefix(base)
                    .map_err(|e| anyhow!("{}: {e}", path.display()))?;
                let rel = rel
                    .components()
                    .map(|c| c.as_os_str().to_string_lossy().into_owned())
                    .collect::<Vec<_>>()
                    .join("/");
                out.push((rel, meta.len()));
            } else {
                bail!(
                    "{}: not a regular file or dir (symlink / special)",
                    path.display()
                );
            }
        }
        Ok(())
    }
    let mut out = Vec::new();
    go(dir, dir, &mut out)?;
    out.sort_by(|a, b| a.0.as_bytes().cmp(b.0.as_bytes()));
    Ok(out)
}

fn copy_file(src: &Path, dst: &Path, rel: &str) -> Result<ManifestEntry> {
    if fs::symlink_metadata(dst).is_ok() {
        bail!("{}: already in the vault", dst.display());
    }
    if let Some(parent) = dst.parent() {
        fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    }
    let (src_sha, src_len) = hash_file(src)?;
    fs::copy(src, dst).with_context(|| format!("copy {} → {}", src.display(), dst.display()))?;
    let (sha, len) = hash_file(dst)?;
    if sha != src_sha || len != src_len {
        bail!(
            "{}: the copy ({sha}, {len} B) differs from the source ({src_sha}, {src_len} B) — the source changed during the copy",
            src.display()
        );
    }
    Ok(ManifestEntry {
        path: rel.to_string(),
        sha256: sha,
        bytes: len,
    })
}

impl Vault for FsVault {
    fn root_display(&self) -> String {
        self.root.display().to_string()
    }

    fn exists(&self) -> bool {
        fs::symlink_metadata(&self.root).is_ok()
    }

    fn inspect_source(&self, source: &str) -> Result<SourceInfo> {
        let path = expand_tilde(Path::new(source));
        let meta =
            fs::symlink_metadata(&path).with_context(|| format!("source {}", path.display()))?;
        let ft = meta.file_type();
        if ft.is_file() {
            let live_wals = if is_sqlite(&path) {
                live_wal(&path)
                    .map(|w| w.display().to_string())
                    .into_iter()
                    .collect()
            } else {
                Vec::new()
            };
            Ok(SourceInfo {
                kind: ItemKind::File,
                files: 1,
                bytes: meta.len(),
                path: path.display().to_string(),
                live_wals,
            })
        } else if ft.is_dir() {
            let files = walk(&path)?;
            let live_wals = files
                .iter()
                .filter(|(rel, len)| rel.ends_with("-wal") && *len > 0)
                .map(|(rel, _)| path.join(rel).display().to_string())
                .collect();
            Ok(SourceInfo {
                kind: ItemKind::Dir,
                files: files.len() as u64,
                bytes: files.iter().map(|f| f.1).sum(),
                path: path.display().to_string(),
                live_wals,
            })
        } else {
            bail!("source {}: a symlink or special file", path.display())
        }
    }

    fn has_dir(&self, path: &str) -> bool {
        fs::symlink_metadata(self.root.join(path)).is_ok_and(|m| m.is_dir())
    }

    fn create(&self) -> Result<()> {
        if let Some(parent) = self.root.parent() {
            fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
        }
        fs::create_dir(&self.root)
            .with_context(|| format!("create the vault {}", self.root.display()))
    }

    fn copy_in(&self, source: &str, kind: ItemKind, path: &str) -> Result<Vec<ManifestEntry>> {
        let src = expand_tilde(Path::new(source));
        let dst = self.root.join(path);
        match kind {
            ItemKind::File => Ok(vec![copy_file(&src, &dst, path)?]),
            ItemKind::Dir => {
                fs::create_dir_all(&dst).with_context(|| format!("create {}", dst.display()))?;
                let mut out = Vec::new();
                for (rel, _) in walk(&src)? {
                    out.push(copy_file(
                        &src.join(&rel),
                        &dst.join(&rel),
                        &format!("{path}/{rel}"),
                    )?);
                }
                Ok(out)
            }
        }
    }

    fn list(&self) -> Result<Vec<String>> {
        Ok(walk(&self.root)?
            .into_iter()
            .map(|(rel, _)| rel)
            .filter(|rel| rel != MANIFEST_FILE)
            .collect())
    }

    fn hash(&self, path: &str) -> Result<ManifestEntry> {
        let (sha256, bytes) = hash_file(&self.root.join(path))?;
        Ok(ManifestEntry {
            path: path.to_string(),
            sha256,
            bytes,
        })
    }

    fn write_manifest(&self, bytes: &[u8]) -> Result<()> {
        use std::io::Write;
        let path = self.root.join(MANIFEST_FILE);
        let mut f = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .with_context(|| format!("create {}", path.display()))?;
        f.write_all(bytes)?;
        f.sync_all()?;
        Ok(())
    }

    fn read_manifest(&self) -> Result<Option<Vec<u8>>> {
        let path = self.root.join(MANIFEST_FILE);
        match fs::read(&path) {
            Ok(b) => Ok(Some(b)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e).with_context(|| format!("read {}", path.display())),
        }
    }

    fn seal(&self) -> Result<()> {
        fn go(path: &Path) -> Result<()> {
            let meta = fs::symlink_metadata(path)?;
            if meta.is_dir() {
                for entry in fs::read_dir(path)? {
                    go(&entry?.path())?;
                }
            }
            let mut perm = meta.permissions();
            perm.set_mode(perm.mode() & !0o222);
            fs::set_permissions(path, perm).with_context(|| format!("chmod a-w {}", path.display()))
        }
        go(&self.root)
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// Make every file and dir under `root` writable again (test cleanup).
    pub(crate) fn unseal(root: &Path) {
        fn go(path: &Path) {
            let meta = fs::symlink_metadata(path).unwrap();
            let mut perm = meta.permissions();
            perm.set_mode(perm.mode() | 0o200);
            fs::set_permissions(path, perm).unwrap();
            if meta.is_dir() {
                for entry in fs::read_dir(path).unwrap() {
                    go(&entry.unwrap().path());
                }
            }
        }
        go(root);
    }

    #[test]
    fn copy_list_hash_seal() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("src");
        fs::create_dir_all(src.join("d/e")).unwrap();
        fs::write(src.join("f.txt"), b"hello").unwrap();
        fs::write(src.join("d/a.bin"), b"a").unwrap();
        fs::write(src.join("d/e/b.bin"), b"").unwrap();
        let home = tmp.path().join("home");
        let v = FsVault::new(&home, "v1");
        assert!(!v.exists());
        let info = v.inspect_source(src.to_str().unwrap()).unwrap();
        assert_eq!((info.kind, info.files, info.bytes), (ItemKind::Dir, 3, 6));
        v.create().unwrap();
        assert!(v.exists());
        assert!(v.create().is_err());
        let file = v
            .copy_in(
                src.join("f.txt").to_str().unwrap(),
                ItemKind::File,
                "x/f.txt",
            )
            .unwrap();
        assert_eq!(file.len(), 1);
        assert_eq!(
            file[0].sha256,
            "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824"
        );
        let dir = v
            .copy_in(src.to_str().unwrap(), ItemKind::Dir, "y")
            .unwrap();
        let paths: Vec<&str> = dir.iter().map(|e| e.path.as_str()).collect();
        assert_eq!(paths, vec!["y/d/a.bin", "y/d/e/b.bin", "y/f.txt"]);
        assert!(v
            .copy_in(
                src.join("f.txt").to_str().unwrap(),
                ItemKind::File,
                "x/f.txt"
            )
            .is_err());
        assert_eq!(
            v.list().unwrap(),
            vec!["x/f.txt", "y/d/a.bin", "y/d/e/b.bin", "y/f.txt"]
        );
        v.write_manifest(b"[]\n").unwrap();
        assert!(v.write_manifest(b"[]\n").is_err());
        assert_eq!(v.list().unwrap().len(), 4);
        v.seal().unwrap();
        let root = home.join("state/evidence/v1");
        assert!(fs::write(root.join("x/f.txt"), b"tamper").is_err());
        assert!(fs::write(root.join("new"), b"x").is_err());
        assert_eq!(v.read_manifest().unwrap().unwrap(), b"[]\n");
        unseal(&root);
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(src.join("f.txt"), src.join("link")).unwrap();
            assert!(v.inspect_source(src.to_str().unwrap()).is_err());
        }
    }
}
