//! The registry: every record of `lineage/` by kind and id, `locks.toml`, and
//! each record file's digest — and its checks ([`Registry::validate`], the
//! code table in `lineage/mod.rs`). Built by `config/lineage.rs::load_registry`
//! (or in code by tests); never written back.
//!
//! | Piece | Holds |
//! |---|---|
//! | records | one `BTreeMap<id, record>` per kind; evidence records are `domain/evidence.rs`'s |
//! | `digests` | `(kind, id)` → `pins::toml_digest` of the record's file: what `[[sealed]]` rows pin |
//! | [`Registry::frozen_digest`] | what a `[[frozen]]` row pins: the canonical sha256 of `{"manifest": <the generation file's digest>, "capabilities": {<id>: <its file's digest, null when absent>}}` over the capabilities it lists — a listed capability edited at the same version changes it |
//! | [`Registry::locator_uses`] | every locator a record names (field path + recorded sha256): what `verify --evidence` resolves |

use std::collections::BTreeMap;

use serde_json::{json, Value};

use super::capability::Capability;
use super::episode::Episode;
use super::experiment::Experiment;
use super::family::Family;
use super::generation::Generation;
use super::incident::Incident;
use super::locks::Locks;
use super::value::{EvidenceRef, Locator, RecordKind, Time};
use super::variant::Variant;
use super::Finding;
use crate::domain::canonical::canonical_sha256;
use crate::domain::evidence::EvidenceRecord;

/// `(kind, id)`.
pub type RecordKey = (RecordKind, String);

/// Every record (module table).
#[derive(Debug, Clone, Default)]
pub struct Registry {
    pub families: BTreeMap<String, Family>,
    pub variants: BTreeMap<String, Variant>,
    pub experiments: BTreeMap<String, Experiment>,
    pub episodes: BTreeMap<String, Episode>,
    pub incidents: BTreeMap<String, Incident>,
    pub capabilities: BTreeMap<String, Capability>,
    pub generations: BTreeMap<String, Generation>,
    pub evidence: BTreeMap<String, EvidenceRecord>,
    pub locks: Locks,
    /// `pins::toml_digest` of each record's file.
    pub digests: BTreeMap<RecordKey, String>,
}

/// One locator a record names (module table).
#[derive(Debug, Clone, PartialEq)]
pub struct LocatorUse<'a> {
    /// `<kind>/<id>`.
    pub record: String,
    /// The field path: `results[0].evidence`, `evidence[2].ref`, …
    pub field: String,
    pub locator: &'a Locator,
    /// The sha256 the record holds for it (an `[[evidence]]` ref's).
    pub sha256: Option<&'a str>,
}

/// `<kind>/<id>`.
pub fn label(kind: RecordKind, id: &str) -> String {
    format!("{kind}/{id}")
}

impl Registry {
    /// The kinds holding a record `id`.
    pub fn kinds_of(&self, id: &str) -> Vec<RecordKind> {
        RecordKind::ALL
            .into_iter()
            .filter(|k| self.exists(*k, id))
            .collect()
    }

    pub fn exists(&self, kind: RecordKind, id: &str) -> bool {
        match kind {
            RecordKind::Family => self.families.contains_key(id),
            RecordKind::Variant => self.variants.contains_key(id),
            RecordKind::Experiment => self.experiments.contains_key(id),
            RecordKind::Episode => self.episodes.contains_key(id),
            RecordKind::Incident => self.incidents.contains_key(id),
            RecordKind::Capability => self.capabilities.contains_key(id),
            RecordKind::Generation => self.generations.contains_key(id),
            RecordKind::Evidence => self.evidence.contains_key(id),
        }
    }

    /// A record as JSON (its typed fields), `None` when absent.
    pub fn record_json(&self, kind: RecordKind, id: &str) -> Option<Value> {
        fn j<T: serde::Serialize>(m: &BTreeMap<String, T>, id: &str) -> Option<Value> {
            m.get(id).and_then(|r| serde_json::to_value(r).ok())
        }
        match kind {
            RecordKind::Family => j(&self.families, id),
            RecordKind::Variant => j(&self.variants, id),
            RecordKind::Experiment => j(&self.experiments, id),
            RecordKind::Episode => j(&self.episodes, id),
            RecordKind::Incident => j(&self.incidents, id),
            RecordKind::Capability => j(&self.capabilities, id),
            RecordKind::Generation => j(&self.generations, id),
            RecordKind::Evidence => j(&self.evidence, id),
        }
    }

