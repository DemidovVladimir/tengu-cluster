# Tengu-cluster manual smoke test

Run this at the end of every implementation phase. Both the TUI and Telegram channels must pass before a phase is considered landed. Phase 0 captures the baseline outputs in the "Regression baselines" section at the bottom so later phases have a concrete diff target; if a later phase changes wording or behaviour, record whether the delta is expected and only update the baseline when the new behaviour is the intended one.

## Pre-flight

- [ ] `cargo build --release` completes with no errors or warnings
- [ ] `cargo test --bin tengu` passes (unit tests — the crate has no `[lib]`)
- [ ] `cargo test --test scope_lint --test run_agent_ipc` passes (structural scope lint + IPC boundary)
- [ ] `cargo clippy --all-targets -- -D warnings` passes
- [ ] Network: `make tor` is up (Arti + lyrebird-rs on 127.0.0.1:9050 — `[egress] network = "tor"` is the default) OR the config sets `[egress] network = "open"`; `cargo run -- doctor [--sandbox <name>]` prints `network:` + proxy reachability (`--tor` = live exit check)
- [ ] (with `--features postgres_memory`) Postgres reachable: `psql "$TENGU_MEMORY_DATABASE_URL" -c 'select count(*) from agentic_memory'` succeeds
- [ ] `git status --short` shows only the changes expected for the current phase

## TUI smoke (default agent)

Launch with `cargo run -- chat` (config: `-c/--config` > `$TENGU_CONFIG` > `~/.tengu/config.toml`; orchestrated only when the file has an `[orchestrator]` block). The TUI must come up without a panic and present a prompt.

- [ ] `> hello` produces a direct conversational response in under 5 seconds
- [ ] `> what agents do you have?` — planner answers from `TENGU_PLANNER_REGISTRY.md` (the `[agents.*]` blocks that carry a `description`); without `[orchestrator]` the default agent replies directly
- [ ] `> research what LLMs were released in April 2026 and summarise the top 3` produces a visible plan (`orch: plan created (N steps)` system bubble), `orch: ▶ <step> [<agent>]` / `orch: ✓ <step>` events as each `tengu run-agent` child runs, and a final synthesised response
- [ ] Ctrl+Q (or Ctrl+C) exits the TUI cleanly with no panic and no dangling `tengu run-agent` processes

## TUI smoke (lping sandbox)

Launch with `cargo run -- chat --sandbox lping` (the sandbox sets `[egress] network = "open"`, so no Tor proxy is needed). The multi-agent roster defined in `sandboxes/lping/config.toml` (`lping`, `crypto_researcher`, …) must load.

- [ ] TUI starts without panic and the lping roster is visible (agent list or startup log)
- [ ] `> summarize what this sandbox is for` returns a coherent response grounded in the lping sandbox context
- [ ] Ctrl+Q exits cleanly

## Telegram smoke (default)

Launch with `cargo run -- telegram` (`TELEGRAM_BOT_TOKEN` env var required; `[telegram]` has no token key). From the phone client associated with that bot token:

- [ ] `/agents` lists the configured agents
- [ ] `hello` returns a direct reply
- [ ] `research what LLMs were released in April 2026` streams status updates during planning and execution, then delivers a final plan response
- [ ] Bot shuts down cleanly on Ctrl-C with no panic

## Telegram smoke (storage-test)

Launch with `cargo run --features claude_code -- telegram --sandbox storage-test`. The sandbox flag must take effect for the Telegram channel too.

- [ ] Bot starts against the storage-test roster without panic
- [ ] A short message from the phone gets a coherent reply grounded in the storage-test sandbox context
- [ ] Ctrl-C shuts down cleanly

## Debug probes

- [ ] `TENGU_PLANNER_REGISTRY.md` at the repo root is regenerated on a planner turn and lists agents (`[agents.*]` with a `description`) + skills + core/MCP tools
- [ ] `TENGU_PLAN.md` at the repo root holds the last accepted plan (debug mirror — children receive the plan over IPC as `plan_state`)
- [ ] (with `--features postgres_memory`) step summaries landed: `psql "$TENGU_MEMORY_DATABASE_URL" -c "select session_id, agent, left(content, 80) from agentic_memory order by created_at desc limit 5"`

