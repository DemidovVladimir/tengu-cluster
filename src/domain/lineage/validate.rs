//! The registry's checks — `Registry::validate` (`docs/lineage-2026-10-06.md`
//! § 2): every code of the table in `lineage/mod.rs`, from record fields and
//! file digests only (pins, evidence and run dirs: `application/lineage/verify.rs`).
//!
//! | Check | Codes |
//! |---|---|
//! | ids, titles | `invalid_id`, `duplicate_id`, `invalid_field` |
//! | references (fields, `record:` locators, lock rows) | `dangling_ref`, `inconsistent_ref` |
//! | families · variants | parents, `[spec]` shape, preregistration |
//! | experiments | windows, CIs, `holdout_missing`, `holdout_overlaps_development`, `forward_incomplete`, preregistration |
//! | experiments of one family | `holdout_seen_before` (a CLEAN HOLDOUT overlapping another experiment's DEVELOPMENT that ran not after it; Warn under `split_by = "INSTRUMENTS"` or a same-day order), `window_unknown` (Warn: a CLEAN HOLDOUT / DEVELOPMENT window with an UNKNOWN bound) |
//! | episodes | alternatives, incidents, `future_leakage` |
//! | capabilities · generations | `binding_conflict`, `capability_version_missing`, frozen_at, commits, pins |
//! | ranking contracts | shape (`RankingContract::shape_errors` → `invalid_field`), `ranking_unsealed` (Warn) |
//! | `locks.toml` | `frozen_manifest_changed` (`Registry::frozen_digest`: the manifest + its listed capability records), `seal_mismatch` (a sealed ranking contract changed since included) |

use std::collections::{BTreeMap, BTreeSet};

use super::episode::EpisodeKind;
use super::experiment::{ExperimentKind, SplitBy, WindowRole};
use super::generation::GenerationStatus;
use super::query::enum_name;
use super::registry::{label, Registry};
use super::value::{
    parse_seal_record, valid_commit, valid_id, windows_overlap, Binding, EvidenceRef, Integrity,
    Locator, RecordKind, TimeOrder, MAX_TITLE_CHARS, UNKNOWN,
};
use super::{Finding, Severity};
use crate::domain::evidence::{valid_sha256, EvidenceClass};

/// Ids that name no record (missing-fact markers).
const NOT_A_REF: [&str; 2] = [UNKNOWN, "NONE"];

/// Every check, sorted by severity, record and code.
pub(super) fn run(reg: &Registry) -> Vec<Finding> {
    let mut v = Checks {
        reg,
        out: Vec::new(),
    };
    v.ids();
    v.families();
    v.variants();
    v.experiments();
    v.episodes();
    v.incidents();
    v.capabilities();
    v.generations();
    v.evidence_records();
    v.rankings();
    v.locks();
    v.locators();
    let mut out = v.out;
    out.sort();
    out.dedup();
    out
}

struct Checks<'a> {
    reg: &'a Registry,
    out: Vec<Finding>,
}

