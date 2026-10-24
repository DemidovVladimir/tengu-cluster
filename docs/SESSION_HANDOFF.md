# SESSION_HANDOFF.md — resume + open gates

Resume here, then **Open gates** (the one place gate state lives), then the last three sessions. Older sessions, verbatim: `docs/archive/handoffs/`. Data that lives only on the operator's Mac: § Local data. Keep it ≤ 40 KB: when a fourth session lands, move the oldest to `docs/archive/handoffs/`.

## Resume here (2026-10-24)

| Where | Value |
|---|---|
| Branch · PR | `fix/audit-bugs` · [#59](https://github.com/DemidovVladimir/tengu-cluster/pull/59) open, not merged (the operator's go) |
| Work list | `TENGU_IMPROVEMENTS.md` § Progress — every row's state and commit hash |
| Worktree | `~/development/tengu-wt-audit`; the main checkout runs `tengu run --sandbox xpower` + 2 `tengu a2a serve` on `feature/xpower-sandbox` — never touch it |
| Next rows | O5 (this file), O7, O9, O10, O11, the O3 svg; then the finish (`handoff_for_check.md`, CI, ask before merging #59) |

## Open gates

| Gate | Whose | State | Next |
|---|---|---|---|
| PR #59 (audit fixes) | operator | open; CI per push | merge go → the "On merge" rows of § 2026-10-23/24 below |
| § 11 decisions | operator | D1, D2, T3, Y7 (→ T8), T6, D4, S4 narrow-only, S14 empty set, W3 `--older-than` | answer each row (`TENGU_IMPROVEMENTS.md` § 11) |
| G-SR1 | operator | `rank.xlab-w2.daily.v1`, `rank.xlab-w2.weekend.v1` unsealed: every ranking refused | `tengu lineage seal ranking:<id>` (or change them first) |
| G-O0 → SOE Review #2 | operator | SOE offline | sign the profile, approve + enable each source row, append SOE-G0's `[[frozen]]` row → 4 frozen weekly cycles → G-R2 (`docs/soe-2026-10-08.md` § 15) |
| Seal proxy (#50) + Privy signer (#51) | operator | Worker not deployed | `npx wrangler deploy`, `CLIENTS` + provider secrets + `SESSION_KEY`, `[keys] strip` (`docs/cloudflare-secrets-setup.html`); then a Privy Solana wallet → `[solana] privy_wallet_id`; the first live send = a separate go (real money) |
| Trading (M3) | operator (weekend runs) | rule W forward tally 2 / 12 (weekend #2: capped +204.0 bps n 4, shadow +108.3 n 75); W1 kept, Jev UNPROVEN | a forward weekend each week; P12 / P13 + trading Review #2 not reached; W2 research in `xlab-w2`, history first |
| xpower | operator | branch `feature/xpower-sandbox` (research paused 2026-10-20) | on merge after #59: README § Sandboxes row + the `sandboxes.html` resolver option (`readme_lists_every_sandbox`), drop `runtime_profile`; its edit of `docs/xlab-2026-10-02.html` lands in `docs/archive/2026-10/` |
| Studio builder gaps | — | decision-loop / feed / `[[mcp_servers]]` cards; import a hand-written sandbox | not started |
| Engine parity | — | live legs not run: `xlab_rank`, `sources`, `soe`, the 11 `codex_*`, `local` (the operator's PC), `agentic_memory` (Postgres), Privy / Solana `send` | run when each is needed |

## 2026-10-23/24: audit fixes — branch `fix/audit-bugs`, PR #59 (open, not merged)

The 2026-10-22 audit (`TENGU_IMPROVEMENTS.md`). Order (operator): bugs → leaks → everything else. Done: A1–A12, R1–R8 (R7 stages 1–3), Y1–Y6, Y8–Y10, B1–B5, B7, W1–W3 (W1 without `tengu paths`, W3 but `--older-than`), S1–S16, T1, T2, T4, T5, T7, T9, D3, D5, D6, O1–O11. Cut on the operator's word (2026-10-24, simplest): B6, B8–B10, `tengu paths`, W4. Waiting: § 11 rows, T8 (on Y7).

| On merge | What changes |
|---|---|
| `~/.tengu/config.toml` | delete line 1 `runtime_profile` (R2) and line 7 `default_lens` (A9) — the base config fails to load otherwise |
| Commands | `tengu webhooks` = `tengu run` with no feeds (`kind = "run"` + heartbeat); `tengu status` gone |
| Config | `[orchestrator]` rejects unknown keys (no `engine`); `[telegram] tool_approvals` / `approve_only` removed (S11); step retries by error class (fatal / decode: none) |
| Docs (O1, O2, O3, O6) | living docs undated (`docs/runtime.md`, `docs/observations.md`, `docs/memory-*.md`, …); 42 shipped docs in `docs/archive/<yyyy-mm>/` (`docs/archive/README.md`); `TENGU_{HANDOFF,ROADMAP,STUDIO_PLAN}.md` → `docs/plans/`; the architecture explorer = walk views only |
| xpower | see Open gates |
| Not retaken | `docs/studio-evidence/` screenshots after B9 (the board header) |

## 2026-10-14 (later): every sandbox LLM agent on the subscriptions — branch `fix/subscription-agents-everywhere`, one PR

| Change | Where | Tests / evidence |
|---|---|---|
| A planner call gets no bridge tools and no `[[mcp_servers]]` (it already got no tools / executor / memory) — a CLI-engine planner that is also the chat agent could call tools | `bootstrap/orchestrator.rs::run_turn_inner` | `planner_turn_gets_no_bridge_tools`; live `a2a-lab` planner (claude-haiku-5-5, `none`) → helper step → "Astana", 6.6 s, no prose fallback |
| OpenRouter LLM agents → `engine = "claude_code"` + `builtin_tools_profile = "none"` | `a2a-lab` (planner, helper, liaison), `lping` (lping, crypto_researcher, lp_executor), `jev-exec` (executor), `control-loop-lab` (lab), `sealed-check` (checker), `tor-check` (probe) | doctor: all engines build; Studio goldens regenerated |
| Lab safety test: claude_code only with profile `none`, never codex | `config/decision_loop.rs` | `control_loop_lab_has_no_dangerous_surface` |

| Item | State |
|---|---|
| Still on OpenRouter | Jev loops (by rule); `xmarket-weekend` (W1-pinned agent model — new generation = operator step; its LLM never runs); `unlimited` (left as is) |
| Needs | `--features claude_code` builds + `claude` login for those six sandboxes (Docker images too) |

## 2026-10-14: findings of the venture-scan research run — branch `fix/subscription-research-and-step-output`, one PR

Operator rule (CLAUDE.md gotcha "Model spend"): frontier models only on the Claude / ChatGPT subscriptions (`claude_code`, `codex`); OpenRouter for Jev alone. The run that found these spent $12 of OpenRouter on Grok / GPT `:online` + a Sonnet planner — the sandbox now runs on the subscriptions only.

| Finding | Fix | Where | Tests |
|---|---|---|---|
| A step's output lost its `compress_and_store` result when the model wrote a short preamble first (Codex: 19,225-char dossier → 181-char answer) | `step_output`: with `compress_and_store`, the longer of text and summary | `adapters/inbound/cli/run_agent.rs` | `step_output_keeps_the_longer_of_text_and_summary` |
| A replan re-ran (and re-paid) every finished step | `DagExecutor::run_reusing` + `ExecResult::NeedsReplan { finished }`: an identical finished step (id, agent, goal, deps, compose) with reused deps keeps its output | `application/orchestrator/{executor,replan}.rs` | `replan_reuses_identical_finished_steps` |
| Codex silent while reasoning → "Engine stream timed out — no data for 120s" | keepalive `ThinkingDelta` every 30 s; CLI killed after max(timeout, 900 s) | `engines/codex.rs` | `web_search_mode_and_reasoning_idle_floor` |
| No web research on the subscriptions (Codex search forced off; Claude Code profiles without WebSearch) | Claude Code profile `research` (+ WebSearch, never WebFetch; one list `config::CLAUDE_CODE_PROFILES`); `[agents.<a>.codex] web_search` = disabled / cached / live; builder field | `engines/{claude_code,codex,mod}.rs`, `config/mod.rs`, `config/builder/` | `profiles_match_config_and_research_has_no_fetch`, `every_field_round_trips…` |
| `http_request` failed on a model's empty optional args (`file_path: "."` → "Is a directory"; `auth_*: ""`) and dropped a file with no field name | empty / blank optional args = absent, `.` ignored, field name defaults to `file` | `tools/http/request.rs` | `unused_optional_args_are_absent` |
| Notes only | ChatGPT-login Codex refuses `gpt-5.5` (use a model the login serves; `--ignore-user-config`); A2A agent endpoints keep the config loaded at start — restart `tengu a2a serve` after an edit | docs | — |

| Item | State |
|---|---|
| `sandboxes/venture-scan` | Claude Code `web_scout` / `social_scout` (`research`) + Codex `critic` (`web_search = "live"`); `[a2a.server] orchestrator = false`; driver, private brief and dossiers in `~/tengu-lab/venture-scan/` (outside the repo). Live-checked: scout WebSearch + `http_request` fetch 45 s, critic live search 15 s, $0 OpenRouter |
| Not changed | other sandboxes' OpenRouter agents (operator decides) |

Older sessions (2026-05-14 → 2026-10-13, incl. the 2026-10-13 review fixes, #50–#53, weekend #2, Studio, A2A, sealed keys, xlab, W1, the hexagonal rewrite and the agentic-memory phases): `docs/archive/handoffs/SESSION_HANDOFF-2026-05-14-to-2026-10-13.md`.

## Local data to clean up later (operator, 2026-10-07)

Data that lives ONLY on the operator's Mac (not in git, the repo is public). Kept on purpose for now. **An agent deletes a group only after the operator says that group is done** — never on its own initiative, never as "cleanup while here". Back up first if the operator asks. When a row is deleted, remove it here in the same commit.

| # | Path | Size (2026-10-09) | What | Delete when | How |
|---|---|---|---|---|---|
| A1 | `~/.tengu/state/evidence/w1-2026-10-06/` | 2.2 GB | W1 weekend evidence vault (read-only); its tree hash is pinned by `lineage/evidence/w1-2026-10-06.toml` (on `main` since #32). Weekend #2's vaults `w1-2026-10-12/` + `w1-2026-10-12-bars/` sit beside it since the 2026-10-12 snapshot | W1 retired (Review #1 APPROVE + P10 kept W1: not yet) | `chmod -R u+w` then `rm -rf`; `tengu evidence verify` then fails for that record — expected |
| A2 | `~/.tengu/state/xmarket-weekend/` | 1.2 GB | weekend paper runs: `ledger.db`, `history/` day files, `run-logs/`, `runtime.db` (the vaults' source); weekend #2 wrote here 2026-10-09 → 10-12 (vaulted + graded 2026-10-12) | with A1 | `rm -rf` |
| A3 | `~/.cache/tengu-xm.noindex/weekend/` | 96 MB | frozen weekend binaries `tengu-6fcb455`, `tengu-desk-2026-10-02` (commit = tag `w1-forward-2026-10-02`, on origin), `tengu-acdef66` (weekend #2, `acdef66fc45252cba181a351cbc237ed6886ad57`; graded 2026-10-12) | with A1 | `rm -rf` |
| B1 | `~/.tengu/state/xlab/` | 535 MB | xlab warehouse `market.db` (+ `-wal` / `-shm`), `backtests/` run dirs, `holdout-reads.jsonl`; shared with `xlab-w2` (its `strategy-rankings/` appear once G-SR1 seals a contract — none yet) | xlab research finished | `rm -rf` (backfill can rebuild `market.db`; run dirs and holdout reads cannot) |
| B2 | `~/.tengu/state/xmarket/` | 646 MB | xmarket paper ledger + recorder history | xmarket work finished | `rm -rf` |
| B3 | `~/.tengu/state/research/` | 8.3 GB | `market-scan-2026-10-05/`, `qnt/`, `sliding/` research data | research finished | `rm -rf` |
| B4 | `~/.cache/tengu-xm.noindex/tengu-xlab-f554db0` | 107 MB | xlab binary (one file) | with B1 | `rm` |
| C1 | main checkout `docs/crypto-opportunity-research-2026-10-05.md`, `docs/crypto-opportunity-deep-dive-2026-10-06.{md,html}`, `docs/software-opportunity-{prd,roadmap,compatibility}-2026-10-04.md` | ~160 KB | private, untracked (never committed — public repo): the two crypto research docs + the three SOE planning docs (private figures; committing sanitized versions is a G-O0 item). Not C1: the untracked `docs/plans/TENGU_STUDIO_PLAN.md` + `docs/ranking.md` — tracked upstream, deleted before the post-weekend pull (§ Weekend no-pull rule) | operator decides: move somewhere private, or drop | `rm` the six files |
| D1 | `~/.tengu/logs/` | 75 MB | `decisions.jsonl`, `egress.jsonl`, `risk.jsonl`, `tengu.log`, `maps/` (execution maps by sha256); `trace/<sandbox>/` appears once a post-#44 binary runs `tengu run` / `decide` on `~/.tengu` | no longer needed for audit | truncate or `rm` the files (the dir is recreated) |
| D2 | `~/lping-workspace/.tengu/`, `~/.tengu/state/solana-writes.db` | 128 KB, 28 KB | lping observation cache, Solana write lease store | lping work finished | `rm -rf` / `rm` |
| F1 | `~/tengu-lab/` | 464 KB | control-loop-lab + Studio: workspace `control-loop-lab/{in,out}` (the lab agent's only fs roots, empty); the lab `TENGU_HOME` `home/` (312 KB: `logs/trace/<sandbox>/<run_id>.jsonl`, `logs/decisions.jsonl`, `logs/maps/`, `state/`) and the Studio validation home `home-validation-2026-10-09/` (152 KB: `logs/` + `state/`; its trace `cbd8cd05-30fb-4177-aa99-0b978f0e83e5` backs `docs/studio-evidence/`) | Studio / lab work finished (after Operator Review #3) | `rm -rf ~/tengu-lab` (only ever lab data — `docs/control-loop-lab.md` reset steps) |
| F2 | `~/.tengu/state/soe/` (+ workspace `~/soe-ws`) | none yet | SOE state root: signed `operator.toml`, `sources.db`, `cycles/`, `replays/`, `reviews/`, `eval/` — private (never in git); created by `tengu soe init` / the first `tengu sources fetch` | never without the operator: it holds the signed profile and the graded cycles of Operator Review #2 | back up first; `rm -rf` |

## Active gotchas

The compiled gotcha list lives in `CLAUDE.md` / `AGENTS.md` ("Key gotchas").
Rework-specific call-outs:

- **`TENGU_PLANNER_REGISTRY.md` / `TENGU_PLAN.md` are generated** — regenerated
  every planner turn / accepted plan. Now gitignored. Don't hand-edit; don't
  commit.
- **`postgres_memory` is off by default** — without it, the planner recall
  lanes compile to `String::new()` and `agentic_memory` is not registered.
  The harness still runs (file registry + in-memory history); it just has no
  durable cross-session memory.
- **`agentic_memory` module is fully feature-gated** — everything in
  `src/adapters/outbound/tools/agentic_memory/` only compiles under `postgres_memory`.
- **`tengu agentic-memory-server` reuses the `mcp-bridge` machinery** — it is
  NOT a Claude-specific protocol; `mcp_bridge.rs` implements standard MCP
  (JSON-RPC 2.0 stdio), it was just originally built for the `claude_code`
  engine. `run_mcp_bridge` and `run_agentic_memory_mcp_server` share
  `serve_mcp_stdio`; the bridge path is byte-identical post-refactor.
- **MCP clients replace the env, not extend it** — when an external client
  spawns `tengu agentic-memory-server`, only the keys in its `env` block are
  visible. Forward `OPENROUTER_API_KEY` alongside `TENGU_MEMORY_DATABASE_URL`
  or `recall`/`ingest_source`/`compile_wiki` silently run in their degraded
  fail-soft modes. Same trap `adapters/outbound/engines/claude_code.rs` documents for the bridge.
