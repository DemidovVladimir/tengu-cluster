//! `tengu lineage` black-box (`src/adapters/inbound/cli/lineage.rs`): the
//! built binary on the fixture registry (`tests/fixtures/lineage/registry`),
//! `TENGU_HOME` a temp dir — never `~/.tengu`.
//!
//! | Case | Expected |
//! |---|---|
//! | `verify` on the fixture | exit 0, `0 error(s), 0 warning(s)` |
//! | `verify` on a copy whose frozen W1 manifest changed | exit 1, `frozen_manifest_changed` naming `generation/W1` and both hashes in full |
//! | a new preregistered variant in a copy | `verify` exit 1 (`seal_mismatch`); `seal variant:<id>` appends the row; `verify` exit 0; a second `seal` refused |
//! | `report rule_w --forward rule_w.forward --format json` | 21 answers, none `UNKNOWN` |
//! | `trace ep.recorder_gap` | the start marked, its family and incident shown |

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const W1_LOCK: &str = "0272b3a042bed4dca49a17993cacf207fa0ef37d7190ddf637d5c0223baa21b8";

fn fixture() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/lineage/registry")
}

fn tengu(home: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_tengu"))
        .args(args)
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .env("TENGU_HOME", home)
        .env_remove("TENGU_CONFIG")
        .output()
        .expect("run tengu")
}

fn text(o: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&o.stdout),
        String::from_utf8_lossy(&o.stderr)
    )
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

/// A writable copy of the fixture registry.
fn copy() -> (tempfile::TempDir, PathBuf) {
    let tmp = tempfile::tempdir().unwrap();
    let reg = tmp.path().join("lineage");
    copy_dir(&fixture(), &reg);
    (tmp, reg)
}

#[test]
fn verify_passes_on_the_fixture() {
    let home = tempfile::tempdir().unwrap();
    let reg = fixture();
    let o = tengu(
        home.path(),
        &["lineage", "verify", "--registry", reg.to_str().unwrap()],
    );
    assert!(o.status.success(), "{}", text(&o));
    assert!(
        text(&o).contains("0 error(s), 0 warning(s)"),
        "{}",
        text(&o)
    );
}

#[test]
fn a_changed_frozen_manifest_fails_verify() {
    let (tmp, reg) = copy();
    let w1 = reg.join("generations/W1.toml");
    let src = std::fs::read_to_string(&w1).unwrap();
    std::fs::write(
        &w1,
        src.replace(
            "status = \"FROZEN\"",
            "status = \"FROZEN\"\nnotes = \"edited after the freeze\"",
        ),
    )
    .unwrap();
    let o = tengu(
        tmp.path(),
        &["lineage", "verify", "--registry", reg.to_str().unwrap()],
    );
    let out = text(&o);
    assert_eq!(o.status.code(), Some(1), "{out}");
    let line = out
        .lines()
        .find(|l| l.contains("frozen_manifest_changed"))
        .unwrap_or_else(|| panic!("{out}"));
    assert!(
        line.contains("generation/W1") && line.contains(W1_LOCK),
        "{line}"
    );
    // JSON findings carry the same.
    let o = tengu(
        tmp.path(),
        &[
            "lineage",
            "verify",
            "--format",
            "json",
            "--registry",
            reg.to_str().unwrap(),
        ],
    );
    let stdout = String::from_utf8_lossy(&o.stdout);
    let findings: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(findings[0]["code"], "frozen_manifest_changed");
    assert_eq!(findings[0]["severity"], "ERROR");
}

#[test]
fn a_new_preregistration_is_sealed_once() {
    let (tmp, reg) = copy();
    let r = reg.to_str().unwrap();
    std::fs::write(
        reg.join("variants/rule_w.monday.toml"),
        r#"id = "rule_w.monday"
family = "rule_w"
title = "Rule W exited Monday 12:00 instead of 09:30"
parent = "rule_w.all"
reason = "Let the open settle."
registered_at = "2026-10-06T11:00:00Z"
preregistered = true
status = "REGISTERED"

[spec]
described = "rule_w.all with the exit at Monday 12:00 New York"
"#,
    )
    .unwrap();
    let o = tengu(tmp.path(), &["lineage", "verify", "--registry", r]);
    assert_eq!(o.status.code(), Some(1));
    assert!(text(&o).contains("seal_mismatch"), "{}", text(&o));
    let o = tengu(
        tmp.path(),
        &["lineage", "seal", "variant:rule_w.monday", "--registry", r],
    );
    assert!(o.status.success(), "{}", text(&o));
    let locks = std::fs::read_to_string(reg.join("locks.toml")).unwrap();
    assert!(
        locks.contains("record = \"variant:rule_w.monday\""),
        "{locks}"
    );
    assert!(
        locks.contains("record = \"variant:rule_w.saturday\""),
        "earlier rows kept"
    );
    let o = tengu(tmp.path(), &["lineage", "verify", "--registry", r]);
    assert!(o.status.success(), "{}", text(&o));
    let o = tengu(
        tmp.path(),
        &["lineage", "seal", "variant:rule_w.monday", "--registry", r],
    );
    assert!(!o.status.success());
    assert!(text(&o).contains("already sealed"), "{}", text(&o));
    // Not a preregistration: refused.
    let o = tengu(
        tmp.path(),
        &[
            "lineage",
            "seal",
            "experiment:rule_w.backtest",
            "--registry",
            r,
        ],
    );
    assert!(!o.status.success());
    assert!(text(&o).contains("not preregistered"), "{}", text(&o));
}

#[test]
fn report_and_trace_answer_from_the_records() {
    let home = tempfile::tempdir().unwrap();
    let reg = fixture();
    let r = reg.to_str().unwrap();
    let o = tengu(
        home.path(),
        &[
            "lineage",
            "report",
            "rule_w",
            "--forward",
            "rule_w.forward",
            "--format",
            "json",
            "--registry",
            r,
        ],
    );
    assert!(o.status.success(), "{}", text(&o));
    let answers: serde_json::Value = serde_json::from_slice(&o.stdout).unwrap();
    let answers = answers.as_array().unwrap();
    assert_eq!(answers.len(), 21);
    assert!(
        answers.iter().all(|a| a.get("unknown_reason").is_none()),
        "{answers:#?}"
    );
    let o = tengu(
        home.path(),
        &["lineage", "trace", "ep.recorder_gap", "--registry", r],
    );
    assert!(o.status.success(), "{}", text(&o));
    let out = String::from_utf8_lossy(&o.stdout);
    assert!(
        out.lines()
            .any(|l| l.starts_with('▶') && l.contains("ep.recorder_gap")),
        "{out}"
    );
    assert!(out.contains("family rule_w") && out.contains("incident inc.recorder_gap"));
    let o = tengu(
        home.path(),
        &["lineage", "show", "nothing", "--registry", r],
    );
    assert!(!o.status.success());
}
