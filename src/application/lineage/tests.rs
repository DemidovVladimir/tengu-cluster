//! Lineage use-case tests on the fixture registry (`tests/fixtures/lineage/`):
//! the fixture is clean, a broken copy fires each validation code, `verify`'s
//! pin / evidence passes through fake ports, and the queries answer the
//! roadmap gates G2–G4 (Rule W end to end, a rejected idea, an operational
//! incident, all 21 acceptance answers).

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use super::attempts::scan;
use super::verify::{pin_status, verify, VerifyOpts};
use crate::config::lineage::load_registry;
use crate::domain::lineage::acceptance::acceptance;
use crate::domain::lineage::episode::Quadrant;
use crate::domain::lineage::experiment::{SplitBy, WindowRole};
use crate::domain::lineage::query::{
    attempt_rows, family_report, trace, Attempts, HoldoutRead, RunAttempt,
};
use crate::domain::lineage::value::{Binding, Locator, PinTarget, RecordKind, Time, UNKNOWN};
use crate::domain::lineage::{Finding, Registry, Severity};
use crate::ports::lineage::{
    AttemptSource, ContractProbe, EvidenceResolver, Extracted, Resolution, ResultSource,
};

fn fixture_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/lineage/registry")
}

pub(crate) fn fixture() -> Registry {
    load_registry(&fixture_dir()).unwrap_or_else(|e| panic!("{e:#?}"))
}

fn codes(findings: &[Finding]) -> BTreeSet<(Severity, String)> {
    findings
        .iter()
        .map(|f| (f.severity, f.code.clone()))
        .collect()
}

#[test]
fn the_fixture_is_clean_and_complete() {
    let r = fixture();
    assert_eq!(r.validate(), vec![], "the fixture has no finding at all");
    assert_eq!(r.families.len(), 3);
    assert!(r.variants.len() >= 4 && r.experiments.len() >= 4);
    assert_eq!(r.incidents.len(), 2);
    assert!(r.capabilities.len() >= 6);
    let classes: BTreeSet<String> = r
        .capabilities
        .values()
        .map(|c| format!("{:?}", c.class))
        .collect();
    assert_eq!(classes.len(), 4, "every capability class: {classes:?}");
    // Every quadrant, an operational incident and a no-action episode.
    let quadrants: BTreeSet<&str> = r.episodes.values().map(|e| e.quadrant().name()).collect();
    for q in [
        Quadrant::GoodDecisionGoodOutcome,
        Quadrant::GoodDecisionBadOutcome,
        Quadrant::BadDecisionGoodOutcome,
        Quadrant::BadDecisionBadOutcome,
        Quadrant::Unknown,
    ] {
        assert!(quadrants.contains(q.name()), "{} missing", q.name());
    }
    // A sealed preregistration and a historical one (prereg evidence).
    assert!(r.variants["rule_w.saturday"].preregistered);
    assert_eq!(r.locks.sealed[0].record, "variant:rule_w.saturday");
    assert!(r.experiments["rule_w.forward"]
        .evidence
        .iter()
        .any(|e| e.role == "prereg"));
}

