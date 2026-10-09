//! The stage window of one run dir (O3 Architect / Critic): what the cycle
//! writes before the model stages (`head.json`, `packet.json`,
//! `carried.json`), the phase markers, and the two tool-facing writes —
//! [`submit_proposal`] (`soe_propose`) and [`submit_challenge`]
//! (`soe_challenge`). A tool never writes a file itself: it hands the
//! model's JSON and the stage's [`Provenance`] here.
//!
//! | Phase (marker file, written once, forward only) | Accepts |
//! |---|---|
//! | `PROPOSE` (`phase-propose.json`) | [`submit_proposal`] |
//! | `CHALLENGE` (`phase-challenge.json`) | [`submit_challenge`] |
//! | `CLOSED` (`phase-closed.json`) | nothing: the cycle decides |
//!
//! | [`submit_proposal`] check (in order; refusals are listed, nothing is written) | Code |
//! |---|---|
//! | the dir is open and in `PROPOSE` | `stage_closed` |
//! | the draft (`proposal::draft_from_json`): computed keys, shape, record rules | `computed_field` · `invalid_record` · the record's codes |
//! | the draft against the cycle's packet at the decision (`ProposalDraft::check`) | `unsupported_evidence` · `future_leakage` · … |
//! | each forecast item resolves within `forecast_max_weeks` of the decision | `horizon_too_long` |
//! | the stamp's `generation` is the cycle's | `generation_mismatch` |
//! | fewer than `max_proposals` so far; the opportunity id not yet proposed this cycle (re-proposing a carried candidate replaces it) | `too_many_proposals` · `duplicate` |
//! | then | stamped `<cycle id>.pNN` (01, 02, … in append order) and appended |
//!
//! | [`submit_challenge`] check | Code |
//! |---|---|
//! | the dir is open and in `CHALLENGE` | `stage_closed` |
//! | the draft (`challenge::challenge_from_json`), its target one of the week's [`candidates`], its evidence in the packet, its field and values (`ChallengeDraft::check`) | `computed_field` · `unknown_target` · `unsupported_evidence` · `unknown_field` · `invalid_field` |
//! | the stamp's `generation` | `generation_mismatch` |
//! | then | stamped `<cycle id>.cNN`, appended |
//!
//! [`candidates`] = this cycle's proposals + the carried ones not
//! re-proposed, by id — what the Critic may challenge and the week decides.

use std::collections::BTreeSet;

use anyhow::{bail, Context, Result};
use serde::{de::DeserializeOwned, Deserialize, Serialize};

use super::{GENERATION_MISMATCH, STAGE_CLOSED, TOO_MANY_PROPOSALS};
use crate::domain::canonical::canonical_json;
use crate::domain::lineage::value::{valid_id, Time};
use crate::domain::soe::challenge::{challenge_from_json, Challenge};
use crate::domain::soe::observe::EvidenceIndex;
use crate::domain::soe::opportunity::Opportunity;
use crate::domain::soe::proposal::{draft_from_json, MechanismProposal, Provenance};
use crate::domain::soe::value::{codes, ValueError};
use crate::domain::source::{AsOfMode, EvidencePacket};
use crate::ports::soe::{CycleStore, RunDir, RunStatus};

/// `head.json`: the cycle's fixed facts, written at the claim.
pub(crate) const HEAD: &str = "head.json";
/// `packet.json`: the O2 evidence packet at the decision.
pub(crate) const PACKET: &str = "packet.json";
/// `carried.json`: the previous frozen cycle's candidates still standing.
pub(crate) const CARRIED: &str = "carried.json";
/// `head.json`'s schema.
pub(crate) const HEAD_SCHEMA: &str = "soe.run_head/1";
/// The generation id of an unbound sandbox: a cycle's pin
/// (`bootstrap::soe::generation_pin`) and the tools' stamp
/// (`tools/soe/mod.rs`) agree on it.
pub(crate) const UNBOUND: &str = "UNBOUND";

const WEEK_MS: i64 = 7 * 86_400_000;

/// The generation a cycle runs under: its id and the sha256 that pins it
/// (`toml_digest` of `lineage/generations/<id>.toml`; an unbound sandbox:
/// id `UNBOUND` and the digest of its config file).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct GenerationPin {
    pub id: String,
    pub sha256: String,
}

impl GenerationPin {
    pub(crate) fn problems(&self) -> Vec<String> {
        let mut out = Vec::new();
        if !valid_id(&self.id) {
            out.push(format!("generation id `{}` is not an id", self.id));
        }
        if !crate::domain::evidence::valid_sha256(&self.sha256) {
            out.push(format!(
                "generation sha256 `{}` is not 64 lowercase hex",
                self.sha256
            ));
        }
        out
    }
}