impl Checks<'_> {
    fn err(&mut self, code: &str, record: &str, message: String) {
        self.out.push(Finding::error(code, record, message));
    }

    fn warn(&mut self, code: &str, record: &str, message: String) {
        self.out.push(Finding::warn(code, record, message));
    }

    /// `field` names a `kind` record `id` (`exempt` ids name none).
    fn reference(&mut self, at: &str, field: &str, kind: RecordKind, id: &str, exempt: &[&str]) {
        if exempt.contains(&id) || self.reg.exists(kind, id) {
            return;
        }
        self.err(
            "dangling_ref",
            at,
            format!("{field} names {kind} `{id}`, which is not in the registry"),
        );
    }

    fn evidence_refs(&mut self, at: &str, refs: &[EvidenceRef]) {
        for (i, e) in refs.iter().enumerate() {
            if let Some(sha) = &e.sha256 {
                if !valid_sha256(sha) {
                    self.err(
                        "invalid_field",
                        at,
                        format!("evidence[{i}].sha256 `{sha}` is not 64 lowercase hex chars"),
                    );
                }
            }
        }
    }

    fn sha(&mut self, at: &str, field: &str, sha: &str) {
        if !valid_sha256(sha) {
            self.err(
                "invalid_field",
                at,
                format!("{field} `{sha}` is not 64 lowercase hex chars"),
            );
        }
    }

    fn ids(&mut self) {
        let mut by_id: BTreeMap<&str, Vec<RecordKind>> = BTreeMap::new();
        for (kind, id, title) in self.reg.all_records() {
            let at = label(kind, id);
            if !valid_id(id) {
                self.err(
                    "invalid_id",
                    &at,
                    format!("id `{id}` is not ^[A-Za-z0-9][A-Za-z0-9._-]{{0,79}}$"),
                );
            }
            let n = title.chars().count();
            if title.trim().is_empty() || n > MAX_TITLE_CHARS {
                self.err(
                    "invalid_field",
                    &at,
                    format!("title: 1–{MAX_TITLE_CHARS} chars (has {n})"),
                );
            }
            by_id.entry(id).or_default().push(kind);
        }
        for (id, kinds) in by_id {
            if kinds.len() > 1 {
                let names: Vec<&str> = kinds.iter().map(|k| k.name()).collect();
                self.err(
                    "duplicate_id",
                    &label(kinds[0], id),
                    format!(
                        "id `{id}` names a {} — an id is unique across kinds",
                        names.join(" and a ")
                    ),
                );
            }
        }
    }

    fn families(&mut self) {
        for f in self.reg.families.values() {
            let at = label(RecordKind::Family, &f.id);
            if f.parent == f.id {
                self.err(
                    "invalid_field",
                    &at,
                    "parent names the family itself".into(),
                );
            } else {
                self.reference(&at, "parent", RecordKind::Family, &f.parent, &NOT_A_REF);
            }
            for p in &f.preceded_by {
                self.reference(&at, "preceded_by", RecordKind::Family, p, &[]);
            }
            for c in &f.controls {
                self.reference(&at, "controls", RecordKind::Family, c, &[]);
            }
            self.evidence_refs(&at, &f.evidence);
        }
    }

    fn variants(&mut self) {
        let reg = self.reg;
        for v in reg.variants.values() {
            let at = label(RecordKind::Variant, &v.id);
            self.reference(&at, "family", RecordKind::Family, &v.family, &[]);
            if v.parent != "ROOT" {
                self.reference(&at, "parent", RecordKind::Variant, &v.parent, &[]);
                if let Some(p) = reg.variants.get(&v.parent) {
                    if p.family != v.family {
                        self.err(
                            "inconsistent_ref",
                            &at,
                            format!(
                                "parent `{}` is of family `{}`, this variant of `{}`",
                                p.id, p.family, v.family
                            ),
                        );
                    }
                }
                // A parent chain reaches ROOT within the registry's size.
                let mut cur = v.parent.as_str();
                let mut seen = BTreeSet::from([v.id.as_str()]);
                while let Some(p) = reg.variants.get(cur) {
                    if !seen.insert(p.id.as_str()) {
                        self.err(
                            "invalid_field",
                            &at,
                            format!("parent chain loops at `{}`", p.id),
                        );
                        break;
                    }
                    cur = p.parent.as_str();
                }
            }
            match v.spec.shape() {
                Err(e) => self.err("invalid_field", &at, e),
                Ok(_) => {
                    if let Some(sha) = &v.spec.spec_sha256 {
                        self.sha(&at, "spec.spec_sha256", sha);
                    }
                    for (field, name) in [
                        ("spec.sandbox", &v.spec.sandbox),
                        ("spec.strategy", &v.spec.strategy),
                    ] {
                        if let Some(n) = name.as_deref().filter(|n| !valid_id(n)) {
                            self.err("invalid_field", &at, format!("{field} `{n}` is not a name"));
                        }
                    }
                }
            }
            if v.preregistered {
                self.prereg(RecordKind::Variant, &v.id, &v.evidence);
            }
            self.evidence_refs(&at, &v.evidence);
        }
    }

    /// A preregistered record is sealed or carries a `prereg` evidence ref
    /// (the seal rows themselves: [`Checks::locks`]).
    fn prereg(&mut self, kind: RecordKind, id: &str, evidence: &[EvidenceRef]) {
        let key = format!("{kind}:{id}");
        let sealed = self.reg.locks.sealed.iter().any(|s| s.record == key);
        if !sealed && !evidence.iter().any(|e| e.role == "prereg") {
            self.err(
                "seal_mismatch",
                &label(kind, id),
                format!(
                    "preregistered, but neither sealed (`tengu lineage seal {key}`) nor carrying \
                     an [[evidence]] with role = \"prereg\""
                ),
            );
        }
    }

    fn experiments(&mut self) {
        let reg = self.reg;
        for e in reg.experiments.values() {
            let at = label(RecordKind::Experiment, &e.id);
            self.reference(&at, "family", RecordKind::Family, &e.family, &[]);
            self.reference(&at, "variant", RecordKind::Variant, &e.variant, &NOT_A_REF);
            self.reference(
                &at,
                "generation",
                RecordKind::Generation,
                &e.generation,
                &NOT_A_REF,
            );
            for c in &e.capabilities {
                self.reference(&at, "capabilities", RecordKind::Capability, c, &[]);
            }
            for i in &e.incidents {
                self.reference(&at, "incidents", RecordKind::Incident, i, &[]);
            }
            if let Some(v) = reg.variants.get(&e.variant) {
                if v.family != e.family {
                    self.err(
                        "inconsistent_ref",
                        &at,
                        format!(
                            "variant `{}` is of family `{}`, the experiment of `{}`",
                            v.id, v.family, e.family
                        ),
                    );
                }
            }
            for (i, w) in e.windows.iter().enumerate() {
                if let (Some(lo), Some(hi)) = (w.from.earliest(), w.to.window_end()) {
                    if lo >= hi {
                        self.err(
                            "invalid_field",
                            &at,
                            format!("windows[{i}]: from {} is not before to {}", w.from, w.to),
                        );
                    }
                }
            }
            for (i, r) in e.results.iter().enumerate() {
                if let Some([lo, hi]) = r.ci95_bps {
                    if lo > hi {
                        self.err(
                            "invalid_field",
                            &at,
                            format!("results[{i}].ci95_bps: low {lo} > high {hi}"),
                        );
                    }
                }
            }
            let has_holdout = e.windows_of(WindowRole::Holdout).next().is_some();
            if !has_holdout {
                if let Some(r) = e.results.iter().find(|r| r.class == EvidenceClass::Holdout) {
                    self.err(
                        "holdout_missing",
                        &at,
                        format!(
                            "result `{}` is HOLDOUT, but no window has role HOLDOUT",
                            r.label
                        ),
                    );
                } else if e.kind == ExperimentKind::HoldoutTest {
                    self.err(
                        "holdout_missing",
                        &at,
                        "a HOLDOUT_TEST without a HOLDOUT window".into(),
                    );
                }
            }
            if e.split_by != Some(SplitBy::Instruments) {
                for d in e.windows_of(WindowRole::Development) {
                    for h in e.windows_of(WindowRole::Holdout) {
                        if windows_overlap((&d.from, &d.to), (&h.from, &h.to)) == Some(true) {
                            self.err(
                                "holdout_overlaps_development",
                                &at,
                                format!(
                                    "DEVELOPMENT {} → {} overlaps HOLDOUT {} → {} — a time \
                                     overlap needs split_by = \"INSTRUMENTS\"",
                                    d.from, d.to, h.from, h.to
                                ),
                            );
                        }
                    }
                }
            }
            if e.kind == ExperimentKind::ForwardPaper {
                let mut missing = Vec::new();
                if e.windows_of(WindowRole::Forward).next().is_none() {
                    missing.push("a FORWARD window");
                } else if !e
                    .windows_of(WindowRole::Forward)
                    .any(|w| w.from.is_known() && w.to.is_known())
                {
                    missing.push("a FORWARD window with known bounds");
                }
                if e.validity.is_none() {
                    missing.push("a validity");
                }
                let forward_results: Vec<_> = e
                    .results
                    .iter()
                    .filter(|r| r.class == EvidenceClass::ForwardPaper)
                    .collect();
                let forward_evidence = e
                    .evidence
                    .iter()
                    .any(|r| r.class == EvidenceClass::ForwardPaper)
                    || !forward_results.is_empty();
                if !forward_evidence {
                    missing.push("FORWARD_PAPER evidence");
                } else if !forward_results.is_empty()
                    && forward_results
                        .iter()
                        .all(|r| r.evidence == Locator::Unknown)
                {
                    missing.push("a FORWARD_PAPER result with known evidence");
                }
                if !missing.is_empty() {
                    self.err(
                        "forward_incomplete",
                        &at,
                        format!("a FORWARD_PAPER experiment without {}", missing.join(", ")),
                    );
                }
            }
            if e.preregistered {
                self.prereg(RecordKind::Experiment, &e.id, &e.evidence);
            }
            self.evidence_refs(&at, &e.evidence);
        }
        self.holdouts_across_experiments();
    }

    /// A CLEAN holdout another experiment of the family developed on first
    /// (`holdout_seen_before`); a CLEAN holdout / development window with an
    /// UNKNOWN bound (`window_unknown`, Warn: nothing can be compared).
    fn holdouts_across_experiments(&mut self) {
        let reg = self.reg;
        for e in reg.experiments.values() {
            let at = label(RecordKind::Experiment, &e.id);
            for (i, w) in e.windows.iter().enumerate() {
                let checked = matches!(w.role, WindowRole::Holdout | WindowRole::Development);
                if checked
                    && w.integrity == Integrity::Clean
                    && !(w.from.is_known() && w.to.is_known())
                {
                    self.warn(
                        "window_unknown",
                        &at,
                        format!(
                            "windows[{i}] {} {} → {} is CLEAN with an UNKNOWN bound: its overlap \
                             with development cannot be checked",
                            enum_name(&w.role),
                            w.from,
                            w.to
                        ),
                    );
                }
            }
            for h in e
                .windows_of(WindowRole::Holdout)
                .filter(|w| w.integrity == Integrity::Clean)
            {
                for d_exp in reg
                    .experiments
                    .values()
                    .filter(|d| d.family == e.family && d.id != e.id)
                {
                    let order = d_exp.ran_at.order(&e.ran_at);
                    if matches!(order, TimeOrder::After | TimeOrder::Unknown) {
                        continue;
                    }
                    for d in d_exp.windows_of(WindowRole::Development) {
                        if windows_overlap((&d.from, &d.to), (&h.from, &h.to)) != Some(true) {
                            continue;
                        }
                        let text = format!(
                            "HOLDOUT {} → {} is CLEAN, but experiment `{}` (ran {}, not after \
                             this one's {}) developed on {} → {}",
                            h.from, h.to, d_exp.id, d_exp.ran_at, e.ran_at, d.from, d.to
                        );
                        if e.split_by == Some(SplitBy::Instruments) {
                            self.warn(
                                "holdout_seen_before",
                                &at,
                                format!(
                                    "{text} — split_by INSTRUMENTS: scopes are text and cannot \
                                     be compared"
                                ),
                            );
                        } else if order == TimeOrder::Ambiguous && d_exp.ran_at != e.ran_at {
                            self.warn(
                                "holdout_seen_before",
                                &at,
                                format!("{text} — same day: give instants"),
                            );
                        } else {
                            self.err(
                                "holdout_seen_before",
                                &at,
                                format!("{text} — mark the window CONTAMINATED"),
                            );
                        }
                    }
                }
            }
        }
    }

    fn episodes(&mut self) {
        let reg = self.reg;
        for ep in reg.episodes.values() {
            let at = label(RecordKind::Episode, &ep.id);
            self.reference(
                &at,
                "generation",
                RecordKind::Generation,
                &ep.generation,
                &NOT_A_REF,
            );
            if let Some(x) = &ep.experiment {
                self.reference(&at, "experiment", RecordKind::Experiment, x, &NOT_A_REF);
            }
            if let Some(f) = &ep.family {
                self.reference(&at, "family", RecordKind::Family, f, &NOT_A_REF);
            }
            if let Some(v) = &ep.variant {
                self.reference(&at, "variant", RecordKind::Variant, v, &NOT_A_REF);
            }
            if let Some(f) = ep.hypothesis.as_ref().and_then(|h| h.family.as_ref()) {
                self.reference(&at, "hypothesis.family", RecordKind::Family, f, &NOT_A_REF);
            }
            for i in &ep.incidents {
                self.reference(&at, "incidents", RecordKind::Incident, i, &[]);
            }
            let family = ep.family.as_deref();
            let x_family = ep
                .experiment
                .as_ref()
                .and_then(|x| reg.experiments.get(x))
                .map(|x| x.family.as_str());
            let v_family = ep
                .variant
                .as_ref()
                .and_then(|v| reg.variants.get(v))
                .map(|v| v.family.as_str());
            let known: Vec<(&str, &str)> = [
                ("family", family),
                ("experiment", x_family),
                ("variant", v_family),
            ]
            .into_iter()
            .filter_map(|(k, f)| f.filter(|f| !NOT_A_REF.contains(f)).map(|f| (k, f)))
            .collect();
            if known.windows(2).any(|w| w[0].1 != w[1].1) {
                let text: Vec<String> = known.iter().map(|(k, f)| format!("{k} → `{f}`")).collect();
                self.err(
                    "inconsistent_ref",
                    &at,
                    format!("names more than one family: {}", text.join(", ")),
                );
            }
            if ep.kind == EpisodeKind::OperationalIncident && ep.incidents.is_empty() {
                self.err(
                    "invalid_field",
                    &at,
                    "an OPERATIONAL_INCIDENT episode names no incident".into(),
                );
            }
            let chosen = ep.alternatives.iter().filter(|a| a.chosen).count();
            let strict = matches!(
                ep.kind,
                EpisodeKind::Strategy | EpisodeKind::RejectedStrategy
            );
            if (strict && chosen != 1) || chosen > 1 {
                self.err(
                    "invalid_field",
                    &at,
                    format!(
                        "{chosen} alternative(s) chosen — {}",
                        if strict {
                            "list the alternatives, exactly one chosen = true"
                        } else {
                            "at most one"
                        }
                    ),
                );
            }
            let decided = ep.decision.decided_at;
            if !decided.is_known() {
                self.err(
                    "future_leakage",
                    &at,
                    "decision.decided_at is UNKNOWN: its context and information cannot be shown \
                     to precede it"
                        .into(),
                );
            }
            // The context is as of the decision or earlier; the action at it or later.
            let as_of = ep.context.as_of;
            match as_of.order(&decided) {
                TimeOrder::After => self.err(
                    "future_leakage",
                    &at,
                    format!("context.as_of {as_of} is after the decision at {decided}"),
                ),
                TimeOrder::Ambiguous if as_of != decided => self.warn(
                    "future_leakage",
                    &at,
                    format!(
                        "context.as_of {as_of} vs decision {decided}: not provably before — give instants"
                    ),
                ),
                _ => {}
            }
            if let Some(executed) = ep.action.as_ref().and_then(|a| a.executed_at) {
                match decided.order(&executed) {
                    TimeOrder::After => self.err(
                        "future_leakage",
                        &at,
                        format!(
                            "action.executed_at {executed} is before the decision at {decided}"
                        ),
                    ),
                    TimeOrder::Ambiguous if executed != decided => self.warn(
                        "future_leakage",
                        &at,
                        format!(
                            "action.executed_at {executed} vs decision {decided}: not provably \
                             after — give instants"
                        ),
                    ),
                    _ => {}
                }
            }
            for (i, info) in ep.information.iter().enumerate() {
                let what = format!("information[{i}] `{}`", info.item);
                match info.available_at.order(&decided) {
                    TimeOrder::NotAfter => {}
                    TimeOrder::After => self.err(
                        "future_leakage",
                        &at,
                        format!(
                            "{what} became available at {}, after the decision at {decided}",
                            info.available_at
                        ),
                    ),
                    TimeOrder::Ambiguous => self.warn(
                        "future_leakage",
                        &at,
                        format!(
                            "{what} available {} vs decision {decided}: not provably before — give instants",
                            info.available_at
                        ),
                    ),
                    TimeOrder::Unknown if !info.available_at.is_known() => self.err(
                        "future_leakage",
                        &at,
                        format!("{what} has available_at UNKNOWN: unknown-timed information"),
                    ),
                    TimeOrder::Unknown => {}
                }
            }
            self.evidence_refs(&at, &ep.evidence);
        }
    }

    fn incidents(&mut self) {
        for inc in self.reg.incidents.values() {
            let at = label(RecordKind::Incident, &inc.id);
            self.reference(
                &at,
                "generation",
                RecordKind::Generation,
                &inc.generation,
                &NOT_A_REF,
            );
            if let Some(x) = &inc.experiment {
                self.reference(&at, "experiment", RecordKind::Experiment, x, &NOT_A_REF);
            }
            if inc.started_at.order(&inc.ended_at) == TimeOrder::After {
                self.err(
                    "invalid_field",
                    &at,
                    format!(
                        "ended_at {} is before started_at {}",
                        inc.ended_at, inc.started_at
                    ),
                );
            }
            for (i, d) in inc.data_impact.iter().enumerate() {
                if d.from.order(&d.to) == TimeOrder::After {
                    self.err(
                        "invalid_field",
                        &at,
                        format!("data_impact[{i}]: to {} is before from {}", d.to, d.from),
                    );
                }
            }
            self.evidence_refs(&at, &inc.evidence);
        }
    }

    fn capabilities(&mut self) {
        let mut owners: BTreeMap<&Binding, Vec<&str>> = BTreeMap::new();
        for c in self.reg.capabilities.values() {
            let at = label(RecordKind::Capability, &c.id);
            if c.version == 0 {
                self.err("invalid_field", &at, "version must be ≥ 1".into());
            }
            for b in &c.bindings {
                owners.entry(b).or_default().push(&c.id);
            }
            self.evidence_refs(&at, &c.evidence);
        }
        for (b, caps) in owners {
            let mut caps = caps;
            caps.dedup();
            if caps.len() > 1 {
                for c in &caps[1..] {
                    self.err(
                        "binding_conflict",
                        &label(RecordKind::Capability, c),
                        format!(
                            "binding `{b}` is owned by capabilities `{}` — a binding belongs to one capability",
                            caps.join("` and `")
                        ),
                    );
                }
            }
        }
    }

    fn generations(&mut self) {
        let reg = self.reg;
        for g in reg.generations.values() {
            let at = label(RecordKind::Generation, &g.id);
            if g.parent == g.id {
                self.err(
                    "invalid_field",
                    &at,
                    "parent names the generation itself".into(),
                );
            } else {
                self.reference(&at, "parent", RecordKind::Generation, &g.parent, &NOT_A_REF);
            }
            if g.status == GenerationStatus::Frozen && !g.frozen_at.is_some_and(|t| t.is_known()) {
                self.err(
                    "invalid_field",
                    &at,
                    "FROZEN without a known frozen_at".into(),
                );
            }
            for s in &g.sandboxes {
                if !valid_id(s) {
                    self.err(
                        "invalid_field",
                        &at,
                        format!("sandboxes: `{s}` is not a name"),
                    );
                }
            }
            if let Some(code) = &g.code {
                for (field, c) in [
                    ("code.commit", Some(&code.commit)),
                    ("code.forward_commit", code.forward_commit.as_ref()),
                ] {
                    if let Some(c) = c.filter(|c| *c != UNKNOWN && !valid_commit(c)) {
                        self.err(
                            "invalid_field",
                            &at,
                            format!("{field} `{c}` is not a full 40-hex commit (or UNKNOWN)"),
                        );
                    }
                }
                if let Some(sha) = &code.forward_binary_sha256 {
                    self.sha(&at, "code.forward_binary_sha256", sha);
                }
            }
            let mut seen = BTreeSet::new();
            for c in &g.capabilities {
                if !seen.insert(c.id.as_str()) {
                    self.err(
                        "invalid_field",
                        &at,
                        format!("capability `{}` listed twice", c.id),
                    );
                }
                match reg.capabilities.get(&c.id) {
                    None => self.reference(&at, "capabilities", RecordKind::Capability, &c.id, &[]),
                    Some(cap) if cap.version != c.version => self.err(
                        "capability_version_missing",
                        &at,
                        format!(
                            "names capability `{}` version {}; the registry holds version {}",
                            c.id, c.version, cap.version
                        ),
                    ),
                    Some(_) => {}
                }
            }
            let mut targets = BTreeSet::new();
            for p in &g.pins {
                self.sha(&at, &format!("pins `{}` sha256", p.target), &p.sha256);
                if !targets.insert(p.target.to_string()) {
                    self.err(
                        "invalid_field",
                        &at,
                        format!("pin `{}` listed twice", p.target),
                    );
                }
            }
            self.evidence_refs(&at, &g.evidence);
        }
    }

    fn evidence_records(&mut self) {
        for r in self.reg.evidence.values() {
            let at = label(RecordKind::Evidence, &r.id);
            for e in r.validation_errors() {
                self.err("invalid_field", &at, e);
            }
            if let Some(x) = &r.experiment {
                self.reference(&at, "experiment", RecordKind::Experiment, x, &NOT_A_REF);
            }
        }
    }

    /// A ranking contract's shape (`RankingContract::shape_errors`), and an
    /// unsealed one (`ranking_unsealed`, Warn: a draft awaiting the
    /// operator's review; the publisher refuses it). A sealed one changed
    /// since: `seal_mismatch` ([`Checks::locks`]).
    fn rankings(&mut self) {
        let reg = self.reg;
        for c in reg.rankings.values() {
            let at = label(RecordKind::Ranking, &c.id);
            for e in c.shape_errors() {
                self.err("invalid_field", &at, e);
            }
            let key = format!("{}:{}", RecordKind::Ranking, c.id);
            if !reg.locks.sealed.iter().any(|s| s.record == key) {
                self.warn(
                    "ranking_unsealed",
                    &at,
                    format!(
                        "not sealed: no ranking runs under it until the operator reviews it and \
                         runs `tengu lineage seal {key}`"
                    ),
                );
            }
            self.evidence_refs(&at, &c.evidence);
        }
    }

    fn locks(&mut self) {
        let reg = self.reg;
        let at = "locks.toml";
        for f in &reg.locks.frozen {
            self.reference(
                at,
                "[[frozen]] generation",
                RecordKind::Generation,
                &f.generation,
                &[],
            );
            self.sha(
                at,
                &format!("[[frozen]] {} manifest_sha256", f.generation),
                &f.manifest_sha256,
            );
        }
        for g in reg.generations.values() {
            let gat = label(RecordKind::Generation, &g.id);
            let last = reg.locks.frozen.iter().rev().find(|f| f.generation == g.id);
            let digest = reg.frozen_digest(&g.id);
            match (last, digest) {
                (None, _) if g.status == GenerationStatus::Frozen => self.err(
                    "frozen_manifest_changed",
                    &gat,
                    "FROZEN, but locks.toml has no [[frozen]] row for it".into(),
                ),
                (Some(row), Some(now)) if row.manifest_sha256 != now => self.err(
                    "frozen_manifest_changed",
                    &gat,
                    format!(
                        "locks.toml freezes {} (at {}); the manifest with its listed capability \
                         records now hashes {now}",
                        row.manifest_sha256, row.frozen_at
                    ),
                ),
                (Some(_), None) => self.err(
                    "frozen_manifest_changed",
                    &gat,
                    "no file digest to compare with its [[frozen]] row".into(),
                ),
                _ => {}
            }
        }
        for s in &reg.locks.sealed {
            let (kind, id) = match parse_seal_record(&s.record) {
                Ok(k) => k,
                Err(e) => {
                    self.err("invalid_field", at, format!("[[sealed]] {e}"));
                    continue;
                }
            };
            if !reg.exists(kind, &id) {
                self.reference(at, "[[sealed]] record", kind, &id, &[]);
                continue;
            }
            let rat = label(kind, &id);
            self.sha(at, &format!("[[sealed]] {} sha256", s.record), &s.sha256);
            match reg.digests.get(&(kind, id.clone())) {
                Some(now) if *now != s.sha256 => self.err(
                    "seal_mismatch",
                    &rat,
                    format!(
                        "sealed {} at {}; the file now hashes {now}",
                        s.sha256, s.sealed_at
                    ),
                ),
                None => self.err(
                    "seal_mismatch",
                    &rat,
                    "no file digest to compare with its seal".into(),
                ),
                _ => {}
            }
            let Some(outcome) = reg.first_outcome(kind, &id) else {
                continue; // no experiment yet: nothing to precede
            };
            match s.sealed_at.order(&outcome) {
                TimeOrder::Unknown if !outcome.is_known() => self.warn(
                    "seal_mismatch",
                    &rat,
                    "first outcome UNKNOWN (a FORWARD window without a known start, or an \
                     experiment without ran_at): the seal cannot be shown to precede it"
                        .into(),
                ),
                TimeOrder::After => self.err(
                    "seal_mismatch",
                    &rat,
                    format!(
                        "sealed at {}, after its first outcome {outcome}: not a preregistration",
                        s.sealed_at
                    ),
                ),
                TimeOrder::Ambiguous if s.sealed_at != outcome => self.warn(
                    "seal_mismatch",
                    &rat,
                    format!(
                        "sealed {} vs first outcome {outcome}: not provably before — give instants",
                        s.sealed_at
                    ),
                ),
                _ => {}
            }
        }
    }

    fn locators(&mut self) {
        let mut found = Vec::new();
        for u in self.reg.locator_uses() {
            if let Locator::Record { kind, id } = u.locator {
                if !self.reg.exists(*kind, id) {
                    found.push(Finding::new(
                        Severity::Error,
                        "dangling_ref",
                        u.record.clone(),
                        format!("{} names {} — not in the registry", u.field, u.locator),
                    ));
                }
            }
        }
        self.out.extend(found);
    }
}

