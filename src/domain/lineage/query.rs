//! Registry queries (`docs/lineage-2026-10-06.md` § 5): answered from record
//! fields only, `UNKNOWN` where nothing says — never a guess.
//!
//! | Query | Returns |
//! |---|---|
//! | [`trace`] | from any record id (or `<kind>/<id>`): family → variants → experiments → windows / results / evidence → verdict → episodes → incidents, as indented lines; the start record marked |
//! | [`attempt_rows`] | every run dir → the variants whose `spec_sha256` it carries, or none (`UNREGISTERED`) |
//! | [`family_report`] | the variant tree + search accounting: registered variants, `prior_search`, run-dir attempts mapped to the family's variants (distinct spec hashes, runs, holdout reads) and the `UNREGISTERED` runs of the family's strategy names |
//!
//! The 21 Rule-W acceptance answers: `acceptance.rs`.

use std::collections::{BTreeMap, BTreeSet};

use serde::Serialize;

use super::experiment::{Experiment, ResultRow};
use super::registry::{label, Registry};
use super::value::{cmp_time, RecordKind, UNKNOWN};
use super::variant::{SpecShape, Variant};

// ---------------------------------------------------------------------------
// Attempts (run dirs)
// ---------------------------------------------------------------------------

/// One backtest run dir (`<state dir>/backtests/<run id>/report.json`).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct RunAttempt {
    pub state: String,
    pub run_id: String,
    /// Pinned by the operator (`keep-<run id>`).
    pub kept: bool,
    pub strategy: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    pub spec_sha256: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub split: Option<String>,
}

/// One line of `<state dir>/backtests/holdout-reads.jsonl`.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct HoldoutRead {
    pub state: String,
    pub run_id: String,
    pub spec_sha256: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub strategy: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub time: Option<String>,
}

/// What the run-dir scan found (`application/lineage/attempts.rs`).
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct Attempts {
    /// The state dirs scanned.
    pub states: Vec<String>,
    pub runs: Vec<RunAttempt>,
    pub holdout_reads: Vec<HoldoutRead>,
    /// Unreadable dirs / lines, one each.
    pub problems: Vec<String>,
}

/// A run dir and the variants it maps to (module table).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct AttemptRow {
    pub run: RunAttempt,
    /// Variant ids carrying the run's `spec_sha256`; empty = `UNREGISTERED`.
    pub variants: Vec<String>,
    pub families: Vec<String>,
}

/// `spec_sha256` → variant ids.
fn variants_by_hash(reg: &Registry) -> BTreeMap<&str, Vec<&Variant>> {
    let mut m: BTreeMap<&str, Vec<&Variant>> = BTreeMap::new();
    for v in reg.variants.values() {
        if let Some(h) = &v.spec.spec_sha256 {
            m.entry(h.as_str()).or_default().push(v);
        }
    }
    m
}