/// One broken copy of the fixture per validation code: each fires exactly
/// where it is meant to.
#[test]
fn every_validation_code_fires_on_a_broken_copy() {
    type Break = fn(&mut Registry);
    let cases: Vec<(&str, Break)> = vec![
        ("invalid_id", |r| {
            let mut f = r.families["rule_w"].clone();
            f.id = "-bad".into();
            r.families.insert(f.id.clone(), f);
        }),
        ("duplicate_id", |r| {
            let mut f = r.families["rule_w"].clone();
            f.id = "rule_w.all".into();
            r.families.insert(f.id.clone(), f);
        }),
        ("invalid_field", |r| {
            r.variants.get_mut("rule_w.liquid").unwrap().spec.sandbox = Some("w1".into())
        }),
        ("dangling_ref", |r| {
            r.experiments.get_mut("rule_w.forward").unwrap().incidents[0] = "inc.gone".into()
        }),
        ("inconsistent_ref", |r| {
            r.experiments.get_mut("rule_w.backtest").unwrap().variant =
                "overnight_follow.base".into()
        }),
        ("capability_version_missing", |r| {
            r.generations.get_mut("W2-SIM").unwrap().capabilities[0].version = 2
        }),
        ("binding_conflict", |r| {
            r.capabilities
                .get_mut("cap.event_window")
                .unwrap()
                .bindings
                .push(Binding::StrategyKind("weekend_window".into()))
        }),
        ("future_leakage", |r| {
            r.episodes.get_mut("ep.rule_w.forward").unwrap().information[1].available_at =
                "2026-10-05T13:00:00Z".parse().unwrap()
        }),
        ("holdout_missing", |r| {
            r.experiments
                .get_mut("rule_w.holdout_names")
                .unwrap()
                .windows
                .retain(|w| w.role != WindowRole::Holdout)
        }),
        ("holdout_overlaps_development", |r| {
            r.experiments
                .get_mut("rule_w.holdout_names")
                .unwrap()
                .split_by = Some(SplitBy::Time)
        }),
        ("forward_incomplete", |r| {
            r.experiments
                .get_mut("rule_w.forward")
                .unwrap()
                .windows
                .retain(|w| w.role != WindowRole::Forward)
        }),
        ("frozen_manifest_changed", |r| {
            r.digests
                .insert((RecordKind::Generation, "W1".into()), "e".repeat(64));
        }),
        ("seal_mismatch", |r| {
            r.digests.insert(
                (RecordKind::Variant, "rule_w.saturday".into()),
                "e".repeat(64),
            );
        }),
    ];
    for (code, break_it) in cases {
        let mut r = fixture();
        break_it(&mut r);
        let found = r.validate();
        let hit: Vec<&Finding> = found.iter().filter(|f| f.code == code).collect();
        assert!(
            hit.iter().any(|f| f.severity == Severity::Error),
            "{code} did not fire: {found:#?}"
        );
    }
    // A preregistered record without seal or prereg evidence; a late seal.
    let mut r = fixture();
    r.experiments
        .get_mut("rule_w.jev_gate")
        .unwrap()
        .preregistered = true;
    assert!(codes(&r.validate()).contains(&(Severity::Error, "seal_mismatch".into())));
    let mut r = fixture();
    r.locks.sealed[0].sealed_at = Time::Unknown;
    assert_eq!(
        r.validate(),
        vec![],
        "an unknown seal time compares nothing"
    );
}

/// A probe / resolver / source answering from maps.
#[derive(Default)]
struct Fake {
    pins: BTreeMap<String, Result<String, String>>,
    tools: BTreeSet<String>,
    files: BTreeMap<String, Resolution>,
    extracted: Option<Extracted>,
}

impl ContractProbe for Fake {
    fn pin_sha256(&self, target: &PinTarget) -> Result<String, String> {
        self.pins
            .get(&target.to_string())
            .cloned()
            .unwrap_or_else(|| Err("not in the fake".into()))
    }
    fn tool_exists(&self, name: &str) -> bool {
        self.tools.contains(name)
    }
    fn strategy_kind_exists(&self, kind: &str) -> bool {
        crate::domain::backtest::spec::KINDS.contains(&kind)
    }
}

impl EvidenceResolver for Fake {
    fn resolve(&self, locator: &Locator) -> Resolution {
        self.files
            .get(&locator.to_string())
            .cloned()
            .unwrap_or(Resolution::NotAFile)
    }
}

impl ResultSource for Fake {
    fn extract(&self, _path: &Path, extract: &str) -> Option<Result<Extracted, String>> {
        extract.starts_with("arm:").then(|| {
            self.extracted
                .clone()
                .ok_or_else(|| "no figures".to_string())
        })
    }
}

