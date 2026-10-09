//! Tengu Studio use cases (`TENGU_STUDIO_PLAN.md` § 5): read models over
//! the validated config and the execution trace. The browser only draws
//! what these return; it owns no workflow, risk or scope rule.
//!
//! | Module | Use case |
//! |---|---|
//! | `graph` | `build_graph`: config + catalog tools (+ an execution map) → `domain::workflow::WorkflowGraph` |
//! | `stream` | `follow_run` / `follow_live`: one run, or the lease holder's run across restarts, as a bounded per-client stream (backlog waits, a live overflow = `Lagged`, resume by `seq`); `--features studio` |

pub(crate) mod graph;
#[cfg(feature = "studio")]
pub(crate) mod stream;
