//! `soe_view` — one run of the weekly cycle as the stages read it, read-only,
//! as the typed row `soe_view/1:<run>:<view>` (ttl 0, never cached, never
//! stored).
//!
//! | View | Items (paged by `offset` / `limit`) | Rule |
//! |---|---|---|
//! | `head` | — | run, status (`OPEN` · `FROZEN`), phase, mode, decision time, generation pin, stage agents, limits, the packet's sha256 and size, counts of candidates (new · carried), challenges and earlier episodes |
//! | `packet` | the packet's current records (facts · pending · expired · withdrawn · unparsed, in that order, each list sorted) | the page is the packet cut to those records: superseded pairs, events, conflicts, issues and citations follow them; demand and freshness stay whole; `EvidencePacket::render_text` — typed tokens outside, every source text inside one fence. The only facts a proposal may cite (`observe::EvidenceIndex`) |
//! | `candidates` | `submit::candidates` (new + carried not re-proposed), by id | ids, mechanism, revenue kind, where it came from, novelty; signals; the gate verdict computed now (`propose::assess_in_run`, before any challenge); every input `field = low..base..high` with its basis (`FACT` ids · `INFERENCE` · none) and why each unknown is unknown; customer, pain and inference text fenced |
//! | `proposals` | this run's `proposals.jsonl` | id, opportunity, provenance (agent, engine, model, time, call id), counts, unsupported claims |
//! | `challenges` | this run's `challenges.jsonl`, then the carried ones (`carried.json`) | id, target, kind, effect, evidence ids, provenance; the claim fenced |
//! | `history` | the state log `episodes.jsonl`: version-1 lines (written at their cycle's freeze — a later version has no recorded write time, so it never shows) decided strictly before this run's decision, newest first | episode id, opportunity, decision time, verdict, base forecast, actual, evidence strength, quadrant, decision note — no look-ahead: a replay of an old week sees only what was decided before it (critic UNCOVERED 5) |
//!
//! | Rule | Value |
//! |---|---|
//! | Refuse | strict arguments (an unknown key, `limit` outside 1–20, a negative `offset`); a run that does not exist (`run_not_found`) |
//! | Fit | at most `limit` items (default 10, `candidates` 5), fewer while the text passes [`TEXT_BUDGET`](super::TEXT_BUDGET) — one at least; the rest named with the next call |
//! | Text | line 1 = the row's headline; ids and hashes in full; one fence note before the first fence |

use std::collections::BTreeSet;

use anyhow::{bail, Result};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::propose::{assess_in_run, assessment_lines, name};
use super::{args, run_arg, SoeShared, TEXT_BUDGET};
use crate::adapters::outbound::tools::xlab::{field, opt_str, whole};
use crate::application::soe::submit::{self, Carried, RunHead};
use crate::domain::lineage::value::Time;
use crate::domain::message::ToolDef;
use crate::domain::observation::{set_int, set_str, Features, ObsSource, Observation, Observed};
use crate::domain::soe::challenge::{Challenge, Effect};
use crate::domain::soe::episode::OpportunityEpisode;
use crate::domain::soe::proposal::{Basis, MechanismProposal};
use crate::domain::soe::record::from_json;
use crate::domain::soe::value::Est;
use crate::domain::source::{fence_untrusted, EvidencePacket, FENCE_NOTE};
use crate::domain::tools as names;
use crate::ports::soe::{CycleStore, RunDir, RunStatus, StateLog};
use crate::ports::tool::{Tool, ToolCtx, ToolOutput};

/// The row schema (observation schema too).
pub(crate) const VIEW_SCHEMA: &str = "soe_view/1";
/// Refusal: the run dir does not exist.
pub(crate) const RUN_NOT_FOUND: &str = "run_not_found";
const DEFAULT_LIMIT: usize = 10;
const DEFAULT_CANDIDATES: usize = 5;
const MAX_LIMIT: usize = 20;
const ARGS: &[&str] = &["run", "view", "offset", "limit"];

/// What to read (module table).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum View {
    Head,
    Packet,
    Candidates,
    Proposals,
    Challenges,
    History,
}

impl View {
    const ALL: [View; 6] = [
        View::Head,
        View::Packet,
        View::Candidates,
        View::Proposals,
        View::Challenges,
        View::History,
    ];