## xmarket paper desk smoke (`sandboxes/xmarket`, ~10 min, no LLM)

From the repo root. Build: `CARGO_TARGET_DIR=$HOME/.cache/tengu-xm.noindex/main CARGO_BUILD_JOBS=2 nice -n 10 cargo build --release`; `T=$HOME/.cache/tengu-xm.noindex/main/release/tengu`. Enter skips the vault prompt. Runbook: top of `sandboxes/xmarket/config.toml`; full checks: `docs/validation-checklist.md` § 9.

- [ ] `"$T" doctor --sandbox xmarket </dev/null` exits 0 (agents `xm`, `xm_architect`, `xm_executor`; `network: open`)
- [ ] `tmux new -s xm`, then `nice -n 10 "$T" run --sandbox xmarket` in the pane (foreground — never `&`)
- [ ] a second `"$T" run --sandbox xmarket </dev/null` exits 1 (lease `runtime:xmarket`)
- [ ] `"$T" doctor --sandbox xmarket --live </dev/null` exits 0 at +2 min (heartbeat fresh, 4 / 4 required feeds live)
- [ ] `"$T" risk status --sandbox xmarket </dev/null`: account `xmarket`, owner sandbox `xmarket`, no halt, `kill-switch file …: absent`
- [ ] `touch ~/.tengu/state/xmarket/KILL` → `risk status` says `PRESENT — every account is halted`; `rm` it (then `"$T" risk resume --sandbox xmarket` at a terminal if a halt was recorded)
- [ ] Ctrl-C in the pane: drained ≤ 20 s, exit 0; `doctor --live` now exits 1 (`stopped`); 0 WARN / ERROR for the run in `~/.tengu/logs/tengu.log`

## xlab smoke (`sandboxes/xlab`, ~5 min, no LLM)

Same `$T`. Runbook: top of `sandboxes/xlab/config.toml`; expected numbers: `docs/validation-checklist.md` § 10.

- [ ] `"$T" doctor --sandbox xlab </dev/null` exits 0 (`xl_architect`, `xl_jev`; allow `api.hyperliquid.xyz`, `api.geckoterminal.com`)
- [ ] `"$T" history coverage --sandbox xlab </dev/null` lists 79 instruments (75 xyz stock perps + BTC / ETH / SOL / HYPE), 1h bars from 2026-03-07
- [ ] `"$T" backtest --sandbox xlab --strategy weekend_fade --split time:2026-07-01T00:00:00Z </dev/null` → research `n=1500 mean_net_bps=+50.45`, holdout `n=886 mean_net_bps=+45.44` (data through 2026-10-01), a run dir under `~/.tengu/state/xlab/backtests/`
- [ ] the same + `--gate xl_gate --max-decisions 1500 --offline` → `cache 1500 hits · 0 misses`, `jev − rules +28.93 bps`
- [ ] (live, optional) `"$T" chat --sandbox xlab`: "test a weekend follow placebo on the holdout split" → in-sample runs first, one counted holdout read (`~/.tengu/state/xlab/backtests/holdout-reads.jsonl` gains a line)

## Regression baselines

Fill these in on the first run of Phase 0, then treat them as the diff target for every later phase. Record any intentional change inline as a comment.

### Phase 0 baseline — `cargo run -- chat` `> hello`

```
<!-- paste actual output here -->
```

### Phase 0 baseline — `cargo run -- telegram` + `/agents`

```
<!-- paste actual output here -->
```

### Phase 0 baseline — stderr on clean startup (first 20 lines)

```
<!-- paste actual output here -->
```

## Skill lifecycle smoke

```
SKL-15  tengu skill seed german-teacher ./materials/ → skills/german-teacher/ exists with SKILL.md (frontmatter editable_by_learner=true, learner_facing=true) + resources/<copied files> + evals/prompts.yaml stub.
SKL-16  tengu skill seed against an existing skill name → refuses with clear error.
```

## Rollback procedure

1. `git checkout main` — restore the last known-good state.
2. Re-run this checklist against `main` to confirm it is healthy.
3. Debug the broken phase branch off `main` with a clean slate.
