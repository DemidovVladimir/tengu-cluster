//! SOE files (`docs/soe-2026-10-08.md` § 2): the private operator profile and
//! the record dirs (opportunities, eval cases). Pure file IO like
//! `config/lineage.rs`, whose dir walk it reuses; the records and their rules
//! are `domain/soe/`. No `[soe]` config section yet — O3 adds it here.
//!
//! | Loader | Rule (each refusal starts with its code) |
//! |---|---|
//! | [`default_profile_path`] | `<TENGU_HOME>/state/soe/operator.toml` (under `state/`, so `state:soe/…` locators resolve) |
//! | [`load_profile`] | no file ⇒ `operator_profile_missing` — never a built-in default (PRD § 14); a parse or rule error ⇒ `invalid_profile`, every problem listed; `synthetic = true` ⇒ `synthetic_profile_refused` unless allowed (public fixtures); a real profile inside a git work tree (a `.git` dir or file in any ancestor) ⇒ `profile_in_repo`, with any group / other permission bit ⇒ `profile_mode` (`chmod 600`); `signed_by = "UNSIGNED"` ⇒ `operator_profile_unsigned` |
//! | [`load_opportunity`] | one `soe.opportunity/1` file, any name; problems listed |
//! | [`load_record_dir`] | `<id>.toml` per record (file stem = `id`); `*.md` and dotfiles skipped; another entry refused; sorted by id; every error names its file |
//! | digest | `lineage::pins::toml_digest` of the file text (the profile's is `profile_sha256`) |

// Consumers land with `tengu soe` and the eval set (O1 W7–W8).
#![allow(dead_code)]

use std::path::{Path, PathBuf};

use serde::de::DeserializeOwned;

use super::lineage::{parse, toml_files};
use super::paths;
use crate::domain::lineage::pins::toml_digest;
use crate::domain::soe::opportunity::Opportunity;
use crate::domain::soe::profile::OperatorProfile;
use crate::domain::soe::record::{from_toml, validate, SoeRecord};
use crate::domain::soe::value::ValueError;

/// The SOE state dir name under `<TENGU_HOME>/state/`.
pub const SOE_STATE: &str = "soe";
/// The profile's file name in it.
pub const PROFILE_FILE: &str = "operator.toml";

/// Module table.
pub fn default_profile_path() -> PathBuf {
    paths::resolve_tengu_home()
        .join("state")
        .join(SOE_STATE)
        .join(PROFILE_FILE)
}

/// A record read from its file, with the file's digest.
#[derive(Debug, Clone)]
pub struct Loaded<R> {
    pub record: R,
    /// `toml_digest` of the file text, 64 hex.
    pub sha256: String,
    pub path: PathBuf,
}

/// `<path>: <code>: <field>: <why>`, one line each.
fn listed(path: &Path, errors: &[ValueError]) -> String {
    errors
        .iter()
        .map(|e| format!("{}: {e}", path.display()))
        .collect::<Vec<_>>()
        .join("\n")
}

fn read_record<R: SoeRecord + DeserializeOwned>(path: &Path) -> Result<Loaded<R>, String> {
    let text = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let record = from_toml::<R>(&text).map_err(|errors| listed(path, &errors))?;
    let sha256 = toml_digest(&text).map_err(|e| format!("{}: {e}", path.display()))?;
    Ok(Loaded {
        record,
        sha256,
        path: path.to_path_buf(),
    })
}

/// The git work tree `path` sits in: the nearest ancestor holding `.git`
/// (a dir, or a file in a linked worktree).
pub fn git_work_tree(path: &Path) -> Option<PathBuf> {
    let abs = paths::absolute_path(path);
    abs.ancestors()
        .find(|a| a.join(".git").exists())
        .map(Path::to_path_buf)
}

/// Group / other permission bits of the file (`None` off unix).
fn loose_mode(path: &Path) -> Result<Option<u32>, String> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(path)
            .map_err(|e| format!("{}: {e}", path.display()))?
            .permissions()
            .mode();
        Ok((mode & 0o077 != 0).then_some(mode & 0o777))
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        Ok(None)
    }
}

