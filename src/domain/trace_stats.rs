//! Run statistics — one recorded run's events (`domain/trace.rs`) folded
//! into counts, error rates, latencies, tokens and Jev cost: `tengu trace
//! stats` and the Studio board header. Pure: events in, numbers out; every
//! rate and percentile is computed here, the page only draws them.
//!
//! | Figure | From |
//! |---|---|
//! | `events` · `span_ms` · `failed` | every event · last `ts_ms` − first · events with status `failed` |
//! | `families` | events that report a duration (`duration_ms`: a finished tool call, Jev call, step, LLM call, …) by kind family — the part before the first `.` (`tool`, `jev`, `step`, `metrics`, …): a [`Timing`] |
//! | `tools` | `tool.completed` / `tool.failed` by node `tool:<agent>/<tool>`: a [`Timing`] |
//! | `tokens` · `tokens_total` | `metrics.recorded` by `payload.agent`: calls, prompt / completion tokens · the same over every agent |
//! | `jev` | `jev.completed` / `jev.failed`: calls, failed, `payload.usage` input / output tokens, cost = the sum of the costs reported (`cost_calls` of the calls reported one) |
//!
//! [`Timing`]: count, failed, `error_rate` = failed / count, nearest-rank
//! p50 / p95 and the max of `duration_ms` (a finish without one counts as
//! 0 ms).

use std::collections::BTreeMap;

use serde::Serialize;
use serde_json::Value;

use crate::domain::trace::{ExecutionEvent, Status};

/// One group of finished things (module table).
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub(crate) struct Timing {
    pub count: u64,
    pub failed: u64,
    pub error_rate: f64,
    pub p50_ms: u64,
    pub p95_ms: u64,
    pub max_ms: u64,
}

/// One agent's LLM calls (`metrics.recorded`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub(crate) struct Tokens {
    pub calls: u64,
    pub prompt: u64,
    pub completion: u64,
}

/// The run's decisions calls.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub(crate) struct JevStats {
    pub calls: u64,
    pub failed: u64,
    pub input_tokens: u64,
    pub output_tokens: u64,
    /// The sum of the costs the calls reported (`usage.cost`).
    pub cost: f64,
    /// Calls that reported a cost.
    pub cost_calls: u64,
}

/// A run's statistics (module table).
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub(crate) struct RunStats {
    pub events: u64,
    pub span_ms: i64,
    pub failed: u64,
    pub families: BTreeMap<String, Timing>,
    pub tools: BTreeMap<String, Timing>,
    pub tokens: BTreeMap<String, Tokens>,
    pub tokens_total: Tokens,
    pub jev: JevStats,
}

#[derive(Default)]
struct Durations {
    ms: Vec<u64>,
    failed: u64,
}

impl Durations {
    fn add(&mut self, ev: &ExecutionEvent) {
        self.ms.push(ev.duration_ms.unwrap_or(0));
        self.failed += u64::from(ev.status == Status::Failed);
    }

    fn timing(mut self) -> Timing {
        self.ms.sort_unstable();
        let count = self.ms.len() as u64;
        // Nearest rank: the smallest value with at least p% at or below it.
        let pct = |p: u64| {
            let rank = (p * count).div_ceil(100).max(1) as usize;
            self.ms.get(rank - 1).copied().unwrap_or(0)
        };
        Timing {
            count,
            failed: self.failed,
            error_rate: if count == 0 {
                0.0
            } else {
                self.failed as f64 / count as f64
            },
            p50_ms: pct(50),
            p95_ms: pct(95),
            max_ms: self.ms.last().copied().unwrap_or(0),
        }
    }
}

fn number(v: &Value) -> u64 {
    v.as_u64().unwrap_or(0)
}

