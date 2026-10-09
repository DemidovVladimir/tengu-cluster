//! `tengu lineage` black-box (`src/adapters/inbound/cli/lineage.rs`): the
//! built binary on the fixture registry (`tests/fixtures/lineage/registry`),
//! `TENGU_HOME` a temp dir — never `~/.tengu`.
//!
//! | Case | Expected |
//! |---|---|
//! | `verify` on the fixture | exit 0, `0 error(s), 0 warning(s)` |
//! | `verify` on a copy whose frozen W1 manifest changed | exit 1, `frozen_manifest_changed` naming `generation/W1` and both hashes in full |
//! | `verify` on a copy where a capability W1 lists gained a binding (same version) | exit 1, `frozen_manifest_changed` on `generation/W1` |
//! | a new preregistered variant in a copy | `verify` exit 1 (`seal_mismatch`); `seal variant:<id>` appends the row; `verify` exit 0; a second `seal` refused |
//! | a new ranking contract in a copy | `verify` exit 0 with one Warn `ranking_unsealed`; `seal ranking:<id>` appends the row; `verify` clean; a second `seal` refused; the contract edited after → exit 1 `seal_mismatch` |
//! | `report rule_w --forward rule_w.forward --format json` | 21 answers, none `UNKNOWN` |
//! | `trace ep.recorder_gap` | the start marked, its family and incident shown |
//! | `verify --pins` on the repo `lineage/` | W1 locked, every pin recomputes (G1 / G5) |
//! | `report` + `trace` on the repo `lineage/` | 21 Rule-W answers with sources; rule E traced to its FAIL and episode; the network-loss episode is OPERATIONAL_INCIDENT (G2–G4) |

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const W1_LOCK: &str = "db85f75c5e8c86681562797d3f9ba1bab78893f9e51d5f4021ae5aaba392ccf6";

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

