//! `config/lineage.rs` tests: the registry loader on the fixture
//! (`tests/fixtures/lineage/registry`) and on broken temp copies.

use std::path::{Path, PathBuf};

use super::*;

fn fixture_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/lineage")
}

fn copy_dir(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).unwrap();
    for e in std::fs::read_dir(from).unwrap().flatten() {
        let target = to.join(e.file_name());
        if e.path().is_dir() {
            copy_dir(&e.path(), &target);
        } else {
            std::fs::copy(e.path(), &target).unwrap();
        }
    }
}

#[test]
fn the_fixture_loads_with_a_digest_per_record() {
    let dir = fixture_root().join("registry");
    let reg = load_registry(&dir).unwrap_or_else(|e| panic!("{e:#?}"));
    let records = reg.families.len()
        + reg.variants.len()
        + reg.experiments.len()
        + reg.episodes.len()
        + reg.incidents.len()
        + reg.capabilities.len()
        + reg.generations.len()
        + reg.evidence.len();
    assert_eq!(reg.digests.len(), records);
    assert_eq!(reg.locks.frozen.len(), 1);
    assert_eq!(
        reg.digests[&(RecordKind::Generation, "W1".to_string())],
        reg.locks.frozen[0].manifest_sha256
    );
    assert_eq!(repo_root(&dir), paths::absolute_path(&fixture_root()));
    assert_eq!(
        record_path(&dir, RecordKind::Variant, "rule_w.all"),
        dir.join("variants/rule_w.all.toml")
    );
    let pin = sandbox_pin(&dir, &"spec:w1/rule_w".parse().unwrap()).unwrap();
    assert_eq!(
        pin.unwrap(),
        "cba7a380444a5e6648f0d1421837ce5504c0a1e55e6b3126a8de6596ea343a7f"
    );
    assert!(sandbox_pin(&dir, &"tool_schema:backtest".parse().unwrap()).is_none());
}

#[test]
fn every_load_problem_names_its_file() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("lineage");
    copy_dir(&fixture_root().join("registry"), &dir);
    // id ≠ file stem, an unknown field, a bad enum, a stray dir and file.
    let fam = dir.join("families/rule_w.toml");
    let text = std::fs::read_to_string(&fam).unwrap();
    std::fs::write(dir.join("families/other.toml"), &text).unwrap();
    std::fs::write(
        dir.join("variants/rule_w.extra.toml"),
        "id = \"rule_w.extra\"\ntitle = \"t\"\nfamily = \"rule_w\"\nbogus = 1\n",
    )
    .unwrap();
    std::fs::write(&fam, text.replace("role = \"PRIMARY\"", "role = \"MAIN\"")).unwrap();
    std::fs::create_dir_all(dir.join("experiment")).unwrap();
    std::fs::write(dir.join("lock.toml"), "").unwrap();
    std::fs::write(dir.join("episodes/notes.txt"), "").unwrap();
    std::fs::write(dir.join("README.md"), "skipped").unwrap();
    let errs = load_registry(&dir).unwrap_err();
    let has = |file: &str, what: &str| errs.iter().any(|e| e.contains(file) && e.contains(what));
    assert!(
        has("families/other.toml", "differs from the file stem `other`"),
        "{errs:#?}"
    );
    assert!(has("variants/rule_w.extra.toml", "bogus"), "{errs:#?}");
    assert!(has("families/rule_w.toml", "MAIN"), "{errs:#?}");
    assert!(has("experiment", "not a registry entry"), "{errs:#?}");
    assert!(has("lock.toml", "not a registry entry"), "{errs:#?}");
    assert!(has("notes.txt", "not a record"), "{errs:#?}");
    assert_eq!(errs.len(), 6, "{errs:#?}");
    assert!(load_registry(&tmp.path().join("none")).is_err());
}
