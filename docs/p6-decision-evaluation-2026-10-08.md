# Phase 6 — decision evaluation (2026-10-08)

`TENGU_ROADMAP.md` Phase 6, gate G6: the same candidate evidence must fairly evaluate the deterministic baseline, Jev and HOLD; Jev stays `UNPROVEN` until evidence shows value. Review #1 = APPROVE (`docs/w1-review-2026-10-06.md` § Verdict).

## Built

| Piece | Where |
|---|---|
| First fixes D1 · D2 · D3 | PR #33: `data_through_ms` + `--data-through`, CLI holdout reads counted, cited runs never pruned. D11 → Phase 9 (operator) |
| Candidate table | `domain/backtest/evaluation.rs`: one row per candidate by `seq` from `candidates.jsonl` + `trades-research.jsonl` + `decisions.jsonl` |
| Command | `tengu evidence evaluate <run dir> [--bootstrap 2000] [--seed 7] [--format json]`: read-only, works on vault copies |
| Policies | rules = take every candidate · jev = its `take`s · hold = none, scored on the same common set |
| Lenses | per candidate (skips count 0, paired bootstrap over periods) · per trade (the gate's own `jev − rules`) |
| Also | calibration + Brier, take rate, latency p50 / p95 / max, Σ cost |
| Verdict | `PROVEN`: jev beats rules and hold per candidate (CI > 0) · `REJECTED`: jev − hold CI < 0 (its takes lose money) · else `UNPROVEN` |

## W1 result (vault copy of `20261001T182905Z-weekend_fade`, 1,500 candidates, 29 weekends)

| Policy | Trades | bps per candidate | bps per trade |
|---|---|---|---|
| rules | 1,500 | +50.45 | +50.45 |
| jev | 277 | +14.66 | +79.38 |
| hold | 0 | 0.00 | — |

| Difference | 95 % CI |
|---|---|
| jev − rules per trade | +28.93 [−23.5, +81.6] — equals the gate's recorded figure |
| jev − rules per candidate | −35.79 [−61.9, −6.6] — it skips winners |
| jev − hold per candidate | +14.66 [+3.0, +27.5] |
| rules − hold per candidate | +50.45 [+15.7, +81.5] |

Jev: **UNPROVEN**. Its picks are better per trade (CI spans 0), but it takes 18.5 % of candidates and leaves profit behind. "Take every candidate" is the uncapped research arm, which the $100 capped book cannot run, so worse-than-rules does not reject it. Brier 0.349. Cost $0.048889 for 1,500 calls.

## G6

| Check | Result |
|---|---|
| Same candidate evidence for rules, Jev, HOLD | PASS: one common set, every policy scored on it |
| Fair: HOLD where meaningful | PASS: HOLD = 0 per candidate, both diffs with CIs |
| Jev represented as UNPROVEN until value is shown | PASS: verdict rule above; W1 = UNPROVEN |
| Reproducible | PASS: seeded bootstrap; the test pins a 30-candidate fixture (`tests/fixtures/evidence/gated-run/`) |

Open: the capped lens (`jev_capped` vs `rules_capped` per period, in USD) is still read from `report.md` (`−25.39 [−113.5, +63.8]` on W1). Next phase: P7 news / information.