    pub(crate) fn as_str(self) -> &'static str {
        match self {
            View::Head => "head",
            View::Packet => "packet",
            View::Candidates => "candidates",
            View::Proposals => "proposals",
            View::Challenges => "challenges",
            View::History => "history",
        }
    }

    fn parse(s: &str) -> Option<View> {
        View::ALL.into_iter().find(|v| v.as_str() == s)
    }
}

/// `soe_view/1` (module doc): what one call showed. `data` = this row —
/// ids and counts only, never model or source text.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct ViewRow {
    /// `cycles/<id>` · `replays/<id>`.
    pub run: String,
    pub view: String,
    /// `OPEN` · `FROZEN`.
    pub status: String,
    /// `PROPOSE` · `CHALLENGE` · `CLOSED` · `NONE`.
    pub phase: String,
    pub decided_at_ms: i64,
    pub offset: usize,
    pub shown: usize,
    pub total: usize,
    /// The ids of the items shown, in full.
    pub ids: Vec<String>,
}

impl Observed for ViewRow {
    const SCHEMA: &'static str = VIEW_SCHEMA;

    fn subject(&self) -> String {
        format!("{}:{}", self.run, self.view)
    }

    fn headline(&self) -> String {
        format!(
            "soe_view {} {} {} {} decided_at={} shown={} of {} from {}",
            self.run,
            self.view,
            self.status,
            self.phase,
            Time::At(self.decided_at_ms),
            self.shown,
            self.total,
            self.offset
        )
    }

    fn features(&self) -> Features {
        let n = |x: usize| Some(i64::try_from(x).unwrap_or(i64::MAX));
        let mut f = Features::new();
        set_str(&mut f, "view", Some(&self.view));
        set_str(&mut f, "status", Some(&self.status));
        set_str(&mut f, "phase", Some(&self.phase));
        set_int(&mut f, "decided_at_ms", Some(self.decided_at_ms));
        set_int(&mut f, "offset", n(self.offset));
        set_int(&mut f, "shown", n(self.shown));
        set_int(&mut f, "total", n(self.total));
        f
    }
}

pub(crate) struct SoeViewTool {
    def: ToolDef,
    shared: SoeShared,
}

impl SoeViewTool {
    pub(crate) fn new(shared: SoeShared) -> Self {
        Self {
            def: super::defs::def(names::SOE_VIEW),
            shared,
        }
    }
}

/// The parsed call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ViewArgs {
    pub run: RunDir,
    pub view: View,
    pub offset: usize,
    pub limit: usize,
}

impl ViewArgs {
    pub(crate) fn parse(args_v: &Value) -> Result<Self> {
        let tool = names::SOE_VIEW;
        let o = args(tool, args_v, ARGS)?;
        let run = run_arg(tool, o)?;
        let view = match opt_str(tool, o, "view")? {
            None => View::Head,
            Some(s) => View::parse(s).ok_or_else(|| {
                anyhow::anyhow!(
                    "{tool}: 'view' must be one of {}, got {s}",
                    View::ALL.map(View::as_str).join(", ")
                )
            })?,
        };
        let offset = match field(o, "offset") {
            None => 0,
            Some(v) => whole(v)
                .and_then(|n| usize::try_from(n).ok())
                .ok_or_else(|| {
                    anyhow::anyhow!("{tool}: 'offset' must be an integer ≥ 0, got {v}")
                })?,
        };
        let limit = match field(o, "limit") {
            None if view == View::Candidates => DEFAULT_CANDIDATES,
            None => DEFAULT_LIMIT,
            Some(v) => whole(v)
                .filter(|n| (1..=MAX_LIMIT as i64).contains(n))
                .map(|n| n as usize)
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "{tool}: 'limit' must be an integer from 1 to {MAX_LIMIT}, got {v}"
                    )
                })?,
        };
        Ok(Self {
            run,
            view,
            offset,
            limit,
        })
    }
}

// ── Rendering helpers ──────────────────────────────────────────────

fn est_text<T: std::fmt::Display>(e: &Est<T>) -> String {
    match e {
        Est::Range { low, base, high } => format!("{low}..{base}..{high}"),
        Est::Unknown { reason: None } => "UNKNOWN".into(),
        Est::Unknown { reason: Some(r) } => format!("UNKNOWN: {r}"),
    }
}