#[cfg(test)]
mod tests {
    use super::super::locks::Sealed;
    use super::super::ranking::tests::CONTRACT;
    use super::super::ranking::RankingContract;
    use super::super::registry::tests::minimal;
    use super::super::value::Time;
    use super::*;
    use crate::domain::lineage::pins::toml_digest;

    /// The minimal registry with the test contract (`ranking/rank.t`) as
    /// `text`, its file digest recorded.
    fn with_contract(text: &str) -> Registry {
        let mut r = minimal();
        let c: RankingContract = toml::from_str(text).unwrap();
        r.digests.insert(
            (RecordKind::Ranking, c.id.clone()),
            toml_digest(text).unwrap(),
        );
        r.rankings.insert(c.id.clone(), c);
        r
    }

    /// `ranking:rank.t` sealed as `text`.
    fn seal(r: &mut Registry, text: &str) {
        r.locks.sealed.push(Sealed {
            record: "ranking:rank.t".into(),
            sha256: toml_digest(text).unwrap(),
            sealed_at: "2026-10-08T13:00:00Z".parse().unwrap(),
        });
    }

    fn found(r: &Registry) -> Vec<(Severity, String, String, String)> {
        r.validate()
            .into_iter()
            .map(|f| (f.severity, f.code, f.record, f.message))
            .collect()
    }

