//! `soe_propose` — one Architect draft into the open run
//! (`application::soe::submit::submit_proposal`; `domain/soe/proposal.rs`),
//! and a read-only preview of what the engine computes of it now.
//!
//! | Step | Rule |
//! |---|---|
//! | Arguments | strict: `run`, `proposal` — a JSON object, or a string holding one (canonical JSON is what `submit_proposal` parses) |
//! | Agent | the stage agent (`mod.rs`); with `[soe]` only `soe.architect` |
//! | Write | `submit_proposal`: open run, phase `PROPOSE`, no computed key, the record's rules, every cited id in the run's packet, forecasts within the horizon, the stamp's generation, under `max_proposals`, one per opportunity — every refusal listed, nothing written; else `<cycle>.pNN` appended to `proposals.jsonl` |
//! | Preview ([`assess_in_run`]) | `rank::assess` of the opportunity under the run's `profile.toml` at its decision time, over the packet's views (`observe::cited_views`): gate verdict, every failed gate, the eight rank keys — before any challenge, never written; the run's own `decide_week` decides. Unknown inputs and inputs with no basis listed. A preview that cannot be computed says why; the write stands |

use anyhow::{anyhow, bail, Result};
use async_trait::async_trait;
use serde_json::Value;

use super::{args, refused, run_arg, SoeShared};
use crate::adapters::outbound::tools::xlab::field;
use crate::application::soe::cycle::PROFILE;
use crate::application::soe::submit::{self, submit_proposal, RunHead};
use crate::domain::canonical::canonical_json;
use crate::domain::message::ToolDef;
use crate::domain::soe::observe::cited_views;
use crate::domain::soe::opportunity::Opportunity;
use crate::domain::soe::profile::OperatorProfile;
use crate::domain::soe::proposal::MechanismProposal;
use crate::domain::soe::rank::{assess, Assessment};
use crate::domain::soe::record::from_toml;
use crate::domain::tools as names;
use crate::ports::soe::{CycleStore, RunDir};
use crate::ports::tool::{Tool, ToolCtx, ToolOutput};

const ARGS: &[&str] = &["run", "proposal"];

pub(crate) struct SoeProposeTool {
    def: ToolDef,
    shared: SoeShared,
}

impl SoeProposeTool {
    pub(crate) fn new(shared: SoeShared) -> Self {
        Self {
            def: super::defs::def(names::SOE_PROPOSE),
            shared,
        }
    }
}

/// The `proposal` argument as JSON text (module table: arguments).
pub(crate) fn draft_text(o: &serde_json::Map<String, Value>) -> Result<String> {
    let tool = names::SOE_PROPOSE;
    match field(o, "proposal") {
        None => bail!("{tool}: 'proposal' is required — the draft object (skill soe-architect)"),
        Some(v @ Value::Object(_)) => Ok(canonical_json(v)),
        Some(Value::String(s)) => match serde_json::from_str::<Value>(s) {
            Ok(v @ Value::Object(_)) => Ok(canonical_json(&v)),
            Ok(other) => bail!("{tool}: 'proposal' must hold a JSON object, got {other}"),
            Err(e) => bail!("{tool}: 'proposal' is a string that is no JSON object: {e}"),
        },
        Some(v) => bail!("{tool}: 'proposal' must be a JSON object, got {v}"),
    }
}

/// Module table: Preview — `opp` assessed in `dir` (its head, packet and
/// profile), or why not.
pub(crate) fn assess_in_run(
    store: &dyn CycleStore,
    dir: &RunDir,
    head: &RunHead,
    opp: &Opportunity,
) -> Result<Assessment> {
    let packet = submit::packet(store, dir)?;
    let text = store
        .read(dir, PROFILE)?
        .ok_or_else(|| anyhow!("{dir}/{PROFILE}: missing"))?;
    let text = String::from_utf8(text).map_err(|_| anyhow!("{dir}/{PROFILE}: not UTF-8"))?;
    let profile: OperatorProfile = from_toml(&text).map_err(|e| {
        anyhow!(
            "{dir}/{PROFILE}: {}",
            e.iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join("; ")
        )
    })?;
    assess(opp, &cited_views(&packet), &profile, head.decided_at()).map_err(|e| anyhow!("{e}"))
}