/// `events` (one run, any order) folded (module table).
pub(crate) fn fold_stats(events: &[ExecutionEvent]) -> RunStats {
    let mut families: BTreeMap<String, Durations> = BTreeMap::new();
    let mut tools: BTreeMap<String, Durations> = BTreeMap::new();
    let mut out = RunStats {
        events: events.len() as u64,
        ..RunStats::default()
    };
    let (mut first, mut last) = (i64::MAX, i64::MIN);
    for ev in events {
        first = first.min(ev.ts_ms);
        last = last.max(ev.ts_ms);
        out.failed += u64::from(ev.status == Status::Failed);
        if ev.duration_ms.is_some() {
            let family = ev.kind.split('.').next().unwrap_or_default();
            families.entry(family.to_string()).or_default().add(ev);
        }
        match ev.kind.as_str() {
            "tool.completed" | "tool.failed" => {
                let node = ev.node_id.clone().unwrap_or_default();
                tools.entry(node).or_default().add(ev);
            }
            "metrics.recorded" => {
                let agent = ev.payload["agent"].as_str().unwrap_or_default();
                let (prompt, completion) = (
                    number(&ev.payload["prompt_tokens"]),
                    number(&ev.payload["completion_tokens"]),
                );
                for t in [
                    out.tokens.entry(agent.to_string()).or_default(),
                    &mut out.tokens_total,
                ] {
                    t.calls += 1;
                    t.prompt += prompt;
                    t.completion += completion;
                }
            }
            "jev.completed" | "jev.failed" => {
                let j = &mut out.jev;
                let usage = &ev.payload["usage"];
                j.calls += 1;
                j.failed += u64::from(ev.status == Status::Failed);
                j.input_tokens += number(&usage["input_tokens"]);
                j.output_tokens += number(&usage["output_tokens"]);
                if let Some(cost) = usage["cost"].as_f64() {
                    j.cost += cost;
                    j.cost_calls += 1;
                }
            }
            _ => {}
        }
    }
    out.span_ms = if events.is_empty() { 0 } else { last - first };
    out.families = families.into_iter().map(|(k, d)| (k, d.timing())).collect();
    out.tools = tools.into_iter().map(|(k, d)| (k, d.timing())).collect();
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::trace::{Component, EventDraft, RunContext};
    use serde_json::json;

    fn ctx() -> RunContext {
        RunContext {
            sandbox: "control-loop-lab".into(),
            config_hash: None,
            runtime_id: None,
            run_id: "5b0c7d0e-8a4e-4f0a-9d8e-2f1c3b4a5d6e".into(),
        }
    }

    /// Counts, error rates, nearest-rank percentiles, tokens by agent, Jev
    /// usage and cost — from the events alone.
    #[test]
    fn a_run_folds_into_its_figures() {
        let c = ctx();
        let tool = |seq: u64, ms: u64, ok: bool| {
            let (kind, status) = if ok {
                ("tool.completed", Status::Ok)
            } else {
                ("tool.failed", Status::Failed)
            };
            c.stamp(
                seq,
                1_000 + seq as i64,
                EventDraft::new(Component::Tool, kind, status)
                    .node("tool:lab/read_file")
                    .duration(ms),
            )
        };
        let mut events = vec![c.stamp(
            1,
            1_000,
            EventDraft::new(Component::Runtime, "run.opened", Status::Ok),
        )];
        for (i, ms) in [10, 20, 30, 40, 50, 60, 70, 80, 90, 100]
            .into_iter()
            .enumerate()
        {
            events.push(tool(2 + i as u64, ms, i != 3));
        }
        events.push(c.stamp(
            12,
            1_100,
            EventDraft::new(Component::Jev, "jev.completed", Status::Ok)
                .duration(400)
                .payload(json!({"usage": {"input_tokens": 880, "output_tokens": 35, "cost": 0.0021}})),
        ));
        events.push(c.stamp(
            13,
            1_200,
            EventDraft::new(Component::Jev, "jev.failed", Status::Failed).duration(30_000),
        ));
        for (seq, agent, p) in [
            (14, "planner/lab", 4210),
            (15, "researcher", 6250),
            (16, "researcher", 100),
        ] {
            events.push(
                c.stamp(
                    seq,
                    1_300,
                    EventDraft::new(Component::Orchestrator, "metrics.recorded", Status::Ok)
                        .payload(
                            json!({"agent": agent, "prompt_tokens": p, "completion_tokens": 10}),
                        ),
                ),
            );
        }
        let s = fold_stats(&events);
        assert_eq!((s.events, s.span_ms, s.failed), (16, 300, 2));
        let t = &s.tools["tool:lab/read_file"];
        assert_eq!(
            (t.count, t.failed, t.p50_ms, t.p95_ms, t.max_ms),
            (10, 1, 50, 100, 100)
        );
        assert!((t.error_rate - 0.1).abs() < 1e-9);
        assert_eq!(s.families["tool"].count, 10);
        assert_eq!((s.families["jev"].count, s.families["jev"].failed), (2, 1));
        assert!(!s.families.contains_key("run") && !s.families.contains_key("metrics"));
        assert_eq!(
            s.tokens["researcher"],
            Tokens {
                calls: 2,
                prompt: 6350,
                completion: 20
            }
        );
        assert_eq!(s.tokens["planner/lab"].calls, 1);
        assert_eq!(
            s.tokens_total,
            Tokens {
                calls: 3,
                prompt: 10_560,
                completion: 30
            }
        );
        let j = &s.jev;
        assert_eq!(
            (
                j.calls,
                j.failed,
                j.input_tokens,
                j.output_tokens,
                j.cost_calls
            ),
            (2, 1, 880, 35, 1)
        );
        assert!((j.cost - 0.0021).abs() < 1e-12);
        assert_eq!(fold_stats(&[]), RunStats::default());
    }
}