/// `head.json` (module doc).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RunHead {
    pub schema: String,
    /// `cycles/<id>` · `replays/<id>`.
    pub run: String,
    pub cycle_id: String,
    pub decided_at_ms: i64,
    pub mode: AsOfMode,
    pub packet_sha256: String,
    pub generation: GenerationPin,
    pub architect: String,
    pub critic: String,
    pub max_proposals: usize,
    pub forecast_max_weeks: u32,
}

impl RunHead {
    pub(crate) fn decided_at(&self) -> Time {
        Time::At(self.decided_at_ms)
    }
}

/// A carried record that no longer checks (or no longer has a target).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Dropped {
    /// `PROPOSAL` · `CHALLENGE`.
    pub kind: String,
    /// The record id, in full.
    pub record: String,
    /// The opportunity id it is about.
    pub candidate: String,
    pub why: Vec<String>,
}

/// `carried.json` (module doc of `cycle.rs`: carry-forward).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Carried {
    /// The previous frozen cycle; none = nothing to carry.
    pub from: Option<String>,
    /// Verbatim records of earlier cycles, each re-checked against this packet.
    pub proposals: Vec<MechanismProposal>,
    pub challenges: Vec<Challenge>,
    pub dropped: Vec<Dropped>,
}

/// One phase of the stage window (module table).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub(crate) enum Phase {
    Propose,
    Challenge,
    Closed,
}

impl Phase {
    const ALL: [Phase; 3] = [Phase::Propose, Phase::Challenge, Phase::Closed];

    pub(crate) fn marker(self) -> &'static str {
        match self {
            Phase::Propose => "phase-propose.json",
            Phase::Challenge => "phase-challenge.json",
            Phase::Closed => "phase-closed.json",
        }
    }
}

/// `v` as one canonical JSON line.
pub(crate) fn json_line<T: Serialize>(v: &T) -> Result<Vec<u8>> {
    let mut b = canonical_json(&serde_json::to_value(v)?).into_bytes();
    b.push(b'\n');
    Ok(b)
}

/// The JSON file `name` of `dir`, parsed.
pub(crate) fn read_json<T: DeserializeOwned>(
    store: &dyn CycleStore,
    dir: &RunDir,
    name: &str,
) -> Result<Option<T>> {
    match store.read(dir, name)? {
        None => Ok(None),
        Some(b) => serde_json::from_slice(&b)
            .map(Some)
            .with_context(|| format!("{dir}/{name}")),
    }
}

fn required<T: DeserializeOwned>(store: &dyn CycleStore, dir: &RunDir, name: &str) -> Result<T> {
    read_json(store, dir, name)?.with_context(|| format!("{dir}/{name}: missing"))
}

pub(crate) fn head(store: &dyn CycleStore, dir: &RunDir) -> Result<RunHead> {
    let h: RunHead = required(store, dir, HEAD)?;
    if h.schema != HEAD_SCHEMA {
        bail!("{dir}/{HEAD}: schema `{}` (want {HEAD_SCHEMA})", h.schema);
    }
    Ok(h)
}

pub(crate) fn packet(store: &dyn CycleStore, dir: &RunDir) -> Result<EvidencePacket> {
    required(store, dir, PACKET)
}

/// `carried.json`; none written = nothing carried.
pub(crate) fn carried(store: &dyn CycleStore, dir: &RunDir) -> Result<Carried> {
    Ok(read_json(store, dir, CARRIED)?.unwrap_or_default())
}

/// The latest phase marker of `dir`.
pub(crate) fn phase(store: &dyn CycleStore, dir: &RunDir) -> Result<Option<Phase>> {
    let mut at = None;
    for p in Phase::ALL {
        if store.read(dir, p.marker())?.is_some() {
            at = Some(p);
        }
    }
    Ok(at)
}

/// Write `p`'s marker: only forward.
pub(crate) fn open_phase(store: &dyn CycleStore, dir: &RunDir, p: Phase) -> Result<()> {
    if let Some(now) = phase(store, dir)? {
        if now >= p {
            bail!("{dir}: phase {now:?} — {p:?} would not move forward");
        }
    }
    store.write(
        dir,
        p.marker(),
        &json_line(&serde_json::json!({"phase": p, "run": dir.to_string()}))?,
    )
}

/// The week's candidates (module doc): new proposals + carried ones not
/// re-proposed, by id.
pub(crate) fn candidates(store: &dyn CycleStore, dir: &RunDir) -> Result<Vec<MechanismProposal>> {
    let mut out = store.proposals(dir)?;
    let proposed: BTreeSet<String> = out.iter().map(|p| p.opportunity().id.clone()).collect();
    out.extend(
        carried(store, dir)?
            .proposals
            .into_iter()
            .filter(|p| !proposed.contains(&p.opportunity().id)),
    );
    out.sort_by(|a, b| a.id.cmp(&b.id));
    Ok(out)
}

