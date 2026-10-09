# Tengu Cluster

Multi-agent harness in Rust. Single binary. One config file per sandbox (`sandboxes/<name>/config.toml`) holds every agent; a planner LLM picks which `[agents.<name>]` handles each message and subagents run as `tengu run-agent` subprocesses with their own tools. All traffic goes through **Tor by default** (Arti + [lyrebird-rs](https://github.com/DemidovVladimir/lyrebird-rs)); a sandbox opts out with `[egress] network = "open"`. Doctrine: **LLM = heart, Open Brain + Karpathy LLM Wiki = brain, tools = hands** (see `CLAUDE.md`).

| Capability | Entry point | Doc |
|---|---|---|
| Orchestrated chat: planner → `tengu run-agent` subagents | `tengu chat` / `telegram` / `webhooks` | § Orchestration |
| Three engines, every tool under each (schema lint, bridge conformance, live engine matrix; the live `local` legs still to run on the operator's PC) | `[agents.<a>] engine` | § Engines, `docs/engine-backends.md` |
| 50 catalog tools (51 with `postgres_memory`: `agentic_memory`); typed rows cached in `<workspace>/.tengu/observations.db` | `tengu tool list` (hidden) | § Tools, `docs/tools.md` |
| Jev decision loops (System One picks the action, existing tools run it) | `tengu decide`, `tengu run` | `docs/decision-loop-plan-2026-09-24.md` |
| Long-running runtime: feeds, loops, webhook routes, lease, heartbeat, recorder | `tengu run`, `tengu doctor --live` | `docs/runtime-2026-09-30.md` |
| Paper desk: `[risk]` gate inside every order tool, paper ledger, kill switch, exit rules, weekend fade | sandboxes `xmarket`, `xmarket-weekend` | § Paper desk (xmarket) |
| History-first research: `market.db` warehouse (+ SEC EDGAR filing events), strategy specs, deterministic backtests, Jev replayed on history | `tengu history`, `tengu backtest`, sandboxes `xlab` (W1), `xlab-w2` | § History-first research (xlab) |
| Research lineage: `lineage/` registry (one TOML per record), generation W1 frozen + bound by `[generation]`, read-only evidence vault, forward grading | `tengu lineage`, `tengu evidence` | `docs/lineage-2026-10-06.md` |
| Scheduled strategy ranking: sealed contracts, deterministic ranker + publisher, no LLM | `tengu ranking`, tool `strategy_ranking`, `tengu run --sandbox xlab-w2` | `docs/strategy-ranking-automation-2026-10-08.md` |
| Source evidence (SOE O2): SEC EDGAR + EU TED records with provenance in `sources.db`, as-of view without lookahead | `tengu sources`, tool `source_evidence`, sandbox `soe` | `docs/source-evidence-2026-10-08.md` |
| Software Opportunity Engine (O0–O1), offline: scenarios, hard gates, rank keys on a signed private profile | `tengu soe` | `docs/soe-2026-10-08.md` |

## Prerequisites

| Requirement | Notes |
|---|---|
| Rust 1.78+ | `Cargo.lock` is v4; Docker image builds on `rust:1.91` |
| OpenRouter API key | [openrouter.ai](https://openrouter.ai) — planner, subagents, embeddings and the Jev decisions endpoint |
| Docker + `lyrebird-rs` checkout | The Tor proxy (`make tor`) is a container: Arti + lyrebird-rs built from `../lyrebird-rs` (`LYREBIRD_RS_DIR=<dir or git URL>` to override). Skip only if every config sets `[egress] network = "open"` |
| Claude Code CLI (optional) | `claude` installed + authenticated; build with `--features claude_code` |
| Postgres + pgvector (optional) | Agentic memory; build with `--features postgres_memory` |

## Quickstart

```bash
git clone https://github.com/DemidovVladimir/tengu-cluster.git
git clone https://github.com/DemidovVladimir/lyrebird-rs.git         # sibling checkout, built into the Tor proxy image
cd tengu-cluster
cargo build                                            # default features: openrouter + telegram
cargo run -- secret init                               # encrypted vault ~/.tengu/secrets.vault
cargo run -- secret set OPENROUTER_API_KEY sk-or-...
mkdir -p ~/.tengu && cp config.example.toml ~/.tengu/config.toml
make tor                                               # Tor proxy on 127.0.0.1:9050 (waits until IsTor=true)
cargo run -- doctor --tor                              # policy + live Tor-exit check
cargo run -- chat                                      # single-agent TUI, over Tor
cargo run -- chat --sandbox lping                      # orchestrated sandbox (planner + subagents; lping is network = "open")
```

| Fact | Detail |
|---|---|
| Vault prompt | `TENGU_MASTER_PASSWORD=...` skips it; Enter skips it when no secret is needed. It reads `/dev/tty`, so `</dev/null` does not skip it; a process with no controlling terminal (cron, Docker without `-t`) skips it with a warning |
| `.env` | loaded first from the cwd or its nearest parent (`dotenvy`); variables already in the shell win |
| Config resolution | `--sandbox <name>` (`sandboxes/<name>/config.toml` relative to the cwd — run from the repo root; replaces the rest wholesale) > `-c/--config <path>` > `TENGU_CONFIG` > `<TENGU_HOME>/config.toml` (`TENGU_HOME` defaults to `~/.tengu`) |
| No Tor | `[egress] network = "open"` in the config |

### tengu + lyrebird-rs over Tor

`make tor` builds one container — Arti 2.6.0 + your `lyrebird-rs` (managed obfs4/snowflake PT) — and every tengu request (tools, OpenRouter, embeddings, Jev, Claude Code CLI, Telegram) goes through `socks5h://127.0.0.1:9050`, fail-closed — except a `local` engine's server, which is reached directly. Verify the transport before anything else:

```bash
make tor                                   # waits until check.torproject.org reports IsTor=true
cargo run -- doctor --sandbox <name> --tor # → proxy reachable · IsTor=true · exit=<ip> · llm api: via proxy
```

| Symptom | Meaning | Fix |
|---|---|---|
| `doctor --tor` shows `IsTor=true` | Arti + lyrebird-rs transport is up | — |
| proxy port unreachable | `make tor` not running / still bootstrapping (~30s obfs4) | `make tor` then `make tor-logs` |
| LLM call returns `403 "Just a moment…"` (`cZone: openrouter.ai`) | **Transport works** — the request reached the provider; its Cloudflare is challenging the Tor exit IP | see below |

**Cloudflare-fronted providers block Tor exits.** OpenRouter (and Privy) sit behind Cloudflare, which serves a managed challenge to Tor exit IPs — the `403` is an application-layer block on the exit node, not a lyrebird/Arti/tengu fault (`curl --socks5-hostname 127.0.0.1:9050 https://api.ipify.org` still returns `200` + the exit IP). For those, set `[egress] network = "open"` on the sandbox (as `sandboxes/lping` does). Tools and non-Cloudflare traffic still route over Tor cleanly; only the challenged provider needs the opt-out.

## CLI

Flags from `tengu <command> --help`. Global: `-c/--config <path>`. `--sandbox <s>` = `sandboxes/<s>/config.toml`.

| Command | Flags that matter | Does | Feature | Env |
|---|---|---|---|---|
| `chat` | `--sandbox` | Interactive TUI; orchestrated when the config has `[orchestrator]`; with `[decision_loops]` it tails `<TENGU_HOME>/logs/decisions.jsonl` as System bubbles | default | `OPENROUTER_API_KEY` |
| `status` | — (`-c` for another file) | Static snapshot of the base config: runtime profile, agents with engine diagnostics, hub | default | — |
| `doctor` | `--sandbox`, `--tor` (live Tor-exit check), `--live` (the sandbox's `tengu run`: heartbeat ≤ `[runtime] heartbeat_stale_secs`, every required feed live — the healthcheck for a `tengu run` container; the shipped image checks plain `doctor`), `--engines` (one `list_directory` + `read_file` turn per agent on its own engine + model; costs tokens) | Builds every agent's engine, checks the `[egress]` proxy; exits non-zero on any failure | default | `OPENROUTER_API_KEY` (an `openrouter` agent's engine fails to build without it) |
| `telegram` | `--sandbox` | Telegram bot channel; refuses to start without an allow-list | `telegram` (default) | `TELEGRAM_BOT_TOKEN`, `OPENROUTER_API_KEY` |
| `webhooks` | `--sandbox` | `[webhooks.endpoints.<n>]` → `POST /webhooks/<n>` (HMAC `X-Tengu-Signature: sha256=<hex>` or a static `auth_header_env` header); 202 + a one-shot orchestrator turn, or `loop = "<name>"` → that decision loop. Takes the leases `tengu run` takes | `webhooks` | `OPENROUTER_API_KEY` (planner / Jev), each endpoint's `secret_env` / `auth_header_env` |
| `run` | `--sandbox` | The sandbox's long-running process: every `[decision_loops.*]` built once, every `[feeds.*]`, the webhook routes (`webhooks` build + `[webhooks] enabled`); lease `runtime:<s>` (+ `state:<dir>` with `[xmarket]`), heartbeat `<state dir>/run-<s>.json`; SIGINT / SIGTERM drain ≤ `[runtime] shutdown_grace_secs` | default | `OPENROUTER_API_KEY` for Jev loops / LLM agents only |
| `decide` | `--sandbox`, `--loop <name>`, `--event <file \| ->`, `--map <file \| ->` (an execution map: order, event, tighter caps — only narrows the loop; names its loop) | One event through `[decision_loops.<name>]` (Jev picks, tools run); prints the step outcomes; no escalation | default | `OPENROUTER_API_KEY` |
| `history range` | `<key> --from --to` | Recorded observation rows (`[recorder]`) of one key, JSON lines | default | — |
| `history asof` | `<keys>... --at [--max-age-secs]` | Per key, the latest row at or before `--at` | default | — |
| `history backfill` | `--instruments <ids \| @universe>` `--from [--to] [--source hl\|gecko] [--interval 1m\|5m\|15m\|1h\|4h\|1d] [--funding] [--no-bars]` | Fill `<state dir>/market.db` from Hyperliquid (bars, funding) or GeckoTerminal (`<id>@<pool>`); resumes | default | `HL_API_URL`, `GECKO_API_URL` (optional overrides) |
| `history events` | `--instruments <ids \| @universe>` (`hyperliquid:xyz:<TICKER>`) `--from [--to]` | SEC EDGAR filings (8-K, 6-K, 10-Q, 10-K, 20-F, 40-F + amendments) → `market.db` `events` (published at the acceptance time) + `event_coverage`; exit 1 when any row failed | default | `SEC_USER_AGENT` ("Name email", required) |
| `history import-hl-archive` | `--dir` | HL S3 archive asset contexts (`*.csv.lz4`, `*.csv`) → `market.db` `ctx` | default | — |
| `history import-json` | `--file` | `[{instrument, interval, source?, bars?, funding?}]` → `market.db`; nothing written unless every row checks | default | — |
| `history coverage` | `[--instrument]` | What `market.db` holds per instrument / kind / interval | default | — |
| `backtest` | `--strategy <name>` \| `--spec <file.json>`, `--from`, `--to`, `--split time:<t>\|instruments:<ids>`, `--data-through <t>` (rerun on a report's `data_through_ms`), `--format table\|json`, `--fetch` (backfill first), `--gate [<loop>]` (+ the Jev gate arm), `--max-decisions` (500), `--concurrency` (4, 1–16), `--offline` (decision cache only) | Strategy spec on `market.db`, no LLM: research arm (+ `[risk]`-capped arm), optional in-sample / holdout split; writes `<state dir>/backtests/<run id>/` | default | `OPENROUTER_API_KEY` only for `--gate` without `--offline` |
| `ranking run` | `--sandbox`, `[--contract <id>]` (default: the only one), `[--date YYYY-MM-DD]` (default: newest date past its cutoff), `--format table\|json` | One ranking date of a sealed `[strategy_ranking]` contract (freshness → one backtest per strategy → rank) → `<state dir>/strategy-rankings/<contract>/<date>/` + `latest`; lease, resumable; refuses an unsealed contract; exit 1 when `INCOMPLETE`; no LLM, no network | default | — |
| `ranking show` | `--sandbox`, `[--contract]`, `[--date]` | Print a published `ranking.md` (default `latest`) | default | — |
| `evidence` | `snapshot --record <file>` · `verify <record>` · `coverage --history <dir>… --schema` · `grade --ledger` · `evaluate <run dir>` · `regrade --history <dir>…`; `--format text\|json` | Forward evidence: copy into the read-only vault `<TENGU_HOME>/state/evidence/<id>/` (a non-empty `-wal` is refused), re-hash it, recorder coverage, paper-ledger grade, rules · Jev · HOLD on a gated run (`PROVEN` / `UNPROVEN` / `REJECTED`), rule W replayed from recorded rows; no config, no network | default | — |
| `lineage` | `verify [--pins] [--evidence]` · `show` · `trace` · `family` · `attempts` · `report` · `capabilities` · `generation <ID>` · `seal <variant:ID \| experiment:ID \| ranking:ID>`; `--registry` (default `./lineage`), `--format` | The `lineage/` registry: checks, views, pins against a generation's lock; only `seal` writes (appends to `locks.toml`); no config | default | — |
| `soe` | `init` · `check <opportunity.toml>` · `portfolio <dir> --as-of` · `sensitivity` · `eval <cases dir>`; `--profile` (default `<TENGU_HOME>/state/soe/operator.toml`), `--allow-synthetic`, `--format` | Software Opportunity Engine, offline: three scenarios, hard gates, rank keys on the signed private profile (`init` writes the unsigned template, never in a git work tree); no config, secrets, network or LLM | default | — |
| `sources` | `--sandbox`; `list` · `fetch --source <id> [--from --to --ciks]` · `import` · `asof --at [--mode captured\|knowable]` · `purge` · `terms` · `disable` / `enable --reason` | Source layer of a `[sources]` sandbox: the operator fetches SEC EDGAR / EU TED into `<TENGU_HOME>/state/<sources.state>/sources.db` (agents never fetch); as-of evidence packet; retention purge; runtime kill switch; no LLM | default | `SEC_USER_AGENT` (`sec_edgar` fetch) |
| `risk status` | `--sandbox`, `[--account]` | Risk state, cash, positions of every ledger account (read-only) | default | — |
| `risk halt` / `risk resume` | `--sandbox`, `[--account]` | Halt new entries / clear a halt — operator at a terminal only (refused in an agent process or with piped stdin); `resume` asks for the account name, refused while the kill-switch file exists | default | `TENGU_RISK_RESUME_SECRET_FILE` (optional 0600 resume secret) |
| `eval` | `[<skill>...]`, `--sandbox`, `--judge-model` (`anthropic/claude-opus-4-7`), `--concurrency`, `--format table\|json`, `--out`, `--filter`, `--keep-workspace`, `--keep-runs` (10), `--no-persist`, `--max-runs` (10) | Skill evals scored by an LLM judge | default | `OPENROUTER_API_KEY` |
| `secret` | `init\|set\|remove\|list\|change-password\|path` | Encrypted vault `<TENGU_HOME>/secrets.vault` (default `~/.tengu/`) | default | `TENGU_MASTER_PASSWORD` (optional) |
| `prune` | `--sandbox`, `--yes`, `--hard` (needs `--sandbox`) | Remove cached / ephemeral state; never `<TENGU_HOME>/state` beyond `state/flows`, config or secrets | default | — |
| `skill` | `evolve [--max-cycles --target-metric --base-branch --sandbox]` · `metrics [--last]` · `accept-proposal` · `remove [--tier --yes]` · `list [--tier]` · `doctor [--sandbox --no-fail]` · `export [--out]` · `install [--tier --strict --yes]` · `seed [--tier --description --learner-facing --yes]` | Skill lifecycle (`docs/skills.md`) | default | `OPENROUTER_API_KEY` (evolve) |
| `mcp-bridge` | — | MCP stdio server exposing Tengu tools; spawned by the Claude Code engine | default | set by the engine (`TENGU_BRIDGE_*`) |
| `agentic-memory-server` | — | Standalone MCP stdio server exposing `agentic_memory` to external agents | `postgres_memory` | `TENGU_MEMORY_DATABASE_URL` (+ `OPENROUTER_API_KEY` for embeddings; without it FTS / text only) |
| `run-agent` (hidden) | — | Plan-step subprocess of `SubprocessRunner`; refuses to run without `TENGU_AGENT_IPC=1` | — | — |

Hidden test command `tengu tool` (`src/adapters/inbound/cli/tool.rs`; bridge conformance + engine matrix):

| Subcommand | Flags | Does |
|---|---|---|
| `tool list` | — | Every catalog tool name (all opt-ins) as a JSON array |
| `tool call` | `--agent`, `--tool`, `--args <json>`, `--call-id`, `--transcript`, `--batch`, `--sandbox`, `-c` | Run tools as `[agents.<agent>]` through the executor a `run-agent` child builds; prints `{text, observation, is_error}` per call |
| `tool turn` | `--agent`, `--goal`, `--sandbox`, `-c` | One engine turn as any agent (private ones included); prints `{status, output, tools, metrics}` |

### Chat commands

| Command | Description |
|---|---|
| `/help` · `/cost` · `/context` · `/engine` | Info |
| `/eco` / `/standard` / `/precise` | Lens mode |
| `/reset` · `/purge` | Clear conversation (+ wipe persistent memory) |
| `/reload` · `/skills` · `/enable <skill>` · `/disable <skill>` | Re-read env + re-scan skills · list · toggle a skill |
| `/theme` · `/dark` · `/light` | TUI theme |
| `/stop` · `/agents` · `/project <name>` | Telegram: cancel · list agents · project scaffold |

## History-first research (xlab)

Answer strategy questions from backfilled public history, never by waiting for live recording (operator rule 2026-10-01). Sandbox `xlab`: Architect `xl_architect` (tools `market_history`, `backtest`, `read_file`, `list_directory`; skill `xlab-research`; never trades), Jev gate loop `xl_gate`, `[risk]` caps for the capped arm. State `~/.tengu/state/xlab/` (`market.db`, `backtests/<run id>/`), workspace `~/xlab-ws`. Run from the repo root. `xlab` is bound to generation W1 (`[generation]`): editing a pinned section fails the load (`pin … drifted`; a change is a new generation, never an edit of W1). W2 research runs in `xlab-w2` (unbound, same state dir; + `tengu history events`, + the strategy-ranking feeds of `tengu run --sandbox xlab-w2`).

| Step | Command |
|---|---|
| 1. Data | `tengu history backfill --sandbox xlab --instruments @xyz_stocks --interval 1h --from 2026-03-01 --funding` (≈ 1 h for 75 names at xlab's HL budget, 600 / min) |
| | `tengu history backfill --sandbox xlab --instruments @crypto --interval 1h --from 2026-03-01 --funding` |
| | `tengu history coverage --sandbox xlab` |
| 2. Rules | `tengu backtest --sandbox xlab --strategy weekend_fade --split time:2026-07-01` |
| 3. + Jev gate | `tengu backtest --sandbox xlab --strategy weekend_fade_top4 --gate --max-decisions 200` (rerun with `--offline`: same decisions from the cache, nothing called) |
| 4. Architect | `tengu chat --sandbox xlab` — tunes on the in-sample half; a holdout read (`"holdout": true`) is counted in `backtests/holdout-reads.jsonl` |

Spec DSL (six kinds), cost model, engine time integrity, results: `docs/xlab-2026-10-01.md` (§ 10 commands, § 14 results).

## Paper desk (xmarket)

Paper only — no key, no order leaves the process. The `[risk]` gate runs inside every exec tool (`paper_order`, `paper_close`, `xm_exits`, `xm_weekend_fade`), which only a private agent may hold.

| Sandbox | Runs | Runbook |
|---|---|---|
| `xmarket` (M0) | `tengu run --sandbox xmarket`: HL contexts + a starter set of books recorded, exit rules every 15 s, daily risk roll; no planner: `xm_architect` default + routable (read-only, Claude CLI Opus), private `xm_executor` (Claude CLI Sonnet). Entries wait for later tracker items | top of `sandboxes/xmarket/config.toml`; `docs/runtime-2026-09-30.md` § xmarket sandbox |
| `xmarket-weekend` | `tengu run --sandbox xmarket-weekend`: rule W (weekend fade) on 75 Hyperliquid xyz stock perps — capped + shadow ledgers, no LLM, no Jev; bound to W1 (`[generation]`) | top of `sandboxes/xmarket-weekend/config.toml`; `docs/runtime-2026-09-30.md` § Weekend run; `docs/forward-evidence-runbook-2026-10-08.md` |

| Operator action | Command |
|---|---|
| Health | `tengu doctor --sandbox <s> --live` |
| Book | `tengu risk status --sandbox <s>` |
| Halt | `touch <kill_switch_file>` or `tengu risk halt --sandbox <s>` (TTY) |
| Resume | remove the kill-switch file, then `tengu risk resume --sandbox <s>` (TTY; type the account name) |

Gate rules, ledger, exits, rule W: `docs/xmarket-risk-paper-2026-09-30.md`. Plan and backlog: `docs/xmarket-tracker-2026-09-29.md` § 0.

## Orchestration

| Piece | Where |
|---|---|
| Planner agent | `sandboxes/<name>/config.toml` → `[orchestrator] agent = "<planner agent>"`, `engine = "rag"` (historical name; file-registry planner — the only supported value) |
| Planner prompt | `skills/orchestrator/SKILL.md`, read from the cwd (inline fallback when missing) |
| Subagents | Any `[agents.<name>]` block **with a `description`** in the same file (`engine`, `model`, `tools`, `skill_packages`, `example_queries`, `limits.max_tool_rounds`, `limits.step_timeout_secs`, …). Same schema as in-process agents — no separate spec files |
| Registry | `TENGU_PLANNER_REGISTRY.md` regenerated from those blocks + skills + tools each planner turn; the accepted plan reaches subagents over IPC (`TENGU_PLAN.md` is a debug copy; both gitignored) |
| Done signal | `compress_and_store` is appended implicitly to every subagent — never list it in `tools` |

```bash
cargo run -- chat --sandbox lping
cargo run --features claude_code -- chat --sandbox xlab     # sandbox agents use engine = "claude_code"
cargo run -- telegram --sandbox storage-test
```

Adding an agent = add an `[agents.<name>]` block with a `description` and restart. Adding a skill = drop `skills/<name>/SKILL.md` and restart.

### Mixed engines

Planner on OpenRouter, subagents on Claude Code. In the agent block set `engine = "claude_code"` and `model = "claude-sonnet-4-6"` (bare slug); OpenRouter agents use `anthropic/claude-sonnet-4-6`. Per `CLAUDE.md`: **use OpenRouter for the planner agent and Claude Code for subagents** — the Claude Code CLI keeps MCP tool access at engine-construction time, so planner-side tool stripping does not stop it from calling tools instead of emitting plan JSON. Build with `--features claude_code`.

### Agentic memory (Postgres)

```bash
docker compose --profile postgres-memory up -d postgres-memory
export TENGU_MEMORY_DATABASE_URL=postgres://tengu:tengu@localhost:5432/tengu_memory
cargo run --features postgres_memory -- chat --sandbox lping
```

Open Brain lives in Postgres `agentic_memory` (pgvector, 1536-dim `text-embedding-3-small`); the `compile_wiki` operation builds the Karpathy LLM Wiki. To check a write landed, query Postgres directly via `TENGU_MEMORY_DATABASE_URL`. Spec: `docs/agentic-memory-prd-2026-05-13.md`, `-implementation-`, `-examples-`.

### Webhooks

```bash
cargo build --features webhooks
cargo run --features webhooks -- webhooks --sandbox lping
```

`[webhooks.endpoints.<name>]` blocks bind `/webhooks/<name>` to the planner (`agent` is informational) or a decision loop (`loop = "<name>"`). Operator doc: `docs/webhooks-2026-05-11.md`.

## Engines

| Backend | Config | Model slug | Feature |
|---|---|---|---|
| OpenRouter | `engine = "openrouter"` | `anthropic/claude-sonnet-4-6` | always built |
| Local (Unsloth, Ollama, llama.cpp, vLLM, LM Studio — any OpenAI-compatible server) | `engine = "local"` + optional `[agents.<a>.local] base_url`, `api_key_env`; set `limits.context_window` | the server's own id, verbatim | always built |
| Claude Code | `engine = "claude_code"` | `claude-sonnet-4-6` (bare) | `claude_code` |

Claude Code runs agents through the local `claude` CLI; Tengu tools reach it via the MCP bridge as `mcp__tengu-tools__<name>` (`docs/mcp-bridge.md`). Operator rule (2026-09-30): every tool works under all three engines (`docs/engine-backends.md` § Engine matrix).

## Tools

One `catalog()` row per always-on group or opt-in name (`src/adapters/outbound/tools/mod.rs`); opt-in names in `src/domain/tools.rs::WORKSPACE_TOOLS`. An agent's `tools` is its allow-list on every surface.

| Family | Tools | Gate |
|---|---|---|
| workspace | `read_file`, `list_directory`, `write_file`, `run_command` | always |
| http | `http_request` | always |
| crypto (Privy) | `sign_and_send_transaction`, `sign_message`, `get_wallet_address`, `abi_encode`, `hex_to_uint256` | always (`[risk]` sandboxes: signing off) |
| memory | `memory_ingest`, `memory_search` · `persistent_store` · `agentic_memory` | `[memory] enabled` · opt-in · opt-in + `postgres_memory` |
| cache | `shared_cache` | opt-in |
| skills | `skill_resource`, `view_skill` · `manage_skill`, `skill_distill`, `apply_improver_proposal` | always · opt-in |
| solana | reads `sol_price`, `dlmm_pools`, `dlmm_pool`, `dlmm_positions`, `jup_perps`, `solana_wallet`, `solana_tx`, `lp_snapshot`, `lp_swap_plan`, `hedge_decide`, `lp_decide`; writes `solana_close_token_accounts`, `jupiter_swap`, `dlmm_open_position`, `dlmm_close_position`, `jup_perps_order` (simulate by default) | opt-in |
| hyperliquid | `hl_ctx`, `hl_book` | opt-in |
| xm (paper desk) | `risk_status`, `paper_positions`; exec `paper_order`, `paper_close`, `xm_exits`, `xm_weekend_fade` | opt-in; exec tools only on a private agent |
| xlab (research) | `market_history`, `backtest`, `strategy_ranking` (run a sealed contract's ranking date or read the published one) | opt-in; need `[xmarket]` (+ `[backtest]`; `strategy_ranking` + `[strategy_ranking]`) |
| sources | `source_evidence` (as-of evidence packet of `sources.db`; read-only — agents never fetch) | opt-in; needs `[sources]` |

Add one, give it to an agent, scopes, `[[mcp_servers]]`: `docs/tools.md`. Rows, keys, TTLs: `docs/typed-observations-2026-09-24.md`.

## Feature flags

| Flag | Default | Purpose |
|---|---|---|
| `openrouter` | on | marker only — no code is gated on it; the OpenRouter engine is always built |
| `telegram` | on | Telegram bot channel |
| `claude_code` | off | Claude Code CLI backend |
| `postgres_memory` | off | Postgres + pgvector agentic memory, `agentic-memory-server` |
| `webhooks` | off | `tengu webhooks` listener, webhook routes under `tengu run` |

```bash
cargo build --features claude_code,postgres_memory,webhooks
cargo build --all-features
```

## Skills

`skills/<name>/SKILL.md` with YAML frontmatter (`name`, `description`). Scan with shadowing (first wins): `~/.tengu/skills/` → `<workspace>/.tengu/skills/` → `<workspace>/skills/` → the cwd's `skills/`. Load per agent with `skill_packages = [...]` (`skills = [...]` is accepted too). Inventory + lifecycle: `docs/skills.md`.

## Sandboxes

One file per sandbox — channel settings, `[egress]`, the planner and every agent.

| Sandbox | Network | Purpose |
|---|---|---|
| `jev-exec` | `open` | Experiment: a Claude Code architect (built-ins off, only `run_command` → `tengu`) drives Jev through `tengu decide --loop executor` |
| `lping` | `open` (RPC / market APIs, latency) | Crypto research + Solana LP / hedge decision loops `lp_watch`, `hedge_watch`, `hedge_exec`, `lp_exec` over 11 typed Solana reads; 5 write tools simulate only (no signer); planner `lping`, routable `crypto_researcher`, private `lp_executor`; webhooks `helius` → loop, `solana_events` → planner. Plan: `docs/lping-2026-09-24.md` |
| `soe` | `open`, `allow_hosts = ["www.sec.gov", "data.sec.gov", "api.ted.europa.eu"]` | Source layer (SOE O2): `[sources]` registry `sec_edgar`, `ted_search` (both ship `enabled = false` until the operator's reviewed terms); one read-only agent `soe_reader` (`claude_code`, tool `source_evidence` only); unbound — `docs/source-evidence-2026-10-08.md` |
| `storage-test` | tor | `persistent_store` file storage + vector indexing; agent `storage` (`claude_code`, skill `telegram-rag-ingest`) |
| `tor-check` | tor | Minimal probe of the egress path over Tor (Arti + lyrebird-rs) |
| `unlimited` | `open` | Single OpenRouter agent (`qwen/qwen3.8-27b`), no orchestrator. Bench recipe: `sandboxes/unlimited/BENCH.md` |
| `xmarket` | `open`, `allow_hosts = ["api.hyperliquid.xyz"]` | Paper desk stage M0 ($100 `[risk]` budget) — § Paper desk |
| `xmarket-weekend` | `open`, `allow_hosts = ["api.hyperliquid.xyz"]` | Rule W weekend run, floor profile (no LLM, no Jev); bound to W1 — § Paper desk |
| `xlab` | `open`, `allow_hosts = ["api.hyperliquid.xyz", "api.geckoterminal.com"]` | History-first harness (operator PRD v0.5); bound to W1 — § History-first research |
| `xlab-w2` | `open`, `allow_hosts = ["api.hyperliquid.xyz", "api.geckoterminal.com", "www.sec.gov", "data.sec.gov"]` | W2 research: `xlab` without the W1 binding, same state dir; + SEC EDGAR events, + `[strategy_ranking]` contracts `rank.xlab-w2.daily.v1`, `rank.xlab-w2.weekend.v1` (committed unsealed: nothing publishes until `tengu lineage seal ranking:<id>`) run by `[feeds.*]` under `tengu run`, private agent `xl_ranker` — `docs/strategy-ranking-automation-2026-10-08.md` |

The `open` market sandboxes stay switchable to Tor: every transport goes through `egress.rs`.

## Telegram

```bash
cargo run -- secret set TELEGRAM_BOT_TOKEN "123456:ABC-..."
# ~/.tengu/config.toml:  [telegram] enabled = true  allowed_users = ["YOUR_USER_ID"]
cargo run -- telegram --sandbox storage-test
```

`allowed_users` (or `TENGU_TELEGRAM_ALLOWED_USERS`) is required: without it `tengu telegram` refuses to start. `@role: message` targets a specific agent (bypasses the planner unless `route_explicit_agents = true`); private agents (no `description`, not `default`) are never reachable from Telegram. `[telegram] tool_approvals` / `approve_only` are not implemented (load warns).

## Docker

```bash
make setup                                   # .env + config.toml from examples
make tor                                     # Tor proxy only (native tengu): Arti + lyrebird-rs on 127.0.0.1:9050
make up                                      # `tengu telegram` on ./config.toml
make up SANDBOX=unlimited                    # `tengu telegram` on sandboxes/unlimited/config.toml
make up-memory [SANDBOX=<name>]              # + Postgres/pgvector, image rebuilt with postgres_memory
make logs | make status | make doctor | make down | make down-all | make clean | make tor-down | make tor-logs   # pass the same SANDBOX=
make chat SANDBOX=unlimited                  # interactive TUI in a throwaway container (Ctrl-C to quit)
```

Network follows the chosen config's `[egress] network`: Tor (tengu's only exit is the `tor` container) unless it says `"open"`. `NETWORK=tor|open` overrides; a mismatch with the config breaks all traffic.

| Fact | Detail |
|---|---|
| Tor proxy | `deploy/tor/`: Arti 2.6.0 + lyrebird-rs (managed obfs4/snowflake transport). Built from the sibling `../lyrebird-rs` checkout (`LYREBIRD_RS_DIR=<dir or git URL>` to override). `make tor-bridges` prints fresh bridge lines for `deploy/tor/arti.toml` |
| `NETWORK=tor` (default) | `docker-compose.tor.yml` includes `deploy/tor/compose.yml` and puts tengu + Postgres on an internal network whose only exit is the `tor` container (`TENGU_TOR_PROXY=socks5h://tor:9050`); no host ports are published (7080 nor 9050) |
| Baked into the image | `skills/`, `sandboxes/`, `lineage/` under `/opt/tengu` (a `[generation]` sandbox loads `registry = "../../lineage"` at every config load; without it the load fails closed) |
| Config | mounted read-only as `TENGU_CONFIG` (Makefile `TENGU_CONTAINER_CONFIG`): `./config.toml` at `/opt/tengu/config.toml`, or with `SANDBOX=<name>` `sandboxes/<name>/config.toml` at `/opt/tengu/sandboxes/<name>/config.toml` (its own path: the sandbox name and relative `registry` paths resolve); TOML edits apply on container restart |
| Container command | `tengu telegram` (image `CMD`); `tengu run` is not wired into compose yet |
| `engine = "claude_code"` sandboxes (`jev-exec`, `soe`, `storage-test`, `xlab`, `xlab-w2`, `xmarket`) | Not runnable in the image: it has no Claude Code CLI. `doctor` fails → container unhealthy |
| `~` in sandbox paths | Expands to `/root` in the container — not the `tengu-data` volume, so `~/<name>-workspace` is lost on container recreate |
| Port | `7080` = webhook listener only (`TENGU_WEBHOOK_PORT` on the host, `NETWORK=open` only); nothing listens on `[hub].port` |
| Features | `TENGU_FEATURES=openrouter,telegram,postgres_memory docker compose --profile postgres-memory up -d --build` |
| Healthcheck | `tengu doctor` exits non-zero when any agent engine fails to build or the Tor proxy is unreachable — a config with `engine = "claude_code"` agents needs `claude_code` in `TENGU_FEATURES` or the container reports unhealthy |

VPS one-liner: `curl -fsSL https://raw.githubusercontent.com/DemidovVladimir/tengu-cluster/main/deploy/install.sh | bash` (`TENGU_NETWORK=open` to skip Tor, `TENGU_PROFILE=postgres-memory` for memory; clones lyrebird-rs next to the checkout). Cloud-init: `deploy/cloud-init.yml`.

### Systemd (native binary)

```ini
[Service]
WorkingDirectory=/opt/tengu-cluster
Environment=TENGU_MASTER_PASSWORD=your-password
Environment=RUST_LOG=tengu=info
ExecStart=/opt/tengu-cluster/target/release/tengu telegram --sandbox storage-test
Restart=on-failure
```

## Configuration

`config.example.toml` is the commented reference; `docs/configuration.md` has every section's fields, defaults, load rules and the environment-variable table. `Config` is `deny_unknown_fields`: a misspelled top-level key fails the load.

| Section | Purpose |
|---|---|
| `runtime_profile` | `auto` / `cloud` / `desktop` / `minimal` |
| `[agents.<id>]` | engine, model, workspace, `tools` (allow-list), `skill_packages`, `workspace_tools`, `scopes`, limits, identity, `claude_code`, `local`; a `description` (+ `example_queries`) makes it planner-routable |
| `[orchestrator]` | `agent`, `engine = "rag"`, `max_attempts_per_step`, `max_replans`, `route_explicit_agents` |
| `[memory]` | disk vector store, recall knobs, `within_session_output_top_k` |
| `[telegram]` · `[webhooks]` | channels: allow-list · listener + `[webhooks.endpoints.<n>]` |
| `[decision_loops.<n>]` | Jev loop: goal, agent, actions, slots, `world`, `requires`, `dry_run`, `act_at` |
| `[scaffold]` | workspace root, directories and seed files `tengu telegram` creates at start |
| `[claude_code]` | `cli_path`, `timeout_secs` |
| `[default_scopes.<tool>]` | per-tool fallback scope (`fs_roots`, `net_hosts`, `env_reads`, `shell_bins`, `wallets`); an agent's own `scopes.<tool>` replaces it wholesale |
| `[egress]` | `network = "tor"` (default) or `"open"`, proxy, host ceiling, shell isolation, JSONL audit — `docs/egress-2026-09-16.md` |
| `[[mcp_servers]]` | external MCP servers; tools appear as `<server>__<tool>` |
| `[solana]` | `signer_key_file` for the Solana write tools' `mode = "send"`; hardens the sandbox |
| `[xmarket]` (+ `.calendars.<id>`, `.weekend_fade`) | state dir `<TENGU_HOME>/state/<state>`, session calendars, rule W |
| `[risk]` (+ `.exits`, `.max_data_age_ms`) · `[paper]` | every limit of the gate inside the exec tools (all required) · the paper fill engine |
| `[rate_limits.<name>]` | request budgets (`hyperliquid`, `geckoterminal`, `sec`, `ted`) |
| `[runtime]` | `tengu run` knobs: shutdown grace, loop events in flight / queued, heartbeat |
| `[recorder]` | observation history into `<state dir>/history/<YYYYMMDD>.db` |
| `[backtest]` (+ `.costs`, `.universes`, `.strategies`, `.splits`; `notional_usd`, `bootstrap`, `seed`, `gate`, `max_candidates`, `keep_runs`) | xlab: costs per id prefix, universes, the strategy library, share splits, the Jev gate loop, run guards |
| `[feeds.<n>]` | scheduled work of `tengu run`: `kind = "tool"` / `"tick"` on `every_secs` / `windows` / `at` |
| `[generation]` | `id`, `registry` (relative to the config file): binds the sandbox to a frozen lineage generation — every load checks its tools, feeds, loop actions, strategy kinds and `config:` / `spec:` pins (a drifted pin fails it); `xlab`, `xmarket-weekend` = W1 — `docs/lineage-2026-10-06.md` § 4 |
| `[strategy_ranking]` | `registry`, `contracts` (`<registry>/rankings/<id>.toml`): the ranking contracts `tengu ranking` and the `strategy_ranking` tool run (`xlab-w2`) |
| `[sources]` (+ `.registry.<id>`) | `state` (`<TENGU_HOME>/state/<state>/sources.db`) and one row per approved source (`sec_edgar`, `ted_search`): hosts, auth, rate limit, retention, reviewed terms (`soe`) |
| `[skill_lifecycle]` | `tengu skill evolve` (improver agent, cycles) |
| `[hub]` | bind / port / auth — config-only (shown by `tengu status`; nothing listens) |

## Documentation

| Doc | Covers |
|---|---|
| `docs/index.html` | Docs hub |
| `docs/tutorial/index.html` | Visual tutorial: one animated page per feature, built from the code; every code change updates its pages (`docs/tutorial/sources.toml`, `cargo test --test tutorial_map`, `docs/tutorial/AUTHORING.md`) |
| `docs/changes-2026-09-29-to-10-02.html` | What changed 2026-09-29 → 2026-10-02 |
| `docs/xmarket-2026-10-02.html` · `docs/xlab-2026-10-02.html` | xmarket paper desk · xlab history-first harness (visual) |
| `docs/architecture-2026-04-27.md` (+ `.svg`, `.html`) | **Canonical** — seven steps from prompt to reply, file map per subsystem |
| `docs/code-map.md` · `docs/code-map.html` | Where every file lives + how to add tools / engines / config / channels |
| `docs/SESSION_HANDOFF.md` | Running state log, open items, gotchas |
| `docs/configuration.md` · `docs/tools.md` · `docs/skills.md` | Config schema + env vars · tools · skills |
| `docs/engine-backends.md` · `docs/mcp-bridge.md` | Engines + engine matrix · the Claude Code bridge |
| `docs/egress-2026-09-16.md` · `docs/webhooks-2026-05-11.md` | Tor / host allowlist / audit · webhook listener |
| `docs/context-management-2026-04-27.md` (+ `.svg`, `.html`) | Every mechanism that shapes what an LLM sees |
| `docs/agentic-memory-*-2026-05-13.md` | Open Brain + LLM Wiki spec |
| `docs/typed-observations-2026-09-24.md` | Observation envelope, cache, recorder; Solana, Hyperliquid, xm, xlab rows |
| `docs/decision-loop-plan-2026-09-24.md` · `docs/lping-2026-09-24.md` | Jev decision loops · the lping sandbox |
| `docs/runtime-2026-09-30.md` | `tengu run`: lease, feeds, health, state layout, the xmarket sandboxes |
| `docs/xmarket-prd-2026-09-29.md` · `docs/xmarket-tracker-2026-09-29.md` · `docs/xmarket-gaps-2026-09-29.md` | xmarket PRD (+ operator addendum) · tracker (§ 0 rules, W1 notes, backlog) · per-item detail |
| `docs/xmarket-build-plan-2026-09-30.md` · `docs/xmarket-feasibility-2026-09-30.md` | Waves, gates, engine matrix · edge evidence after costs |
| `docs/xmarket-risk-paper-2026-09-30.md` | `[risk]` gate, paper ledger, exits, weekend fade, halts, `tengu risk` |
| `docs/xlab-2026-10-01.md` | xlab: `market.db`, spec DSL, backtest engine, Jev gate arm, tools, results |
| `TENGU_HANDOFF.md` · `TENGU_ROADMAP.md` | Operator intent: W1 → W2 → evolution, gated phases, operator reviews |
| `docs/lineage-2026-10-06.md` | `lineage/` registry, `tengu evidence` (vault, grade, regrade), `tengu lineage`, `[generation]` binding |
| `docs/w1-review-2026-10-06.md` · `docs/p{6,7,8,9,10}-*-2026-10-08.md` | Operator Review #1 (verdict APPROVE) · Phases 6–11 reports (no W2 candidate built; W1 kept) |
| `docs/forward-evidence-runbook-2026-10-08.md` | Rule W forward weekend #2: start, stop, snapshot, grade, regrade |
| `docs/strategy-ranking-automation-2026-10-08.md` | Scheduled strategy ranking: contracts, ranker, publisher, `tengu ranking`, xlab-w2 feeds |
| `docs/soe-2026-10-08.md` · `docs/source-evidence-2026-10-08.md` | Software Opportunity Engine contract + `tengu soe` · the source layer (SEC EDGAR, EU TED, as-of view, `tengu sources`) |

## Module map

Hexagonal, single crate. Every file, extension recipe (tools, engines, config, channels) and dependency: **`docs/code-map.md`** (interactive graph: `docs/code-map.html`). Layer rules are enforced by `tests/layering_lint.rs`.

```
src/main.rs                 14 lines → adapters::inbound::cli::run
src/domain/                 plain data + pure policy: message, session, plan, scope (ToolScope), secrets, tools, metrics, memory,
                            observation, decision, schedule, tz, calendar, backoff, market, book, solana*, marketdata (+ _decode, _stats),
                            canonical (JSON + sha256), evidence (+ _coverage), sec (EDGAR decoders); hl/ (ctx, book), lp/ (DLMM, perps,
                            hedge), xm/ (risk gate, ledger, paper, exits, weekend_fade, cost, grade, regrade), backtest/ (spec, kinds,
                            engine, fills, costs, features, gate, stats, report, checks, evaluation, labels, ranking), lineage/ (records,
                            registry + checks, pins, locks, ranking contracts), source/ (records, as-of view, packet, SEC, TED),
                            soe/ (opportunity, economics, gates, rank, portfolio, profile, eval)
src/ports/                  traits: engine, tool, memory, orchestration, shell, observation, decision, clock, history, market_data,
                            book, paper, runtime, solana_signer, solana_writes, skill_source, tool_activity, evidence, lineage, source_store
src/config/                 schema + validation (mod.rs) + one file per section: egress, decision_loop, solana, hardening, xmarket,
                            risk, rate_limits, runtime, recorder, feeds, backtest, skill_lifecycle, lineage ([generation] + registry
                            loader), strategy_ranking, sources; execution_map (`tengu decide --map`), soe (private profile loader);
                            sections (what tools read), paths
src/application/            use cases: chat/, orchestrator/, memory/, skills/, tools/, decision_loop/ (Jev loop, world, slots),
                            runtime/ (feeds, loops, health), backtest/ (run, run dir, Jev gate arm), ranking/ (coordinator, files),
                            lineage/ (verify, attempts), evidence.rs, sources.rs (as-of packet, purge), observe.rs, paper.rs, metrics.rs
src/bootstrap/              composition root: tools.rs, memory.rs, orchestrator.rs, sandbox.rs, decision.rs, runtime.rs
src/adapters/outbound/      engines/ (openrouter, local, claude_code), tools/ (catalog: workspace, http, crypto, memory, cache, skill*,
                            manage_skill, view_skill, agentic_memory, solana, hyperliquid, xm, xlab, sources), mcp_client/, memory/,
                            hyperliquid/ (info client), solana/ (RPC, send), backfill/ (hl, gecko, hl_archive, json, sec),
                            sources/ (SEC, TED fetchers, sources.db), evidence/ (vault, read-only readers), lineage/ (read-only
                            resolvers), market_data.rs, paper_store.rs, history_sqlite.rs, decision_cache.rs, decisions.rs,
                            observations.rs, runtime_store.rs, rate_limit.rs, http_class.rs, clock.rs, egress.rs, secrets.rs, shell.rs,
                            bridge_env.rs, subprocess_runner.rs, noop.rs, prune.rs, scaffold.rs
src/adapters/inbound/       cli/ (mod + run_agent, doctor, decide, history, backtest, ranking, risk, evidence, lineage, soe, sources,
                            skill, tool), run.rs (`tengu run`), tui/, telegram.rs, webhooks.rs, mcp_bridge.rs, eval.rs, evolve.rs,
                            channel.rs, activity.rs
```