    /// Every `(kind, id, title)`.
    pub(super) fn all_records(&self) -> Vec<(RecordKind, &str, &str)> {
        let mut out = Vec::new();
        macro_rules! push {
            ($m:expr, $k:expr) => {
                for (id, r) in &$m {
                    out.push(($k, id.as_str(), r.title.as_str()));
                }
            };
        }
        push!(self.families, RecordKind::Family);
        push!(self.variants, RecordKind::Variant);
        push!(self.experiments, RecordKind::Experiment);
        push!(self.episodes, RecordKind::Episode);
        push!(self.incidents, RecordKind::Incident);
        push!(self.capabilities, RecordKind::Capability);
        push!(self.generations, RecordKind::Generation);
        push!(self.evidence, RecordKind::Evidence);
        out
    }

    /// Module table: the digest a generation's `[[frozen]]` row locks;
    /// `None` without the generation's file digest.
    pub fn frozen_digest(&self, generation: &str) -> Option<String> {
        let manifest = self
            .digests
            .get(&(RecordKind::Generation, generation.to_string()))?;
        let g = self.generations.get(generation)?;
        let capabilities: serde_json::Map<String, Value> = g
            .capabilities
            .iter()
            .map(|c| {
                let digest = self
                    .digests
                    .get(&(RecordKind::Capability, c.id.clone()))
                    .map_or(Value::Null, |d| Value::String(d.clone()));
                (c.id.clone(), digest)
            })
            .collect();
        Some(canonical_sha256(
            &json!({"manifest": manifest, "capabilities": capabilities}),
        ))
    }

    /// The first outcome a seal must precede: an experiment's `outcome_at`
    /// (a forward one: its FORWARD window start); a variant's earliest over
    /// its experiments, UNKNOWN when any of theirs is. `None` = a variant
    /// with no experiment yet: no outcome exists.
    pub fn first_outcome(&self, kind: RecordKind, id: &str) -> Option<Time> {
        match kind {
            RecordKind::Experiment => Some(
                self.experiments
                    .get(id)
                    .map_or(Time::Unknown, Experiment::outcome_at),
            ),
            RecordKind::Variant => {
                let times: Vec<Time> = self
                    .experiments
                    .values()
                    .filter(|e| e.variant == id)
                    .map(Experiment::outcome_at)
                    .collect();
                if times.is_empty() {
                    None
                } else if times.iter().any(|t| !t.is_known()) {
                    Some(Time::Unknown)
                } else {
                    times.into_iter().min_by_key(Time::sort_key)
                }
            }
            _ => Some(Time::Unknown),
        }
    }