    #[test]
    fn ranking_contract_shape_errors_name_their_field() {
        type Edit = fn(&mut RankingContract);
        let cases: [(Edit, &str); 17] = [
            (|c| c.preregistered = false, "preregistered: must be true"),
            (
                |c| c.registered_at = Time::Unknown,
                "registered_at: UNKNOWN",
            ),
            (|c| c.sandbox = "a/b".into(), "sandbox: `a/b`"),
            (
                |c| c.evidence_class = crate::domain::evidence::EvidenceClass::Holdout,
                "evidence_class: HOLDOUT",
            ),
            (|c| c.arm = "jev".into(), "arm: `jev`"),
            (|c| c.tz = "Asia/Tokyo".into(), "tz: `Asia/Tokyo`"),
            (|c| c.cutoff = "24:00".into(), "cutoff: `24:00`"),
            (|c| c.days = vec!["Funday".into()], "days: `Funday`"),
            (
                |c| c.days = vec!["Mon".into(), "mon".into()],
                "days: `mon` listed twice",
            ),
            (
                |c| c.from = "2026-03-01T05:00:00Z".parse().unwrap(),
                "from: `2026-03-01T05:00:00Z`",
            ),
            (|c| c.strategies.clear(), "strategies: empty"),
            (
                |c| c.strategies.push("Rule-W".into()),
                "strategies: `Rule-W` is not a strategy name",
            ),
            (
                |c| c.strategies.push("rule_w".into()),
                "strategies: `rule_w` listed twice",
            ),
            (|c| c.cohort.truncate(0), "cohort: empty"),
            (
                |c| {
                    let neg = "-ci95_lo_bps".parse().unwrap();
                    c.rating.order.push(neg);
                },
                "rating.order: `ci95_lo_bps` listed twice",
            ),
            (|c| c.rating.quantum = 0.0, "rating.quantum: 0"),
            (
                |c| c.eligibility.min_trades = 0,
                "eligibility.min_trades: 0",
            ),
        ];
        for (edit, prefix) in cases {
            let mut r = with_contract(CONTRACT);
            seal(&mut r, CONTRACT);
            edit(r.rankings.get_mut("rank.t").unwrap());
            let f = found(&r);
            let hit = f.iter().find(|(s, code, rec, msg)| {
                *s == Severity::Error
                    && code == "invalid_field"
                    && rec == "ranking/rank.t"
                    && msg.starts_with(prefix)
            });
            assert!(hit.is_some(), "{prefix}: {f:#?}");
        }
        // Empty rating order and a duplicate cohort field.
        let mut r = with_contract(CONTRACT);
        let c = r.rankings.get_mut("rank.t").unwrap();
        c.rating.order.clear();
        c.cohort.push(c.cohort[0]);
        let msgs: Vec<String> = found(&r).into_iter().map(|f| f.3).collect();
        assert!(msgs
            .iter()
            .any(|m| m == "rating.order: empty — list the rating keys"));
        assert!(msgs
            .iter()
            .any(|m| m == "cohort: `generation` listed twice"));
    }

