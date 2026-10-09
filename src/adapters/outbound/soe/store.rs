//! `FsCycleStore` — `ports::soe::CycleStore` on the private SOE state root
//! `<TENGU_HOME>/state/<sources.state>/` (critic C8). The port's module table
//! is the contract; this file says how the filesystem keeps it. Files mode
//! 0600, dirs 0700; nothing is ever rewritten.
//!
//! | Path | How |
//! |---|---|
//! | `cycles/<id>/` · `replays/<id>/` | `claim` = `create_dir` (atomic: of two concurrent claims one fails); the space dir and the root are created on first use; the id must be a lineage id (`valid_id`) on every call |
//! | run files | `write` = `create_new` + one `write_all` + `sync_all`: an existing name, an unclaimed or a frozen dir is refused; `read` takes any one path segment (a planted file shows up in `freeze::verify` as `EXTRA`) |
//! | `proposals.jsonl` · `challenges.jsonl` | `O_APPEND`, one canonical JSON line per `write_all`; read back with `domain::soe::record::from_json`, a bad line named with its file and number |
//! | freeze | `MANIFEST.json` as a new file, then `chmod a-w` on every file and the dir (dir last); a dir holding `MANIFEST.json` is `FROZEN` |
//! | `<log>.jsonl` (`StateLog`) | `O_APPEND`, one line per `write_all`; its number = the newlines up to this handle's offset after the write — a concurrent append never takes it (the holdout ledger's rule, `tools/xlab/holdout.rs`) |
//! | `files` · `cycles` | regular files but `MANIFEST.json`, sorted bytewise · dirs under `cycles/` named by an id, sorted |

use std::fs::{DirBuilder, File, OpenOptions};
use std::io::{ErrorKind, Read, Seek, Write};
use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Context, Result};
use serde::de::DeserializeOwned;

use crate::domain::canonical::canonical_json;
use crate::domain::lineage::value::valid_id;
use crate::domain::soe::challenge::Challenge;
use crate::domain::soe::proposal::MechanismProposal;
use crate::domain::soe::record::{from_json, SoeRecord};
use crate::ports::soe::{
    valid_file_name, CycleStore, RunDir, RunStatus, StateLog, CHALLENGES, MANIFEST, PROPOSALS,
};

/// The SOE state root (module table).
#[derive(Debug, Clone)]
pub(crate) struct FsCycleStore {
    root: PathBuf,
}

/// One path segment: what `read` takes (module table).
fn plain_segment(name: &str) -> bool {
    !name.is_empty() && name != "." && name != ".." && !name.contains(['/', '\\', '\0'])
}

fn dir_builder() -> DirBuilder {
    let mut b = DirBuilder::new();
    b.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        b.mode(0o700);
    }
    b
}

fn file_options() -> OpenOptions {
    let mut o = OpenOptions::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        o.mode(0o600);
    }
    o
}

/// `path` without any write bit.
fn read_only(path: &Path) -> Result<()> {
    let mut perms = std::fs::metadata(path)
        .with_context(|| format!("stat {}", path.display()))?
        .permissions();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        perms.set_mode(perms.mode() & !0o222);
    }
    #[cfg(not(unix))]
    perms.set_readonly(true);
    std::fs::set_permissions(path, perms).with_context(|| format!("chmod a-w {}", path.display()))
}

impl FsCycleStore {
    /// The store at `root` (`<TENGU_HOME>/state/<sources.state>`); nothing is
    /// created until the first claim or append.
    pub(crate) fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    #[cfg(test)]
    pub(crate) fn root(&self) -> &Path {
        &self.root
    }

    /// `<root>/<space>/<id>`; the id checked (module table).
    fn path(&self, dir: &RunDir) -> Result<PathBuf> {
        if !valid_id(dir.id()) {
            bail!("run id `{}` is not an id", dir.id());
        }
        Ok(self.root.join(dir.space()).join(dir.id()))
    }

    fn log_path(&self, log: StateLog) -> PathBuf {
        self.root.join(log.file_name())
    }

