//! Rule-W acceptance (handoff § 57, `docs/lineage-2026-10-06.md` § 5): the 21
//! questions answered from record fields only — each answer names the fields
//! it read; one with nothing to say is `UNKNOWN` with the reason.
//!
//! | # | Read from |
//! |---|---|
//! | 1 | family `hypothesis`, `mechanism`, `origin*`, `parent` |
//! | 2 | `preceded_by` + `controls` families (status, reason), the family's REJECTED variants |
//! | 3 | the family's BACKTEST / HOLDOUT_TEST experiments: ran_at, verdict, result evidence |
//! | 4 · 5 | the family's variants · registered count, `prior_search`, run-dir attempts (`query::family_report`) |
//! | 6 · 7 · 8 | DEVELOPMENT · HOLDOUT windows (integrity, `split_by`) · HOLDOUT-class results |
//! | 9 · 10 | `cost_model` + `cost_pin` · `capabilities` (class, version, permission, lifecycle) — family + forward experiments |
//! | 11 · 12 | results of JEV_GATE experiments · their `gate` / jev results + the forward generation's `[decision_policy]` arms |
//! | 13 | `uncertainty` + every result CI |
//! | 14 · 15 | the forward experiment's `preregistered`, `[[sealed]]` row, `prereg` evidence · its evidence + results' evidence + the generation's `[code]` |
//! | 16 · 17 · 18 | the generation's RISK_POLICY pins · its incidents · BACKFILLED evidence refs, evidence-record items, incident `data_impact`, episode information |
//! | 19 · 20 · 21 | FORWARD_PAPER results + verdict + validity + limitations · its episodes (quadrant) · its generation + every generation listing its capabilities |

use std::collections::{BTreeMap, BTreeSet};

use serde::Serialize;

use super::experiment::{Experiment, ExperimentKind, Policy, WindowRole};
use super::generation::PinRole;
use super::query::{enum_name, family_report, result_text, Attempts};
use super::registry::Registry;
use super::value::{cmp_time, RecordKind, UNKNOWN};
use super::variant::VariantStatus;
use crate::domain::evidence::{EvidenceClass, Provenance};

/// The 21 questions of handoff § 57, in order.
pub const QUESTIONS: [&str; 21] = [
    "What hypothesis produced Rule W?",
    "Which alternatives were rejected?",
    "Which historical runs tested it?",
    "Which variants exist?",
    "How many related variants were tried?",
    "Which data was development?",
    "Which data was holdout?",
    "What did holdout show?",
    "What costs were modelled?",
    "Which capabilities produced the evidence?",
    "What did JEV decide?",
    "How did JEV compare with deterministic Rule W?",
    "What was the uncertainty?",
    "Which forward test was preregistered?",
    "Which run executed it?",
    "Which risk policy governed it?",
    "Which incidents occurred?",
    "Which data was backfilled?",
    "What was the forward outcome?",
    "Which Experience Episode represents it?",
    "Which generation owned the relevant components?",
];

/// One answer: lines read from record fields, and those fields.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Answer {
    pub n: usize,
    pub question: String,
    /// `UNKNOWN` (with `unknown_reason`) when no field answers it.
    pub lines: Vec<String>,
    pub sources: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unknown_reason: Option<String>,
}

impl Answer {
    pub fn is_unknown(&self) -> bool {
        self.unknown_reason.is_some()
    }
}

#[derive(Default)]
struct Draft {
    lines: Vec<String>,
    sources: BTreeSet<String>,
}

impl Draft {
    fn add(&mut self, line: String, kind: RecordKind, id: &str, field: &str) {
        self.lines.push(line);
        self.source(kind, id, field);
    }

    fn source(&mut self, kind: RecordKind, id: &str, field: &str) {
        self.sources
            .insert(format!("{}/{}.{}", kind.dir(), id, field));
    }

