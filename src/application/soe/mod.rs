//! SOE weekly cycle use cases (O3 / O4; `docs/soe-2026-10-08.md` § 10): one
//! cycle over the ports of `ports/soe.rs` and the source store — observe
//! the as-of packet, run the Architect and the Critic as agents, decide with
//! the pure domain (`domain/soe/`), report, freeze, learn. No contact,
//! spend, publish or deploy exists anywhere in it (O5–O8 wait for Operator
//! Review #2).
//!
//! | File | Holds |
//! |---|---|
//! | `cycle.rs` | [`cycle::run_cycle`] (`CycleEnv`, `CycleParams`, `Target` cycle · replay, `CycleOutcome`): the step table, carry-forward of the previous frozen cycle, the stage goals, `decided.json` / `inputs.json` / `stages.json`, candidate events and `OpportunityEpisode`s (C20) |
//! | `submit.rs` | the stage window: `head.json` (`RunHead`, `GenerationPin`), `packet.json`, `carried.json` (`Carried`), phase markers (`Phase`); the tools' writes [`submit::submit_proposal`] / [`submit::submit_challenge`]; the week's `candidates` |
//! | `freeze.rs` | `freeze` (`MANIFEST.json` + read-only), `verify` (re-hash), `decision_sha256` (every file but `ops.json`) |
//! | `tests.rs` | in-memory fakes of the ports, the synthetic cycle cases `tests/fixtures/soe/cycles/`, the acceptance tests |
//!
//! | Refusal code | When |
//! |---|---|
//! | `cycle_already_frozen` | the run dir is frozen, or the forecast log holds the cycle |
//! | `cycle_unfinished` | the run dir was claimed and never frozen |
//! | `stage_closed` | a tool write outside its phase, or to a run that is not open |
//! | `generation_mismatch` | a stamp's generation is not the cycle's |
//! | `too_many_proposals` | the cycle's `max_proposals` is reached |

// Consumers land with `tengu soe cycle|replay`, the `soe_*` tools and the
// stage-runner adapters (next O3 steps).
#![allow(dead_code)]

pub(crate) mod cycle;
pub(crate) mod freeze;
pub(crate) mod submit;
#[cfg(test)]
pub(crate) mod tests;

pub(crate) const CYCLE_ALREADY_FROZEN: &str = "cycle_already_frozen";
pub(crate) const CYCLE_UNFINISHED: &str = "cycle_unfinished";
pub(crate) const STAGE_CLOSED: &str = "stage_closed";
pub(crate) const GENERATION_MISMATCH: &str = "generation_mismatch";
pub(crate) const TOO_MANY_PROPOSALS: &str = "too_many_proposals";
