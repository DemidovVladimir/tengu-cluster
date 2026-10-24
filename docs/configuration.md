# Configuration

One TOML file per sandbox holds the channel settings, `[egress]`, the planner and **every agent** (in-process and subagent alike). Each key — default, meaning, its own load rules — is documented once, in the doc comments of its struct in `src/config/` (§ Root sections); `config.example.toml` shows every section commented. This page: how a config loads, the rules that span sections, environment variables. Code side (who reads each section, how to add a field): `docs/code-map.md` §3–§4.

## Load pipeline

| Step | What | Where |
|---|---|---|
| 1 | `.env` from the cwd or its nearest parent; variables already in the shell win | `cli/mod.rs::run` (`dotenvy`) |
| 2 | Vault `<TENGU_HOME>/secrets.vault` → env (`TENGU_MASTER_PASSWORD` or a prompt on `/dev/tty`: Enter skips it, `</dev/null` does not; a process with no controlling terminal skips it with a warning). An inherited `TENGU_SECRETS_LOADED` means an ancestor `tengu` opened it — no prompt. `mcp-bridge`, `run-agent`, `agentic-memory-server` never prompt | `outbound/secrets.rs` |
| 3 | Base config: `-c/--config` > `$TENGU_CONFIG` > `<TENGU_HOME>/config.toml` (`TENGU_HOME` defaults to `~/.tengu`); a missing file = `Config::default()` (one `main` agent, `openrouter`, `anthropic/claude-sonnet-4.6`); `TENGU_CONFIG` pinned to that path | `cli/mod.rs::run`, `config/paths.rs` |
| 4 | `--sandbox <s>`: `sandboxes/<s>/config.toml` **relative to the cwd** replaces the base config wholesale (the base still loads first — an invalid base fails the command); `TENGU_CONFIG` re-pinned to the absolute sandbox file; its `[egress]` installed; then the effective config's `[keys]` seal-proxy session (`outbound/keys::install`: `strip`, session, `[keys.env]` exports; without `--sandbox` the base config's) | `bootstrap/sandbox.rs::load_sandbox_or` |
| 5 | `Config::load`: read → `${VAR}` substitution (`${UPPER_CASE}` only; an unset var stays literal, with a warn) → TOML parse → validation, all errors at once (+ a hardened sandbox's config-file reach rule, the `[generation]` binding, `[strategy_ranking]`) → warnings logged → `fold_default_scopes` (default scopes into every agent, `~` in `fs_roots`, hardened flag, the sandbox sections tools read — `[a2a]`, `[solana] privy_wallet_id` included) | `config/mod.rs` |
| 6 | Children: `run-agent` gets the sandbox name over IPC and the resolved policy as `TENGU_EGRESS`; `tengu mcp-bridge` loads `TENGU_CONFIG` and runs tools as `[agents.<TENGU_BRIDGE_AGENT>]`; both re-run `keys::install`, reusing the parent's session (same proxy, > 5 min left) | `outbound/subprocess_runner.rs`, `inbound/mcp_bridge.rs`, `cli/run_agent.rs` |

The sandbox name of a config file = `<name>` of `…/sandboxes/<name>/config.toml` however it was loaded (`-c` and `TENGU_CONFIG` included), else `default` — the paper ledger's account owner (`config/sections.rs`).

## Initial setup

```bash
tengu secret init                              # vault + master password
tengu secret set OPENROUTER_API_KEY sk-or-...
tengu secret set TELEGRAM_BOT_TOKEN 123:ABC-..
mkdir -p ~/.tengu && cp config.example.toml ~/.tengu/config.toml
make tor                                       # Tor proxy (default network); or set [egress] network = "open"
tengu chat                                     # or: tengu chat --sandbox lping
```

## Root sections

`Config` is `deny_unknown_fields`: an unknown or misspelled top-level key (`[rsik]`) fails the load. A violated rule fails `Config::load`, all errors listed at once.

| Key | Keys + rules: doc comments (`src/config/`) | Absent = | Unknown keys inside |
|---|---|---|---|
| `[agents.<id>]` | `agent.rs` (`AgentConfig`, `.limits`, `.flow`, `.identity`, `.claude_code`, `.local`, `.codex`; `.prompt_budget`) | validation error (at least one agent) | **error** (direct keys, `.local`, `.codex`); ignored in `.limits`, `.flow`, `.identity`, `.prompt_budget`, `.claude_code`, `.scopes.<tool>` |
| `[orchestrator]` | `agent.rs` (`OrchestratorConfig`) | single-agent dispatch | **error** |
| `[memory]` | `memory.rs` | `enabled = false` | ignored |
| `[telegram]` · `[webhooks]` | `channels.rs` | off | **error** · ignored |
| `[decision_loops.<n>]` | `decision_loop.rs` (+ `docs/decision-loops.md`) | none | **error** |
| `[scaffold]` | `channels.rs` | none | ignored |
| `[claude_code]` · `[[mcp_servers]]` | `agent.rs` (+ `docs/tools.md` § MCP servers) | defaults · none | ignored |
| `[default_scopes.<tool>]` | `domain/scope.rs` (`ToolScope`); § Scopes | the fallback per tool: workspace, any host, any binary, no env var (deny-all when hardened) | ignored |
| `[egress]` | `egress.rs` (+ `docs/egress.md`) | Tor | **error** |
| `[solana]` | `solana.rs` | write tools simulate only | **error** |
| `[keys]` · `[keys.env]` | `keys.rs` (+ `docs/sealed-keys.md`) | seal proxy off (provider keys from env / vault) | **error** |
| `[a2a]` | `a2a.rs` (+ `docs/a2a.md`) | no A2A (the `a2a` tool refuses every call; `tengu a2a serve` needs `[a2a.server]`) | **error** (remotes, `server` too) |
| `[xmarket]` | `xmarket.rs` (calendars, `weekend_fade`) | not an xmarket sandbox | **error** (calendars, `weekend_fade` too) |
| `[risk]` · `[paper]` | `risk.rs` (+ `docs/xmarket-risk-paper.md`) | exec tools refuse (`risk_config_missing`) | **error** (nested tables too) |
| `[rate_limits.<name>]` | `rate_limits.rs` | that name unlimited | **error** |
| `[runtime]` | `runtime.rs` (+ `docs/runtime.md`) | defaults | **error** |
| `[recorder]` | `recorder.rs` | off | **error** |
| `[feeds.<n>]` | `feeds.rs` | no feeds | **error** |
| `[backtest]` | `backtest.rs` (+ `docs/xlab-2026-10-01.md` § 5–9) | no backtests | **error** (costs and `half_spread` too) |
| `[strategy_ranking]` | `strategy_ranking.rs` (+ `docs/ranking.md`) | no rankings (`no_strategy_ranking`) | **error** |
| `[generation]` | `lineage.rs` (+ `docs/lineage.md`) | unbound (no capability / pin check) | **error** |
| `[sources]` | `sources.rs` (+ `docs/source-evidence.md`) | no source registry (`tengu sources` and `source_evidence` refuse: `sources_state_missing`) | **error** (every registry row too) |
| `[soe]` | `soe.rs` (+ `docs/soe-2026-10-08.md` § 11) | not an SOE sandbox (`tengu soe cycle` / `replay` refuse: `soe_config_missing`) | **error** |
| `[studio]` | `studio.rs` (+ `docs/studio.md`) | `control = false` (read-only Studio) | **error** |
| `[skill_lifecycle]` | `skill_lifecycle.rs` | `tengu skill evolve` refuses | ignored |

## Scopes — `[default_scopes.<tool>]`, `[agents.<id>.scopes.<tool>]`

Fields (`fs_roots`, `net_hosts`, `env_reads`, `shell_bins`, `wallets`; empty = none): `domain/scope.rs`.

| Rule | Effect |
|---|---|
| Precedence | an agent's own `scopes.<tool>` replaces `[default_scopes.<tool>]` wholesale (never field-merged); a tool with neither gets the fallback (workspace, any host, any binary, no env var; deny-all in a hardened sandbox); `tengu doctor` lists, per agent, the tools on it |
| Plan steps | `run-agent` (and its bridge) add the step workspace to every configured scope's `fs_roots` — except a deny-all scope (every field empty), which stays a deny |
| Writers | `write_file` refuses `.tengu/`, `.claude/`, `.git/`, `skills/` at any depth and `CLAUDE.md` / `CLAUDE.local.md` / `AGENTS.md` / `.mcp.json`; a hardened sandbox also `MEMORY.md` / `USER.md` / `IDENTITY.md` / `PROFILE.md` / `CONTEXT.md` |
| Per-family needs | `docs/tools.md` § Give it to an agent |

## Hardened sandboxes (`[risk]`, `[soe]` or a `[solana]` signer)

| Rule | Enforced |
|---|---|
| every `claude_code` agent sets `builtin_tools_profile = "none"` (the CLI then runs without settings files, hooks, plugins, skills, CLAUDE.md discovery, auto-memory) | load + `engines/claude_code.rs` |
| no `engine = "codex"` agent (its built-in shell ignores tengu scopes) | load |
| no `[[mcp_servers]]`; no scope grants `shell_bins`; tools without a scope run no shell; no shell skill loads | load + runtime |
| `<TENGU_HOME>/state`, `kill_switch_file` and the config file outside every `fs_roots` and `workspace` (there is no signer key file: Privy signs through the seal proxy) | load |
| `[a2a.server] allow_unauthenticated` refused (set `token_env`) | load (`config/a2a.rs`) |
| a plan step's `compose` only narrows its base agent's tools / skills | `run-agent` |
| `write_file` also refuses the system-prompt files (`MEMORY.md`, `USER.md`, `IDENTITY.md`, `PROFILE.md`, `CONTEXT.md`) | runtime (`AgentConfig::hardened`) |
| `[studio] control = true` refused; Studio never starts or stops a hardened runtime | load + `tengu studio` |

Source: `src/config/hardening.rs` (`requires_hardened_claude_code`).

## Sandboxes

`sandboxes/<name>/config.toml`, run from the repo root. The list, each with its network and purpose: README § Sandboxes. Configs that show a feature — purposes and networks in the README § Sandboxes. `tengu sandbox new` adds more (with a `builder.json`: `docs/builder.md`).

| Sandbox | What its config shows |
|---|---|
| `a2a-lab` | A2A both ways on one machine: `[a2a.server]` (planner + `helper`), the remote `self` (this sandbox's own server) and the routable `liaison` agent holding `a2a`; `network = "open"` (`docs/a2a.md`) |
| `control-loop-lab` | the safe control-loop reference run: one Jev loop, two feeds, `[studio] control = true`; nothing touches money, keys, the shell or the network (`docs/control-loop-lab.md`) |
| `sealed-check` | proves the seal proxy: Jev, loop `check`, `tengu run`, Studio reach OpenRouter only through the `tengu-seal` Worker (`[keys]`); one pure-compute tool, own root `~/tengu-sealed-check` (`docs/sealed-check.md`) |
| `soe` | the SOE weekly cycle: `[sources]` (rows off), `[soe]` with `soe_architect` / `soe_critic` (`claude_code`, built-ins off), the read-only `soe_reader`, `[feeds.soe_week]` (`kind = "job"`), bound to `SOE-G0` (`docs/soe-2026-10-08.md`) |
| `xlab-w2` | `[strategy_ranking]` (contracts `rank.xlab-w2.daily.v1`, `rank.xlab-w2.weekend.v1`, unsealed until G-SR1), `strategy_ranking` on the private `xl_ranker` + its feeds `strategy_ranking_daily` / `_weekend`, unbound (no `[generation]`), `[backtest] keep_runs = 200` (cited runs never pruned) |

Every one and `config.example.toml` must load (`config::risk::tests::every_sandbox_and_the_example_load`).

```bash
tengu chat --sandbox lping
cargo run -- chat --sandbox xlab                          # xlab's agents use engine = "claude_code"
tengu run --sandbox xmarket                               # long-running: feeds, loops, webhooks
tengu backtest --sandbox xlab --strategy weekend_fade --split time:2026-07-01
tengu sources --sandbox soe list                          # the source registry; fetch needs an enabled, reviewed row
```

## Environment variables

| Var | Read by | Default | Purpose |
|---|---|---|---|
| `OPENROUTER_API_KEY` | `engines/mod.rs`, `bootstrap/memory.rs`, `bootstrap/orchestrator.rs`, `outbound/decisions.rs`, `tools/agentic_memory/`, `cli/run_agent.rs`, `inbound/webhooks.rs` | — | OpenRouter chat, embeddings, Jev decisions |
| `OPENROUTER_BASE_URL` | `engines/mod.rs`, `outbound/decisions.rs`, `tools/agentic_memory/` | `https://openrouter.ai/api` | API base |
| `OPENROUTER_REFERER` · `OPENROUTER_TITLE` | `engines/openrouter.rs` | unset | `HTTP-Referer` · `X-OpenRouter-Title` headers |
| `UNSLOTH_API_KEY` | `engines/local.rs` (the default `[agents.<a>.local] api_key_env`) | unset = no auth header | local server key |
| `TELEGRAM_BOT_TOKEN` | `inbound/telegram/` | — (required for `telegram`) | bot token |
| `TENGU_TELEGRAM_ALLOWED_USERS` | `inbound/telegram/` | unset | comma-separated ids merged with `[telegram] allowed_users` |
| `TENGU_HOME` | `config/paths.rs::resolve_tengu_home` | `~/.tengu` (`~/` expanded) | state root: config, vault, logs, `state/` |
| `TENGU_CONFIG` | `config/paths.rs::default_config_path` | `<TENGU_HOME>/config.toml` | config file; `-c` wins; pinned for children by the CLI |
| `TENGU_MASTER_PASSWORD` | `outbound/secrets.rs` | unset (prompt) | vault password; always redacted |
| `TENGU_SECRETS_LOADED` | `outbound/secrets.rs` | set by the first `tengu` that opens the vault | names of vault vars; children never prompt, register them for redaction |
| `TENGU_SESSION_ID` | `bootstrap/orchestrator.rs`, `engines/claude_code.rs`, `tools/agentic_memory/`, `outbound/memory/embedder.rs`, `outbound/egress.rs`, `tools/xm/exec_common.rs` | fresh UUID | session shared by planner + runner |
| `TENGU_AGENT_NAME` · `TENGU_AGENT_IPC` | set by `run-agent` (`cli/run_agent.rs`) · by its parent (`outbound/subprocess_runner.rs`, `=1`); read by `agentic_memory`, the egress audit, `cli/risk.rs`, `bootstrap/sandbox.rs` | unset | agent of a child process · IPC mode (`run-agent` refuses without `=1`); either set ⇒ `tengu risk halt` / `resume` refused |
| `TENGU_AGENT_SKILL_SHA256` | set by `run-agent` (`cli/run_agent.rs`); read by the `soe_*` tools (`tools/soe/mod.rs`) | unset = the tool hashes the agent's skills itself | the step's skill identity (`outbound/soe/runner.rs::skill_sha256`), stamped into SOE proposals / challenges; a Claude Code bridge inherits it |
| `TENGU_MEMORY_DATABASE_URL` | `tools/agentic_memory/` | — (required for `postgres_memory`) | Postgres + pgvector DSN |
| `TENGU_WIKI_COMPILER_MODEL` | `tools/agentic_memory/` | `anthropic/claude-sonnet-4-6` | `compile_wiki` model |
| `TENGU_EGRESS` | `outbound/egress.rs::install` | unset | resolved `[egress]` handed to children (JSON); wins over the child's config |
| `TENGU_TOR_PROXY` | `config/egress.rs::EgressConfig::resolved` | `socks5h://127.0.0.1:9050` | Tor proxy when `[egress].proxy` is unset (`docker-compose.tor.yml`: `socks5h://tor:9050`) |
| `HL_API_URL` | `outbound/hyperliquid/info.rs`, `outbound/backfill/hl.rs` | `https://api.hyperliquid.xyz` | Hyperliquid API base (testnet `https://api.hyperliquid-testnet.xyz`); tools read it only through `env_reads`, operator commands (`history backfill`, `backtest --fetch`) directly |
| `GECKO_API_URL` | `outbound/backfill/gecko.rs` | `https://api.geckoterminal.com/api/v2` | GeckoTerminal base; same rule as `HL_API_URL` |
| `SEC_USER_AGENT` | `outbound/backfill/sec.rs`; a `[sources]` `sec_edgar` row names its own variable (`auth = "user_agent_env:<VAR>"`, `outbound/sources/sec.rs`) | — (required for SEC) | the declared `User-Agent` SEC fair access asks for ("Name email"); a request header only, never stored or logged |
| `SOLANA_RPC_URL` | `outbound/solana/rpc.rs` | `https://api.mainnet-beta.solana.com` | Solana RPC; through `env_reads` only (else the public RPC, silently) |
| `TENGU_RISK_RESUME_SECRET_FILE` | `cli/risk.rs` | unset = no guard | `tengu risk resume` also asks for this file's content; refused when the file is missing, not 0600, over 4 KiB, empty, or inside an agent's `fs_roots` / `workspace` |
| `CHAIN_ID` · `EVM_RPC_URL` | `tools/crypto/helpers.rs` | `1` · `https://ethereum-rpc.publicnode.com` | Privy tools: chain id when a call omits it · JSON-RPC for receipts |
| `PRIVY_APP_ID` · `PRIVY_APP_SECRET` · `PRIVY_WALLET_ID` | `tools/crypto/helpers.rs`; `PRIVY_APP_ID` also `outbound/solana/privy.rs` | — | Privy agentic wallet (through `env_reads`); the Solana write tools' wallet is `[solana] privy_wallet_id`, not `PRIVY_WALLET_ID` |
| `PRIVY_API_URL` | `tools/crypto/helpers.rs`, `outbound/solana/privy.rs` | `https://api.privy.io` (crypto tools) | Privy base URL; `[keys.env] PRIVY_API_URL = "privy"` = the seal proxy (session, no `PRIVY_APP_SECRET`; `strip` it — required). Crypto tools: scope-checked only when set. Solana write tools: required, through `env_reads`, and must be the seal proxy with a session |
| `TENGU_KEYS_PROXY` · `TENGU_KEYS_SESSION_TOKEN` · `TENGU_KEYS_SESSION_EXP_MS` | set by `outbound/keys::install`; read by its children, `keys::auth_header_for` / `header_mode` | unset (no `[keys]`) | seal-proxy origin, session token (redacted: ends in `_TOKEN`), expiry; a child reuses a session with > 5 min left |
| `SSH_AUTH_SOCK` | `outbound/keys/mod.rs` | — | the SSH agent that signs the session request (`[keys] agent_socket` overrides) |
| `TENGU_BRIDGE_*` (`AGENT`, `TOOLS`, `WORKSPACE`, `GRANT_WORKSPACE`, `SCOPES`, `MCP_SERVERS`, `SUMMARY_FILE`, `TRANSCRIPT_FILE`, `MAX_RESULT_CHARS`) | `inbound/mcp_bridge.rs` (names: `outbound/bridge_env.rs`) | set by the Claude Code and Codex engines | engine → bridge contract — `docs/mcp-bridge.md` |
| `TENGU_PERSISTENT_STORE_CHUNK_SIZE` · `_OVERLAP` | forwarded by `engines/claude_code.rs` | unset | read by nothing — the bridge takes `[memory]` from its config |
| `TENGU_TUI_METRICS` | `tui/mod.rs` | off | metrics line as a System bubble |
| `NO_COLOR` | `inbound/eval.rs` | unset | plain eval table |
| `RUST_LOG` | `cli/mod.rs::init_logging` | `tengu=info` (`tengu sandbox`: `tengu=warn`) | log filter, replaces the default whole (`tengu=debug` works); one `metrics` line per LLM call at info. Sinks: TUI → `tengu.log`; telegram / webhooks / run / studio / `a2a serve` → `tengu.log` + stderr; protocol and data commands → stderr; the rest → stdout. Colours only on a terminal; an unopenable `tengu.log` → stderr (TUI: no log) with one line saying why |
| Claude CLI child env | `engines/claude_code.rs` | — | strips a parent Claude Code session's vars (`CLAUDECODE`, `CLAUDE_PID`, `CLAUDE_EFFORT`, `CLAUDE_CODE_*` but auth / provider) and `ANTHROPIC_API_KEY` |
| Codex CLI child env | `engines/codex.rs::child_env` | `[agents.<a>.codex] forward_env` | an EMPTY env + `BASE_ENV` + the `forward_env` names (vault secrets, `.env` keys, the master password and the seal-proxy session never reach it; a value equal to the session token is dropped); never `OPENAI_API_KEY` / `CODEX_API_KEY` (the ChatGPT login bills, never an API key); adds `[egress]` `HTTPS_PROXY` / `HTTP_PROXY` / `NO_PROXY` when the LLM API is proxied |
| `LYREBIRD_RS_DIR` · `NETWORK` · `SANDBOX` | `Makefile` | `../lyrebird-rs` · the config's `[egress] network` · none | Tor image source · compose network · sandbox config to mount |
| `TENGU_FEATURES` · `TENGU_WEBHOOK_PORT` | `docker-compose.yml` | `telegram` · 7080 | image features · host port |
| Tests only | `tests/*.rs`, `outbound/solana/rpc.rs` tests | — | `TENGU_MATRIX_LOCAL_BASE_URL` (local engine-matrix leg), `TENGU_CONFORMANCE_ONLY` / `_VERBOSE`, `TENGU_REGEN_CODE_MAP`, `TENGU_REGEN_SOURCE_EVAL` (the source eval set), `TENGU_REGEN_GOLDEN` (Studio graph goldens), `TENGU_REGEN_SOE_STATE` (the SOE fixture state root), `SOE_CASE` (one synthetic SOE cycle case), `TENGU_CAPTURE_FIXTURES` |

## Feature flags

| Flag | Default | Purpose |
|---|---|---|
| `telegram` | on | Telegram bot channel |
| `studio` | on | the `tengu studio` local server (loopback; `web/studio/` embedded) incl. the builder (`--allow-edit`); `tengu studio graph` and `tengu sandbox compile` work without it |
| `webhooks` | on | `tengu webhooks`; webhook routes and decision-loop escalation under `tengu run` |
| `a2a` | on | the `tengu a2a serve` server; the `a2a` tool and `tengu a2a send` need no feature |
| `postgres_memory` | off | Postgres + pgvector agentic memory, `tengu agentic-memory-server` |
| `openrouter` · `claude_code` · `codex` | off | no-ops: every engine (`openrouter`, `local`, `claude_code`, `codex`) is always built, its CLI spawned only when an agent names it; kept so old build commands still work |

## Validation summary

| Errors (load fails) | Warnings (load continues) |
|---|---|
| unknown top-level key, unknown `[agents.<id>]` key, unknown key in a `deny_unknown_fields` section (§ Root sections) | a `local` agent on the default `context_window` |
| no agent; more than one `default`; engine / flow / profile values outside their lists | a routable agent without `workspace` whose tools write (`write_file`, `manage_skill`, `skill_distill`, `apply_improver_proposal`) |
| `workspace_tools` outside `WORKSPACE_TOOLS`; an unknown `[telegram]` key (`tool_approvals` / `approve_only` were removed) | an agent that reads outside text and holds a side effect (`docs/tools.md` § Untrusted input) |
| `[egress]`, `[keys]`, `[solana]`, hardening, `[xmarket]`, `[risk]` / `[paper]`, `[rate_limits]`, `[runtime]`, `[recorder]`, `[backtest]`, `[generation]`, `[strategy_ranking]`, `[sources]`, `[soe]`, `[studio]`, `[a2a]`, `[feeds]`, `[decision_loops]` rules (each module's load-rule table, § Root sections) | an unset `${VAR}` (stays literal) · an agent holding `a2a` with no `[a2a.remotes.*]` |

## Reset

```bash
tengu prune                              # the shared <TENGU_HOME>/memory/ and logs/ (every sandbox's traces, the audit logs)
tengu prune --sandbox <name>             # only <name>: logs/trace/<name>/, each workspace's memory/, .tengu-attachments/, .tengu/storage/, scaffold project dirs, the vectors.bin of its own [memory] store_path
tengu prune --sandbox <name> --hard      # the same, each workspace root emptied (every child) instead, the root kept
```

| Fact | Detail |
|---|---|
| Never pruned | `<TENGU_HOME>/state` — the xmarket state dirs (`ledger.db`, `runtime.db`, `history/`, `market.db`, `backtests/`, `strategy-rankings/`), the `[sources]` state dir (`sources.db`; with `[soe]` the SOE state root: `operator.toml`, `cycles/`, `replays/`, the state logs) and `solana-writes.db`; the sandbox config, secrets, managed skills |
| `--hard` | needs `--sandbox` (without it: global state only, with a note); the workspaces are the agents' `workspace` dirs — an xmarket workspace loses its `.tengu/observations.db`; does not clear the Postgres `agentic_memory` store — `make clean` does (global, destructive) |
| Logs | `tengu prune` deletes `<TENGU_HOME>/logs/` (`tengu.log`, `egress.jsonl`, `decisions.jsonl`, `risk.jsonl` — `ledger.db` keeps the canonical verdicts —, `maps/`, every sandbox's traces `trace/<sandbox>/`); `--sandbox <s>` deletes only `trace/<s>/` |
| Manual | `rm -rf ~/.tengu` wipes everything, the paper ledgers and `market.db` included |

## Related
- `docs/architecture.md` (canonical) · `docs/code-map.md` §3
- `docs/engine-backends.md` · `docs/skills.md` · `docs/tools.md` · `docs/webhooks.md`
- `docs/runtime.md` · `docs/xmarket-risk-paper.md` · `docs/xlab-2026-10-01.md` · `docs/source-evidence.md`
- `docs/lineage.md` · `docs/ranking.md` · `docs/soe-2026-10-08.md` · `docs/studio.md`
- `docs/sealed-keys.md` · `docs/a2a.md` · `docs/builder.md`