/// Module table: the operator profile at `path`, with its digest.
pub fn load_profile(path: &Path, allow_synthetic: bool) -> Result<Loaded<OperatorProfile>, String> {
    if !path.exists() {
        return Err(format!(
            "operator_profile_missing: {}: no operator profile — the operator writes and signs \
             it; no parameter has a built-in default (PRD § 14)",
            path.display()
        ));
    }
    let loaded =
        read_record::<OperatorProfile>(path).map_err(|e| format!("invalid_profile:\n{e}"))?;
    let p = &loaded.record;
    if p.synthetic {
        if !allow_synthetic {
            return Err(format!(
                "synthetic_profile_refused: {}: a synthetic test profile (`synthetic = true`) — \
                 allow synthetic profiles to use it",
                path.display()
            ));
        }
    } else {
        if let Some(tree) = git_work_tree(path) {
            return Err(format!(
                "profile_in_repo: {}: inside the git work tree {} — the operator profile is \
                 private; keep it under <TENGU_HOME>/state/{SOE_STATE}/",
                path.display(),
                tree.display()
            ));
        }
        if let Some(mode) = loose_mode(path)? {
            return Err(format!(
                "profile_mode: {}: mode {mode:o} lets others read it — chmod 600",
                path.display()
            ));
        }
    }
    if !p.is_signed() {
        return Err(format!(
            "operator_profile_unsigned: {}: `signed_by` / `signed_at` not set — the operator \
             reviews the values and signs the profile before anything decides on it",
            path.display()
        ));
    }
    Ok(loaded)
}

/// Module table: one opportunity file.
pub fn load_opportunity(path: &Path) -> Result<Loaded<Opportunity>, String> {
    read_record(path)
}

