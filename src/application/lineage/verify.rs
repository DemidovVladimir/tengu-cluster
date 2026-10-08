//! `tengu lineage verify` (`docs/lineage-2026-10-06.md` § 2): is the registry
//! consistent, is W1 unchanged, is the evidence intact. IO through
//! `ports/lineage.rs`; any `Error` finding fails the command.
//!
//! | Pass | Codes | Checks |
//! |---|---|---|
//! | always | the `Registry::validate` table (`domain/lineage/mod.rs`) | fields, references, time, holdout, forward, locks |
//! | `--pins` | `pin_drift` · `pin_unresolved` | every generation `[[pins]]` recomputes to its sha256; a library variant's `spec_sha256` = its `spec:<sandbox>/<strategy>` now; every `contract`, `cost_pin` and model `where` resolves |
//! | `--pins` | `unknown_binding` | a `tool:` binding names a catalog tool, a `strategy_kind:` one a kind (`ContractProbe`) |
//! | `--pins` | `sandbox_unbound` | every sandbox a FROZEN / ACTIVE generation lists has `[generation] id = <it>` in `<repo>/sandboxes/<s>/config.toml`, its `registry` resolving to the registry verified |
//! | `--evidence` | `evidence_missing` · `evidence_mismatch` · `mutable_evidence` (Warn) · `evidence_planned` (Info) | every locator a record names resolves; its sha256 (the ref's, else the vault's record) equals the file's now; each captured evidence record's items and `MANIFEST.json` too (a record not captured yet: Info); a `state:` locator is live, mutable |
//! | `--evidence` | `result_mismatch` · `extract_unsupported` (Warn) | a result with `extract` recomputes from its evidence through the first `ResultSource` that knows the kind: n exact, bps ± [`BPS_TOLERANCE`], USD ± [`USD_TOLERANCE`], t ± [`T_TOLERANCE`]; no source knows it → Warn |

use serde::Serialize;

use crate::domain::lineage::experiment::ResultRow;
use crate::domain::lineage::generation::{GenerationStatus, PinRole};
use crate::domain::lineage::query::enum_name;
use crate::domain::lineage::registry::label;
use crate::domain::lineage::value::{Binding, Locator, PinTarget, RecordKind};
use crate::domain::lineage::variant::SpecShape;
use crate::domain::lineage::{Finding, Registry, Severity};
use crate::ports::lineage::{ContractProbe, EvidenceResolver, Extracted, Resolution, ResultSource};

/// A recomputed mean / CI end may differ from the record by this much.
pub(crate) const BPS_TOLERANCE: f64 = 0.05;
/// A recomputed net USD may differ by this much.
pub(crate) const USD_TOLERANCE: f64 = 0.005;
/// A recomputed t statistic may differ by this much.
pub(crate) const T_TOLERANCE: f64 = 0.005;

/// What `verify` checks beyond the registry.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct VerifyOpts {
    pub pins: bool,
    pub evidence: bool,
}

/// The module table's passes, findings sorted (errors first).
pub(crate) fn verify(
    reg: &Registry,
    opts: VerifyOpts,
    probe: &dyn ContractProbe,
    resolver: &dyn EvidenceResolver,
    sources: &[&dyn ResultSource],
) -> Vec<Finding> {
    let mut out = reg.validate();
    if opts.pins {
        out.extend(pin_findings(reg, probe));
    }
    if opts.evidence {
        out.extend(evidence_findings(reg, resolver, sources));
    }
    out.sort();
    out.dedup();
    out
}

/// One pin of a generation, recomputed.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub(crate) struct PinRow {
    pub target: String,
    pub role: PinRole,
    pub pinned: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub now: Option<String>,
    /// `OK` · `DRIFT` · `UNRESOLVED`.
    pub status: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// Every pin of generation `id`, recomputed (`tengu lineage generation`).
pub(crate) fn pin_status(
    reg: &Registry,
    id: &str,
    probe: &dyn ContractProbe,
) -> Result<Vec<PinRow>, String> {
    let g = reg
        .generations
        .get(id)
        .ok_or_else(|| format!("no generation `{id}`"))?;
    Ok(g.pins
        .iter()
        .map(|p| {
            let (now, status, reason) = match probe.pin_sha256(&p.target) {
                Ok(h) if h == p.sha256 => (Some(h), "OK", None),
                Ok(h) => (Some(h), "DRIFT", None),
                Err(e) => (None, "UNRESOLVED", Some(e)),
            };
            PinRow {
                target: p.target.to_string(),
                role: p.role,
                pinned: p.sha256.clone(),
                now,
                status,
                reason,
            }
        })
        .collect())
}