/// Every pin of the fixture resolving to its recorded hash, every binding
/// but the simulated tool existing.
fn honest_probe(r: &Registry) -> Fake {
    let mut f = Fake::default();
    for g in r.generations.values() {
        for m in &g.models {
            f.pins.insert(m.location.to_string(), Ok("a".repeat(64)));
        }
    }
    for c in r.capabilities.values() {
        f.pins.insert(c.contract.to_string(), Ok("b".repeat(64)));
        for b in &c.bindings {
            if let Binding::Tool(t) = b {
                f.tools.insert(t.clone());
            }
        }
    }
    for x in r.experiments.values() {
        if let Some(p) = &x.cost_pin {
            f.pins.insert(p.to_string(), Ok("c".repeat(64)));
        }
    }
    // Recorded hashes last: a target pinned and also used elsewhere answers
    // its pin.
    for g in r.generations.values() {
        for p in &g.pins {
            f.pins.insert(p.target.to_string(), Ok(p.sha256.clone()));
        }
    }
    for v in r.variants.values() {
        if let (Some(s), Some(st), Some(h)) =
            (&v.spec.sandbox, &v.spec.strategy, &v.spec.spec_sha256)
        {
            f.pins.insert(format!("spec:{s}/{st}"), Ok(h.clone()));
        }
    }
    f
}

#[test]
fn verify_pins_flags_drift_unresolved_and_unknown_bindings() {
    let r = fixture();
    let none = Fake::default();
    let opts = VerifyOpts {
        pins: true,
        evidence: false,
    };
    let mut probe = honest_probe(&r);
    assert_eq!(verify(&r, opts, &probe, &none, &[]), vec![]);
    probe
        .pins
        .insert("config:w1/risk".into(), Ok("d".repeat(64)));
    probe.pins.remove("tool_schema:backtest");
    probe.tools.remove("w2_news_probe");
    let found = verify(&r, opts, &probe, &none, &[]);
    let got = codes(&found);
    for code in ["pin_drift", "pin_unresolved", "unknown_binding"] {
        assert!(
            got.contains(&(Severity::Error, code.into())),
            "{code}: {found:#?}"
        );
    }
    let drift = found.iter().find(|f| f.code == "pin_drift").unwrap();
    assert!(drift.message.contains(&"d".repeat(64)), "{}", drift.message);
    let rows = pin_status(&r, "W1", &probe).unwrap();
    let status: BTreeMap<String, &str> =
        rows.iter().map(|p| (p.target.clone(), p.status)).collect();
    assert_eq!(status["config:w1/risk"], "DRIFT");
    assert_eq!(status["tool_schema:backtest"], "UNRESOLVED");
    assert_eq!(status["spec:w1/rule_w"], "OK");
    // A library variant whose strategy changed in the sandbox: drift.
    let mut probe = honest_probe(&r);
    probe
        .pins
        .insert("spec:w1/overnight".into(), Ok("f".repeat(64)));
    let found = verify(&r, opts, &probe, &none, &[]);
    assert!(found
        .iter()
        .any(|f| f.code == "pin_drift" && f.record == "variant/overnight_follow.base"));
}