/// The dir is open, its head read and the phase is `want` (else a refusal).
fn window(
    store: &dyn CycleStore,
    dir: &RunDir,
    want: Phase,
) -> Result<std::result::Result<RunHead, Vec<ValueError>>> {
    if store.status(dir)? != RunStatus::Open {
        return Ok(Err(vec![ValueError::new(
            STAGE_CLOSED,
            format!("{dir} is not an open run"),
        )]));
    }
    let h = head(store, dir)?;
    let now = phase(store, dir)?;
    if now != Some(want) {
        return Ok(Err(vec![ValueError::new(
            STAGE_CLOSED,
            format!(
                "{dir} is in phase {}, this write needs {want:?}",
                now.map_or("NONE".to_string(), |p| format!("{p:?}"))
            ),
        )]));
    }
    Ok(Ok(h))
}

fn generation_ok(h: &RunHead, p: &Provenance) -> Option<ValueError> {
    (p.generation != h.generation.id).then(|| {
        ValueError::new(
            GENERATION_MISMATCH,
            format!(
                "provenance.generation `{}` is not the cycle's `{}`",
                p.generation, h.generation.id
            ),
        )
    })
}

/// Module table: check, stamp and append one Architect draft. `Ok(Err)` =
/// refused (every problem listed, nothing written); `Err` = IO.
pub(crate) fn submit_proposal(
    store: &dyn CycleStore,
    dir: &RunDir,
    draft_json: &str,
    provenance: Provenance,
) -> Result<std::result::Result<MechanismProposal, Vec<ValueError>>> {
    let h = match window(store, dir, Phase::Propose)? {
        Ok(h) => h,
        Err(e) => return Ok(Err(e)),
    };
    let draft = match draft_from_json(draft_json) {
        Ok(d) => d,
        Err(e) => return Ok(Err(e)),
    };
    let index = EvidenceIndex::of(&packet(store, dir)?);
    let at = h.decided_at();
    let mut errors = draft.check(&index, &at).err().unwrap_or_default();
    let limit = h
        .decided_at_ms
        .saturating_add(i64::from(h.forecast_max_weeks).saturating_mul(WEEK_MS));
    for (i, x) in draft.forecast.iter().enumerate() {
        let within = matches!(x.resolve_by.latest(), Some(by) if by <= limit);
        if !within {
            errors.push(ValueError::new(
                codes::HORIZON_TOO_LONG,
                format!(
                    "forecast[{i}].resolve_by: {} is more than {} weeks after the decision {at}",
                    x.resolve_by, h.forecast_max_weeks
                ),
            ));
        }
    }
    errors.extend(generation_ok(&h, &provenance));
    let existing = store.proposals(dir)?;
    if existing.len() >= h.max_proposals {
        errors.push(ValueError::new(
            TOO_MANY_PROPOSALS,
            format!(
                "{} proposals already — this cycle takes at most {}",
                existing.len(),
                h.max_proposals
            ),
        ));
    }
    let id = &draft.opportunity.id;
    if let Some(p) = existing.iter().find(|p| &p.opportunity().id == id) {
        errors.push(ValueError::new(
            codes::DUPLICATE,
            format!(
                "opportunity `{id}` is already proposal `{}` of this cycle",
                p.id
            ),
        ));
    }
    if !errors.is_empty() {
        return Ok(Err(errors));
    }
    let stamped = match MechanismProposal::stamp(
        &format!("{}.p{:02}", h.cycle_id, existing.len() + 1),
        &h.cycle_id,
        draft,
        provenance,
    ) {
        Ok(p) => p,
        Err(e) => return Ok(Err(e)),
    };
    store.append_proposal(dir, &stamped)?;
    Ok(Ok(stamped))
}

/// Module table: check, stamp and append one Critic draft.
pub(crate) fn submit_challenge(
    store: &dyn CycleStore,
    dir: &RunDir,
    draft_json: &str,
    provenance: Provenance,
) -> Result<std::result::Result<Challenge, Vec<ValueError>>> {
    let h = match window(store, dir, Phase::Challenge)? {
        Ok(h) => h,
        Err(e) => return Ok(Err(e)),
    };
    let draft = match challenge_from_json(draft_json) {
        Ok(d) => d,
        Err(e) => return Ok(Err(e)),
    };
    let index = EvidenceIndex::of(&packet(store, dir)?);
    let week = candidates(store, dir)?;
    let targets: Vec<&Opportunity> = week.iter().map(|p| p.opportunity()).collect();
    let mut errors = draft.check(&index, &targets).err().unwrap_or_default();
    errors.extend(generation_ok(&h, &provenance));
    if !errors.is_empty() {
        return Ok(Err(errors));
    }
    let n = store.challenges(dir)?.len();
    let stamped = match Challenge::stamp(
        &format!("{}.c{:02}", h.cycle_id, n + 1),
        &h.cycle_id,
        draft,
        provenance,
    ) {
        Ok(c) => c,
        Err(e) => return Ok(Err(e)),
    };
    store.append_challenge(dir, &stamped)?;
    Ok(Ok(stamped))
}
