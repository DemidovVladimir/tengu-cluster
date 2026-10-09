//! `soe_challenge` — one Critic draft into the open run
//! (`application::soe::submit::submit_challenge`; `domain/soe/challenge.rs`).
//!
//! | Step | Rule |
//! |---|---|
//! | Arguments | strict (an unknown key is an error); flat: `target`, `kind`, `claim`, `evidence[]`, `effect` (`WIDEN` · `BLOCK_GATE` · `NONE`) and its fields — `WIDEN`: `field` + any of `low` `base` `high` or `unknown_reason`; `BLOCK_GATE`: `gate`; a field another effect does not take is refused |
//! | Draft | `{target, kind, claim, evidence, effect: {kind, …}}` — the `ChallengeDraft` a model would write nested; the stage cache records this draft |
//! | Agent | the stage agent (`mod.rs`); with `[soe]` only `soe.critic` |
//! | Write | `submit_challenge`: open run, phase `CHALLENGE`, target one of the week's candidates, evidence in the packet, the field an input, the values parse, the stamp's generation — every refusal listed, nothing written; else `<cycle>.cNN` appended to `challenges.jsonl` |

use anyhow::{bail, Result};
use async_trait::async_trait;
use serde_json::{json, Map, Value};

use super::{args, opt_strings, refused, run_arg, SoeShared};
use crate::adapters::outbound::tools::xlab::opt_str;
use crate::application::soe::submit::submit_challenge;
use crate::domain::canonical::canonical_json;
use crate::domain::message::ToolDef;
use crate::domain::soe::challenge::{Challenge, Effect};
use crate::domain::tools as names;
use crate::ports::tool::{Tool, ToolCtx, ToolOutput};

const ARGS: &[&str] = &[
    "run",
    "target",
    "kind",
    "claim",
    "evidence",
    "effect",
    "field",
    "low",
    "base",
    "high",
    "unknown_reason",
    "gate",
];

/// The fields of each effect (module table: arguments).
const WIDEN: &[&str] = &["field", "low", "base", "high", "unknown_reason"];
const BLOCK_GATE: &[&str] = &["gate"];

pub(crate) struct SoeChallengeTool {
    def: ToolDef,
    shared: SoeShared,
}

impl SoeChallengeTool {
    pub(crate) fn new(shared: SoeShared) -> Self {
        Self {
            def: super::defs::def(names::SOE_CHALLENGE),
            shared,
        }
    }
}

/// The nested draft of the flat arguments (module table: draft).
pub(crate) fn draft_of(o: &Map<String, Value>) -> Result<Value> {
    let tool = names::SOE_CHALLENGE;
    let need = |key: &str| -> Result<String> {
        opt_str(tool, o, key)?
            .map(str::to_string)
            .ok_or_else(|| anyhow::anyhow!("{tool}: '{key}' is required"))
    };
    let (target, kind, claim) = (need("target")?, need("kind")?, need("claim")?);
    let effect = need("effect")?;
    let takes: &[&str] = match effect.as_str() {
        "WIDEN" => WIDEN,
        "BLOCK_GATE" => BLOCK_GATE,
        "NONE" => &[],
        other => bail!("{tool}: 'effect' must be WIDEN, BLOCK_GATE or NONE, got {other}"),
    };
    let mut e = Map::new();
    e.insert("kind".into(), Value::String(effect.clone()));
    for key in WIDEN.iter().chain(BLOCK_GATE) {
        let Some(v) = opt_str(tool, o, key)? else {
            continue;
        };
        if !takes.contains(key) {
            bail!("{tool}: '{key}' does not go with effect {effect}");
        }
        e.insert((*key).to_string(), Value::String(v.to_string()));
    }
    Ok(json!({
        "target": target,
        "kind": kind,
        "claim": claim,
        "evidence": opt_strings(tool, o, "evidence")?,
        "effect": Value::Object(e),
    }))
}

/// The tool's text for a written challenge.
fn written(c: &Challenge, run: &str) -> String {
    let d = &c.draft;
    let effect = match &d.effect {
        Effect::Widen { field, .. } => format!("WIDEN {field}"),
        Effect::BlockGate { gate } => format!("BLOCK_GATE {}", gate.as_str()),
        Effect::None => "NONE".into(),
    };
    let kind = serde_json::to_value(d.kind)
        .ok()
        .and_then(|v| v.as_str().map(String::from))
        .unwrap_or_default();
    format!(
        "soe_challenge {} target={} kind={kind} effect={effect} run={run} | written\n\
         evidence: {}\n\
         The week merges it when it decides: only toward the conservative side (an input made \
         worse or unknown, a gate held), never the other way.",
        c.id,
        d.target,
        if d.evidence.is_empty() {
            "none (an inference)".to_string()
        } else {
            d.evidence.join(", ")
        }
    )
}

#[async_trait]
impl Tool for SoeChallengeTool {
    fn definition(&self) -> &ToolDef {
        &self.def
    }

    async fn execute(&self, args_v: &Value, ctx: &ToolCtx<'_>) -> Result<ToolOutput> {
        ctx.scope.check_fs_write(ctx.workspace)?;
        let tool = names::SOE_CHALLENGE;
        let o = args(tool, args_v, ARGS)?;
        let run = run_arg(tool, o)?;
        let draft = canonical_json(&draft_of(o)?);
        let store = self.shared.store()?;
        let want = self.shared.sandbox.soe.as_ref().map(|s| s.critic.as_str());
        let agent = self.shared.stamp.check_agent(tool, want)?;
        let provenance =
            self.shared
                .stamp
                .provenance(agent, ctx.call_id, self.shared.clock.now_ms());
        match submit_challenge(store, &run, &draft, provenance)? {
            Ok(c) => Ok(ToolOutput::from(written(&c, &run.to_string()))),
            Err(e) => Err(refused(tool, &run, &e)),
        }
    }
}