fn pin_findings(reg: &Registry, probe: &dyn ContractProbe) -> Vec<Finding> {
    let mut out = Vec::new();
    let mut resolves = |at: &str, field: &str, target: &PinTarget| {
        if let Err(e) = probe.pin_sha256(target) {
            out.push(Finding::error(
                "pin_unresolved",
                at,
                format!("{field} `{target}` does not resolve: {e}"),
            ));
        }
    };
    for g in reg.generations.values() {
        let at = label(RecordKind::Generation, &g.id);
        for m in &g.models {
            resolves(&at, &format!("models `{}` where", m.role), &m.location);
        }
    }
    for c in reg.capabilities.values() {
        resolves(
            &label(RecordKind::Capability, &c.id),
            "contract",
            &c.contract,
        );
    }
    for x in reg.experiments.values() {
        if let Some(p) = &x.cost_pin {
            resolves(&label(RecordKind::Experiment, &x.id), "cost_pin", p);
        }
    }
    for g in reg.generations.values() {
        let at = label(RecordKind::Generation, &g.id);
        for row in pin_status(reg, &g.id, probe).unwrap_or_default() {
            match row.status {
                "DRIFT" => out.push(Finding::error(
                    "pin_drift",
                    &at,
                    format!(
                        "pin `{}` ({}): pinned {}, now {}",
                        row.target,
                        enum_name(&row.role),
                        row.pinned,
                        row.now.as_deref().unwrap_or("")
                    ),
                )),
                "UNRESOLVED" => out.push(Finding::error(
                    "pin_unresolved",
                    &at,
                    format!(
                        "pin `{}` does not resolve: {}",
                        row.target,
                        row.reason.as_deref().unwrap_or("")
                    ),
                )),
                _ => {}
            }
        }
    }
    for v in reg.variants.values() {
        if v.spec.shape() != Ok(SpecShape::Library) {
            continue;
        }
        let (Some(sandbox), Some(strategy), Some(sha)) =
            (&v.spec.sandbox, &v.spec.strategy, &v.spec.spec_sha256)
        else {
            continue;
        };
        let target = PinTarget::Spec {
            sandbox: sandbox.clone(),
            strategy: strategy.clone(),
        };
        let at = label(RecordKind::Variant, &v.id);
        match probe.pin_sha256(&target) {
            Ok(now) if &now == sha => {}
            Ok(now) => out.push(Finding::error(
                "pin_drift",
                &at,
                format!("spec `{target}`: the variant records {sha}, the sandbox now hashes {now}"),
            )),
            Err(e) => out.push(Finding::error(
                "pin_unresolved",
                &at,
                format!("spec `{target}` does not resolve: {e}"),
            )),
        }
    }
    for g in reg.generations.values() {
        if !matches!(
            g.status,
            GenerationStatus::Frozen | GenerationStatus::Active
        ) {
            continue;
        }
        let at = label(RecordKind::Generation, &g.id);
        for s in &g.sandboxes {
            let why = match probe.sandbox_binding(s) {
                Err(e) => Some(e),
                Ok(None) => Some("its config.toml has no [generation]".to_string()),
                Ok(Some(b)) if b.generation != g.id => {
                    Some(format!("its [generation] id is `{}`", b.generation))
                }
                Ok(Some(b)) if !b.same_registry => Some(format!(
                    "its [generation] registry `{}` is not this registry",
                    b.registry
                )),
                Ok(Some(_)) => None,
            };
            if let Some(why) = why {
                out.push(Finding::error(
                    "sandbox_unbound",
                    &at,
                    format!(
                        "sandbox `{s}` is listed by {} generation `{}`, but {why} — it loads \
                         without the generation's checks",
                        enum_name(&g.status),
                        g.id
                    ),
                ));
            }
        }
    }
    for c in reg.capabilities.values() {
        let at = label(RecordKind::Capability, &c.id);
        for b in &c.bindings {
            let exists = match b {
                Binding::Tool(t) => probe.tool_exists(t),
                Binding::StrategyKind(k) => probe.strategy_kind_exists(k),
            };
            if !exists {
                out.push(Finding::error(
                    "unknown_binding",
                    &at,
                    format!(
                        "binding `{b}` names no {}",
                        match b {
                            Binding::Tool(_) => "catalog tool",
                            Binding::StrategyKind(_) => "strategy kind",
                        }
                    ),
                ));
            }
        }
    }
    out
}