    /// Module table: every locator a record names.
    pub fn locator_uses(&self) -> Vec<LocatorUse<'_>> {
        fn add<'a>(
            out: &mut Vec<LocatorUse<'a>>,
            record: &str,
            field: String,
            locator: &'a Locator,
            sha256: Option<&'a str>,
        ) {
            out.push(LocatorUse {
                record: record.to_string(),
                field,
                locator,
                sha256,
            });
        }
        fn refs<'a>(out: &mut Vec<LocatorUse<'a>>, record: &str, evidence: &'a [EvidenceRef]) {
            for (i, e) in evidence.iter().enumerate() {
                let field = format!("evidence[{i}].ref");
                add(out, record, field, &e.locator, e.sha256.as_deref());
            }
        }
        let mut out = Vec::new();
        for f in self.families.values() {
            let r = label(RecordKind::Family, &f.id);
            add(&mut out, &r, "origin".into(), &f.origin, None);
            if let Some(p) = &f.prior_search {
                add(&mut out, &r, "prior_search.source".into(), &p.source, None);
            }
            refs(&mut out, &r, &f.evidence);
        }
        for v in self.variants.values() {
            let r = label(RecordKind::Variant, &v.id);
            if let Some(s) = &v.spec.source {
                add(&mut out, &r, "spec.source".into(), s, None);
            }
            refs(&mut out, &r, &v.evidence);
        }
        for e in self.experiments.values() {
            let r = label(RecordKind::Experiment, &e.id);
            for (i, res) in e.results.iter().enumerate() {
                let field = format!("results[{i}].evidence");
                add(&mut out, &r, field, &res.evidence, None);
            }
            refs(&mut out, &r, &e.evidence);
        }
        for ep in self.episodes.values() {
            let r = label(RecordKind::Episode, &ep.id);
            for (i, f) in ep.context.facts.iter().enumerate() {
                if let Some(l) = &f.locator {
                    add(&mut out, &r, format!("context.facts[{i}].ref"), l, None);
                }
            }
            for (i, info) in ep.information.iter().enumerate() {
                let field = format!("information[{i}].evidence");
                add(&mut out, &r, field, &info.evidence, None);
            }
            for (i, a) in ep.alternatives.iter().enumerate() {
                if let Some(l) = &a.counterfactual_ref {
                    let field = format!("alternatives[{i}].counterfactual_ref");
                    add(&mut out, &r, field, l, None);
                }
            }
            if let Some(l) = ep.action.as_ref().and_then(|a| a.evidence.as_ref()) {
                add(&mut out, &r, "action.evidence".into(), l, None);
            }
            if let Some(lesson) = &ep.lesson {
                for (i, l) in lesson.affects.iter().enumerate() {
                    add(&mut out, &r, format!("lesson.affects[{i}]"), l, None);
                }
            }
            refs(&mut out, &r, &ep.evidence);
        }
        for inc in self.incidents.values() {
            let r = label(RecordKind::Incident, &inc.id);
            for (i, d) in inc.data_impact.iter().enumerate() {
                if let Some(l) = &d.backfill {
                    add(&mut out, &r, format!("data_impact[{i}].backfill"), l, None);
                }
            }
            refs(&mut out, &r, &inc.evidence);
        }
        for c in self.capabilities.values() {
            refs(&mut out, &label(RecordKind::Capability, &c.id), &c.evidence);
        }
        for g in self.generations.values() {
            refs(&mut out, &label(RecordKind::Generation, &g.id), &g.evidence);
        }
        out
    }

    /// Every check of the code table (`lineage/mod.rs`, `validate.rs`),
    /// sorted by severity, record and code.
    pub fn validate(&self) -> Vec<Finding> {
        super::validate::run(self)
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::domain::evidence::EvidenceClass;
    use crate::domain::lineage::episode::EpisodeKind;
    use crate::domain::lineage::experiment::{SplitBy, WindowRole};
    use crate::domain::lineage::locks::{Frozen, Sealed};
    use crate::domain::lineage::pins::toml_digest;
    use crate::domain::lineage::value::{Integrity, UNKNOWN};
    use crate::domain::lineage::Severity;

    /// A minimal valid registry: one family, a root variant, a backtest with
    /// a time split, a forward experiment, an episode, an incident, a
    /// capability and a frozen generation (locked).
    pub(crate) fn minimal() -> Registry {
        let mut r = Registry::default();
        let f: Family = toml::from_str(
            r#"
id = "fam"
title = "a family"
hypothesis = "weekend moves revert"
role = "PRIMARY"
status = "SURVIVING"
status_reason = "holdout positive"
origin_at = "2026-09-30"
origin = "repo:docs/a.md"
origin_by = "operator"
parent = "NONE"
"#,
        )
        .unwrap();
        r.families.insert(f.id.clone(), f);
        let v: Variant = toml::from_str(
            r#"
id = "var"
family = "fam"
title = "the root variant"
parent = "ROOT"
reason = "UNKNOWN"
registered_at = "2026-09-30"
preregistered = false
status = "SURVIVING"
[spec]
described = "fade everything"
"#,
        )
        .unwrap();
        r.variants.insert(v.id.clone(), v);
        let x: Experiment = toml::from_str(
            r#"
id = "bt"
title = "a backtest"
family = "fam"
variant = "var"
generation = "G1"
environment = "XLAB"
kind = "BACKTEST"
registered_at = "2026-10-01"
ran_at = "2026-10-01T10:00:00Z"
preregistered = false
policies = ["DETERMINISTIC"]
cost_model = "3.8 bps round trip"
capabilities = ["cap"]
uncertainty = "bootstrap CI"
split_by = "TIME"
[[windows]]
role = "DEVELOPMENT"
from = "2026-03-01"
to = "2026-06-30"
integrity = "CLEAN"
[[windows]]
role = "HOLDOUT"
from = "2026-07-01"
to = "2026-09-30"
integrity = "CLEAN"
[[results]]
label = "holdout"
class = "HOLDOUT"
n = 40
mean_net_bps = 12.5
ci95_bps = [1.0, 20.0]
evidence = "run:xlab/20261001T100000Z-fade"
[verdict]
value = "PASS"
reason = "positive"
decided_at = "2026-10-01"
decided_by = "operator"
"#,
        )
        .unwrap();
        r.experiments.insert(x.id.clone(), x);
        let fw: Experiment = toml::from_str(
            r#"
id = "fw"
title = "a forward run"
family = "fam"
variant = "var"
generation = "G1"
environment = "XMARKET"
kind = "FORWARD_PAPER"
registered_at = "2026-10-02"
ran_at = "2026-10-02"
preregistered = false
cost_model = "paper fills"
uncertainty = "one weekend"
incidents = ["inc"]
validity = "VALID_WITH_LIMITATIONS"
[[windows]]
role = "FORWARD"
from = "2026-10-02T20:00:00Z"
to = "2026-10-05T13:00:00Z"
integrity = "CLEAN"
[[results]]
label = "capped"
class = "FORWARD_PAPER"
n = 4
net_usd = 1.37
evidence = "vault:w1/ledger.db"
[verdict]
value = "PENDING"
reason = "one weekend"
decided_at = "UNKNOWN"
decided_by = "operator"
"#,
        )
        .unwrap();
        r.experiments.insert(fw.id.clone(), fw);
        let ep: Episode = toml::from_str(
            r#"
id = "ep"
title = "the weekend"
kind = "STRATEGY"
generation = "G1"
experiment = "fw"
family = "fam"
[context]
summary = "a weekend"
as_of = "2026-10-04T22:00:00Z"
[[information]]
item = "Friday close"
available_at = "2026-10-03T00:00:00Z"
evidence = "vault:w1/ledger.db"
provenance = "LIVE_RECORDED"
[[alternatives]]
action = "fade"
chosen = true
counterfactual_valid = false
[[alternatives]]
action = "hold"
chosen = false
counterfactual_valid = true
[decision]
action = "fade"
decided_at = "2026-10-04T22:00:00Z"
policy = "DETERMINISTIC"
[quality]
decision = "SUPPORTED"
decision_note = "the rule"
execution = "CLEAN"
outcome = "FAVORABLE"
"#,
        )
        .unwrap();
        r.episodes.insert(ep.id.clone(), ep);
        let inc: Incident = toml::from_str(
            r#"
id = "inc"
title = "a recorder gap"
class = "DATA"
started_at = "2026-10-03T01:00:00Z"
ended_at = "2026-10-03T02:00:00Z"
detected_by = "coverage"
generation = "G1"
experiment = "fw"
strategy_impact = "NONE"
strategy_impact_note = "outside the window"
"#,
        )
        .unwrap();
        r.incidents.insert(inc.id.clone(), inc);
        let cap_text = r#"
id = "cap"
title = "weekend window"
class = "CAPITAL"
version = 1
permission = "RESEARCH"
lifecycle = "FORWARD_PAPER"
contract = "tool_schema:backtest"
bindings = ["strategy_kind:weekend_window", "tool:backtest"]
"#;
        let cap: Capability = toml::from_str(cap_text).unwrap();
        r.capabilities.insert(cap.id.clone(), cap);
        r.digests.insert(
            (RecordKind::Capability, "cap".into()),
            toml_digest(cap_text).unwrap(),
        );
        let g_text = r#"
id = "G1"
title = "generation one"
status = "FROZEN"
parent = "NONE"
frozen_at = "2026-10-06"
sandboxes = ["w1"]
[[capabilities]]
id = "cap"
version = 1
"#;
        let g: Generation = toml::from_str(g_text).unwrap();
        r.generations.insert(g.id.clone(), g);
        r.digests.insert(
            (RecordKind::Generation, "G1".into()),
            toml_digest(g_text).unwrap(),
        );
        r.locks.frozen.push(Frozen {
            generation: "G1".into(),
            manifest_sha256: r.frozen_digest("G1").unwrap(),
            frozen_at: "2026-10-06".parse().unwrap(),
            commit: None,
        });
        r
    }

    fn codes(r: &Registry) -> Vec<(Severity, String)> {
        r.validate()
            .into_iter()
            .map(|f| (f.severity, f.code))
            .collect()
    }

    fn fires(r: &Registry, code: &str) {
        let found = r.validate();
        assert!(
            found
                .iter()
                .any(|f| f.code == code && f.severity == Severity::Error),
            "{code} not among {found:#?}"
        );
    }

    #[test]
    fn the_minimal_registry_is_clean() {
        assert_eq!(codes(&minimal()), vec![]);
    }

    #[test]
    fn ids_titles_and_shapes() {
        let mut r = minimal();
        r.families.get_mut("fam").unwrap().title = "x".repeat(161);
        fires(&r, "invalid_field");
        let mut r = minimal();
        let mut f = r.families["fam"].clone();
        f.id = "_bad".into();
        r.families.insert(f.id.clone(), f);
        fires(&r, "invalid_id");
        let mut r = minimal();
        let mut f = r.families["fam"].clone();
        f.id = "var".into();
        r.families.insert("var".into(), f);
        fires(&r, "duplicate_id");
        let mut r = minimal();
        r.variants.get_mut("var").unwrap().spec.spec_sha256 = Some("a".repeat(64));
        fires(&r, "invalid_field");
        let mut r = minimal();
        r.capabilities.get_mut("cap").unwrap().version = 0;
        fires(&r, "invalid_field");
    }

    #[test]
    fn references_must_resolve_and_agree() {
        let mut r = minimal();
        r.experiments.get_mut("bt").unwrap().variant = "nope".into();
        fires(&r, "dangling_ref");
        let mut r = minimal();
        r.experiments.get_mut("bt").unwrap().generation = UNKNOWN.into();
        assert_eq!(codes(&r), vec![]);
        let mut r = minimal();
        r.episodes
            .get_mut("ep")
            .unwrap()
            .evidence
            .push(EvidenceRef {
                locator: "record:incident/gone".parse().unwrap(),
                role: "see".into(),
                class: EvidenceClass::None,
                provenance: crate::domain::evidence::Provenance::NotApplicable,
                sha256: None,
                note: None,
            });
        fires(&r, "dangling_ref");
        let mut r = minimal();
        let mut f2 = r.families["fam"].clone();
        f2.id = "fam2".into();
        r.families.insert(f2.id.clone(), f2);
        r.experiments.get_mut("bt").unwrap().family = "fam2".into();
        fires(&r, "inconsistent_ref");
    }

    #[test]
    fn capabilities_and_generations() {
        let mut r = minimal();
        let mut c2 = r.capabilities["cap"].clone();
        c2.id = "cap2".into();
        r.capabilities.insert(c2.id.clone(), c2);
        fires(&r, "binding_conflict");
        let mut r = minimal();
        r.generations.get_mut("G1").unwrap().capabilities[0].version = 2;
        fires(&r, "capability_version_missing");
    }

    #[test]
    fn point_in_time_and_holdout_rules() {
        let mut r = minimal();
        r.episodes.get_mut("ep").unwrap().information[0].available_at =
            "2026-10-05T00:00:00Z".parse().unwrap();
        fires(&r, "future_leakage");
        let mut r = minimal();
        r.episodes.get_mut("ep").unwrap().information[0].available_at = Time::Unknown;
        fires(&r, "future_leakage");
        let mut r = minimal();
        r.episodes.get_mut("ep").unwrap().information[0].available_at =
            "2026-10-04".parse().unwrap();
        assert_eq!(codes(&r), vec![(Severity::Warn, "future_leakage".into())]);

        let mut r = minimal();
        r.experiments
            .get_mut("bt")
            .unwrap()
            .windows
            .retain(|w| w.role != WindowRole::Holdout);
        fires(&r, "holdout_missing");
        let mut r = minimal();
        r.experiments.get_mut("bt").unwrap().windows[1].from = "2026-06-01".parse().unwrap();
        fires(&r, "holdout_overlaps_development");
        r.experiments.get_mut("bt").unwrap().split_by = Some(SplitBy::Instruments);
        assert_eq!(codes(&r), vec![]);
    }

    #[test]
    fn forward_experiments_are_complete() {
        let mut r = minimal();
        r.experiments.get_mut("fw").unwrap().validity = None;
        fires(&r, "forward_incomplete");
        let mut r = minimal();
        r.experiments.get_mut("fw").unwrap().results[0].class = EvidenceClass::Development;
        fires(&r, "forward_incomplete");
    }

    /// Review #22: a forward result row with UNKNOWN evidence, or a FORWARD
    /// window UNKNOWN → UNKNOWN, is not a complete forward record.
    #[test]
    fn a_forward_record_with_unknown_evidence_or_bounds_is_incomplete() {
        let mut r = minimal();
        r.experiments.get_mut("fw").unwrap().results[0].evidence = Locator::Unknown;
        fires(&r, "forward_incomplete");
        let mut r = minimal();
        let w = &mut r.experiments.get_mut("fw").unwrap().windows[0];
        w.from = Time::Unknown;
        w.to = Time::Unknown;
        fires(&r, "forward_incomplete");
    }

    /// Review #6: a second experiment of the family developed on months the
    /// first one calls a CLEAN holdout, and ran before it.
    #[test]
    fn a_clean_holdout_another_experiment_developed_on_first_is_flagged() {
        let with_dev = |ran_at: &str| {
            let mut r = minimal();
            let mut d = r.experiments["bt"].clone();
            d.id = "bt.early".into();
            d.ran_at = ran_at.parse().unwrap();
            d.windows.retain(|w| w.role == WindowRole::Development);
            d.windows[0].from = "2026-08-01".parse().unwrap();
            d.windows[0].to = "2026-09-01".parse().unwrap();
            d.results.clear();
            r.experiments.insert(d.id.clone(), d);
            r
        };
        let r = with_dev("2026-09-15T00:00:00Z");
        let found = r.validate();
        let hit: Vec<_> = found
            .iter()
            .filter(|f| f.code == "holdout_seen_before")
            .collect();
        assert!(
            hit.len() == 1
                && hit[0].severity == Severity::Error
                && hit[0].record == "experiment/bt",
            "{found:#?}"
        );
        assert!(hit[0].message.contains("bt.early"), "{}", hit[0].message);
        // Developed after the holdout was read: no finding.
        assert_eq!(codes(&with_dev("2026-10-02T00:00:00Z")), vec![]);
        // Split by instruments: the scopes are text — a warning.
        let mut r = with_dev("2026-09-15T00:00:00Z");
        r.experiments.get_mut("bt").unwrap().split_by = Some(SplitBy::Instruments);
        assert_eq!(
            codes(&r),
            vec![(Severity::Warn, "holdout_seen_before".into())]
        );
        // Marked CONTAMINATED: nothing to flag.
        let mut r = with_dev("2026-09-15T00:00:00Z");
        r.experiments.get_mut("bt").unwrap().windows[1].integrity = Integrity::Contaminated;
        assert_eq!(codes(&r), vec![]);
        // A CLEAN holdout with an UNKNOWN bound cannot be checked: a warning.
        let mut r = minimal();
        r.experiments.get_mut("bt").unwrap().windows[1].from = Time::Unknown;
        assert_eq!(codes(&r), vec![(Severity::Warn, "window_unknown".into())]);
    }

    /// Review #21: the context is as of the decision or before; the action
    /// at it or after; the decision time is known.
    #[test]
    fn an_episode_context_after_or_action_before_its_decision_leaks() {
        let mut r = minimal();
        r.episodes.get_mut("ep").unwrap().context.as_of = "2026-10-05T14:00:00Z".parse().unwrap();
        r.episodes.get_mut("ep").unwrap().information.clear();
        fires(&r, "future_leakage");
        let mut r = minimal();
        r.episodes.get_mut("ep").unwrap().action = Some(super::super::episode::ActionTaken {
            executed: true,
            executed_at: Some("2026-10-04T21:00:00Z".parse().unwrap()),
            evidence: None,
        });
        fires(&r, "future_leakage");
        let mut r = minimal();
        let ep = r.episodes.get_mut("ep").unwrap();
        ep.information.clear();
        ep.decision.decided_at = Time::Unknown;
        fires(&r, "future_leakage");
    }

    #[test]
    fn locks_freeze_and_seal() {
        let mut r = minimal();
        r.digests
            .insert((RecordKind::Generation, "G1".into()), "b".repeat(64));
        fires(&r, "frozen_manifest_changed");
        let mut r = minimal();
        r.locks.frozen.clear();
        fires(&r, "frozen_manifest_changed");

        // A preregistered experiment: unsealed fails, sealed before the
        // outcome passes, a changed file or a late seal fails.
        let mut r = minimal();
        r.experiments.get_mut("fw").unwrap().preregistered = true;
        fires(&r, "seal_mismatch");
        r.digests
            .insert((RecordKind::Experiment, "fw".into()), "c".repeat(64));
        r.locks.sealed.push(Sealed {
            record: "experiment:fw".into(),
            sha256: "c".repeat(64),
            sealed_at: "2026-10-02T12:00:00Z".parse().unwrap(),
        });
        assert_eq!(codes(&r), vec![]);
        let mut changed = r.clone();
        changed
            .digests
            .insert((RecordKind::Experiment, "fw".into()), "d".repeat(64));
        fires(&changed, "seal_mismatch");
        let mut late = r.clone();
        late.locks.sealed[0].sealed_at = "2026-10-06T00:00:00Z".parse().unwrap();
        fires(&late, "seal_mismatch");
    }

    /// Review #5: a forward experiment's outcomes accrue from its first
    /// entry — a seal after the FORWARD window start (here: 16 h into the
    /// weekend, before its end) is late; an unknown start cannot be sealed.
    #[test]
    fn a_forward_seal_after_the_window_start_is_late() {
        let mut r = minimal();
        r.experiments.get_mut("fw").unwrap().preregistered = true;
        r.digests
            .insert((RecordKind::Experiment, "fw".into()), "c".repeat(64));
        r.locks.sealed.push(Sealed {
            record: "experiment:fw".into(),
            sha256: "c".repeat(64),
            sealed_at: "2026-10-03T12:00:00Z".parse().unwrap(),
        });
        fires(&r, "seal_mismatch");
        assert_eq!(
            r.first_outcome(RecordKind::Experiment, "fw"),
            Some("2026-10-02T20:00:00Z".parse().unwrap())
        );
        // The variant's first outcome is its forward experiment's start.
        assert_eq!(
            r.first_outcome(RecordKind::Variant, "var"),
            Some("2026-10-01T10:00:00Z".parse().unwrap()),
            "the backtest ran earlier"
        );
        r.experiments.get_mut("fw").unwrap().windows[0].from = Time::Unknown;
        assert_eq!(
            r.first_outcome(RecordKind::Experiment, "fw"),
            Some(Time::Unknown)
        );
        assert_eq!(
            r.first_outcome(RecordKind::Variant, "var"),
            Some(Time::Unknown)
        );
        assert!(r.validate().iter().any(|f| f.code == "seal_mismatch"
            && f.severity == Severity::Warn
            && f.message.contains("UNKNOWN")));
        // A variant with no experiment has no outcome yet.
        assert_eq!(r.first_outcome(RecordKind::Variant, "none"), None);
    }

    /// Review #1: a capability a FROZEN generation lists gains a binding at
    /// the same version — the lock covers its record, so the freeze breaks.
    #[test]
    fn a_listed_capability_widened_at_the_same_version_breaks_the_freeze() {
        let mut r = minimal();
        let widened = r#"
id = "cap"
title = "weekend window"
class = "CAPITAL"
version = 1
permission = "RESEARCH"
lifecycle = "FORWARD_PAPER"
contract = "tool_schema:backtest"
bindings = ["strategy_kind:weekend_window", "strategy_kind:event_window", "tool:backtest"]
"#;
        r.capabilities
            .insert("cap".into(), toml::from_str(widened).unwrap());
        r.digests.insert(
            (RecordKind::Capability, "cap".into()),
            toml_digest(widened).unwrap(),
        );
        fires(&r, "frozen_manifest_changed");
        // A capability the generation does not list changes nothing.
        let mut r = minimal();
        r.digests
            .insert((RecordKind::Capability, "other".into()), "e".repeat(64));
        assert_eq!(codes(&r), vec![]);
    }

    #[test]
    fn episodes_need_one_chosen_alternative() {
        let mut r = minimal();
        r.episodes.get_mut("ep").unwrap().alternatives[1].chosen = true;
        fires(&r, "invalid_field");
        let mut r = minimal();
        let ep = r.episodes.get_mut("ep").unwrap();
        ep.kind = EpisodeKind::OperationalIncident;
        fires(&r, "invalid_field");
    }
}
