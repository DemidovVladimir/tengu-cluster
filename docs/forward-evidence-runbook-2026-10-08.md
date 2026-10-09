# Forward evidence runbook — rule W weekend #2 (2026-10-09 → 10-12)

Why: Phases 7–11 kept W1 (`docs/p10-w2-candidate-2026-10-08.md`). Forward weekends are the only clean evidence, and rule W's M3 go needs 12 (1 so far). Preregistered and sealed: `lineage/experiments/fwd.rule_w.2026-10-09.toml`. Paper only: no key, no order leaves this Mac.

## Operator: one start, one stop

| When (New York · UTC · Berlin) | Do |
|---|---|
| Before the stop (Mon 2026-10-12) | Do NOT `git pull` in the main checkout: origin/main now has `lineage/rankings/`, which the frozen binary `tengu-acdef66` refuses (W1 load error, verified 2026-10-08). After the stop: delete the untracked local copies `docs/strategy-ranking-automation-2026-10-08.md` and `TENGU_STUDIO_PLAN.md` (they are tracked upstream), then pull. |
| ≤ Fri 10-09 19:30 · 23:30 · Sat 01:30 | Mac on AC power, lid open, online. In a terminal: `tmux new -s xmw`, then `cd ~/development/tengu-cluster && TENGU_SECRETS_LOADED= caffeinate -i -s nice -n 10 ~/.cache/tengu-xm.noindex/weekend/tengu-acdef66 run --sandbox xmarket-weekend` (detach: Ctrl-b d) |
| any time | `~/.cache/tengu-xm.noindex/weekend/tengu-acdef66 doctor --sandbox xmarket-weekend --live` (exit 0 = heartbeat fresh, feeds live) · `… risk status --sandbox xmarket-weekend` |
| Fri 20:00 · Sat 00:00 · Sat 02:00 | anchor: `mkt_ctx/1` recorded |
| Sun 10-11 18:00 · 22:00 · Mon 00:00 | entry: 4 capped + every shadow fade |
| Mon 10-12 09:00 · 13:00 · 15:00 | exit |
| ≥ Mon 10:00 · 14:00 · 16:00 | `tmux attach -t xmw`, Ctrl-C (drains ≤ 20 s, exit 0) |

Binary: `~/.cache/tengu-xm.noindex/weekend/tengu-acdef66` = main `acdef66fc45252cba181a351cbc237ed6886ad57`, sha256 `fdcf2c270dd94347b2786d2b1a0b6dda162d91c1a4f02dc37408ce497555b445`. Never swap it mid-run. The unattended supervisor from weekend #1 (`~/.tengu/state/xmarket-weekend/run-logs/supervise.sh`) also works: point it at this binary.

## Monday grading (Claude, after the stop)

| Step | Command |
|---|---|
| 1 Snapshot | a new vault record `lineage/evidence/w1-2026-10-12.toml` (ledger.db, history day files 20261009–20261012), then `tengu evidence snapshot --record …` and `verify` |
| 2 Grade | `tengu evidence grade --ledger <vault>/xmarket-weekend/ledger.db` (12 checks, both accounts) |
| 3 Regrade | `tengu evidence regrade --history <vault>/xmarket-weekend/history --anchor 2026-10-10T00:00:00Z --entry 2026-10-11T22:00:00Z --exit 2026-10-12T13:00:00Z --notional-usd 25 --top-n 4 --min-abs-signal-bps 50 --fees recorded --compare-ledger <vault>/xmarket-weekend/ledger.db --compare-account xmarket-weekend` |
| 4 Bars + filings | `tengu history backfill --sandbox xlab-w2 --instruments @xyz_stocks --interval 1h --from 2026-10-05 --funding` · `--interval 1m --from 2026-10-09T19:00:00Z` · `tengu history events --sandbox xlab-w2 --instruments @xyz_stocks --from 2026-10-05` |
| 5 Secondary | `tengu backtest --sandbox xlab-w2 --strategy <s> --from 2026-10-10 --to 2026-10-12` for each of `weekend_fade_top4`, `weekend_fade_top4_stop500`, `weekend_fade_top4_noise_only`, `weekend_fade_top4_net_of_cost` |
| 6 Record | a new record `fwd.rule_w.2026-10-09.graded` (results, verdict, evidence `record:experiment/fwd.rule_w.2026-10-09`): the sealed prereg stays byte-identical (any edit is `seal_mismatch`); M3 tally 2 / 12 |