    fn finish(self, n: usize, unknown: &str) -> Answer {
        let known = !self.lines.is_empty();
        Answer {
            n,
            question: QUESTIONS[n - 1].to_string(),
            lines: if known {
                self.lines
            } else {
                vec![UNKNOWN.to_string()]
            },
            sources: self.sources.into_iter().collect(),
            unknown_reason: (!known).then(|| unknown.to_string()),
        }
    }
}

/// Module table: the 21 answers for `family` and its `forward` experiment.
pub fn acceptance(
    reg: &Registry,
    family: &str,
    forward: &str,
    attempts: &Attempts,
) -> Result<Vec<Answer>, String> {
    let f = reg
        .families
        .get(family)
        .ok_or_else(|| format!("no family `{family}`"))?;
    let fw = reg
        .experiments
        .get(forward)
        .ok_or_else(|| format!("no experiment `{forward}`"))?;
    let (fam, x) = (RecordKind::Family, RecordKind::Experiment);
    let mut family_x: Vec<&Experiment> = reg
        .experiments
        .values()
        .filter(|e| e.family == family)
        .collect();
    family_x.sort_by(|a, b| cmp_time(&a.ran_at, &b.ran_at).then(a.id.cmp(&b.id)));
    let mut with_fw = family_x.clone();
    if !with_fw.iter().any(|e| e.id == fw.id) {
        with_fw.push(fw);
    }
    let gen = reg.generations.get(&fw.generation);
    let mut out = Vec::new();

    // 1
    let mut d = Draft::default();
    d.add(
        format!("hypothesis: {}", f.hypothesis),
        fam,
        family,
        "hypothesis",
    );
    if let Some(m) = &f.mechanism {
        d.add(format!("mechanism: {m}"), fam, family, "mechanism");
    }
    d.add(
        format!("origin: {} at {} by {}", f.origin, f.origin_at, f.origin_by),
        fam,
        family,
        "origin",
    );
    d.source(fam, family, "origin_at");
    d.source(fam, family, "origin_by");
    d.add(
        format!("parent family: {}", f.parent),
        fam,
        family,
        "parent",
    );
    out.push(d.finish(1, "the family states no hypothesis"));

    // 2
    let mut d = Draft::default();
    for p in &f.preceded_by {
        let text = reg.families.get(p).map_or(UNKNOWN.to_string(), |p| {
            format!("{}: {}", enum_name(&p.status), p.status_reason)
        });
        d.add(
            format!("preceded by {p} — {text}"),
            fam,
            family,
            "preceded_by",
        );
        d.source(fam, p, "status");
    }
    for c in &f.controls {
        let text = reg.families.get(c).map_or(UNKNOWN.to_string(), |c| {
            format!(
                "{} {}: {}",
                enum_name(&c.role),
                enum_name(&c.status),
                c.status_reason
            )
        });
        d.add(format!("control {c} — {text}"), fam, family, "controls");
        d.source(fam, c, "status");
    }
    for v in reg
        .variants
        .values()
        .filter(|v| v.family == family && v.status == VariantStatus::Rejected)
    {
        d.add(
            format!("variant {} REJECTED — {}", v.id, v.reason),
            RecordKind::Variant,
            &v.id,
            "status",
        );
    }
    out.push(d.finish(2, "no preceding, control or rejected record"));

    // 3
    let mut d = Draft::default();
    for e in family_x.iter().filter(|e| {
        matches!(
            e.kind,
            ExperimentKind::Backtest | ExperimentKind::HoldoutTest
        )
    }) {
        let runs: Vec<String> = e.results.iter().map(|r| r.evidence.to_string()).collect();
        let mut runs = runs;
        runs.dedup();
        d.add(
            format!(
                "{} ({}, variant {}, ran {}): {} — {}; evidence {}",
                e.id,
                enum_name(&e.kind),
                e.variant,
                e.ran_at,
                enum_name(&e.verdict.value),
                e.verdict.reason,
                if runs.is_empty() {
                    UNKNOWN.to_string()
                } else {
                    runs.join(", ")
                }
            ),
            x,
            &e.id,
            "results",
        );
        d.source(x, &e.id, "verdict");
    }
    out.push(d.finish(3, "no BACKTEST / HOLDOUT_TEST experiment of the family"));

    // 4
    let report = family_report(reg, family, attempts)?;
    let mut d = Draft::default();
    for v in &report.variants {
        d.add(
            format!(
                "{}{} [{}{}] {}{}",
                "  ".repeat(v.depth),
                v.id,
                v.status,
                if v.preregistered {
                    ", preregistered"
                } else {
                    ""
                },
                if v.changed.is_empty() {
                    String::new()
                } else {
                    format!("changed {}; ", v.changed.join(", "))
                },
                v.spec
            ),
            RecordKind::Variant,
            &v.id,
            "spec",
        );
    }
    out.push(d.finish(4, "no variant of the family is registered"));

    // 5
    let s = &report.search;
    let mut d = Draft::default();
    d.add(
        format!("registered variants: {}", s.registered_variants),
        fam,
        family,
        "id",
    );
    d.add(
        format!("prior search: {}", s.prior_search),
        fam,
        family,
        "prior_search",
    );
    if s.states.is_empty() {
        d.lines.push("run dirs: not scanned".into());
    } else {
        d.lines.push(format!(
            "run dirs ({}): {} distinct spec hash(es) of its variants, {} run(s), {} holdout read(s); {} unregistered hash(es){}",
            s.states.join(", "),
            s.distinct_spec_hashes,
            s.runs,
            s.holdout_reads,
            s.unregistered.len(),
            if s.unregistered.is_empty() {
                String::new()
            } else {
                format!(
                    ": {}",
                    s.unregistered
                        .iter()
                        .map(|u| format!("{} ({}, {} run(s))", u.spec_sha256, u.strategy, u.runs.len()))
                        .collect::<Vec<_>>()
                        .join(", ")
                )
            }
        ));
    }
    d.lines.push(format!("total tried: {}", s.total_tried));
    out.push(d.finish(5, "nothing counted"));

    // 6 · 7
    for (n, role) in [(6, WindowRole::Development), (7, WindowRole::Holdout)] {
        let mut d = Draft::default();
        for e in &family_x {
            for w in e.windows_of(role) {
                d.add(
                    format!(
                        "{}: {} → {}{} integrity {}{}",
                        e.id,
                        w.from,
                        w.to,
                        w.scope
                            .as_ref()
                            .map_or(String::new(), |s| format!(" scope {s}")),
                        enum_name(&w.integrity),
                        if role == WindowRole::Holdout {
                            format!(
                                ", split_by {}",
                                e.split_by.map_or(UNKNOWN.to_string(), |s| enum_name(&s))
                            )
                        } else {
                            String::new()
                        }
                    ),
                    x,
                    &e.id,
                    "windows",
                );
            }
        }
        out.push(d.finish(
            n,
            &format!("no {} window in the family's experiments", enum_name(&role)),
        ));
    }

    // 8
    let mut d = Draft::default();
    for e in &family_x {
        for r in e
            .results
            .iter()
            .filter(|r| r.class == EvidenceClass::Holdout)
        {
            d.add(
                format!(
                    "{}: {} → verdict {}",
                    e.id,
                    result_text(r),
                    enum_name(&e.verdict.value)
                ),
                x,
                &e.id,
                "results",
            );
        }
    }
    out.push(d.finish(8, "no HOLDOUT-class result in the family's experiments"));

    // 9
    let mut d = Draft::default();
    for e in &with_fw {
        d.add(
            format!(
                "{}: {}{}",
                e.id,
                e.cost_model,
                e.cost_pin
                    .as_ref()
                    .map_or(String::new(), |p| format!(" (pin {p})"))
            ),
            x,
            &e.id,
            "cost_model",
        );
    }
    out.push(d.finish(9, "no experiment"));

    // 10
    let mut d = Draft::default();
    let mut caps: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    for e in &with_fw {
        for c in &e.capabilities {
            caps.entry(c.as_str()).or_default().push(e.id.as_str());
        }
    }
    for (c, xs) in &caps {
        let text = reg.capabilities.get(*c).map_or(UNKNOWN.to_string(), |c| {
            format!(
                "{} v{} {} {} {}",
                enum_name(&c.class),
                c.version,
                enum_name(&c.permission),
                enum_name(&c.lifecycle),
                c.title
            )
        });
        d.add(
            format!("{c}: {text} — used by {}", xs.join(", ")),
            RecordKind::Capability,
            c,
            "class",
        );
        for e in xs {
            d.source(x, e, "capabilities");
        }
    }
    out.push(d.finish(10, "no experiment lists a capability"));

    // 11
    let jev: Vec<&&Experiment> = with_fw
        .iter()
        .filter(|e| e.policies.contains(&Policy::JevGate))
        .collect();
    let mut d = Draft::default();
    for e in &jev {
        for r in &e.results {
            d.add(format!("{}: {}", e.id, result_text(r)), x, &e.id, "results");
        }
        d.source(x, &e.id, "policies");
    }
    out.push(d.finish(11, "no experiment ran the JEV_GATE policy"));

    // 12
    let mut d = Draft::default();
    for e in &jev {
        for r in e.results.iter().filter(|r| {
            r.extract.as_deref() == Some("gate")
                || r.arm.as_deref().is_some_and(|a| a.contains("jev"))
        }) {
            d.add(format!("{}: {}", e.id, result_text(r)), x, &e.id, "results");
        }
    }
    if let Some(g) = gen {
        if let Some(p) = &g.decision_policy {
            d.add(
                format!(
                    "generation {} decision policy: {} — {}",
                    g.id, p.primary, p.summary
                ),
                RecordKind::Generation,
                &g.id,
                "decision_policy",
            );
            for a in &p.arms {
                d.lines.push(format!(
                    "  arm {}: {}{}",
                    a.name,
                    enum_name(&a.status),
                    a.summary
                        .as_ref()
                        .map_or(String::new(), |s| format!(" — {s}"))
                ));
            }
        }
    }
    out.push(d.finish(12, "no JEV comparison result and no decision-policy arms"));

    // 13
    let mut d = Draft::default();
    for e in &with_fw {
        d.add(
            format!("{}: {}", e.id, e.uncertainty),
            x,
            &e.id,
            "uncertainty",
        );
        for r in e.results.iter().filter(|r| r.ci95_bps.is_some()) {
            let [lo, hi] = r.ci95_bps.unwrap_or_default();
            d.add(
                format!("  {} {}: CI95 [{lo}, {hi}] bps, n {}", e.id, r.label, r.n),
                x,
                &e.id,
                "results",
            );
        }
    }
    out.push(d.finish(13, "no experiment"));

    // 14
    let mut d = Draft::default();
    let preregs: Vec<&&Experiment> = with_fw
        .iter()
        .filter(|e| e.kind == ExperimentKind::ForwardPaper && (e.preregistered || e.id == fw.id))
        .collect();
    for e in preregs {
        let seal = reg
            .locks
            .sealed
            .iter()
            .filter(|s| s.record == format!("experiment:{}", e.id))
            .map(|s| format!("sealed {} at {}", s.sha256, s.sealed_at))
            .collect::<Vec<_>>();
        let refs: Vec<String> = e
            .evidence
            .iter()
            .filter(|r| r.role.starts_with("prereg"))
            .map(|r| {
                format!(
                    "{} {}{}",
                    r.role,
                    r.locator,
                    r.sha256
                        .as_ref()
                        .map_or(String::new(), |s| format!(" sha256 {s}"))
                )
            })
            .collect();
        let variant_prereg = reg
            .variants
            .get(&e.variant)
            .is_some_and(|v| v.preregistered);
        if !e.preregistered && seal.is_empty() && refs.is_empty() && !variant_prereg {
            continue;
        }
        let mut parts = vec![format!(
            "{}: preregistered {}{}",
            e.id,
            e.preregistered,
            if variant_prereg {
                format!(" (variant {} preregistered)", e.variant)
            } else {
                String::new()
            }
        )];
        parts.extend(seal);
        parts.extend(refs);
        d.add(parts.join("; "), x, &e.id, "preregistered");
        d.source(x, &e.id, "evidence");
    }
    out.push(d.finish(14, "the forward experiment carries no preregistration"));

    // 15
    let mut d = Draft::default();
    for r in &fw.evidence {
        d.add(
            format!(
                "{} {} [{} · {}]{}",
                r.role,
                r.locator,
                enum_name(&r.class),
                enum_name(&r.provenance),
                r.sha256
                    .as_ref()
                    .map_or(String::new(), |s| format!(" sha256 {s}"))
            ),
            x,
            &fw.id,
            "evidence",
        );
    }
    for r in &fw.results {
        d.add(
            format!("result {} — {}", r.label, r.evidence),
            x,
            &fw.id,
            "results",
        );
    }
    if let Some(c) = gen.and_then(|g| g.code.as_ref()) {
        d.add(
            format!(
                "code: commit {}{}{}{}",
                c.commit,
                c.forward_commit
                    .as_ref()
                    .map_or(String::new(), |s| format!(", forward commit {s}")),
                c.forward_binary
                    .as_ref()
                    .map_or(String::new(), |s| format!(", binary {s}")),
                c.forward_binary_sha256
                    .as_ref()
                    .map_or(String::new(), |s| format!(" sha256 {s}"))
            ),
            RecordKind::Generation,
            &fw.generation,
            "code",
        );
    }
    out.push(d.finish(
        15,
        "the forward experiment names no evidence and its generation no code",
    ));

    // 16
    let mut d = Draft::default();
    if let Some(g) = gen {
        for p in g.pins.iter().filter(|p| p.role == PinRole::RiskPolicy) {
            d.add(
                format!("{}: {} sha256 {}", g.id, p.target, p.sha256),
                RecordKind::Generation,
                &g.id,
                "pins",
            );
        }
    }
    out.push(d.finish(
        16,
        "the forward experiment's generation pins no RISK_POLICY",
    ));

    // 17
    let mut d = Draft::default();
    let mut incs: BTreeSet<&str> = fw.incidents.iter().map(String::as_str).collect();
    incs.extend(
        reg.incidents
            .values()
            .filter(|i| i.experiment.as_deref() == Some(fw.id.as_str()))
            .map(|i| i.id.as_str()),
    );
    for i in &incs {
        if let Some(inc) = reg.incidents.get(*i) {
            d.add(
                format!(
                    "{}: {} [{} · {} → {}] strategy impact {} — {}",
                    inc.id,
                    inc.title,
                    enum_name(&inc.class),
                    inc.started_at,
                    inc.ended_at,
                    enum_name(&inc.strategy_impact),
                    inc.strategy_impact_note
                ),
                RecordKind::Incident,
                &inc.id,
                "strategy_impact",
            );
        }
    }
    out.push(d.finish(17, "no incident is recorded for the forward experiment"));

    // 18
    let mut d = Draft::default();
    for r in fw
        .evidence
        .iter()
        .filter(|r| r.provenance == Provenance::Backfilled)
    {
        d.add(
            format!("{} {} BACKFILLED", r.role, r.locator),
            x,
            &fw.id,
            "evidence",
        );
    }
    for ev in reg
        .evidence
        .values()
        .filter(|ev| ev.experiment.as_deref() == Some(fw.id.as_str()))
    {
        for it in &ev.items {
            let line = format!(
                "evidence {} item {}: {}",
                ev.id,
                it.path,
                enum_name(&it.provenance)
            );
            d.add(line, RecordKind::Evidence, &ev.id, "items");
        }
    }
    for i in &incs {
        if let Some(inc) = reg.incidents.get(*i) {
            for di in &inc.data_impact {
                d.add(
                    format!(
                        "{}: {} {} → {} {}{}",
                        inc.id,
                        di.stream,
                        di.from,
                        di.to,
                        enum_name(&di.after),
                        di.backfill
                            .as_ref()
                            .map_or(String::new(), |b| format!(" from {b}"))
                    ),
                    RecordKind::Incident,
                    &inc.id,
                    "data_impact",
                );
            }
        }
    }
    for ep in reg
        .episodes
        .values()
        .filter(|ep| ep.experiment.as_deref() == Some(fw.id.as_str()))
    {
        for info in ep
            .information
            .iter()
            .filter(|i| i.provenance == Provenance::Backfilled)
        {
            d.add(
                format!(
                    "{}: information `{}` BACKFILLED ({})",
                    ep.id, info.item, info.evidence
                ),
                RecordKind::Episode,
                &ep.id,
                "information",
            );
        }
    }
    out.push(d.finish(
        18,
        "no provenance of the forward experiment's data is recorded",
    ));

    // 19
    let mut d = Draft::default();
    for r in fw
        .results
        .iter()
        .filter(|r| r.class == EvidenceClass::ForwardPaper)
    {
        d.add(result_text(r), x, &fw.id, "results");
    }
    let v = &fw.verdict;
    d.add(
        format!(
            "verdict {} — {} ({} by {})",
            enum_name(&v.value),
            v.reason,
            v.decided_at,
            v.decided_by
        ),
        x,
        &fw.id,
        "verdict",
    );
    d.add(
        format!(
            "validity {}",
            fw.validity.map_or(UNKNOWN.to_string(), |v| enum_name(&v))
        ),
        x,
        &fw.id,
        "validity",
    );
    for l in &fw.limitations {
        d.add(format!("limitation: {l}"), x, &fw.id, "limitations");
    }
    out.push(d.finish(19, "no forward result"));

    // 20
    let mut d = Draft::default();
    for ep in reg
        .episodes
        .values()
        .filter(|ep| ep.experiment.as_deref() == Some(fw.id.as_str()))
    {
        d.add(
            format!(
                "{} [{} · {}]: {}",
                ep.id,
                enum_name(&ep.kind),
                ep.quadrant().name(),
                ep.title
            ),
            RecordKind::Episode,
            &ep.id,
            "quality",
        );
    }
    out.push(d.finish(20, "no episode names the forward experiment"));

    // 21
    let mut d = Draft::default();
    match gen {
        Some(g) => d.add(
            format!(
                "{} owns the experiment: {} [{} · frozen {} · parent {}]",
                g.id,
                g.title,
                enum_name(&g.status),
                g.frozen_at.map_or(UNKNOWN.to_string(), |t| t.to_string()),
                g.parent
            ),
            x,
            &fw.id,
            "generation",
        ),
        None => d
            .lines
            .push(format!("generation {}: not in the registry", fw.generation)),
    }
    for c in &fw.capabilities {
        let owners: Vec<String> = reg
            .generations
            .values()
            .filter_map(|g| {
                g.capabilities
                    .iter()
                    .find(|r| &r.id == c)
                    .map(|r| format!("{} (v{})", g.id, r.version))
            })
            .collect();
        d.add(
            format!(
                "capability {c}: {}",
                if owners.is_empty() {
                    "in no generation".to_string()
                } else {
                    owners.join(", ")
                }
            ),
            RecordKind::Capability,
            c,
            "id",
        );
    }
    out.push(d.finish(21, "the forward experiment names no generation"));
    Ok(out)
}
