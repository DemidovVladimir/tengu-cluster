//! SOE adapters (O3 / O4): the ports of the weekly cycle
//! (`ports/soe.rs`) over the private SOE state root
//! `<TENGU_HOME>/state/<sources.state>/` (critic C8) and the `run-agent`
//! machinery. Wired by `bootstrap/soe.rs` (the `soe_cycle` job).
//!
//! | File | Holds |
//! |---|---|
//! | `store.rs` | `FsCycleStore` — `CycleStore`: `cycles/<id>/` · `replays/<id>/` claimed once (`create_dir`), write-once files, `proposals.jsonl` / `challenges.jsonl` appends, freeze = `MANIFEST.json` + read-only, the append-only state logs |
//! | `runner.rs` | `SubprocessStageRunner` — `StageRunner`: one `tengu run-agent` child per stage (`SubprocessRunner`; session = the run id, step = the stage, the agent's turn cap and wall clock), metrics kept on a failed child; `skill_sha256` (the stage skill identity) |
//! | `cache.rs` | `CachedStageRunner` — `StageRunner` that records each stage under `stage-cache/<key>.jsonl` (key = canonical sha256 of `{agent, model, skill_sha256, stage, goal}`) and replays a hit verbatim through the tools' path; an offline miss is an error naming the full key |

pub(crate) mod cache;
pub(crate) mod runner;
pub(crate) mod store;