fn effect_text(e: &Effect) -> String {
    match e {
        Effect::Widen {
            field,
            low,
            base,
            high,
            unknown_reason,
        } => {
            let mut parts = vec![format!("WIDEN {field}")];
            for (k, v) in [("low", low), ("base", base), ("high", high)] {
                if let Some(v) = v {
                    parts.push(format!("{k}={v}"));
                }
            }
            if unknown_reason.is_some() {
                parts.push("to UNKNOWN (reason fenced below)".into());
            }
            parts.join(" ")
        }
        Effect::BlockGate { gate } => format!("BLOCK_GATE {}", gate.as_str()),
        Effect::None => "NONE".into(),
    }
}

fn ids_or_none(ids: &[String]) -> String {
    if ids.is_empty() {
        "none".into()
    } else {
        ids.join(", ")
    }
}

/// The items of one list view: `(id, text)`.
type Items = Vec<(String, String)>;

fn candidate_item(
    store: &dyn CycleStore,
    dir: &RunDir,
    head: &RunHead,
    p: &MechanismProposal,
    carried_from: Option<&str>,
) -> (String, String) {
    let o = p.opportunity();
    let revenue = serde_json::to_value(&o.economics.revenue)
        .ok()
        .and_then(|v| v.get("kind").and_then(Value::as_str).map(String::from))
        .unwrap_or_default();
    let from = match carried_from {
        Some(c) => format!("carried from {c}"),
        None => "new this run".into(),
    };
    let mut lines = vec![format!(
        "## {} · {} v{} · {} · revenue {revenue} · {from} · by {} · novelty {}",
        p.id,
        o.id,
        o.version,
        name(&o.mechanism),
        p.provenance.agent,
        name(&p.draft.novelty)
    )];
    lines.push(format!(
        "alternatives: {} · requires_skills: {} · as_of {}",
        ids_or_none(&o.alternatives.iter().map(name).collect::<Vec<_>>()),
        ids_or_none(&o.requires_skills),
        o.as_of
    ));
    lines.push(format!("signals: {}", ids_or_none(&o.signals)));
    match assess_in_run(store, dir, head, o) {
        Ok(a) => lines.extend(
            assessment_lines(&a)
                .into_iter()
                .map(|l| format!("computed now (before challenges): {l}")),
        ),
        Err(e) => lines.push(format!("computed now: unavailable ({e:#})")),
    }
    lines.push(format!(
        "inputs ({}; base = the middle value):",
        o.economics.currency
    ));
    let mut whys = Vec::new();
    for (f, i) in o.economics.inputs() {
        let basis = match p.draft.basis(&f) {
            Some(Basis::Fact { evidence }) => format!("FACT {}", evidence.join(", ")),
            Some(Basis::Inference { why }) => {
                whys.push(fence_untrusted(&p.id, &format!("{f}.why"), why));
                "INFERENCE (why fenced below)".into()
            }
            None if i.is_known() => "no basis (an unsupported claim)".into(),
            None => "—".into(),
        };
        lines.push(format!("  {f} = {} · {basis}", i.value_text()));
    }
    lines.push(format!(
        "experiment: {} · forecast items: {}",
        o.experiment
            .as_ref()
            .map_or("none".to_string(), |x| format!(
                "{} stage(s)",
                x.stages.len()
            )),
        p.draft.forecast.len()
    ));
    lines.push(fence_untrusted(&p.id, "customer", &o.customer));
    lines.push(fence_untrusted(&p.id, "pain", &o.pain));
    lines.extend(whys);
    (p.id.clone(), lines.join("\n"))
}

fn candidates(store: &dyn CycleStore, dir: &RunDir, head: &RunHead) -> Result<Items> {
    let carried = submit::carried(store, dir)?;
    let carried_ids: BTreeSet<&str> = carried.proposals.iter().map(|p| p.id.as_str()).collect();
    Ok(submit::candidates(store, dir)?
        .iter()
        .map(|p| {
            let from = carried_ids
                .contains(p.id.as_str())
                .then_some(p.cycle_id.as_str());
            candidate_item(store, dir, head, p, from)
        })
        .collect())
}

fn proposals(store: &dyn CycleStore, dir: &RunDir) -> Result<Items> {
    Ok(store
        .proposals(dir)?
        .iter()
        .map(|p| {
            let o = p.opportunity();
            let v = &p.provenance;
            let unsupported: Vec<String> =
                p.draft.unsupported().into_iter().map(|u| u.field).collect();
            let text = format!(
                "## {} · {} v{} · {} · by {} ({} {}) at {} · call {} · generation {}\n\
                 bases {} · forecast items {} · signals {}\n\
                 unsupported claims: {}",
                p.id,
                o.id,
                o.version,
                name(&o.mechanism),
                v.agent,
                v.engine,
                v.model,
                v.proposed_at,
                v.call_id,
                v.generation,
                p.draft.bases.len(),
                p.draft.forecast.len(),
                o.signals.len(),
                ids_or_none(&unsupported)
            );
            (p.id.clone(), text)
        })
        .collect())
}