/// Review #1: W1 lists `cap.backtest`; a strategy kind added to it at the
/// same version widens W1 — the lock covers the capability records.
#[test]
fn a_listed_capability_widened_at_the_same_version_fails_verify() {
    let (tmp, reg) = copy();
    let cap = reg.join("capabilities/cap.backtest.toml");
    let src = std::fs::read_to_string(&cap).unwrap();
    std::fs::write(
        &cap,
        src.replace(
            "bindings = [\"tool:backtest\"]",
            "bindings = [\"tool:backtest\", \"strategy_kind:pair_spread\"]",
        ),
    )
    .unwrap();
    let o = tengu(
        tmp.path(),
        &["lineage", "verify", "--registry", reg.to_str().unwrap()],
    );
    let out = text(&o);
    assert_eq!(o.status.code(), Some(1), "{out}");
    assert!(
        out.lines()
            .any(|l| l.contains("frozen_manifest_changed") && l.contains("generation/W1")),
        "{out}"
    );
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

/// A ranking contract (`lineage/rankings/`): unsealed is a warning only (a
/// draft may sit in the repo registry, whose verify CI runs), sealed once,
/// and an edit after the seal is an error.
#[test]
fn a_ranking_contract_is_sealed_once() {
    let (tmp, reg) = copy();
    let r = reg.to_str().unwrap();
    let src = std::fs::read_to_string(reg.join("rankings/rank.fixture.v1.toml")).unwrap();
    let path = reg.join("rankings/rank.weekly.v1.toml");
    std::fs::write(
        &path,
        src.replace("id = \"rank.fixture.v1\"", "id = \"rank.weekly.v1\"")
            .replace("cutoff = \"00:00\"", "cutoff = \"10:00\"\ndays = [\"Mon\"]"),
    )
    .unwrap();
    let o = tengu(tmp.path(), &["lineage", "verify", "--registry", r]);
    let out = text(&o);
    assert!(o.status.success(), "{out}");
    assert!(out.contains("0 error(s), 1 warning(s)"), "{out}");
    assert!(
        out.lines()
            .any(|l| l.contains("ranking_unsealed") && l.contains("ranking/rank.weekly.v1")),
        "{out}"
    );
    let o = tengu(
        tmp.path(),
        &["lineage", "seal", "ranking:rank.weekly.v1", "--registry", r],
    );
    assert!(o.status.success(), "{}", text(&o));
    let locks = std::fs::read_to_string(reg.join("locks.toml")).unwrap();
    assert!(
        locks.contains("record = \"ranking:rank.weekly.v1\""),
        "{locks}"
    );
    let o = tengu(tmp.path(), &["lineage", "verify", "--registry", r]);
    assert!(
        text(&o).contains("0 error(s), 0 warning(s)"),
        "{}",
        text(&o)
    );
    let o = tengu(
        tmp.path(),
        &["lineage", "seal", "ranking:rank.weekly.v1", "--registry", r],
    );
    assert!(!o.status.success());
    assert!(text(&o).contains("already sealed"), "{}", text(&o));
    // Loosened after the seal: the seal no longer matches.
    let sealed = std::fs::read_to_string(&path).unwrap();
    std::fs::write(&path, sealed.replace("min_trades = 20", "min_trades = 5")).unwrap();
    let o = tengu(tmp.path(), &["lineage", "verify", "--registry", r]);
    let out = text(&o);
    assert_eq!(o.status.code(), Some(1), "{out}");
    assert!(
        out.lines()
            .any(|l| l.contains("seal_mismatch") && l.contains("ranking/rank.weekly.v1")),
        "{out}"
    );
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

/// The repo's own registry (`lineage/`), the operator's W1.
fn repo_registry() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("lineage")
}

#[test]
fn the_repo_registry_is_frozen_and_every_pin_recomputes() {
    // G1 / G5: W1 is locked and its pins (specs, costs, risk, rule W, Jev,
    // models, tool schemas, the research skill) equal the sources now. No
    // `--evidence`: the vault lives on the operator's machine only.
    let home = tempfile::tempdir().unwrap();
    let reg = repo_registry();
    let o = tengu(
        home.path(),
        &[
            "lineage",
            "verify",
            "--pins",
            "--registry",
            reg.to_str().unwrap(),
        ],
    );
    let out = text(&o);
    assert!(o.status.success(), "{out}");
    assert!(out.contains(" 0 error(s)"), "{out}");
    let o = tengu(
        home.path(),
        &[
            "lineage",
            "generation",
            "W1",
            "--registry",
            reg.to_str().unwrap(),
        ],
    );
    let out = text(&o);
    assert!(out.contains("FROZEN"), "{out}");
    assert!(!out.contains("DRIFT") && !out.contains("NO_LOCK"), "{out}");
}

#[test]
fn the_repo_registry_answers_the_acceptance_questions() {
    // G2–G4 on the real records (TENGU_HANDOFF.md § 57–58, roadmap G4).
    let home = tempfile::tempdir().unwrap();
    let reg = repo_registry();
    let r = reg.to_str().unwrap();
    let o = tengu(
        home.path(),
        &[
            "lineage",
            "report",
            "weekend_overshoot_fade",
            "--forward",
            "fwd.rule_w.2026-10-02",
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
    for a in answers {
        let lines = a["lines"].as_array().unwrap();
        assert!(!lines.is_empty(), "{a:#}");
        assert!(a.get("unknown_reason").is_none(), "{a:#}");
        assert!(!a["sources"].as_array().unwrap().is_empty(), "{a:#}");
    }
    // A rejected idea: hypothesis → experiment → evidence → result → verdict
    // → Experience.
    let o = tengu(
        home.path(),
        &[
            "lineage",
            "trace",
            "ep.rule_e.rejected.2026-09-30",
            "--registry",
            r,
        ],
    );
    let out = String::from_utf8_lossy(&o.stdout).to_string();
    assert!(o.status.success(), "{}", text(&o));
    for needle in [
        "family post_earnings_follow",
        "experiment ext.holdout_names.rule_e.2026-09-30",
        "evidence ext.holdout_names.rule_e.2026-09-30",
        "result ext.holdout_names.rule_e.2026-09-30",
        "verdict ext.holdout_names.rule_e.2026-09-30: FAIL",
        "episode ep.rule_e.rejected.2026-09-30",
    ] {
        assert!(out.contains(needle), "{needle} missing:\n{out}");
    }
    // An operational incident, apart from strategy performance.
    let o = tengu(
        home.path(),
        &[
            "lineage",
            "trace",
            "ep.ops.2026-10-05.network-loss",
            "--registry",
            r,
        ],
    );
    let out = String::from_utf8_lossy(&o.stdout).to_string();
    let start = out
        .lines()
        .find(|l| l.starts_with('▶'))
        .unwrap_or_else(|| panic!("{out}"));
    assert!(start.contains("OPERATIONAL_INCIDENT"), "{start}");
    assert!(
        out.contains("incident inc.2026-10-05.network-loss-1"),
        "{out}"
    );
}