#[test]
fn verify_evidence_resolves_hashes_and_recomputes_results() {
    let r = fixture();
    let probe = honest_probe(&r);
    let opts = VerifyOpts {
        pins: false,
        evidence: true,
    };
    let present = |sha: &str| Resolution::Present {
        path: PathBuf::from("/fixture"),
        sha256: Some(sha.to_string()),
        recorded: None,
        mutable: false,
    };
    let mut files = Fake::default();
    // The ledger as recorded; the run dir present; the rest not files.
    let ledger = "vault:w1-fixture/xmarket-weekend/ledger.db";
    files.files.insert(
        ledger.into(),
        present("f20d3793f2c4430cfa05260f0e01a75b82822cc1644e8a49c6a81e79e0b8c38d"),
    );
    files.files.insert(
        "run:xlab/20261001T120034Z-rule_w".into(),
        present(&"1".repeat(64)),
    );
    files.extracted = Some(Extracted {
        n: Some(1000),
        mean_net_bps: Some(38.5),
        ci95_bps: Some([10.2, 66.1]),
        net_usd: Some(385.0),
        t_stat: Some(2.4),
    });
    let sources: Vec<&dyn ResultSource> = vec![&files];
    let found = verify(&r, opts, &probe, &files, &sources);
    let got = codes(&found);
    // The in-sample row matches; the holdout row (same fake figures) does not.
    assert!(
        got.contains(&(Severity::Error, "result_mismatch".into())),
        "{found:#?}"
    );
    assert!(found
        .iter()
        .filter(|f| f.code == "result_mismatch")
        .all(|f| f.message.contains("holdout")));
    // ledger:… has no source here: a warning, never an error.
    assert!(got.contains(&(Severity::Warn, "extract_unsupported".into())));
    // A changed file, a missing one, live state.
    files.files.insert(ledger.into(), present(&"2".repeat(64)));
    files.files.insert(
        "vault:w1-fixture/xmarket-weekend/prereg.md".into(),
        Resolution::Missing("gone".into()),
    );
    files.files.insert(
        "state:xlab/market.db".into(),
        Resolution::Present {
            path: PathBuf::from("/state"),
            sha256: None,
            recorded: None,
            mutable: true,
        },
    );
    let sources: Vec<&dyn ResultSource> = vec![];
    let found = verify(&r, opts, &probe, &files, &sources);
    let got = codes(&found);
    for (sev, code) in [
        (Severity::Error, "evidence_mismatch"),
        (Severity::Error, "evidence_missing"),
        (Severity::Warn, "mutable_evidence"),
    ] {
        assert!(got.contains(&(sev, code.into())), "{code}: {found:#?}");
    }
    let mismatch = found
        .iter()
        .find(|f| f.code == "evidence_mismatch")
        .unwrap();
    assert!(
        mismatch
            .message
            .contains("f20d3793f2c4430cfa05260f0e01a75b82822cc1644e8a49c6a81e79e0b8c38d"),
        "the recorded hash in full: {}",
        mismatch.message
    );
}

/// A run-dir source answering from a list.
struct Runs(Vec<RunAttempt>, Vec<HoldoutRead>);

impl AttemptSource for Runs {
    fn runs(&self, state: &str) -> (Vec<RunAttempt>, Vec<String>) {
        let rs = self
            .0
            .iter()
            .filter(|r| r.state == state)
            .cloned()
            .collect();
        (rs, vec!["one unreadable dir".into()])
    }
    fn holdout_reads(&self, state: &str) -> (Vec<HoldoutRead>, Vec<String>) {
        (
            self.1
                .iter()
                .filter(|r| r.state == state)
                .cloned()
                .collect(),
            vec![],
        )
    }
}

fn run(id: &str, strategy: &str, sha: &str) -> RunAttempt {
    RunAttempt {
        state: "xlab".into(),
        run_id: id.into(),
        kept: false,
        strategy: strategy.into(),
        kind: Some("weekend_window".into()),
        spec_sha256: sha.into(),
        split: None,
    }
}

fn attempts() -> Attempts {
    let all = "cba7a380444a5e6648f0d1421837ce5504c0a1e55e6b3126a8de6596ea343a7f";
    let sweep = "9dc3e51ced90a91f357a39414b0073b3e9511ce8a21ade6257b2b978c1cdf3b3";
    let source = Runs(
        vec![
            run("20261001T120034Z-rule_w", "rule_w", all),
            run("20261001T130000Z-rule_w", "rule_w", all),
            run("20260930T090000Z-rule_w", "rule_w", sweep),
            run("20260930T100000Z-other", "other", &"7".repeat(64)),
        ],
        vec![HoldoutRead {
            state: "xlab".into(),
            run_id: "20261001T120034Z-rule_w".into(),
            spec_sha256: all.into(),
            strategy: Some("rule_w".into()),
            time: None,
        }],
    );
    scan(&source, &["xlab".to_string(), "xmarket".to_string()])
}