fn challenge_item(c: &Challenge, carried_from: Option<&str>) -> (String, String) {
    let d = &c.draft;
    let from = match carried_from {
        Some(x) => format!("carried from {x}"),
        None => "this run".into(),
    };
    let mut lines = vec![
        format!(
            "## {} · target {} · {} · {} · by {} · {from}",
            c.id,
            d.target,
            name(&d.kind),
            effect_text(&d.effect),
            c.provenance.agent
        ),
        format!("evidence: {}", ids_or_none(&d.evidence)),
        fence_untrusted(&c.id, "claim", &d.claim),
    ];
    if let Effect::Widen {
        unknown_reason: Some(r),
        ..
    } = &d.effect
    {
        lines.push(fence_untrusted(&c.id, "unknown_reason", r));
    }
    (c.id.clone(), lines.join("\n"))
}

fn challenges(store: &dyn CycleStore, dir: &RunDir) -> Result<Items> {
    let Carried { challenges, .. } = submit::carried(store, dir)?;
    let mut out: Items = store
        .challenges(dir)?
        .iter()
        .map(|c| challenge_item(c, None))
        .collect();
    out.extend(
        challenges
            .iter()
            .map(|c| challenge_item(c, Some(c.cycle_id.as_str()))),
    );
    Ok(out)
}

/// Module table: history — the episodes decided before `decided_at_ms`,
/// newest first, and the lines that did not read.
pub(crate) fn prior_episodes(
    store: &dyn CycleStore,
    decided_at_ms: i64,
) -> Result<(Vec<OpportunityEpisode>, Vec<String>)> {
    let mut out = Vec::new();
    let mut bad = Vec::new();
    for (i, line) in store.lines(StateLog::Episodes)?.iter().enumerate() {
        match from_json::<OpportunityEpisode>(line) {
            Ok(e) => {
                let before = e.decided_at.latest().is_some_and(|t| t < decided_at_ms);
                if e.version == 1 && before {
                    out.push(e);
                }
            }
            Err(errs) => bad.push(format!(
                "{} line {}: {}",
                StateLog::Episodes.file_name(),
                i + 1,
                errs.iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
                    .join("; ")
            )),
        }
    }
    out.sort_by(|a, b| {
        b.decided_at
            .sort_key()
            .cmp(&a.decided_at.sort_key())
            .then(a.id.cmp(&b.id))
    });
    Ok((out, bad))
}

fn history(store: &dyn CycleStore, head: &RunHead) -> Result<(Items, Vec<String>)> {
    let (episodes, bad) = prior_episodes(store, head.decided_at_ms)?;
    let items = episodes
        .iter()
        .map(|e| {
            let f = &e.forecast_base;
            let text = format!(
                "## {} · {} v{} · decided {} · verdict {} · evidence {} · quadrant {}\n\
                 forecast base ({}/month): cash {} · time-adjusted {} · owner hours {}\n\
                 actual: cash {} · owner hours {} · spend {} · actions {}\n\
                 decision: {}",
                e.id,
                e.opportunity,
                e.opportunity_version,
                e.decided_at,
                name(&e.verdict),
                name(&e.evidence_strength),
                name(&e.quadrant()),
                e.currency,
                est_text(&f.monthly_cash),
                est_text(&f.time_adjusted),
                est_text(&f.owner_hours),
                est_text(&e.actual.monthly_cash),
                est_text(&e.actual.owner_hours),
                e.spend,
                e.actions.len(),
                e.quality.decision_note.replace(['\n', '\r'], " ")
            );
            (e.id.clone(), text)
        })
        .collect();
    Ok((items, bad))
}