/// Module table: every `<id>.toml` record under `dir`, sorted by id; `Err`
/// lists every problem, each naming its file.
pub fn load_record_dir<R: SoeRecord + DeserializeOwned>(
    dir: &Path,
) -> Result<Vec<Loaded<R>>, Vec<String>> {
    if !dir.is_dir() {
        return Err(vec![format!("{}: no such directory", dir.display())]);
    }
    let mut errors = Vec::new();
    let mut out = Vec::new();
    for (path, stem) in toml_files(dir, &mut errors) {
        let Some((record, sha256)) = parse::<R>(&path, &stem, |r: &R| r.id(), &mut errors) else {
            continue;
        };
        match validate(&record) {
            Ok(()) => out.push(Loaded {
                record,
                sha256,
                path,
            }),
            Err(problems) => errors.push(listed(&path, &problems)),
        }
    }
    if !errors.is_empty() {
        return Err(errors);
    }
    out.sort_by(|a, b| a.record.id().cmp(b.record.id()));
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::soe::opportunity::tests::RECURRING;
    use crate::domain::soe::profile::tests::SYNTHETIC;

    fn write(path: &Path, text: &str, mode: u32) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
        }
        #[cfg(not(unix))]
        let _ = mode;
    }

    fn real() -> String {
        SYNTHETIC.replace("synthetic = true", "synthetic = false")
    }

    fn code(r: Result<Loaded<OperatorProfile>, String>) -> String {
        r.unwrap_err().split(':').next().unwrap().to_string()
    }

    #[test]
    fn missing_profile_is_an_error_not_a_default() {
        let tmp = tempfile::TempDir::new().unwrap();
        let path = tmp.path().join("state/soe/operator.toml");
        let e = load_profile(&path, true).unwrap_err();
        assert!(e.starts_with("operator_profile_missing: "), "{e}");
        assert!(e.contains(&path.display().to_string()), "{e}");
        assert!(default_profile_path().ends_with("state/soe/operator.toml"));
        // A broken one lists its problems, never falls back.
        write(
            &path,
            &SYNTHETIC.replace("version = 1", "version = 0"),
            0o600,
        );
        let e = load_profile(&path, true).unwrap_err();
        assert!(e.starts_with("invalid_profile:\n"), "{e}");
        assert!(e.contains("invalid_version: version: must be ≥ 1"), "{e}");
    }

    #[test]
    fn synthetic_profile_refused_without_flag() {
        let tmp = tempfile::TempDir::new().unwrap();
        let path = tmp.path().join("fixtures/operator.toml");
        write(&path, SYNTHETIC, 0o644);
        assert_eq!(
            code(load_profile(&path, false)),
            "synthetic_profile_refused"
        );
        let loaded = load_profile(&path, true).unwrap();
        assert!(loaded.record.synthetic);
        assert_eq!(loaded.sha256, toml_digest(SYNTHETIC).unwrap());
        assert_eq!(loaded.sha256.len(), 64);
        // Unsigned is refused even for a fixture.
        let unsigned = SYNTHETIC
            .replace("signed_by = \"fixture\"", "signed_by = \"UNSIGNED\"")
            .replace(
                "signed_at = \"2026-10-01T09:00:00Z\"",
                "signed_at = \"UNKNOWN\"",
            );
        write(&path, &unsigned, 0o644);
        assert_eq!(code(load_profile(&path, true)), "operator_profile_unsigned");
    }

    #[test]
    fn real_profile_inside_git_repo_refused() {
        let tmp = tempfile::TempDir::new().unwrap();
        // A repo (`.git` dir) and a linked worktree (`.git` file).
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(repo.join(".git")).unwrap();
        let in_repo = repo.join("private/operator.toml");
        write(&in_repo, &real(), 0o600);
        let e = load_profile(&in_repo, true).unwrap_err();
        assert!(e.starts_with("profile_in_repo: "), "{e}");
        let worktree = tmp.path().join("wt");
        write(&worktree.join(".git"), "gitdir: elsewhere\n", 0o644);
        let in_wt = worktree.join("operator.toml");
        write(&in_wt, &real(), 0o600);
        assert_eq!(code(load_profile(&in_wt, false)), "profile_in_repo");

        // Outside any work tree: 0600 loads, a group / other bit is refused.
        if git_work_tree(tmp.path()).is_some() {
            return; // the temp dir itself sits in a repo here
        }
        let home = tmp.path().join("home/state/soe/operator.toml");
        write(&home, &real(), 0o600);
        let loaded = load_profile(&home, false).unwrap();
        assert!(!loaded.record.synthetic && loaded.record.is_signed());
        #[cfg(unix)]
        {
            write(&home, &real(), 0o640);
            let e = load_profile(&home, false).unwrap_err();
            assert!(e.starts_with("profile_mode: ") && e.contains("640"), "{e}");
        }
    }

    #[test]
    fn case_dir_file_stem_must_equal_id() {
        let tmp = tempfile::TempDir::new().unwrap();
        let dir = tmp.path().join("cases");
        let with_id =
            |id: &str| RECURRING.replace("id = \"example-automation\"", &format!("id = \"{id}\""));
        write(&dir.join("b.toml"), &with_id("b"), 0o644);
        write(&dir.join("a-x.toml"), &with_id("a-x"), 0o644);
        write(&dir.join("a.toml"), &with_id("a"), 0o644);
        write(&dir.join("README.md"), "# cases\n", 0o644);
        let ids: Vec<String> = load_record_dir::<Opportunity>(&dir)
            .unwrap()
            .into_iter()
            .map(|l| l.record.id)
            .collect();
        assert_eq!(ids, ["a", "a-x", "b"]);

        // A stem that is not the id, a stray file, a sub-dir, a bad record.
        write(&dir.join("other.toml"), &with_id("c"), 0o644);
        write(&dir.join("notes.txt"), "x", 0o644);
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        let fake = with_id("d").replace(
            "mechanism = \"AUTOMATE\"",
            "mechanism = \"HIGH_TICKET_DELIVERY\"",
        );
        write(&dir.join("d.toml"), &fake, 0o644);
        let errors = load_record_dir::<Opportunity>(&dir).unwrap_err();
        assert_eq!(errors.len(), 4, "{errors:#?}");
        let all = errors.join("\n");
        assert!(
            all.contains("id `c` differs from the file stem `other`"),
            "{all}"
        );
        assert!(all.contains("notes.txt: not a record"), "{all}");
        assert!(all.contains("sub: not a record"), "{all}");
        assert!(
            all.contains("d.toml: fake_recurring: economics.revenue.kind:"),
            "{all}"
        );
        assert!(load_record_dir::<Opportunity>(&tmp.path().join("none")).is_err());

        // One opportunity file loads by any name.
        let one = load_opportunity(&dir.join("other.toml")).unwrap();
        assert_eq!((one.record.id.as_str(), one.sha256.len()), ("c", 64));
        let e = load_opportunity(&dir.join("d.toml")).unwrap_err();
        assert!(e.contains("fake_recurring"), "{e}");
    }
}