#[test]
fn search_accounting_counts_runs_reads_and_unregistered_hashes() {
    let r = fixture();
    let a = attempts();
    assert_eq!(a.problems.len(), 2, "one per state scanned");
    let rows = attempt_rows(&r, &a);
    let by_run: BTreeMap<&str, &Vec<String>> = rows
        .iter()
        .map(|x| (x.run.run_id.as_str(), &x.variants))
        .collect();
    assert_eq!(
        by_run["20261001T120034Z-rule_w"],
        &vec!["rule_w.all".to_string()]
    );
    assert!(by_run["20260930T090000Z-rule_w"].is_empty(), "UNREGISTERED");
    let rep = family_report(&r, "rule_w", &a).unwrap();
    let s = &rep.search;
    assert_eq!(s.registered_variants, 4);
    assert_eq!((s.distinct_spec_hashes, s.runs, s.holdout_reads), (1, 2, 1));
    assert_eq!(
        s.unregistered.len(),
        1,
        "the sweep of strategy rule_w, not `other`"
    );
    assert_eq!(
        s.unregistered[0].spec_sha256,
        "9dc3e51ced90a91f357a39414b0073b3e9511ce8a21ade6257b2b978c1cdf3b3"
    );
    assert!(
        s.prior_search.starts_with("27 (APPROX)"),
        "{}",
        s.prior_search
    );
    assert_eq!(s.total_tried, "≥ 32 (prior search 27)");
    // The tree: rule_w.all is the root; the others its children.
    let tree: Vec<(usize, &str)> = rep
        .variants
        .iter()
        .map(|v| (v.depth, v.id.as_str()))
        .collect();
    assert_eq!(tree[0], (0, "rule_w.all"));
    assert!(tree[1..].iter().all(|(d, _)| *d == 1));
    assert!(family_report(&r, "nope", &a).is_err());
}

#[test]
fn trace_reconstructs_rule_w_a_rejected_idea_and_an_incident() {
    let r = fixture();
    let ids = |t: &crate::domain::lineage::query::Trace| -> BTreeSet<(String, String)> {
        t.lines
            .iter()
            .map(|l| (l.kind.clone(), l.id.clone()))
            .collect()
    };
    // G4 · Rule W end to end: hypothesis → variants → experiments →
    // windows / results / evidence → verdicts → episodes → incidents.
    let t = trace(&r, "rule_w").unwrap();
    let seen = ids(&t);
    for (kind, id) in [
        ("family", "rule_w"),
        ("variant", "rule_w.top4"),
        ("experiment", "rule_w.backtest"),
        ("window", "rule_w.backtest"),
        ("result", "rule_w.backtest"),
        ("verdict", "rule_w.forward"),
        ("evidence", "rule_w.forward"),
        ("episode", "ep.rule_w.forward"),
        ("incident", "inc.tor_outage"),
    ] {
        assert!(seen.contains(&(kind.into(), id.into())), "{kind} {id}");
    }
    assert_eq!(t.start, "family/rule_w");
    // From any record: an incident leads to the same lineage, marked.
    let t = trace(&r, "inc.tor_outage").unwrap();
    assert!(t.lines.iter().any(|l| l.start && l.kind == "incident"));
    assert!(ids(&t).contains(&("family".into(), "rule_w".into())));
    // G4 · a failed idea and why: verdict NO_GO, the rejected-strategy episode.
    let t = trace(&r, "overnight_follow").unwrap();
    let text: Vec<&str> = t.lines.iter().map(|l| l.text.as_str()).collect();
    assert!(text
        .iter()
        .any(|l| l.starts_with("NO_GO — Negative after costs")));
    assert!(text
        .iter()
        .any(|l| l.contains("REJECTED_STRATEGY · BAD_DECISION_BAD_OUTCOME")));
    // G4 · an operational incident, distinct from strategy failure.
    let t = trace(&r, "ep.recorder_gap").unwrap();
    let ep = t.lines.iter().find(|l| l.start).unwrap();
    assert!(ep.text.contains("OPERATIONAL_INCIDENT"), "{}", ep.text);
    let inc = t
        .lines
        .iter()
        .find(|l| l.kind == "incident" && l.id == "inc.recorder_gap" && l.depth > ep.depth)
        .unwrap();
    assert!(inc.text.contains("strategy impact NONE"));
    // Capabilities, generations, evidence and `<kind>/<id>` resolve too.
    for id in [
        "cap.event_window",
        "W2-SIM",
        "ev.w1-fixture",
        "variant/rule_w.saturday",
    ] {
        let t = trace(&r, id).unwrap();
        assert!(t.lines.iter().any(|l| l.start), "{id}");
    }
    assert!(trace(&r, "nothing").is_err());
}