    #[test]
    fn an_unsealed_ranking_contract_warns_ranking_unsealed() {
        let r = with_contract(CONTRACT);
        let f = found(&r);
        assert_eq!(f.len(), 1, "{f:#?}");
        assert_eq!(
            (f[0].0, f[0].1.as_str(), f[0].2.as_str()),
            (Severity::Warn, "ranking_unsealed", "ranking/rank.t")
        );
        assert!(
            f[0].3.contains("tengu lineage seal ranking:rank.t"),
            "{}",
            f[0].3
        );
        assert!(!crate::domain::lineage::has_errors(&r.validate()));
        // Sealed: clean. A ranking has no outcome in the registry to precede.
        let mut r = r;
        seal(&mut r, CONTRACT);
        assert_eq!(found(&r), vec![]);
        assert_eq!(r.first_outcome(RecordKind::Ranking, "rank.t"), None);
    }

    #[test]
    fn a_ranking_contract_changed_after_its_seal_is_seal_mismatch() {
        let changed = CONTRACT.replace("min_trades = 20", "min_trades = 10");
        let mut r = with_contract(&changed);
        seal(&mut r, CONTRACT);
        let f = found(&r);
        assert!(
            f.iter().any(|(s, code, rec, msg)| *s == Severity::Error
                && code == "seal_mismatch"
                && rec == "ranking/rank.t"
                && msg.contains(&toml_digest(&changed).unwrap())
                && msg.contains(&toml_digest(CONTRACT).unwrap())),
            "{f:#?}"
        );
        // A comment or layout change keeps the digest: still sealed.
        let commented = format!("# reviewed by the operator\n{CONTRACT}");
        let mut r = with_contract(&commented);
        seal(&mut r, CONTRACT);
        assert_eq!(found(&r), vec![]);
    }
}