    /// The dir of a claimed run that is not frozen.
    fn open_dir(&self, dir: &RunDir) -> Result<PathBuf> {
        match self.status(dir)? {
            RunStatus::Open => self.path(dir),
            RunStatus::Frozen => bail!("{dir} in {} is frozen", self.root.display()),
            RunStatus::Absent => bail!("{dir} in {} is not claimed", self.root.display()),
        }
    }

    fn append(&self, dir: &RunDir, name: &str, line: &str) -> Result<()> {
        let path = self.open_dir(dir)?.join(name);
        let mut f = file_options()
            .create(true)
            .append(true)
            .open(&path)
            .with_context(|| format!("open {}", path.display()))?;
        let mut bytes = line.as_bytes().to_vec();
        bytes.push(b'\n');
        f.write_all(&bytes)
            .with_context(|| format!("append to {}", path.display()))?;
        f.sync_all()
            .with_context(|| format!("sync {}", path.display()))
    }

    fn records<T: SoeRecord + DeserializeOwned>(&self, dir: &RunDir, name: &str) -> Result<Vec<T>> {
        let Some(bytes) = self.read(dir, name)? else {
            return Ok(Vec::new());
        };
        let text = String::from_utf8(bytes).map_err(|_| anyhow!("{dir}/{name}: not UTF-8"))?;
        text.lines()
            .enumerate()
            .map(|(i, l)| {
                from_json::<T>(l).map_err(|e| {
                    anyhow!(
                        "{dir}/{name} line {}: {}",
                        i + 1,
                        e.iter()
                            .map(ToString::to_string)
                            .collect::<Vec<_>>()
                            .join("; ")
                    )
                })
            })
            .collect()
    }
}

impl CycleStore for FsCycleStore {
    fn root_display(&self) -> String {
        self.root.display().to_string()
    }

    fn status(&self, dir: &RunDir) -> Result<RunStatus> {
        let path = self.path(dir)?;
        match std::fs::metadata(&path) {
            Err(e) if e.kind() == ErrorKind::NotFound => Ok(RunStatus::Absent),
            Err(e) => Err(e).with_context(|| format!("stat {}", path.display())),
            Ok(m) if !m.is_dir() => bail!("{} is not a directory", path.display()),
            Ok(_) if path.join(MANIFEST).exists() => Ok(RunStatus::Frozen),
            Ok(_) => Ok(RunStatus::Open),
        }
    }