/// The packet cut to its current records `[offset, offset + n)` (module
/// table: packet).
pub(crate) fn packet_page(
    p: &EvidencePacket,
    offset: usize,
    n: usize,
) -> (Vec<String>, EvidencePacket) {
    let mut order: Vec<&str> = Vec::with_capacity(p.rows());
    order.extend(
        p.facts
            .iter()
            .chain(&p.pending)
            .chain(&p.expired)
            .map(|f| f.record_id.as_str()),
    );
    order.extend(p.withdrawn.iter().map(|w| w.record_id.as_str()));
    order.extend(p.unparsed.iter().map(|u| u.record_id.as_str()));
    let ids: Vec<String> = order
        .iter()
        .skip(offset)
        .take(n)
        .map(|s| s.to_string())
        .collect();
    let kept: BTreeSet<&str> = ids.iter().map(String::as_str).collect();
    let mut page = p.clone();
    page.facts.retain(|f| kept.contains(f.record_id.as_str()));
    page.pending.retain(|f| kept.contains(f.record_id.as_str()));
    page.expired.retain(|f| kept.contains(f.record_id.as_str()));
    page.withdrawn
        .retain(|w| kept.contains(w.record_id.as_str()));
    page.unparsed
        .retain(|u| kept.contains(u.record_id.as_str()));
    let mut events: BTreeSet<String> = BTreeSet::new();
    for f in page.facts.iter().chain(&page.pending).chain(&page.expired) {
        events.insert(f.event_key.clone());
    }
    events.extend(page.withdrawn.iter().map(|w| w.event_key.clone()));
    events.extend(page.unparsed.iter().map(|u| u.event_key.clone()));
    page.superseded
        .retain(|s| kept.contains(s.old.as_str()) || kept.contains(s.new.as_str()));
    let mut cited: BTreeSet<String> = kept.iter().map(|s| s.to_string()).collect();
    for s in &page.superseded {
        cited.insert(s.old.clone());
        cited.insert(s.new.clone());
    }
    cited.extend(page.withdrawn.iter().filter_map(|w| w.last_fact.clone()));
    cited.extend(page.unparsed.iter().filter_map(|u| u.last_fact.clone()));
    page.citations.retain(|c| cited.contains(&c.record_id));
    page.events.retain(|e| events.contains(&e.event_key));
    page.conflicts.retain(|c| events.contains(&c.event_key));
    page.issues.retain(|i| kept.contains(i.record_id.as_str()));
    (ids, page)
}

/// What one view pages over.
enum Source {
    /// A fixed text, one item.
    Head(String),
    Items {
        preamble: Vec<String>,
        items: Items,
    },
    Packet(Box<EvidencePacket>),
}

impl Source {
    fn total(&self) -> usize {
        match self {
            Source::Head(_) => 1,
            Source::Items { items, .. } => items.len(),
            Source::Packet(p) => p.rows(),
        }
    }

    /// The ids and the text of items `[offset, offset + n)`.
    fn render(&self, offset: usize, n: usize) -> (Vec<String>, String) {
        match self {
            Source::Head(t) => (Vec::new(), t.clone()),
            Source::Items { preamble, items } => {
                let page: Vec<&(String, String)> = items.iter().skip(offset).take(n).collect();
                let mut lines = preamble.clone();
                if items.is_empty() {
                    lines.push("(none)".to_string());
                }
                if page.iter().any(|(_, t)| t.contains("<source-text")) {
                    lines.push(FENCE_NOTE.to_string());
                }
                lines.extend(page.iter().map(|(_, t)| t.clone()));
                (
                    page.iter().map(|(id, _)| id.clone()).collect(),
                    lines.join("\n"),
                )
            }
            Source::Packet(p) => {
                let (ids, page) = packet_page(p, offset, n);
                (ids, page.render_text())
            }
        }
    }
}

fn head_text(
    store: &dyn CycleStore,
    dir: &RunDir,
    h: &RunHead,
    status: &str,
    phase: &str,
) -> Result<String> {
    let packet = submit::packet(store, dir)?;
    let carried = submit::carried(store, dir)?;
    let all = submit::candidates(store, dir)?;
    let mine = store.proposals(dir)?.len();
    let n_challenges = store.challenges(dir)?.len();
    let (episodes, _) = prior_episodes(store, h.decided_at_ms)?;
    let lines = [
        format!(
            "run {} · cycle {} · status {status} · phase {phase} · mode {}",
            h.run,
            h.cycle_id,
            h.mode.as_str()
        ),
        format!(
            "decided_at {} · generation {} {}",
            h.decided_at(),
            h.generation.id,
            h.generation.sha256
        ),
        format!(
            "stage agents: architect {} · critic {}",
            h.architect, h.critic
        ),
        format!(
            "limits: max_proposals {} ({mine} proposed) · forecast_max_weeks {}",
            h.max_proposals, h.forecast_max_weeks
        ),
        format!(
            "packet {}: {} records — view packet (the only facts a proposal may cite)",
            h.packet_sha256,
            packet.rows()
        ),
        format!(
            "candidates {}: new {mine} · carried {}{} — view candidates",
            all.len(),
            all.len() - mine.min(all.len()),
            carried
                .from
                .as_deref()
                .map(|f| format!(" (from {f})"))
                .unwrap_or_default()
        ),
        format!(
            "challenges: {n_challenges} this run · {} carried — view challenges",
            carried.challenges.len()
        ),
        format!(
            "history: {} earlier episode(s) decided before this run — view history",
            episodes.len()
        ),
    ];
    Ok(lines.join("\n"))
}