/// Module table: every run → its variants.
pub fn attempt_rows(reg: &Registry, attempts: &Attempts) -> Vec<AttemptRow> {
    let by_hash = variants_by_hash(reg);
    attempts
        .runs
        .iter()
        .map(|run| {
            let vs = by_hash
                .get(run.spec_sha256.as_str())
                .cloned()
                .unwrap_or_default();
            let mut families: Vec<String> = vs.iter().map(|v| v.family.clone()).collect();
            families.sort();
            families.dedup();
            AttemptRow {
                run: run.clone(),
                variants: vs.iter().map(|v| v.id.clone()).collect(),
                families,
            }
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Family report
// ---------------------------------------------------------------------------

/// One variant in the tree.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct VariantNode {
    pub depth: usize,
    pub id: String,
    pub title: String,
    pub status: String,
    pub preregistered: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub order: Option<u32>,
    pub registered_at: String,
    pub changed: Vec<String>,
    pub spec: String,
    pub experiments: Vec<String>,
    pub runs: usize,
    pub holdout_reads: usize,
}

/// A run hash no variant carries, of one of the family's strategy names.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Unregistered {
    pub spec_sha256: String,
    pub strategy: String,
    pub runs: Vec<String>,
    pub holdout_reads: usize,
}

/// Search accounting (handoff § 21: how much searching happened).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SearchAccounting {
    pub registered_variants: usize,
    /// `count (precision) — source: note`, or `UNKNOWN` / none.
    pub prior_search: String,
    /// The state dirs scanned (empty = none: run-dir counts UNKNOWN).
    pub states: Vec<String>,
    pub distinct_spec_hashes: usize,
    pub runs: usize,
    pub holdout_reads: usize,
    pub unregistered: Vec<Unregistered>,
    /// Registered + prior search + unregistered hashes, as a lower bound
    /// when anything is unknown.
    pub total_tried: String,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct FamilyReport {
    pub family: String,
    pub title: String,
    pub hypothesis: String,
    pub role: String,
    pub status: String,
    pub status_reason: String,
    pub variants: Vec<VariantNode>,
    pub search: SearchAccounting,
}

pub fn enum_name<T: Serialize>(v: &T) -> String {
    serde_json::to_value(v)
        .ok()
        .and_then(|v| v.as_str().map(String::from))
        .unwrap_or_else(|| UNKNOWN.to_string())
}

fn scalar(v: &serde_json::Value) -> String {
    match v {
        serde_json::Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

fn spec_text(v: &Variant) -> String {
    let s = &v.spec;
    match s.shape() {
        Ok(SpecShape::Library) => format!(
            "sandbox {} strategy {} spec_sha256 {}",
            s.sandbox.as_deref().unwrap_or(UNKNOWN),
            s.strategy.as_deref().unwrap_or(UNKNOWN),
            s.spec_sha256.as_deref().unwrap_or(UNKNOWN)
        ),
        Ok(SpecShape::Run) => format!(
            "spec_sha256 {} seen in {}",
            s.spec_sha256.as_deref().unwrap_or(UNKNOWN),
            s.source
                .as_ref()
                .map_or(UNKNOWN.to_string(), |l| l.to_string())
        ),
        Ok(SpecShape::Described) => {
            format!("described: {}", s.described.as_deref().unwrap_or(UNKNOWN))
        }
        Err(e) => e,
    }
}

/// The family's variants, depth-first from the roots (children by order,
/// registration time, id).
fn variant_tree<'a>(reg: &'a Registry, family: &str) -> Vec<(usize, &'a Variant)> {
    let mine: Vec<&Variant> = reg
        .variants
        .values()
        .filter(|v| v.family == family)
        .collect();
    let ids: BTreeSet<&str> = mine.iter().map(|v| v.id.as_str()).collect();
    let sort = |vs: &mut Vec<&Variant>| {
        vs.sort_by(|a, b| {
            (
                a.order.unwrap_or(u32::MAX),
                a.registered_at.sort_key(),
                &a.id,
            )
                .cmp(&(
                    b.order.unwrap_or(u32::MAX),
                    b.registered_at.sort_key(),
                    &b.id,
                ))
        })
    };
    let mut roots: Vec<&Variant> = mine
        .iter()
        .copied()
        .filter(|v| v.parent == "ROOT" || !ids.contains(v.parent.as_str()))
        .collect();
    sort(&mut roots);
    let mut out = Vec::new();
    let mut seen = BTreeSet::new();
    fn walk<'a>(
        v: &'a Variant,
        depth: usize,
        mine: &[&'a Variant],
        sort: &dyn Fn(&mut Vec<&'a Variant>),
        seen: &mut BTreeSet<&'a str>,
        out: &mut Vec<(usize, &'a Variant)>,
    ) {
        if !seen.insert(v.id.as_str()) {
            return;
        }
        out.push((depth, v));
        let mut kids: Vec<&Variant> = mine.iter().copied().filter(|c| c.parent == v.id).collect();
        sort(&mut kids);
        for k in kids {
            walk(k, depth + 1, mine, sort, seen, out);
        }
    }
    for r in roots {
        walk(r, 0, &mine, &sort, &mut seen, &mut out);
    }
    // A parent loop leaves members unreached: list them flat.
    for v in mine {
        if !seen.contains(v.id.as_str()) {
            out.push((0, v));
        }
    }
    out
}

/// Module table: the family's variants and its search accounting.
pub fn family_report(
    reg: &Registry,
    id: &str,
    attempts: &Attempts,
) -> Result<FamilyReport, String> {
    let f = reg
        .families
        .get(id)
        .ok_or_else(|| format!("no family `{id}`"))?;
    let tree = variant_tree(reg, id);
    let hashes: BTreeMap<&str, Vec<&str>> = tree.iter().fold(BTreeMap::new(), |mut m, (_, v)| {
        if let Some(h) = &v.spec.spec_sha256 {
            m.entry(h.as_str())
                .or_insert_with(Vec::new)
                .push(v.id.as_str());
        }
        m
    });
    fn runs_of<'a>(attempts: &'a Attempts, h: &'a str) -> impl Iterator<Item = &'a RunAttempt> {
        attempts.runs.iter().filter(move |r| r.spec_sha256 == h)
    }
    let reads_of = |h: &str| {
        attempts
            .holdout_reads
            .iter()
            .filter(|r| r.spec_sha256 == h)
            .count()
    };
    let variants = tree
        .iter()
        .map(|(depth, v)| {
            let h = v.spec.spec_sha256.as_deref();
            VariantNode {
                depth: *depth,
                id: v.id.clone(),
                title: v.title.clone(),
                status: enum_name(&v.status),
                preregistered: v.preregistered,
                order: v.order,
                registered_at: v.registered_at.to_string(),
                changed: v
                    .changed
                    .iter()
                    .map(|c| format!("{}: {} → {}", c.dim, scalar(&c.from), scalar(&c.to)))
                    .collect(),
                spec: spec_text(v),
                experiments: reg
                    .experiments
                    .values()
                    .filter(|e| e.variant == v.id)
                    .map(|e| e.id.clone())
                    .collect(),
                runs: h.map_or(0, |h| runs_of(attempts, h).count()),
                holdout_reads: h.map_or(0, reads_of),
            }
        })
        .collect();
    // The family's strategy names: its library variants' and those of the
    // runs its hashes match.
    let mut names: BTreeSet<&str> = tree
        .iter()
        .filter_map(|(_, v)| v.spec.strategy.as_deref())
        .collect();
    for h in hashes.keys() {
        names.extend(runs_of(attempts, h).map(|r| r.strategy.as_str()));
    }
    let registered_hashes = variants_by_hash(reg);
    let mut unregistered: BTreeMap<(&str, &str), Unregistered> = BTreeMap::new();
    for r in &attempts.runs {
        if registered_hashes.contains_key(r.spec_sha256.as_str())
            || !names.contains(r.strategy.as_str())
        {
            continue;
        }
        let u = unregistered
            .entry((r.spec_sha256.as_str(), r.strategy.as_str()))
            .or_insert_with(|| Unregistered {
                spec_sha256: r.spec_sha256.clone(),
                strategy: r.strategy.clone(),
                runs: Vec::new(),
                holdout_reads: reads_of(&r.spec_sha256),
            });
        u.runs.push(r.run_id.clone());
    }
    let unregistered: Vec<Unregistered> = unregistered.into_values().collect();
    let runs = hashes.keys().map(|h| runs_of(attempts, h).count()).sum();
    let holdout_reads = hashes.keys().map(|h| reads_of(h)).sum();
    let distinct = hashes
        .keys()
        .filter(|h| runs_of(attempts, h).next().is_some())
        .count();
    let (prior_text, prior_n) = match &f.prior_search {
        None => ("none recorded".to_string(), Some(0)),
        Some(p) => (
            format!(
                "{}{} — source {}{}",
                p.count,
                p.precision
                    .map_or(String::new(), |x| format!(" ({})", enum_name(&x))),
                p.source,
                p.note.as_ref().map_or(String::new(), |n| format!(": {n}"))
            ),
            p.count.known().filter(|_| {
                p.precision
                    .map_or(true, |x| x == super::value::Precision::Exact)
            }),
        ),
    };
    let base = tree.len() as u64 + unregistered.len() as u64;
    let total_tried = match (prior_n, attempts.states.is_empty()) {
        (Some(n), false) => format!("{}", base + n),
        (Some(n), true) => format!("≥ {} (run dirs not scanned)", base + n),
        (None, _) => format!(
            "≥ {} (prior search {})",
            base + f
                .prior_search
                .as_ref()
                .and_then(|p| p.count.known())
                .unwrap_or(0),
            f.prior_search
                .as_ref()
                .map_or(UNKNOWN.to_string(), |p| p.count.to_string())
        ),
    };
    Ok(FamilyReport {
        family: f.id.clone(),
        title: f.title.clone(),
        hypothesis: f.hypothesis.clone(),
        role: enum_name(&f.role),
        status: enum_name(&f.status),
        status_reason: f.status_reason.clone(),
        variants,
        search: SearchAccounting {
            registered_variants: tree.len(),
            prior_search: prior_text,
            states: attempts.states.clone(),
            distinct_spec_hashes: distinct,
            runs,
            holdout_reads,
            unregistered,
            total_tried,
        },
    })
}

// ---------------------------------------------------------------------------
// Trace
// ---------------------------------------------------------------------------

/// One line of a trace.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct TraceLine {
    pub depth: usize,
    /// `family`, `variant`, `experiment`, `window`, `result`, `evidence`,
    /// `verdict`, `episode`, `incident`, `capability`, `generation`, `note`.
    pub kind: String,
    pub id: String,
    pub text: String,
    /// The record the trace started from.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub start: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Trace {
    pub start: String,
    pub lines: Vec<TraceLine>,
}

/// `id` or `<kind>/<id>` → the one record it names.
pub fn resolve_id(reg: &Registry, id: &str) -> Result<(RecordKind, String), String> {
    if let Some((k, rest)) = id.split_once('/') {
        if let Some(kind) = RecordKind::parse(k) {
            return if reg.exists(kind, rest) {
                Ok((kind, rest.to_string()))
            } else {
                Err(format!("no {kind} `{rest}`"))
            };
        }
    }
    match reg.kinds_of(id).as_slice() {
        [] => Err(format!("no record `{id}`")),
        [k] => Ok((*k, id.to_string())),
        many => Err(format!(
            "`{id}` names {} records — say which: {}",
            many.len(),
            many.iter()
                .map(|k| format!("{k}/{id}"))
                .collect::<Vec<_>>()
                .join(", ")
        )),
    }
}

pub(super) fn result_text(r: &ResultRow) -> String {
    let mut s = format!("{} [{}]", r.label, enum_name(&r.class));
    if let Some(a) = &r.arm {
        s.push_str(&format!(" arm {a}"));
    }
    s.push_str(&format!(": n {}", r.n));
    if let Some(m) = r.mean_net_bps {
        s.push_str(&format!(", mean {m} bps"));
    }
    if let Some([lo, hi]) = r.ci95_bps {
        s.push_str(&format!(", CI95 [{lo}, {hi}] bps"));
    }
    if let Some(u) = r.net_usd {
        s.push_str(&format!(", net {u} USD"));
    }
    if let Some(t) = r.t_stat {
        s.push_str(&format!(", t {t}"));
    }
    s.push_str(&format!(" — {}", r.evidence));
    if let Some(x) = &r.extract {
        s.push_str(&format!(" ({x})"));
    }
    s
}

struct TraceBuilder<'a> {
    reg: &'a Registry,
    start: (RecordKind, String),
    lines: Vec<TraceLine>,
    shown: BTreeSet<(String, String)>,
}

impl TraceBuilder<'_> {
    fn line(&mut self, depth: usize, kind: &str, id: &str, text: String) {
        let start = self.start.0.name() == kind && self.start.1 == id;
        self.shown.insert((kind.to_string(), id.to_string()));
        self.lines.push(TraceLine {
            depth,
            kind: kind.to_string(),
            id: id.to_string(),
            text,
            start,
        });
    }

    fn family(&mut self, id: &str) {
        let reg = self.reg;
        let Some(f) = reg.families.get(id) else {
            return;
        };
        self.line(
            0,
            "family",
            id,
            format!(
                "{} [{} · {}: {}] — hypothesis: {}",
                f.title,
                enum_name(&f.role),
                enum_name(&f.status),
                f.status_reason,
                f.hypothesis
            ),
        );
        for p in &f.preceded_by {
            let s = reg.families.get(p).map_or(UNKNOWN.to_string(), |p| {
                format!("{}: {}", enum_name(&p.status), p.status_reason)
            });
            self.line(1, "note", p, format!("preceded by family {p} — {s}"));
        }
        for c in &f.controls {
            let s = reg.families.get(c).map_or(UNKNOWN.to_string(), |c| {
                format!(
                    "{} {}: {}",
                    enum_name(&c.role),
                    enum_name(&c.status),
                    c.status_reason
                )
            });
            self.line(1, "note", c, format!("control family {c} — {s}"));
        }
        for (depth, v) in variant_tree(reg, id) {
            self.line(
                1 + depth,
                "variant",
                &v.id,
                format!(
                    "{} [{}{}] — {}",
                    v.title,
                    enum_name(&v.status),
                    if v.preregistered {
                        ", preregistered"
                    } else {
                        ""
                    },
                    spec_text(v)
                ),
            );
            let mut xs: Vec<&Experiment> = reg
                .experiments
                .values()
                .filter(|e| e.variant == v.id)
                .collect();
            xs.sort_by(|a, b| cmp_time(&a.ran_at, &b.ran_at).then(a.id.cmp(&b.id)));
            for x in xs {
                self.experiment(2 + depth, x);
            }
        }
        let mut loose: Vec<&Experiment> = reg
            .experiments
            .values()
            .filter(|e| e.family == id && !reg.variants.contains_key(&e.variant))
            .collect();
        loose.sort_by(|a, b| cmp_time(&a.ran_at, &b.ran_at).then(a.id.cmp(&b.id)));
        for x in loose {
            self.experiment(1, x);
        }
        let eps: Vec<String> = reg
            .episodes
            .values()
            .filter(|ep| {
                ep.family.as_deref() == Some(id)
                    && !ep
                        .experiment
                        .as_ref()
                        .is_some_and(|x| reg.experiments.contains_key(x))
            })
            .map(|ep| ep.id.clone())
            .collect();
        for ep in eps {
            self.episode(1, &ep);
        }
    }

    fn experiment(&mut self, depth: usize, x: &Experiment) {
        let reg = self.reg;
        self.line(
            depth,
            "experiment",
            &x.id,
            format!(
                "{} [{} · {} · generation {} · ran {}{}] cost: {}",
                x.title,
                enum_name(&x.kind),
                enum_name(&x.environment),
                x.generation,
                x.ran_at,
                if x.preregistered {
                    " · preregistered"
                } else {
                    ""
                },
                x.cost_model
            ),
        );
        for w in &x.windows {
            self.line(
                depth + 1,
                "window",
                &x.id,
                format!(
                    "{} {} → {}{} integrity {}",
                    enum_name(&w.role),
                    w.from,
                    w.to,
                    w.scope
                        .as_ref()
                        .map_or(String::new(), |s| format!(" ({s})")),
                    enum_name(&w.integrity)
                ),
            );
        }
        for r in &x.results {
            self.line(depth + 1, "result", &x.id, result_text(r));
        }
        for e in &x.evidence {
            self.line(
                depth + 1,
                "evidence",
                &x.id,
                format!(
                    "{} {} [{} · {}]{}",
                    e.role,
                    e.locator,
                    enum_name(&e.class),
                    enum_name(&e.provenance),
                    e.sha256
                        .as_ref()
                        .map_or(String::new(), |s| format!(" sha256 {s}"))
                ),
            );
        }
        let v = &x.verdict;
        self.line(
            depth + 1,
            "verdict",
            &x.id,
            format!(
                "{} — {} ({} by {}){}",
                enum_name(&v.value),
                v.reason,
                v.decided_at,
                v.decided_by,
                x.validity.map_or(String::new(), |val| format!(
                    " · validity {}",
                    enum_name(&val)
                ))
            ),
        );
        let mut incidents: BTreeSet<&str> = x.incidents.iter().map(String::as_str).collect();
        incidents.extend(
            reg.incidents
                .values()
                .filter(|i| i.experiment.as_deref() == Some(x.id.as_str()))
                .map(|i| i.id.as_str()),
        );
        let eps: Vec<String> = reg
            .episodes
            .values()
            .filter(|ep| ep.experiment.as_deref() == Some(x.id.as_str()))
            .map(|ep| ep.id.clone())
            .collect();
        for ep in eps {
            self.episode(depth + 1, &ep);
        }
        for i in incidents {
            self.incident(depth + 1, i);
        }
    }

    fn episode(&mut self, depth: usize, id: &str) {
        let Some(ep) = self.reg.episodes.get(id) else {
            return;
        };
        let outcome = ep.outcome.as_ref().map_or(String::new(), |o| {
            let mut s = String::new();
            if let Some(u) = o.net_usd {
                s.push_str(&format!(" net {u} USD"));
            }
            if let Some(b) = o.net_bps {
                s.push_str(&format!(" net {b} bps"));
            }
            s
        });
        self.line(
            depth,
            "episode",
            id,
            format!(
                "{} [{} · {}] decided `{}` at {} by {}; decision {}, execution {}, outcome {}{}",
                ep.title,
                enum_name(&ep.kind),
                ep.quadrant().name(),
                ep.decision.action,
                ep.decision.decided_at,
                enum_name(&ep.decision.policy),
                enum_name(&ep.quality.decision),
                enum_name(&ep.quality.execution),
                enum_name(&ep.quality.outcome),
                outcome
            ),
        );
        if let Some(l) = &ep.lesson {
            self.line(
                depth + 1,
                "note",
                id,
                format!("lesson [{}]: {}", enum_name(&l.status), l.text),
            );
        }
        for i in ep.incidents.clone() {
            self.incident(depth + 1, &i);
        }
    }

    fn incident(&mut self, depth: usize, id: &str) {
        let Some(i) = self.reg.incidents.get(id) else {
            return;
        };
        self.line(
            depth,
            "incident",
            id,
            format!(
                "{} [{} · {} → {} · strategy impact {}: {}]",
                i.title,
                enum_name(&i.class),
                i.started_at,
                i.ended_at,
                enum_name(&i.strategy_impact),
                i.strategy_impact_note
            ),
        );
    }
}

/// Module table: the lineage around one record.
pub fn trace(reg: &Registry, id: &str) -> Result<Trace, String> {
    let (kind, id) = resolve_id(reg, id)?;
    let mut families: BTreeSet<String> = BTreeSet::new();
    let fam_of_x = |x: &str| reg.experiments.get(x).map(|e| e.family.clone());
    match kind {
        RecordKind::Family => {
            families.insert(id.clone());
        }
        RecordKind::Variant => families.extend(reg.variants.get(&id).map(|v| v.family.clone())),
        RecordKind::Experiment => families.extend(fam_of_x(&id)),
        RecordKind::Episode => {
            let ep = &reg.episodes[&id];
            families.extend(ep.family.clone());
            families.extend(ep.experiment.as_deref().and_then(fam_of_x));
            families.extend(
                ep.variant
                    .as_ref()
                    .and_then(|v| reg.variants.get(v))
                    .map(|v| v.family.clone()),
            );
        }
        RecordKind::Incident => {
            let inc = &reg.incidents[&id];
            families.extend(inc.experiment.as_deref().and_then(fam_of_x));
            for x in reg
                .experiments
                .values()
                .filter(|x| x.incidents.contains(&id))
            {
                families.insert(x.family.clone());
            }
            for ep in reg
                .episodes
                .values()
                .filter(|ep| ep.incidents.contains(&id))
            {
                families.extend(ep.family.clone());
                families.extend(ep.experiment.as_deref().and_then(fam_of_x));
            }
        }
        RecordKind::Capability => {
            for x in reg
                .experiments
                .values()
                .filter(|x| x.capabilities.contains(&id))
            {
                families.insert(x.family.clone());
            }
        }
        RecordKind::Generation => {
            for x in reg.experiments.values().filter(|x| x.generation == id) {
                families.insert(x.family.clone());
            }
        }
        RecordKind::Evidence => {
            let ev = &reg.evidence[&id];
            families.extend(ev.experiment.as_deref().and_then(fam_of_x));
            let vault = format!("vault:{}/", ev.vault);
            for u in reg.locator_uses() {
                if u.locator.to_string().starts_with(&vault) {
                    if let Some((k, rid)) = u.record.split_once('/') {
                        if k == "experiment" {
                            families.extend(fam_of_x(rid));
                        }
                    }
                }
            }
        }
    }
    families.retain(|f| reg.families.contains_key(f));
    let mut b = TraceBuilder {
        reg,
        start: (kind, id.clone()),
        lines: Vec::new(),
        shown: BTreeSet::new(),
    };
    for f in &families {
        b.family(f);
    }
    // Records outside any family, and context the start record adds.
    match kind {
        RecordKind::Episode if !b.shown.contains(&("episode".into(), id.clone())) => {
            b.episode(0, &id)
        }
        RecordKind::Incident if !b.shown.contains(&("incident".into(), id.clone())) => {
            b.incident(0, &id)
        }
        RecordKind::Capability => {
            let c = &reg.capabilities[&id];
            let gens: Vec<&str> = reg
                .generations
                .values()
                .filter(|g| g.capabilities.iter().any(|r| r.id == id))
                .map(|g| g.id.as_str())
                .collect();
            b.line(
                0,
                "capability",
                &id,
                format!(
                    "{} [{} v{} · {} · {}] contract {} · bindings {} · generations {}",
                    c.title,
                    enum_name(&c.class),
                    c.version,
                    enum_name(&c.permission),
                    enum_name(&c.lifecycle),
                    c.contract,
                    c.bindings
                        .iter()
                        .map(|b| b.to_string())
                        .collect::<Vec<_>>()
                        .join(", "),
                    if gens.is_empty() {
                        "none".to_string()
                    } else {
                        gens.join(", ")
                    }
                ),
            );
        }
        RecordKind::Generation => {
            let g = &reg.generations[&id];
            b.line(
                0,
                "generation",
                &id,
                format!(
                    "{} [{} · parent {} · frozen {}] sandboxes {} · capabilities {}",
                    g.title,
                    enum_name(&g.status),
                    g.parent,
                    g.frozen_at.map_or(UNKNOWN.to_string(), |t| t.to_string()),
                    g.sandboxes.join(", "),
                    g.capabilities
                        .iter()
                        .map(|c| format!("{} v{}", c.id, c.version))
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
            );
        }
        RecordKind::Evidence => {
            let ev = &reg.evidence[&id];
            b.line(
                0,
                "evidence",
                &id,
                format!(
                    "{} — vault {} captured {} manifest_sha256 {} · {} item(s)",
                    ev.title,
                    ev.vault,
                    ev.captured_at.as_deref().unwrap_or("not yet (a plan)"),
                    ev.manifest_sha256.as_deref().unwrap_or(UNKNOWN),
                    ev.items.len()
                ),
            );
        }
        _ => {}
    }
    Ok(Trace {
        start: label(kind, &id),
        lines: b.lines,
    })
}