/// A closed value as serialized (`HOLD`, `AUTOMATE`).
pub(crate) fn name<T: serde::Serialize>(v: &T) -> String {
    match serde_json::to_value(v) {
        Ok(Value::String(s)) => s,
        Ok(other) => other.to_string(),
        Err(_) => String::new(),
    }
}

/// The verdict lines of `a` (gates, then the rank keys).
pub(crate) fn assessment_lines(a: &Assessment) -> Vec<String> {
    let mut out = vec![format!(
        "verdict {} (gates as of {}, economics v{})",
        name(&a.verdict.verdict),
        a.verdict.as_of,
        a.verdict.economics_version
    )];
    for f in &a.verdict.failures {
        out.push(format!(
            "  {} {}{}: {}",
            name(&f.outcome),
            f.code.as_str(),
            f.field
                .as_deref()
                .map(|x| format!(" {x}"))
                .unwrap_or_default(),
            f.detail
        ));
    }
    let keys: Vec<String> = a
        .keys
        .iter()
        .map(|(k, v)| format!("{}={v}", name(k)))
        .collect();
    out.push(format!("keys: {}", keys.join(" · ")));
    out
}

/// The tool's text for a written proposal: its id, then the preview.
fn written(p: &MechanismProposal, run: &RunDir, preview: Result<Assessment>) -> String {
    let o = p.opportunity();
    let mut lines = vec![format!(
        "soe_propose {} {} v{} {} run={run} | written",
        p.id,
        o.id,
        o.version,
        name(&o.mechanism)
    )];
    lines.push(
        "preview (read-only — computed now, before the Critic; the week decides):".to_string(),
    );
    match preview {
        Ok(a) => lines.extend(assessment_lines(&a)),
        Err(e) => lines.push(format!("  unavailable: {e:#}")),
    }
    let unsupported: Vec<String> = p.draft.unsupported().into_iter().map(|u| u.field).collect();
    lines.push(format!(
        "unsupported claims (known inputs with no basis): {}",
        if unsupported.is_empty() {
            "none".to_string()
        } else {
            unsupported.join(", ")
        }
    ));
    let unknown: Vec<String> = o
        .economics
        .inputs()
        .into_iter()
        .filter(|(_, i)| !i.is_known())
        .map(|(f, _)| f)
        .collect();
    lines.push(format!(
        "unknown inputs: {}",
        if unknown.is_empty() {
            "none".to_string()
        } else {
            unknown.join(", ")
        }
    ));
    lines.join("\n")
}

#[async_trait]
impl Tool for SoeProposeTool {
    fn definition(&self) -> &ToolDef {
        &self.def
    }

    async fn execute(&self, args_v: &Value, ctx: &ToolCtx<'_>) -> Result<ToolOutput> {
        ctx.scope.check_fs_write(ctx.workspace)?;
        let tool = names::SOE_PROPOSE;
        let o = args(tool, args_v, ARGS)?;
        let run = run_arg(tool, o)?;
        let draft = draft_text(o)?;
        let store = self.shared.store()?;
        let want = self
            .shared
            .sandbox
            .soe
            .as_ref()
            .map(|s| s.architect.as_str());
        let agent = self.shared.stamp.check_agent(tool, want)?;
        let provenance =
            self.shared
                .stamp
                .provenance(agent, ctx.call_id, self.shared.clock.now_ms());
        let p = match submit_proposal(store, &run, &draft, provenance)? {
            Ok(p) => p,
            Err(e) => return Err(refused(tool, &run, &e)),
        };
        let preview =
            submit::head(store, &run).and_then(|h| assess_in_run(store, &run, &h, p.opportunity()));
        Ok(ToolOutput::from(written(&p, &run, preview)))
    }
}