/// The row and text of one call, fitted to the budget (module table: fit).
fn fit(
    source: &Source,
    a: &ViewArgs,
    status: &str,
    phase: &str,
    decided_at_ms: i64,
    now_ms: i64,
) -> (Observation, String) {
    let total = source.total();
    let left = total.saturating_sub(a.offset);
    let mut n = a.limit.min(left);
    loop {
        let (ids, body) = source.render(a.offset, n);
        let row = ViewRow {
            run: a.run.to_string(),
            view: a.view.as_str().to_string(),
            status: status.to_string(),
            phase: phase.to_string(),
            decided_at_ms,
            offset: a.offset,
            shown: if matches!(source, Source::Head(_)) {
                1
            } else {
                n
            },
            total,
            ids,
        };
        let obs = Observation::of(names::SOE_VIEW, &row, now_ms, 0, ObsSource::Live);
        let mut head = obs.clone();
        head.data = Value::Null;
        let mut text = format!("{}\n{body}", head.render_text(now_ms));
        if a.offset + n < total {
            text.push_str(&format!(
                "\nmore: {} of {total} left — soe_view {{\"run\": \"{}\", \"view\": \"{}\", \"offset\": {}}}",
                total - a.offset - n,
                a.run,
                a.view.as_str(),
                a.offset + n
            ));
        } else if left == 0 && total > 0 && !matches!(source, Source::Head(_)) {
            text.push_str(&format!(
                "\nnothing at offset {} — this view holds {total}",
                a.offset
            ));
        }
        if text.len() <= TEXT_BUDGET || n <= 1 {
            return (obs, text);
        }
        n = (n * TEXT_BUDGET / text.len()).clamp(1, n - 1);
    }
}

#[async_trait]
impl Tool for SoeViewTool {
    fn definition(&self) -> &ToolDef {
        &self.def
    }

    async fn execute(&self, args_v: &Value, ctx: &ToolCtx<'_>) -> Result<ToolOutput> {
        ctx.scope.check_fs_read(ctx.workspace)?;
        let a = ViewArgs::parse(args_v)?;
        let store = self.shared.store()?;
        let dir = &a.run;
        let status = match store.status(dir)? {
            RunStatus::Absent => bail!(
                "{RUN_NOT_FOUND}: {dir} is not in the SOE state root {} — give the run your goal \
                 names",
                store.root_display()
            ),
            RunStatus::Open => "OPEN",
            RunStatus::Frozen => "FROZEN",
        };
        let h = submit::head(store, dir)?;
        let phase = submit::phase(store, dir)?.map_or("NONE".to_string(), |p| name(&p));
        let source = match a.view {
            View::Head => Source::Head(head_text(store, dir, &h, status, &phase)?),
            View::Packet => Source::Packet(Box::new(submit::packet(store, dir)?)),
            View::Candidates => Source::Items {
                preamble: vec![format!(
                    "the week's candidates (new + carried, by id); verdicts computed now under the \
                     run's profile at {} — challenges not applied",
                    h.decided_at()
                )],
                items: candidates(store, dir, &h)?,
            },
            View::Proposals => Source::Items {
                preamble: Vec::new(),
                items: proposals(store, dir)?,
            },
            View::Challenges => Source::Items {
                preamble: Vec::new(),
                items: challenges(store, dir)?,
            },
            View::History => {
                let (items, bad) = history(store, &h)?;
                let mut preamble = vec![format!(
                    "episodes of earlier cycles decided before {} (newest first; outcomes after \
                     this run are never shown)",
                    h.decided_at()
                )];
                preamble.extend(bad.into_iter().map(|b| format!("unreadable: {b}")));
                Source::Items { preamble, items }
            }
        };
        let now = self.shared.clock.now_ms();
        let (obs, text) = fit(&source, &a, status, &phase, h.decided_at_ms, now);
        Ok(ToolOutput {
            text,
            observation: Some(obs),
        })
    }
}