#[test]
fn acceptance_answers_all_21_questions_from_fields() {
    let r = fixture();
    let answers = acceptance(&r, "rule_w", "rule_w.forward", &attempts()).unwrap();
    assert_eq!(answers.len(), 21);
    for a in &answers {
        assert!(
            !a.is_unknown(),
            "#{} {} is UNKNOWN: {:?}",
            a.n,
            a.question,
            a.unknown_reason
        );
        assert!(
            !a.lines.is_empty() && !a.sources.is_empty() || a.n == 5,
            "#{}",
            a.n
        );
    }
    let text = |n: usize| answers[n - 1].lines.join("\n");
    assert!(text(1).contains("revert by Monday's open"));
    assert!(text(2).contains("overnight_follow") && text(2).contains("rule_w.liquid"));
    assert!(text(5).contains("total tried: ≥ 32"));
    assert!(text(7).contains("split_by INSTRUMENTS"));
    assert!(text(8).contains("n 480, mean 47.04 bps"));
    assert!(text(12).contains("arm jev: UNPROVEN"));
    assert!(text(14).contains("prereg vault:w1-fixture/xmarket-weekend/prereg.md"));
    assert!(text(15).contains("c80d5beb724ff7eaf2fc61a4928ff15aa9a7cd16"));
    assert!(text(16).contains(
        "config:w1/risk sha256 241b167a509e9e561b11424f877a61c0aa13d75189b11acfaaee000869d98eeb"
    ));
    assert!(text(17).contains("inc.recorder_gap") && text(17).contains("inc.tor_outage"));
    assert!(text(18).contains("BACKFILLED"));
    assert!(text(19).contains("validity VALID_WITH_LIMITATIONS"));
    assert!(text(20).contains("ep.rule_w.stale_price [STRATEGY · BAD_DECISION_GOOD_OUTCOME]"));
    assert!(text(21).contains("W1 owns the experiment"));
    assert!(answers[15]
        .sources
        .contains(&"generations/W1.pins".to_string()));
    // A forward experiment with nothing recorded answers UNKNOWN, with why.
    let mut bare = r.clone();
    let fw = bare.experiments.get_mut("rule_w.forward").unwrap();
    fw.incidents.clear();
    fw.generation = UNKNOWN.into();
    bare.incidents.clear();
    let answers = acceptance(&bare, "rule_w", "rule_w.forward", &Attempts::default()).unwrap();
    assert!(answers[15].is_unknown() && answers[16].is_unknown());
    assert_eq!(answers[15].lines, vec![UNKNOWN.to_string()]);
    assert!(answers[4]
        .lines
        .iter()
        .any(|l| l == "run dirs: not scanned"));
}