fn evidence_findings(
    reg: &Registry,
    resolver: &dyn EvidenceResolver,
    sources: &[&dyn ResultSource],
) -> Vec<Finding> {
    let mut out = Vec::new();
    for u in reg.locator_uses() {
        check(&mut out, &u.record, &u.field, u.locator, u.sha256, resolver);
    }
    for ev in reg.evidence.values() {
        let at = label(RecordKind::Evidence, &ev.id);
        if !ev.is_captured() {
            out.push(Finding::new(
                Severity::Info,
                "evidence_planned",
                &at,
                format!(
                    "a plan: vault `{}` not captured yet (tengu evidence snapshot)",
                    ev.vault
                ),
            ));
            continue;
        }
        let manifest = Locator::Vault {
            snapshot: ev.vault.clone(),
            path: "MANIFEST.json".into(),
        };
        check(
            &mut out,
            &at,
            "manifest_sha256",
            &manifest,
            ev.manifest_sha256.as_deref(),
            resolver,
        );
        for (i, it) in ev.items.iter().enumerate() {
            let l = Locator::Vault {
                snapshot: ev.vault.clone(),
                path: it.path.clone(),
            };
            check(
                &mut out,
                &at,
                &format!("items[{i}]"),
                &l,
                it.sha256.as_deref(),
                resolver,
            );
        }
    }
    for x in reg.experiments.values() {
        let at = label(RecordKind::Experiment, &x.id);
        for (i, r) in x.results.iter().enumerate() {
            let Some(extract) = &r.extract else {
                continue;
            };
            let Resolution::Present { path, .. } = resolver.resolve(&r.evidence) else {
                continue; // reported above
            };
            let what = format!("results[{i}] `{}` ({extract} of {})", r.label, r.evidence);
            match sources.iter().find_map(|s| s.extract(&path, extract)) {
                None => out.push(Finding::warn(
                    "extract_unsupported",
                    &at,
                    format!("{what}: no result source reads `{extract}` — not recomputed"),
                )),
                Some(Err(e)) => out.push(Finding::error(
                    "result_mismatch",
                    &at,
                    format!("{what}: {e}"),
                )),
                Some(Ok(got)) => {
                    for diff in result_diffs(r, &got) {
                        out.push(Finding::error(
                            "result_mismatch",
                            &at,
                            format!("{what}: {diff}"),
                        ));
                    }
                }
            }
        }
    }
    out
}

/// One locator against the resolver (module table).
fn check(
    out: &mut Vec<Finding>,
    at: &str,
    field: &str,
    locator: &Locator,
    recorded: Option<&str>,
    resolver: &dyn EvidenceResolver,
) {
    match resolver.resolve(locator) {
        Resolution::NotAFile => {}
        Resolution::Missing(why) => out.push(Finding::error(
            "evidence_missing",
            at,
            format!("{field} {locator}: {why}"),
        )),
        Resolution::Present {
            sha256,
            recorded: vault,
            mutable,
            ..
        } => {
            if mutable {
                out.push(Finding::warn(
                    "mutable_evidence",
                    at,
                    format!("{field} {locator} is live state — snapshot it into a vault"),
                ));
            }
            let expected = recorded.map(String::from).or(vault);
            if let (Some(want), Some(now)) = (expected, sha256) {
                if want != now {
                    out.push(Finding::error(
                        "evidence_mismatch",
                        at,
                        format!(
                            "{field} {locator}: recorded sha256 {want}, the file now hashes {now}"
                        ),
                    ));
                }
            }
        }
    }
}

/// The fields of `r` the source disagrees with (module table tolerances).
fn result_diffs(r: &ResultRow, got: &Extracted) -> Vec<String> {
    let mut out = Vec::new();
    if let Some(want) = r.n.known() {
        match got.n {
            Some(n) if n == want => {}
            Some(n) => out.push(format!("n {want} recorded, {n} recomputed")),
            None => out.push(format!("n {want} recorded, the source has none")),
        }
    }
    let mut close = |field: &str, want: Option<f64>, have: Option<f64>, tol: f64| {
        if let Some(w) = want {
            match have {
                Some(h) if (h - w).abs() <= tol => {}
                Some(h) => out.push(format!("{field} {w} recorded, {h} recomputed (± {tol})")),
                None => out.push(format!("{field} {w} recorded, the source has none")),
            }
        }
    };
    close(
        "mean_net_bps",
        r.mean_net_bps,
        got.mean_net_bps,
        BPS_TOLERANCE,
    );
    close(
        "ci95_bps low",
        r.ci95_bps.map(|c| c[0]),
        got.ci95_bps.map(|c| c[0]),
        BPS_TOLERANCE,
    );
    close(
        "ci95_bps high",
        r.ci95_bps.map(|c| c[1]),
        got.ci95_bps.map(|c| c[1]),
        BPS_TOLERANCE,
    );
    close("net_usd", r.net_usd, got.net_usd, USD_TOLERANCE);
    close("t_stat", r.t_stat, got.t_stat, T_TOLERANCE);
    out
}
