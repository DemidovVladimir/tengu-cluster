//! Tengu Studio use cases (`TENGU_STUDIO_PLAN.md` § 5): read models over
//! the validated config and the execution trace. The browser only draws
//! what these return; it owns no workflow, risk, scope or colour rule.
//!
//! | Module | Use case |
//! |---|---|
//! | `graph` | `build_graph`: config + catalog tools (+ an execution map) → `domain::workflow::WorkflowGraph` |
//! | `stream` | `follow_run` / `follow_live`: one run, or the lease holder's run across restarts, as a bounded per-client stream (backlog waits, a live overflow = `Lagged`, resume by `seq`); feature `studio` |
//! | `board` | `fold_run`: a run's events over the graph → each event's view (tone, facets, highlighted edges) + the board at a `seq` (node colours, grey legal set, header facts); feature `studio` |
//! | `inspect` | `config_slice` / `node_evidence`: the validated config section behind a node + the store rows it names; feature `studio` |
//! | `control` | Play / Stop / send-event rules: this Studio's own runtime phase + the heartbeat → the control state, its tone, and what each action may do now (or why not); feature `studio` |

#[cfg(feature = "studio")]
pub(crate) mod board;
#[cfg(feature = "studio")]
pub(crate) mod control;
pub(crate) mod graph;
#[cfg(feature = "studio")]
pub(crate) mod inspect;
#[cfg(feature = "studio")]
pub(crate) mod stream;
