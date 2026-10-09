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
        + reg.evidence.len()
        + reg.rankings.len();
    assert_eq!(reg.digests.len(), records);
    assert_eq!(reg.locks.frozen.len(), 1);
    assert_eq!(
        reg.frozen_digest("W1").unwrap(),
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

/// `rankings/<id>.toml` loads as a `RankingContract` with its digest; a
/// field outside the schema is a load error naming the file.
#[test]
fn the_loader_reads_rankings() {
    use crate::domain::lineage::ranking::{CohortField, MissingPolicy};
    let dir = fixture_root().join("registry");
    let reg = load_registry(&dir).unwrap_or_else(|e| panic!("{e:#?}"));
    let c = &reg.rankings["rank.fixture.v1"];
    assert_eq!(c.sandbox, "ranked");
    assert_eq!(c.strategies, ["rule_w", "rule_w_top4"]);
    assert_eq!(c.cohort.len(), 9);
    assert_eq!(c.cohort[0], CohortField::Generation);
    assert_eq!(c.on_missing, MissingPolicy::Incomplete);
    assert_eq!(c.rating.order[5].to_string(), "-max_drawdown_bps");
    let key = (RecordKind::Ranking, "rank.fixture.v1".to_string());
    let sealed = reg
        .locks
        .sealed
        .iter()
        .find(|s| s.record == "ranking:rank.fixture.v1")
        .expect("the fixture seals it");
    assert_eq!(reg.digests[&key], sealed.sha256);
    assert_eq!(
        record_path(&dir, RecordKind::Ranking, "rank.fixture.v1"),
        dir.join("rankings/rank.fixture.v1.toml")
    );
    assert!(reg.kinds_of("rank.fixture.v1") == [RecordKind::Ranking]);
    // An unknown field fails the load, naming the file.
    let tmp = tempfile::tempdir().unwrap();
    let copy = tmp.path().join("lineage");
    copy_dir(&dir, &copy);
    let f = copy.join("rankings/rank.fixture.v1.toml");
    let text = std::fs::read_to_string(&f).unwrap();
    std::fs::write(&f, text.replace("on_missing", "weight = 1\non_missing")).unwrap();
    let errs = load_registry(&copy).unwrap_err();
    assert!(
        errs.len() == 1
            && errs[0].contains("rankings/rank.fixture.v1.toml")
            && errs[0].contains("weight"),
        "{errs:#?}"
    );
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

// ---------------------------------------------------------------------------
// `[generation]` binding (roadmap G5: W1 cannot reach W2, W1 unchanged)
// ---------------------------------------------------------------------------

use crate::config::Config;
use crate::domain::backtest::checks::assert_rule_w_golden;
use crate::domain::backtest::spec::spec_sha256;
use crate::domain::backtest::testkit::{run_params, utc};

fn w1() -> PathBuf {
    fixture_root().join("sandboxes/w1/config.toml")
}

fn w2() -> PathBuf {
    fixture_root().join("sandboxes/w2sim/config.toml")
}

fn load_err(path: &Path) -> String {
    match Config::load(path) {
        Ok(_) => panic!("{} loaded", path.display()),
        Err(e) => format!("{e:#}"),
    }
}

/// A copy of the fixture repo (registry + sandboxes) with `sandbox`'s config
/// text edited; returns the copy and that config's path.
fn copy_with(sandbox: &str, edit: impl Fn(String) -> String) -> (tempfile::TempDir, PathBuf) {
    let tmp = tempfile::tempdir().unwrap();
    for d in ["registry", "sandboxes"] {
        copy_dir(&fixture_root().join(d), &tmp.path().join(d));
    }
    let cfg = tmp
        .path()
        .join("sandboxes")
        .join(sandbox)
        .join("config.toml");
    let text = std::fs::read_to_string(&cfg).unwrap();
    std::fs::write(&cfg, edit(text)).unwrap();
    (tmp, cfg)
}

#[test]
fn w1_loads_bound_to_its_generation() {
    let cfg = Config::load(&w1()).unwrap_or_else(|e| panic!("{e:#}"));
    let scope = cfg.generation_scope.clone().expect("bound");
    assert_eq!(scope.id, "W1");
    assert!(scope.available_kinds.contains("weekend_window"));
    assert!(!scope.available_kinds.contains("event_window"));
    assert!(scope.available_tools.contains("backtest"));
    assert!(!scope.available_tools.contains("w2_news_probe"));
    // Every agent's tools see the same scope (`SandboxSections`).
    let agent = &cfg.agents["architect"];
    assert_eq!(agent.sandbox.sandbox.as_deref(), Some("w1"));
    assert_eq!(
        agent.sandbox.generation.as_deref().map(|g| g.id.as_str()),
        Some("W1")
    );
    let w2 = Config::load(&w2()).unwrap_or_else(|e| panic!("{e:#}"));
    let s2 = w2.generation_scope.unwrap();
    assert!(s2.available_kinds.contains("event_window"));
    assert!(s2.available_tools.contains("w2_news_probe"));
}

#[test]
fn w1_cannot_reach_a_w2_only_capability() {
    let (_t, cfg) = copy_with("w1", |t| {
        t.replace(
            r#"tools = ["backtest", "market_history", "read_file"]"#,
            r#"tools = ["backtest", "market_history", "read_file", "w2_news_probe", "hl_ctx"]"#,
        )
    });
    let e = load_err(&cfg);
    assert!(
        e.contains(
            "sandbox `w1` agent `architect`: tool `w2_news_probe` is bound by capability \
             `cap.news_probe`, which generation `W1` does not include"
        ),
        "{e}"
    );
    assert!(
        e.contains(
            "sandbox `w1` agent `architect`: opt-in tool `hl_ctx` is bound by no capability"
        ),
        "closed world: {e}"
    );
    let (_t, cfg) = copy_with("w1", |t| {
        t + "\n[backtest.strategies.news_event]\nkind = \"event_window\"\n\
             events = [{ instrument = \"hyperliquid:xyz:TSLA\", t = \"2026-09-29T13:30:00Z\" }]\n\
             interval = \"5m\"\ndirection = \"follow\"\nexit_after_mins = 60\n"
    });
    let e = load_err(&cfg);
    assert!(
        e.contains(
            "sandbox `w1` strategy `news_event`: strategy kind `event_window` is bound by \
             capability `cap.event_window`, which generation `W1` does not include"
        ),
        "{e}"
    );
    let (_t, cfg) = copy_with("w1", |t| {
        t + "\n[decision_loops.probe]\ngoal = \"read the news\"\nagent = \"architect\"\n\
             [decision_loops.probe.actions.ask]\ndescription = \"ask\"\ntool = \"w2_news_probe\"\n\
             [decision_loops.probe.actions.hold]\ndescription = \"stop\"\n\
             [feeds.probe]\nkind = \"tool\"\nevery_secs = 60\nagent = \"architect\"\ntool = \"w2_news_probe\"\n"
    });
    let e = load_err(&cfg);
    for at in ["loop `probe` action `ask`", "feed `probe`"] {
        assert!(
            e.contains(&format!(
                "sandbox `w1` {at}: tool `w2_news_probe` is bound by capability `cap.news_probe`"
            )),
            "{at}: {e}"
        );
    }
}

#[test]
fn a_generation_binds_only_the_sandboxes_it_lists() {
    let (_t, cfg) = copy_with("w2sim", |t| t.replace("id = \"W2-SIM\"", "id = \"W1\""));
    let e = load_err(&cfg);
    assert!(
        e.contains("generation `W1` does not list sandbox `w2sim` (its sandboxes: w1)"),
        "{e}"
    );
    let (_t, cfg) = copy_with("w1", |t| t.replace("id = \"W1\"", "id = \"W9\""));
    assert!(load_err(&cfg).contains("no generations/W9.toml"));
    let (_t, cfg) = copy_with("w1", |t| {
        t.replace(
            "registry = \"../../registry\"",
            "registry = \"../../nowhere\"",
        )
    });
    assert!(load_err(&cfg).contains("no registry directory"));
    // Not a sandboxes/<name>/config.toml: no sandbox name to list.
    let tmp = tempfile::tempdir().unwrap();
    let loose = tmp.path().join("config.toml");
    let text = std::fs::read_to_string(w1()).unwrap().replace(
        "registry = \"../../registry\"",
        &format!(
            "registry = \"{}\"",
            fixture_root().join("registry").display()
        ),
    );
    std::fs::write(&loose, text).unwrap();
    assert!(load_err(&loose).contains("is no sandboxes/<name>/config.toml"));
}

#[test]
fn a_frozen_generation_refuses_drift_at_load() {
    let (_t, cfg) = copy_with("w1", |t| {
        t.replace("max_order_notional_usd = 25", "max_order_notional_usd = 30")
    });
    let e = load_err(&cfg);
    assert!(
        e.contains(
            "FROZEN generation `W1` pin `config:w1/risk` drifted: pinned \
             241b167a509e9e561b11424f877a61c0aa13d75189b11acfaaee000869d98eeb, now "
        ),
        "{e}"
    );
    let (tmp, cfg) = copy_with("w1", |t| t);
    let manifest = tmp.path().join("registry/generations/W1.toml");
    let text = std::fs::read_to_string(&manifest).unwrap();
    std::fs::write(
        &manifest,
        text.replace("status = \"BASELINE\"", "status = \"PROVEN\""),
    )
    .unwrap();
    let e = load_err(&cfg);
    assert!(e.contains("frozen_manifest_changed generation/W1"), "{e}");
}

/// Review #11: a copy of the W1 sandbox elsewhere, pointing at the real
/// registry, with an edited `[risk]` — the pins hash the text loaded, not
/// the untouched repo file.
#[test]
fn the_load_time_drift_check_hashes_the_loaded_copy() {
    let tmp = tempfile::tempdir().unwrap();
    let copy = tmp.path().join("sandboxes/w1/config.toml");
    std::fs::create_dir_all(copy.parent().unwrap()).unwrap();
    let text = std::fs::read_to_string(w1())
        .unwrap()
        .replace(
            "registry = \"../../registry\"",
            &format!(
                "registry = \"{}\"",
                fixture_root().join("registry").display()
            ),
        )
        .replace("max_order_notional_usd = 25", "max_order_notional_usd = 30");
    std::fs::write(&copy, text).unwrap();
    let e = load_err(&copy);
    assert!(
        e.contains("FROZEN generation `W1` pin `config:w1/risk` drifted"),
        "{e}"
    );
}

/// The Docker image (`Dockerfile`: `sandboxes/` + `lineage/` under
/// /opt/tengu): `make up SANDBOX=<s>` mounts the sandbox at its own path
/// (review #10) — its name is detected and `../../lineage` resolves; the old
/// mount point `/opt/tengu/config.toml` finds no registry.
#[test]
fn docker_mounts_a_sandbox_where_its_generation_resolves() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let make = std::fs::read_to_string(root.join("Makefile")).unwrap();
    assert!(
        make.contains(
            "export TENGU_CONTAINER_CONFIG := $(if $(SANDBOX),/opt/tengu/sandboxes/$(SANDBOX)/config.toml,/opt/tengu/config.toml)"
        ),
        "Makefile"
    );
    let compose = std::fs::read_to_string(root.join("docker-compose.yml")).unwrap();
    for line in [
        "- TENGU_CONFIG=${TENGU_CONTAINER_CONFIG:-/opt/tengu/config.toml}",
        "- ./${TENGU_CONFIG_FILE:-config.toml}:${TENGU_CONTAINER_CONFIG:-/opt/tengu/config.toml}:ro",
    ] {
        assert!(compose.contains(line), "docker-compose.yml lacks {line}");
    }
    let tmp = tempfile::tempdir().unwrap();
    let opt = tmp.path().join("opt/tengu");
    copy_dir(&root.join("sandboxes"), &opt.join("sandboxes"));
    copy_dir(&root.join("lineage"), &opt.join("lineage"));
    let own = opt.join("sandboxes/xlab/config.toml");
    let cfg = Config::load(&own).unwrap_or_else(|e| panic!("{e:#}"));
    assert_eq!(
        cfg.generation_scope.as_ref().map(|g| g.id.as_str()),
        Some("W1")
    );
    std::fs::copy(&own, opt.join("config.toml")).unwrap();
    assert!(load_err(&opt.join("config.toml")).contains("no registry directory"));
}

/// Review #1: a capability W1 lists gains a strategy kind at the same
/// version — W1 would reach `pair_spread`; the freeze covers the record.
#[test]
fn a_listed_capability_widened_after_the_freeze_fails_the_load() {
    let (tmp, cfg) = copy_with("w1", |t| t);
    let cap = tmp.path().join("registry/capabilities/cap.backtest.toml");
    let text = std::fs::read_to_string(&cap).unwrap();
    std::fs::write(
        &cap,
        text.replace(
            "bindings = [\"tool:backtest\"]",
            "bindings = [\"tool:backtest\", \"strategy_kind:pair_spread\"]",
        ),
    )
    .unwrap();
    let e = load_err(&cfg);
    assert!(e.contains("frozen_manifest_changed generation/W1"), "{e}");
}

/// Roadmap G5 / handoff § 59: loading the W2 candidate changes nothing in
/// W1 — its manifest digest and lock, its pinned rule-W spec hash — and W1's
/// rule W, taken from the W1 config, replays the 2026-09-26 golden exactly,
/// before and after.
#[test]
fn w2_leaves_w1_and_its_replay_unchanged() {
    let dir = fixture_root().join("registry");
    let before = load_registry(&dir).unwrap();
    let key = (RecordKind::Generation, "W1".to_string());
    let digest = before.digests[&key].clone();
    assert_eq!(
        before.locks.frozen[0].manifest_sha256,
        before.frozen_digest("W1").unwrap()
    );
    let pinned = before.generations["W1"]
        .pins
        .iter()
        .find(|p| p.target.to_string() == "spec:w1/rule_w")
        .map(|p| p.sha256.clone())
        .unwrap();
    let replay = || {
        let cfg = Config::load(&w1()).unwrap_or_else(|e| panic!("{e:#}"));
        let spec = cfg.backtest.as_ref().unwrap().strategy("rule_w").unwrap();
        assert_eq!(spec_sha256(&spec.to_value()), pinned, "W1's pinned spec");
        let mut p = run_params(utc("2026-09-25 00:00"), utc("2026-09-29 00:00"));
        p.calendars = cfg.agents["architect"].sandbox.calendars.clone();
        assert_rule_w_golden(&spec, &p);
    };
    replay();
    Config::load(&w2()).unwrap_or_else(|e| panic!("{e:#}"));
    let after = load_registry(&dir).unwrap();
    assert_eq!(after.digests[&key], digest, "W1 manifest unchanged");
    assert_eq!(after.locks.frozen, before.locks.frozen, "W1 lock unchanged");
    let text = std::fs::read_to_string(w1()).unwrap();
    assert_eq!(
        crate::domain::lineage::pins::spec_pin(&text, "rule_w").unwrap(),
        pinned
    );
    replay();
}

/// Lineage D3: the scope lists every run the registry cites, by state, so
/// run-dir retention can spare them.
#[test]
fn the_scope_lists_the_runs_the_registry_cites() {
    let reg = load_registry(&fixture_root().join("registry")).unwrap_or_else(|e| panic!("{e:#?}"));
    let scope = GenerationScope::of(&reg, "W1").unwrap();
    assert_eq!(
        scope.cited_runs.get("xlab"),
        Some(&BTreeSet::from([
            "20261001T120034Z-rule_w".to_string(),
            "20261002T100000Z-rule_w_top4".to_string(),
        ]))
    );
    assert_eq!(scope.cited_runs.len(), 1);
}

/// SOE-G0 (critic C13; roadmap § 13 generation isolation) on the repo's own
/// registry: the SOE generation, its capability records and its family
/// leave W1 alone — W1's manifest still hashes to its lock row, no Error
/// finding (no `binding_conflict`), W1 cannot reach a `soe_*` tool, SOE-G0
/// reaches only those and binds only the `soe` sandbox, unlocked (its lock
/// row waits for the operator's signed profile) — and W1's two sandboxes
/// still load bound to W1, every FROZEN pin recomputing. (W1's rule-W
/// golden: `config::xmarket::tests::weekend_sandbox_replays_the_golden`.)
#[test]
fn soe_generation_leaves_w1_manifest_and_golden() {
    use crate::domain::tools::{SOE_CHALLENGE, SOE_PROPOSE, SOE_VIEW};
    let repo = Path::new(env!("CARGO_MANIFEST_DIR"));
    let reg = load_registry(&repo.join("lineage")).unwrap_or_else(|e| panic!("{e:#?}"));
    let w1_lock = reg
        .locks
        .frozen
        .iter()
        .rev()
        .find(|f| f.generation == "W1")
        .expect("W1 is locked");
    assert_eq!(w1_lock.manifest_sha256, reg.frozen_digest("W1").unwrap());
    let errors: Vec<String> = reg
        .validate()
        .into_iter()
        .filter(|f| f.severity == Severity::Error)
        .map(|f| format!("{} {}: {}", f.code, f.record, f.message))
        .collect();
    assert!(errors.is_empty(), "{errors:#?}");
    let g0 = &reg.generations["SOE-G0"];
    assert_eq!(g0.status, GenerationStatus::Candidate);
    assert_eq!(g0.sandboxes, ["soe"]);
    assert!(reg.locks.frozen.iter().all(|f| f.generation != "SOE-G0"));
    assert!(reg.families.contains_key("soe"));
    let soe = GenerationScope::of(&reg, "SOE-G0").unwrap();
    let w1 = GenerationScope::of(&reg, "W1").unwrap();
    for t in [SOE_VIEW, SOE_PROPOSE, SOE_CHALLENGE] {
        assert!(soe.tool_refusal(t).is_none(), "SOE-G0 reaches {t}");
        assert!(w1.tool_refusal(t).is_some(), "W1 reaches {t}");
    }
    assert!(soe.available_kinds.is_empty());
    assert!(w1.available_tools.is_disjoint(&soe.available_tools));
    for s in ["xlab", "xmarket-weekend"] {
        let file = repo.join("sandboxes").join(s).join("config.toml");
        let cfg = Config::load(&file).unwrap_or_else(|e| panic!("{s}: {e:#}"));
        assert_eq!(
            cfg.generation_scope.as_ref().map(|g| g.id.as_str()),
            Some("W1"),
            "{s}"
        );
    }
}