    fn claim(&self, dir: &RunDir) -> Result<()> {
        let path = self.path(dir)?;
        let space = self.root.join(dir.space());
        dir_builder()
            .create(&space)
            .with_context(|| format!("create {}", space.display()))?;
        let mut once = DirBuilder::new();
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            once.mode(0o700);
        }
        match once.create(&path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == ErrorKind::AlreadyExists => {
                bail!("{dir} exists in {}", self.root.display())
            }
            Err(e) => Err(e).with_context(|| format!("create {}", path.display())),
        }
    }

    fn cycles(&self) -> Result<Vec<String>> {
        let space = self.root.join("cycles");
        let entries = match std::fs::read_dir(&space) {
            Err(e) if e.kind() == ErrorKind::NotFound => return Ok(Vec::new()),
            r => r.with_context(|| format!("read {}", space.display()))?,
        };
        let mut out = Vec::new();
        for entry in entries {
            let entry = entry.with_context(|| format!("read {}", space.display()))?;
            let name = entry.file_name().to_string_lossy().into_owned();
            if entry.file_type()?.is_dir() && valid_id(&name) {
                out.push(name);
            }
        }
        out.sort();
        Ok(out)
    }

    fn write(&self, dir: &RunDir, name: &str, bytes: &[u8]) -> Result<()> {
        if !valid_file_name(name) {
            bail!("`{name}` is not a run file name");
        }
        let path = self.open_dir(dir)?.join(name);
        let mut f = match file_options().write(true).create_new(true).open(&path) {
            Ok(f) => f,
            Err(e) if e.kind() == ErrorKind::AlreadyExists => bail!("{dir}/{name} exists"),
            Err(e) => return Err(e).with_context(|| format!("create {}", path.display())),
        };
        f.write_all(bytes)
            .with_context(|| format!("write {}", path.display()))?;
        f.sync_all()
            .with_context(|| format!("sync {}", path.display()))
    }

    fn read(&self, dir: &RunDir, name: &str) -> Result<Option<Vec<u8>>> {
        if !plain_segment(name) {
            bail!("`{name}` is not one file name");
        }
        let path = self.path(dir)?.join(name);
        match std::fs::read(&path) {
            Ok(b) => Ok(Some(b)),
            Err(e) if e.kind() == ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e).with_context(|| format!("read {}", path.display())),
        }
    }

    fn files(&self, dir: &RunDir) -> Result<Vec<String>> {
        let path = self.path(dir)?;
        let mut out = Vec::new();
        for entry in std::fs::read_dir(&path).with_context(|| format!("read {}", path.display()))? {
            let entry = entry.with_context(|| format!("read {}", path.display()))?;
            if !entry.file_type()?.is_file() {
                continue;
            }
            let name = entry
                .file_name()
                .into_string()
                .map_err(|n| anyhow!("{dir}: file name {n:?} is not UTF-8"))?;
            if name != MANIFEST {
                out.push(name);
            }
        }
        out.sort();
        Ok(out)
    }

    fn append_proposal(&self, dir: &RunDir, p: &MechanismProposal) -> Result<()> {
        self.append(dir, PROPOSALS, &canonical_json(&serde_json::to_value(p)?))
    }

    fn proposals(&self, dir: &RunDir) -> Result<Vec<MechanismProposal>> {
        self.records(dir, PROPOSALS)
    }

    fn append_challenge(&self, dir: &RunDir, c: &Challenge) -> Result<()> {
        self.append(dir, CHALLENGES, &canonical_json(&serde_json::to_value(c)?))
    }

    fn challenges(&self, dir: &RunDir) -> Result<Vec<Challenge>> {
        self.records(dir, CHALLENGES)
    }

    fn freeze(&self, dir: &RunDir, manifest: &[u8]) -> Result<()> {
        let path = self.open_dir(dir)?;
        let manifest_path = path.join(MANIFEST);
        let mut f = match file_options()
            .write(true)
            .create_new(true)
            .open(&manifest_path)
        {
            Ok(f) => f,
            Err(e) if e.kind() == ErrorKind::AlreadyExists => bail!("{dir} is frozen"),
            Err(e) => return Err(e).with_context(|| format!("create {}", manifest_path.display())),
        };
        f.write_all(manifest)
            .with_context(|| format!("write {}", manifest_path.display()))?;
        f.sync_all()
            .with_context(|| format!("sync {}", manifest_path.display()))?;
        for entry in std::fs::read_dir(&path).with_context(|| format!("read {}", path.display()))? {
            let entry = entry?;
            if entry.file_type()?.is_file() {
                read_only(&entry.path())?;
            }
        }
        read_only(&path)
    }

    fn append_line(&self, log: StateLog, line: &str) -> Result<u64> {
        if line.contains('\n') {
            bail!("a {} line holds no newline", log.file_name());
        }
        dir_builder()
            .create(&self.root)
            .with_context(|| format!("create {}", self.root.display()))?;
        let path = self.log_path(log);
        let mut f = file_options()
            .create(true)
            .append(true)
            .open(&path)
            .with_context(|| format!("open {}", path.display()))?;
        let mut bytes = line.as_bytes().to_vec();
        bytes.push(b'\n');
        f.write_all(&bytes)
            .with_context(|| format!("append to {}", path.display()))?;
        f.sync_all()
            .with_context(|| format!("sync {}", path.display()))?;
        // O_APPEND: this handle's offset is now the end of our own line.
        let end = f.stream_position()?;
        let mut head = Vec::new();
        File::open(&path)
            .with_context(|| format!("read {}", path.display()))?
            .take(end)
            .read_to_end(&mut head)
            .with_context(|| format!("read {}", path.display()))?;
        Ok(head.iter().filter(|b| **b == b'\n').count() as u64)
    }

    fn lines(&self, log: StateLog) -> Result<Vec<String>> {
        let path = self.log_path(log);
        match std::fs::read_to_string(&path) {
            Ok(text) => Ok(text.lines().map(str::to_string).collect()),
            Err(e) if e.kind() == ErrorKind::NotFound => Ok(Vec::new()),
            Err(e) => Err(e).with_context(|| format!("read {}", path.display())),
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::application::soe::cycle::Target;
    use crate::application::soe::freeze::{decision_sha256, verify, FileCheck};
    use crate::application::soe::tests::{load_case, Bench};

    /// Give every file and dir under `root` its write bit back (a frozen
    /// dir would outlive the temp dir otherwise).
    pub(crate) fn writable(root: &Path) {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let Ok(entries) = std::fs::read_dir(root) else {
                return;
            };
            let _ = std::fs::set_permissions(root, std::fs::Permissions::from_mode(0o700));
            for e in entries.flatten() {
                let p = e.path();
                if p.is_dir() {
                    let _ = std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o700));
                    writable(&p);
                } else {
                    let _ = std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o600));
                }
            }
        }
    }

    /// A temp state root that is made writable again when dropped.
    pub(crate) struct Root(pub tempfile::TempDir);

    impl Drop for Root {
        fn drop(&mut self) {
            writable(self.0.path());
        }
    }

    fn root() -> Root {
        Root(tempfile::TempDir::new().unwrap())
    }

    #[cfg(unix)]
    fn mode(p: &Path) -> u32 {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(p).unwrap().permissions().mode() & 0o777
    }

    #[test]
    fn append_is_one_line_never_rewritten() {
        let r = root();
        let s = FsCycleStore::new(r.0.path().join("state/soe"));
        assert!(s.lines(StateLog::HoldoutReads).unwrap().is_empty());
        assert_eq!(
            s.append_line(StateLog::HoldoutReads, "{\"a\":1}").unwrap(),
            1
        );
        assert_eq!(
            s.append_line(StateLog::HoldoutReads, "{\"a\":2}").unwrap(),
            2
        );
        assert_eq!(s.append_line(StateLog::Grades, "{\"g\":1}").unwrap(), 1);
        assert!(s.append_line(StateLog::Grades, "a\nb").is_err());
        let path = s.root().join("holdout-reads.jsonl");
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "{\"a\":1}\n{\"a\":2}\n"
        );
        assert_eq!(
            s.lines(StateLog::HoldoutReads).unwrap(),
            ["{\"a\":1}", "{\"a\":2}"]
        );
        #[cfg(unix)]
        assert_eq!(mode(&path), 0o600);

        // Run files: written once, never over; appends add one line each.
        let d = RunDir::Cycle("2026-W41".into());
        assert!(s.write(&d, "head.json", b"{}").is_err(), "not claimed");
        s.claim(&d).unwrap();
        s.write(&d, "head.json", b"{\"v\":1}\n").unwrap();
        let e = s.write(&d, "head.json", b"{\"v\":2}\n").unwrap_err();
        assert!(e.to_string().contains("exists"), "{e}");
        assert!(s.write(&d, MANIFEST, b"x").is_err());
        assert!(s.write(&d, "../x", b"x").is_err());
        assert_eq!(
            s.read(&d, "head.json").unwrap().unwrap(),
            b"{\"v\":1}\n".to_vec()
        );
        assert_eq!(s.read(&d, "absent.json").unwrap(), None);
        assert!(s.read(&d, "../x").is_err());
        #[cfg(unix)]
        {
            assert_eq!(mode(&s.root().join("cycles/2026-W41")), 0o700);
            assert_eq!(mode(&s.root().join("cycles/2026-W41/head.json")), 0o600);
        }
        assert_eq!(s.files(&d).unwrap(), ["head.json"]);
    }

    #[test]
    fn claim_twice_refused() {
        let r = root();
        let s = FsCycleStore::new(r.0.path());
        let d = RunDir::Cycle("2026-W41".into());
        assert_eq!(s.status(&d).unwrap(), RunStatus::Absent);
        s.claim(&d).unwrap();
        assert_eq!(s.status(&d).unwrap(), RunStatus::Open);
        let e = s.claim(&d).unwrap_err().to_string();
        assert!(e.starts_with("cycles/2026-W41 exists in "), "{e}");
        // A replay of the same id is its own dir; never listed as a cycle.
        let rp = RunDir::Replay("2026-W41".into());
        s.claim(&rp).unwrap();
        assert_eq!(s.cycles().unwrap(), ["2026-W41"]);
        assert!(s.root().join("replays/2026-W41").is_dir());
        // An id that is not one is refused before any IO.
        let bad = RunDir::Cycle("../escape".into());
        assert!(s.claim(&bad).is_err() && s.status(&bad).is_err());
        assert!(!r.0.path().join("escape").exists());

        // Frozen: the manifest, then read-only; every write refused.
        s.write(&d, "memo.md", b"# memo\n").unwrap();
        s.freeze(&d, b"[]\n").unwrap();
        assert_eq!(s.status(&d).unwrap(), RunStatus::Frozen);
        assert!(s.claim(&d).is_err());
        assert!(s.write(&d, "late.json", b"{}").is_err());
        assert!(s.freeze(&d, b"[]\n").is_err());
        assert_eq!(s.read(&d, MANIFEST).unwrap().unwrap(), b"[]\n".to_vec());
        assert_eq!(s.files(&d).unwrap(), ["memo.md"]);
        #[cfg(unix)]
        {
            assert_eq!(mode(&s.root().join("cycles/2026-W41")) & 0o222, 0);
            assert_eq!(mode(&s.root().join("cycles/2026-W41/memo.md")) & 0o222, 0);
        }
    }

    /// The cycle runs on the filesystem as on the in-memory fake: the same
    /// case decides the same bytes; frozen dirs verify; a planted file shows.
    #[tokio::test]
    async fn a_cycle_on_disk_matches_the_in_memory_one() {
        let case = load_case("strong_news_weak_demand");
        let mem = Bench::new();
        let (want, _) = mem.run_case(&case, Target::Cycle).await;
        let want_sha = decision_sha256(&*mem.store, &want.dir).unwrap();

        let r = root();
        let fs = FsCycleStore::new(r.0.path());
        let mem_files = mem.store.dir(&want.dir);
        // Replay the in-memory run's files through the fs store.
        fs.claim(&want.dir).unwrap();
        for (name, bytes) in &mem_files {
            if name != MANIFEST && name != PROPOSALS && name != CHALLENGES {
                fs.write(&want.dir, name, bytes).unwrap();
            }
        }
        for p in mem.store.proposals(&want.dir).unwrap() {
            fs.append_proposal(&want.dir, &p).unwrap();
        }
        for c in mem.store.challenges(&want.dir).unwrap() {
            fs.append_challenge(&want.dir, &c).unwrap();
        }
        for name in [PROPOSALS, CHALLENGES] {
            assert_eq!(
                fs.read(&want.dir, name).unwrap(),
                mem_files.get(name).cloned(),
                "{name}: the same canonical lines"
            );
        }
        assert_eq!(
            fs.proposals(&want.dir).unwrap().len(),
            mem.store.proposals(&want.dir).unwrap().len()
        );
        assert_eq!(decision_sha256(&fs, &want.dir).unwrap(), want_sha);
        crate::application::soe::freeze::freeze(&fs, &want.dir).unwrap();
        assert_eq!(
            fs.read(&want.dir, MANIFEST).unwrap().unwrap(),
            mem_files[MANIFEST]
        );
        assert!(verify(&fs, &want.dir).unwrap().ok());

        // A file planted behind the store's back is reported, not hidden.
        writable(r.0.path());
        std::fs::write(fs.root().join("cycles/2026-W41/.planted"), "x").unwrap();
        let v = verify(&fs, &want.dir).unwrap();
        assert!(
            v.files
                .contains(&(".planted".to_string(), FileCheck::Extra)),
            "{v:?}"
        );
    }

    #[test]
    fn a_bad_record_line_names_its_file_and_line() {
        let r = root();
        let s = FsCycleStore::new(r.0.path());
        let d = RunDir::Replay("r1".into());
        s.claim(&d).unwrap();
        std::fs::write(
            s.root().join("replays/r1/proposals.jsonl"),
            "{\"schema\":1}\n",
        )
        .unwrap();
        let e = s.proposals(&d).unwrap_err().to_string();
        assert!(e.starts_with("replays/r1/proposals.jsonl line 1: "), "{e}");
        assert!(s.challenges(&d).unwrap().is_empty());
    }
}
