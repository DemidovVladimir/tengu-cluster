# SESSION_HANDOFF.md — Tengu-Cluster running state log

> Restored 2026-05-14. The file was dropped from the tree during the
> agentic-memory rework branch; `CLAUDE.md` / `AGENTS.md` still list it as
> required reading, so it is back. Keep the top section current.

---

## TL;DR — current state (2026-09-23): hexagonal layout

Branch `refactor/hexagonal` (not merged). Plan + per-phase status: `docs/hexagonal-plan-2026-09-23.md`. Start any "where is / how do I" question at **`docs/code-map.md`** (+ interactive `docs/code-map.html`).

| Area | Change |
|---|---|
| Layout | `src/{domain,ports,config,application,bootstrap}` + `src/adapters/{inbound,outbound}`; `main.rs` is 14 lines. Old `channel_runtime.rs` / `engine_builder.rs` / `types.rs` / `tool_plugin.rs` / `plugins/` / `*_builder.rs` are gone — every doc path was rewritten |
| Enforcement | `tests/layering_lint.rs` (layer rules, 0 exceptions), `tests/code_map.rs` (code map lists every file, graph block current — regen `TENGU_REGEN_CODE_MAP=1 cargo test --test code_map`), `tests/scope_lint.rs` (now actually scans `outbound/tools` + `mcp_client`) |
| Tools | One `ToolEntry` row in `adapters/outbound/tools/mod.rs::catalog()`; opt-in names in `domain/tools.rs::WORKSPACE_TOOLS`. Guide: `docs/tools.md` |
| New ports | `Embedding`, `MemoryService`, `RecallStore` (`ports/memory.rs`), `ToolDirectory` (`ports/tool.rs`); `SecretRegistry` → `domain/secrets.rs` |
| MCP | `[[mcp_servers]]` tools are `{server}__{tool}` and reach plan-step subagents (both engines) + in-process TUI/Telegram agents (see rows below) |

| Open | Detail |
|---|---|
| ~~`[[mcp_servers]]` tools invisible to plan-step subagents~~ fixed 2026-09-23 | `build_subprocess_tool_executor` now advertises the executor's MCP tools (through `tools`); Claude Code engine passes servers to the bridge (`TENGU_BRIDGE_MCP_SERVERS` + forwarded `$VAR`s), bridge registers `McpPlugin`. Names `{server}.{tool}` → `{server}__{tool}` (providers reject `.`). Tests: `mcp_client` fake-server test, `subprocess_executor_advertises_mcp_server_tools`, `claude_code` bridge-config test, `tests/mcp_bridge_external.rs` (real `tengu mcp-bridge` ↔ `tests/fixtures/fake_mcp_server.sh`). Not tested against a live LLM. |
| ~~In-process Claude Code agents (TUI/Telegram) don't see `[[mcp_servers]]` tools~~ fixed 2026-09-23 | `bootstrap::tools::with_mcp_bridge_tools` lists the servers once at agent setup and appends `{server}__{tool}` to the bridge list; `ChatRuntimeService.mcp_servers` / `ChatTurnInputs.mcp_servers` reach `EngineContext`. Test: `in_process_claude_code_agent_gets_mcp_tools_and_servers`. |
| Flaky test `learner_state::tests::save_is_atomic_concurrent` | failed once in a full run 2026-09-23, 20/20 passes on re-run; two threads race `save()` on the same file. Pre-existing, untouched by the rewrite. |
| Webhook agents on `engine = "claude_code"` get no bridge tools | `adapters/inbound/webhooks.rs` builds `EngineContext { bridge_tools: None }` — no tengu tools, no MCP tools. Pre-existing; found 2026-09-23. |

---

## Previous TL;DR (2026-09-18)

Tor-by-default egress, one config per sandbox, deploy/tor = Arti + lyrebird-rs.
Everything below is **uncommitted on `main`** (on top of the 2026-09-12 /
2026-09-16 work, also uncommitted). `cargo check --all-features`, `cargo fmt
--check`, scoped tests, `tests/run_agent_ipc.rs`, `tests/scope_lint.rs` pass.

| Area | Change |
|---|---|
| `[egress]` default = Tor | `EgressConfig.network = "tor" \| "open"` (default `tor`). `resolved()`: tor → `proxy` = `TENGU_TOR_PROXY` or `socks5h://127.0.0.1:9050`, `route_llm_api = true`; open → direct. Explicit values win; `route_llm_api` is now `Option<bool>`. Children receive the *resolved* config via `TENGU_EGRESS`. `warn_if_proxy_unreachable` (parent only, after sandbox resolution). `tengu doctor` prints `network`. |
| Claude Code + Telegram over Tor | `claude_code_profile` no longer refuses `route_llm_api`; the CLI child gets `HTTPS_PROXY`/`HTTP_PROXY` = HTTP CONNECT form of the proxy (`EgressPolicy::http_connect_proxy`, `claude_cli_env`; Arti serves CONNECT on the SOCKS port). `TelegramPipe::build_bot` builds teloxide's reqwest 0.11 client itself (`reqwest011` alias, 2026-09-19). |
| One agent schema | `agents/*.toml` + `src/adapters/agents/` (`AgentSpec`) **deleted**. `AgentConfig` gained `description` (presence = planner-routable), `example_queries`, `tools`, alias `skills` → `skill_packages`; `LimitsConfig.step_timeout_secs` (default 600). `shared_files::routable_agents` feeds `render_registry`; `RagPlanner::new(.., agents, ..)`; `SubprocessRunner::new(sandbox, session, agents)` — fail-fast on unknown agent, per-step `max_tool_rounds` / `step_timeout_secs` (the old spec `max_turns`/`timeout_secs` were never wired — parent always sent 20 / 180s). `run_agent_subprocess` loads the parent config first, then `[agents.<name>]`; `bootstrap::tools::subagent_config` replaces `agent_config_from_spec`; subagents now run with their real `limits` and per-agent `claude_code` profile. `skill_doctor(&config, ..)`. Also fixed: `tools = [...]` on `[agents.*]` used to be silently dropped (no field). |
| Sandboxes | `aura`: `[egress] network = "open"` (Molecule/Privy/Beach block Tor); `[agents.aura]` = planner + DeSci subagent (description, tools, `step_timeout_secs = 720`); new `[agents.researcher]`, `[agents.learning-agent]` (from the deleted specs). `storage-test`: description/tools, `model = "claude-sonnet-4-6"`. `unlimited`: explicit `[egress] network = "tor"`. `config.example.toml` documents `[egress]` + a subagent example. |
| deploy/tor | `deploy/snowflake/` (Go lyrebird + socat, fixed-IP subnet) and `refresh-snowflake-bridges.sh` deleted. `deploy/tor/Dockerfile` = Arti 2.6.0 (`--features http-connect`) + lyrebird-rs built from the BuildKit named context `lyrebird-rs` (`../lyrebird-rs`, `LYREBIRD_RS_SRC` = dir or git URL). `arti.toml`: managed transport (`path = /usr/local/bin/lyrebird`, `run_on_startup = true`, protocols obfs4 + snowflake), Tor Browser bridge lines (7 obfs4 + 2 snowflake from lyrebird-rs `tools/arti-e2e/bridges-*.txt`), `tor-state` volume. `compose.yml`: one `tor` service on `127.0.0.1:9050`, healthcheck = `check.torproject.org` `IsTor:true`. `docker-compose.tor.yml` includes it and sets `TENGU_TOR_PROXY=socks5h://tor:9050`. |
| Makefile | `NETWORK=tor` (default) / `open` selects the compose file set for `up`/`up-memory`/`down`/`logs`/`status`/`doctor`/`clean`/`build`. `tor`, `tor-down`, `tor-logs`, `tor-bridges` (calls lyrebird-rs `bridges.sh`). Removed `up-tor`, `down-tor`, `tor-native`, `tor-native-down`. `LYREBIRD_RS_DIR` (default `../lyrebird-rs`) exported as `LYREBIRD_RS_SRC`. |
| Docker sandbox (2026-09-19) | `make up` had no way to pick a sandbox (always `./config.toml`). Now `SANDBOX=<name>` → `TENGU_CONFIG_FILE=sandboxes/<name>/config.toml`, bind-mounted by compose at `/opt/tengu/config.toml`; `NETWORK` defaults to that file's `[egress] network` (explicit `NETWORK=` still overrides); `check-config` guard on `up`/`up-memory` (a missing file used to make Docker create a `config.toml/` directory); `down --remove-orphans`; `make down-all` (both compose file sets + standalone `make tor`); `make chat` = `run --rm tengu chat` with the same wiring (README's `docker compose run tengu chat --sandbox aura` could never work: missing `./config.toml` becomes a directory, aura is `claude_code`). Open: `install.sh` / `cloud-init.yml` take no sandbox; image has no Claude Code CLI (`aura` can't run in Docker); `~` in sandbox paths = `/root` (not persisted). |
| Telegram over Tor (2026-09-19) | Docker `make up SANDBOX=unlimited` crash-looped: `GetMe` timed out. teloxide 0.10 defaults are 5s connect / 17s total and it does not extend the total for the 10s long poll; `api.telegram.org` over Tor measured 11–15s (sometimes >10s to connect). `TelegramPipe::build_bot` now builds the reqwest 0.11 client itself (`reqwest011` alias in `Cargo.toml`, `telegram` feature; lockfile gains only the dep edge) — proxy + 30s connect / 60s total; open network keeps teloxide defaults. `TELOXIDE_PROXY` no longer used. `sandboxes/unlimited` gained `[telegram] allowed_users` (empty rejected every message). |
| unlimited: LLM off Tor (2026-09-20) | OpenRouter is Cloudflare-fronted -- 403 "Just a moment..." on Tor exit IPs. `sandboxes/unlimited` set `route_llm_api = false` (network stays `tor`): LLM API direct, `http_request`/shell still proxied. Works for NATIVE `chat`/`telegram`. Does NOT work for Docker `make up SANDBOX=unlimited`: the container is on the internal `tor-front` net, so "direct" has no route to OpenRouter (`doctor` `IsTor=true`/`llm api: direct` only tests the tool client, not an LLM call). Docker + this sandbox would need `network = "open"` or a second internet-capable network on the tengu service. |
| Docker/installer | `Dockerfile` no longer copies `agents/`; `.dockerignore` no longer excludes `sandboxes/` (the previous `COPY sandboxes` could not have worked). `install.sh`: `TENGU_NETWORK`, `LYREBIRD_RS_SRC`/`LYREBIRD_RS_REPO`, clones lyrebird-rs, drives `make up NETWORK=…`; `cloud-init.yml` clones lyrebird-rs to `/opt/lyrebird-rs`, systemd uses `make up`/`make down`. |
| `http_request` | Hop-0 egress + `net_hosts` gate moved from `PreparedRequest::from_args` into `execute` (audit record on denial unchanged); `tests/scope_lint.rs` passes again. |
| Repo cleanup | Root `index/features/howto/memory/context.html` deleted (`docs/*.html` are the maintained pages); `Adaptive_AI_Learning_Marketplace_PRD.md` + `tengu/ideas/*` → `docs/ideas/`; `docs/configs/tui-memory-smoke.toml` deleted; empty `memory/`, `scripts/`, `.worktrees/` removed; `.githooks/pre-commit` now `cargo fmt --all --check` (it pointed at a script that did not exist). |
| Docs | README, `docs/configuration.md`, `docs/egress-2026-09-16.md`, `CLAUDE.md`/`AGENTS.md`, `config.example.toml` rewritten by hand; every other doc/diagram/skill/inline comment audited and fixed in place by the 2026-09-18 workflow (11 auditors, 65 files). `docs/architecture-v2.md` deleted (condensed duplicate of `REDESIGN.md`); superseded banners on `docs/architecture.md`, `docs/harness-architecture.md`, comparison/radar pages, every `docs/superpowers/*`. |

### Review findings applied (2026-09-18 workflow: 4 lenses × 3 skeptics, 22 raised, 20 confirmed)

| Finding | Fix |
|---|---|
| `-c/--config` never reached `run-agent` children (they resolve `$TENGU_CONFIG`) | `main` pins `TENGU_CONFIG` to the resolved path right after clap; verified with a live child run |
| `AgentConfig` lost the strict schema `AgentSpec` had | `#[serde(deny_unknown_fields)]` on `AgentConfig` + tests (`max_turns`/`timeout_secs`/`sandbox` rejected, `skills` alias, blank `description`, `step_timeout_secs = 0`) — all three sandboxes, `~/.tengu/config.toml` and `config.example.toml` still load |
| Non-routable blocks (no `description`) could still be dispatched | `run_step` and the child both filter on `description.is_some()`; error lists routable agents |
| Child used raw `toml::from_str` (no env substitution / validation) and swallowed parse errors | `Config::load` with an `error!` log; built-in defaults only when the file is absent |
| Child used `workspace = "~/…"` unexpanded | `expand_tilde` before scopes/memory/engine |
| aura subagent silently moved from OpenRouter (4 tools) to Claude Code with builtin Bash/Write | `[agents.aura.claude_code] builtin_tools_profile = "read_only"` (planner never needed builtins; subagent keeps the old no-shell contract) |
| `tengu skill doctor` could not see sandbox agents | `skill doctor --sandbox <name>` |
| Telegram `set_var(TELOXIDE_PROXY)` per message from tokio workers | `Bot` built once in `TelegramPipe::new` (`build_bot`), cloned per send |
| `http_connect_proxy` broke IPv6 proxy hosts | authority built from `host_str()` (brackets kept) + test |
| Startup probe checked only the first resolved address | tries every address (reqwest semantics) |
| MCP stdio / shells got `NO_PROXY=""` while MCP http exempted loopback | one `LOOPBACK_NO_PROXY` for all proxied children |
| Standalone `tengu mcp-bridge` ignored the operator's `[egress]` (forced Tor) | loads `$TENGU_CONFIG` / `<TENGU_HOME>/config.toml` `[egress]` when `TENGU_EGRESS` is absent |
| `make doctor` always passed `--tor` (fails under `NETWORK=open`) | `--tor` only under `NETWORK=tor` |
| `make tor` + `make up` both published 127.0.0.1:9050 | `deploy/tor/compose.internal.yml` (`ports: !reset []`) merged via the include path list in `docker-compose.tor.yml` |
| Relative `LYREBIRD_RS_SRC` resolved against `deploy/tor/`; `tor-bridges` ignored it | `LYREBIRD_RS_DIR` is the single knob (abspath unless `://`), exported as `LYREBIRD_RS_SRC` |
| cloud-init unit `TimeoutStartSec=120` < Tor bootstrap; ufw/bind comments implied 7080 is reachable under Tor | `TimeoutStartSec=900`; comments scoped to `NETWORK=open` |
| rustfmt failing on the hoisted `http_request` gate; scope_lint regex needs `scope.check_` on one line | formatted; gate kept on one line |
| Deleted `AgentSpec` tests had no successors; new paths untested | `config::` (3), `shared_files::registry_lists_routable_agents_only`, `channel_runtime::subagent_config_merges_workspace_tool_optins_from_tools`, `runner::run_step_fails_fast_on_unknown_agent`, `egress::` (+3) |
| Stale help text / comments (`agents/*.toml`, `agent_config_from_spec`, `tengu_outputs`, `RagStore`, Qdrant, `tengu.toml`, missing spec paths) | swept across `src/` (inline-comment auditor + follow-up); `skills/orchestrator/plan_schema.json` wording; `skills/orchestration-e2e/evals/config.toml` workers gained `description` so the eval can route |

### Verified (2026-09-18)

| Check | Result |
|---|---|
| `make tor` (Arti + lyrebird-rs image, obfs4 managed transport) | container healthy; Arti log `[pt lyrebird] connected`, `guard [… via obfs4 …] is usable` |
| `curl --socks5-hostname 127.0.0.1:9050 https://check.torproject.org/api/ip` | `IsTor:true` |
| `curl -x http://127.0.0.1:9050 …` (HTTP CONNECT — the Claude CLI / teloxide path) | `IsTor:true` |
| `tengu doctor --sandbox unlimited --tor` and `tengu doctor --tor` on a config with no `[egress]` | `network: tor`, `llm api: via proxy`, `tor: IsTor=true`, exit 0 |
| OpenRouter `GET /api/v1/models` and `api.telegram.org` through the proxy | HTTP 200 / 302 |
| `docker compose … config` for `deploy/tor/compose.yml`, base, base + tor override | resolves; `lyrebird-rs` context = `/Users/…/lyrebird-rs`, `TENGU_TOR_PROXY` set on tengu |
| CI gate (final) | `cargo fmt --all --check` clean; `cargo test --all-features` = 375 unit + 4 `run_agent_ipc` + 2 `scope_lint`, 0 failed; `cargo clippy --all-features` = same 40 pre-existing warnings as before this session, none new |
| Strict schema | `tengu doctor --sandbox {aura,storage-test,unlimited}`, `tengu status` on `~/.tengu/config.toml`, `config.example.toml` all load under `deny_unknown_fields` |
| `--config` propagation | child spawned with only `TENGU_CONFIG` (as pinned by the parent) resolves `[agents.foo]` from that file and builds its engine |
| `make -n doctor` / `NETWORK=open` / `LYREBIRD_RS_DIR=../foo` / `LYREBIRD_RS_DIR=https://…` | expand as intended; `bash -n deploy/install.sh` ok |
| Docker | `tengu-cluster:latest` builds (bakes `sandboxes/` + `skills/`, no `agents/`); `tengu-tor:latest` builds; merged compose config: `tor` has no host port inside the project, `tengu` no ports, `TENGU_TOR_PROXY` set |

### Open after this pass

| Item | Note |
|---|---|
| lyrebird-rs on GitHub | **Closed 2026-09-18** — pushed to `DemidovVladimir/lyrebird-rs` at `082ea0254fd057c617fdad86158129d0ec78aaec`; a fresh clone matches the local checkout, and `LYREBIRD_RS_DIR=https://github.com/DemidovVladimir/lyrebird-rs.git` builds `tengu-tor` from the git context (every lyrebird layer a content cache hit against the local build). `install.sh` / `cloud-init.yml` clones now work. |
| Claude Code CLI proxying is env-based | `HTTPS_PROXY` (advisory); network-enforced only under Docker `make up`. |
| Docker `make up` end-to-end | both images build and the merged compose config is right; a full Telegram session over Tor was not exercised in this pass. |
| `sandboxes/unlimited` model | The working tree changed `moonshotai/kimi-k3` → `qwen/qwen3.8-27b` (+ identity name) **before** this session (already modified at session start); the 900 s timeout / 32k cap comments still mention kimi-k3. Confirm or revert that hunk before committing. |
| `skills/orchestration-e2e/evals/config.toml` | Pre-existing: `workspace_tools` lists `http_request` / `memory_search` / `memory_ingest`, which `Config::validate` rejects (`tengu status` on it fails with 5 issues); `tengu eval` loads it through its own path. Also `skill_distill` seeds `evals/config.toml` with the calling agent's engine while `eval_builder::load_eval_config` refuses `claude_code` (auditor finding, not fixed). |
| Skill tier precedence | `shared_files::scan_skill_summaries` (registry) is project-first while `skill_builder::skill_directories` / `view_skill` are managed-first (auditor finding, not fixed). |
| `adapters::outbound::engines::build_planner_engine` | `#[allow(dead_code)]`, only consumer of `[claude_code] timeout_secs`; delete or wire (auditor finding). |
| Local Docker leftovers | `tengu-snowflake:latest` image from the deleted stack is still in the local Docker cache (`docker rmi tengu-snowflake:latest`). |
| Secrets in git history | unchanged from 2026-09-12 — rotate + purge. |

---

## Previous state (2026-09-12)

Audit-and-fix pass over the uncommitted agentic-memory tree (two Workflow
runs: 6 finders + 6 skeptics, then 4 fix clusters + 1 verifier). Everything
below is **uncommitted on `main`**; every feature combo compiles, scoped
tests pass, `cargo fmt --check` is clean.

| Area | Change |
|---|---|
| Scopes | `[default_scopes]` / `[agents.*.scopes]` are now **enforced** (were parsed, never used). `Config::fold_default_scopes` at load; `resolve_tool_scopes` in `build_tool_executor` + MCP bridge via `TENGU_BRIDGE_SCOPES` (`ClaudeCodeEngine::with_scopes`); children get their own workspace in `fs_roots` (`grant_workspace_root`). `check_env_read` honours `"*"`. `sandboxes/aura/config.toml` gained `fs_roots` for `http_request`. |
| agentic_memory | `execute` gates on `env_reads` for `TENGU_MEMORY_DATABASE_URL` (scope_lint passes); wrong-dim embeddings fail-soft on event insert + hybrid recall; `capture` defaults `session_id` / `agent` from `TENGU_SESSION_ID` / `TENGU_AGENT_NAME`; `pg_trgm` dropped from `ensure_schema`; DDL runs once per process (`OnceCell`). |
| Plan hand-off | `AgentIpcInput.plan_state` (per-session, `shared_files::set_active_plan`) is the source of truth; `TENGU_PLAN.md` is a debug artifact + fallback. Fixes the cross-session race for webhooks / Telegram. |
| Planner registry | Write failure of `TENGU_PLANNER_REGISTRY.md` is fail-soft (roster kept in memory). TOOLS section lists MCP server tools again (`<server>__<tool>` since 2026-09-23, enumerated once per `RagPlanner`). |
| Config | `OrchestratorConfig.engine` defaults to `"rag"` and is validated; dead `MemoryConfig` qdrant/backend/vector_size/embedding_provider/ttl_days fields + `[rag]` removed; `AgentConfig.requires` removed; `AgentSpec` is `deny_unknown_fields` + engine validated; `skill_lifecycle.fixture_runner_agent` optional; `TENGU_CONFIG` env honoured (`--config` > `$TENGU_CONFIG` > `<TENGU_HOME>/config.toml`). |
| CLI | `tengu doctor` exits non-zero on engine build failure (Docker HEALTHCHECK). `run-agent` exports `TENGU_AGENT_NAME`. |
| Dead code | `channel_runtime::build_vector_stack`, `VectorStore::delete_older_than`, `src/adapters/rag/**`, `src/application/memory/vector/qdrant.rs`, `__deltest.tmp` removed. `regex-lite` dropped; `rust-version = "1.78"`. |
| Tests | `tests/run_agent_ipc.rs` (Rust) replaces `scripts/test-runner.sh`; `tests/memory_search_tool.rs` (grep-test) and `scripts/phase-0-checks.sh` deleted. CI: single `quality.yml` (fmt, check --all-features, clippy advisory, test). |
| Run docs | README rewritten (real CLI table, features, mixed engines, Postgres memory, webhooks); Makefile `up-memory`/`native-memory` replace qdrant targets; Dockerfile bakes `agents/` + `sandboxes/`, exposes 7080 (webhooks); compose/installer/cloud-init use `postgres-memory` + real GitHub URL; `docs/configuration.md` has the env-var table; `config.example.toml` matches the real schema; root HTML pages de-Qdrant'd (`rag.html` deleted). |
| `unlimited` sandbox (2026-09-13) | `sandboxes/unlimited/config.toml` — direct-agent bench on OpenRouter DeepSeek (`unlimited`=v4-flash default, `pro`=v4-pro, `r1`=r1-0528; Telegram `@pro:` routing). Verified through `tengu run-agent` on all three models — recipe + results in `sandboxes/unlimited/BENCH.md`. |
| `tengu prune --hard` (2026-09-14) | `prune.rs::plan_prune` now takes `PruneOptions` (was 3 positional args). New `--hard` flag: with `--sandbox`, **empties the workspace root entirely** — enumerates every direct child (`.tengu/`, `memory/`, and any arbitrary agent-created dir like `image_payload/`) so the allow-list gap no longer leaves generated folders behind; keeps the root dir itself so it's reusable. Soft prune unchanged (allow-list + `scaffold.project.directories`). Never touches `sandboxes/<name>/config.toml`; does not clear Postgres `agentic_memory` (that's `make clean`). 2 unit tests in `prune.rs`; `docs/configuration.md` Reset section updated. |
| OpenRouter body-read timeout + output cap → config (2026-09-16) | Two `[limits]` knobs now drive the OpenRouter engine (were hardcoded / dead). **`request_timeout_secs`** (default 600) is the reqwest total timeout, body read included — `stream: false` means the body arrives only when generation ends, so slow reasoning models (`kimi-k3`) previously hit the 120s wall as `error decoding response body`. **`max_output_tokens_per_turn`** is now actually sent: set → `max_tokens: <n>`, unset → field **omitted** (model/provider default; no more synthetic `context÷8` send-ceiling). The `context÷8` formula survives only as a budget-reservation estimate when unset (`OpenRouterEngine::max_output_tokens_per_turn`, prompt-budget/TUI only). Body-read errors print the full cause via anyhow `{:#}`. Wiring: `build_openrouter_engine_with_limits(model, ctx, timeout, cap_opt)`; `build_engine` passes `agent_config.limits.{request_timeout_secs,max_output_tokens_per_turn}`; planner/eval use `config::default_request_timeout_secs()` + `None`. Tests: `engine_builder::tests::{body_read_failure_surfaces_source_chain, max_tokens_omitted_when_unset_sent_when_configured, budget_reservation_reflects_configured_cap}`. Note: `engine.run().await` sits outside the `stream_event_timeout_secs` idle loop, so `request_timeout_secs` is the only limit on a non-streaming call, and cancel isn't checked while it waits. |
| `[egress]` — Tor / host allowlist / audit (2026-09-16; **superseded by the 2026-09-18 section above**: Tor is now the default, `deploy/snowflake` + `tor-native`/`up-tor` are gone) | New `src/adapters/outbound/egress.rs`, process-wide policy (`install`; `TENGU_EGRESS` to `run-agent` + `mcp-bridge`). `proxy` (socks5h, reqwest `socks` feature) on the tool client (http_request, crypto), MCP-http, and — with `route_llm_api` — OpenRouter/embeddings/wiki compiler. `allow_hosts`/`deny_hosts`/`https_only` ceiling on every `http_request` hop (redirects now followed manually; credentials dropped cross-origin — previously reqwest auto-followed redirects past `net_hosts`). `run_command`/shell skills: URL-literal guard + proxy env; `shell_network = "isolated"` wraps `sh` in macOS `sandbox-exec` (only the proxy port). Claude Code `editor_shell` → `editor` under a proxy; `route_llm_api` refuses `claude_code`. JSONL audit per hop / network-looking shell command. `build_tool_executor` lost its `shared_http_client` arg (all callers passed `None`). `OpenRouterEngine::new` returns `Result`. `tengu doctor [--sandbox] [--tor]`. Docker: `deploy/tor/compose.yml` = **Arti 2.6.0** (SOCKS + HTTP CONNECT on 9050, internal networks only) **→ Snowflake** (`deploy/snowflake/`, lyrebird 0.8.1 pinned by tag+commit, `socat`-exposed unmanaged PT at 10.213.47.2:9150; `[bridges] enabled = true`, official Tor Browser 15.0.23 lines). `make tor-native` (adds `tor-port` → 127.0.0.1:9050) / `make up-tor` (`docker-compose.tor.yml` includes the stack; tengu on internal `tor-front`) / `make tor-bridges` (signed-bundle refresh). Arti deliberately not embedded (see egress doc). Upkeep: bump Arti + lyrebird pins and bridge lines with Tor Browser releases. Verified against real Tor — `docs/egress-2026-09-16.md`. Open: Linux `isolated` shell (use Docker override); `http_request` sends no `User-Agent`. |
| Secrets | `.env.example` values blanked. **The old values are still in git history (`0266655`, `d9037e3`) — rotate Molecule, Beach, Alchemy and the Qdrant Cloud JWT, then purge history.** |

### Decisions left to the maintainer

| Item | Options |
|---|---|
| Planner agent on `claude_code` (`sandboxes/aura/config.toml` `[agents.aura]`) | Doctrine says OpenRouter for the planner; the sandbox keeps `claude_code` for the subscription. Comment now states the trade-off; value unchanged. |
| `[hub]` config + `HubConfig` | Nothing listens on it (only `tengu status` prints it). Remove the struct, or keep as a placeholder. Container ports now point at the webhook listener. |
| `allowed_users = ['848344935']` in aura sandbox | Personal Telegram id in a tracked file; `${TELEGRAM_ALLOWED_USER}` substitution is available. |
| `build_tool_executor` is sync and drives MCP connect via `futures::executor::block_on`; the TUI calls it outside a tokio context | Latent panic if a sandbox sets `mcp_servers` and runs `tengu chat`. Make it `async` (all other callers already are). |
| `OrchestratorChatPort::run_orchestrator_turn` + `memory/injector.rs` + `MemoryProvider::prefetch` | Dead path (only `run_orchestrator_turn_with_system` is live). Delete in a follow-up. |
| Postgres connection per memory call | DDL is now once per process; connections are still per call. Pool if it shows up in latency. |

---

## Previous state (2026-05-14)

Runtime memory has been **replaced**: Qdrant-RAG → **Open Brain** (Postgres +
pgvector) + **Karpathy LLM Wiki** (compiled Markdown). New work is uncommitted
on `main`. Planner routing also moved off Qdrant — it is now file-backed
(`TENGU_PLANNER_REGISTRY.md` + `TENGU_PLAN.md`). All six implementation-doc
phases have landed, including the Phase 6 cleanup that fully removed the
legacy Qdrant `rag/` module, the `qdrant` cargo feature, and the
`tengu registry` / `tengu memory inspect` CLIs. Smaller gaps remain (below).

**Compile-verified 2026-09-12** (all feature combos, scoped tests). Postgres
smoke tests and the end-to-end turn were NOT run in that pass — see the
verification block.

---

## What landed (agentic-memory rework — uncommitted)

| Area | Change |
|---|---|
| Plugin | `src/adapters/outbound/tools/agentic_memory/mod.rs` — Postgres store + `agentic_memory` tool (`capture`/`recall`/`ingest_source`/`promote`/`compile_wiki`/`lint`) + free fns for planner/runner recall |
| Schema | `memory_events`, `memory_sources`, `memory_chunks`, `memory_promotions` (created under an advisory lock by `ensure_schema`); `vector` extension (`pg_trgm` dropped 2026-09-12 — nothing used it); FTS + HNSW indexes |
| Planner | `orchestrator/planner.rs` — `RagPlanner` de-Qdrant'd: registry loaded from `TENGU_PLANNER_REGISTRY.md`; recall lanes (cross-session / within-session / cross-plan) read Postgres `agentic_memory` under `postgres_memory` |
| Shared files | `orchestrator/shared_files.rs` — generates `TENGU_PLANNER_REGISTRY.md`, writes/reads `TENGU_PLAN.md` (plan state → subagent prompt) |
| Runner | `main.rs` — subagent summaries captured to Postgres (`try_persist_agentic_step_summary`) on both the `compress_and_store` path and the no-call backstop; subagent prompt loads `TENGU_PLAN.md` |
| Wiring | `bootstrap/` — `agentic_memory` registered in `register_catalog`, added to `WORKSPACE_TOOLS` + `compute_base_tools`/`advertised_defs`; `build_orchestrator` no longer gated on `qdrant` |
| Config / build | `config.rs` `valid_workspace_tools` += `agentic_memory`; `Cargo.toml` `tokio-postgres` + `postgres_memory` feature; `docker-compose.yml` `postgres-memory` profile (pgvector/pg16) |
| Docs | `docs/agentic-memory-{prd,implementation,examples}-2026-05-13.md`; architecture / comparison / context-management docs rewritten for the new model |

## What landed (this session — 2026-05-14, finish-code + Phases 4/5/6 + doc reconciliation)

| Area | Change |
|---|---|
| `.gitignore` | Ignore `TENGU_PLANNER_REGISTRY.md`, `TENGU_PLAN.md`, `/.codex/`, `**/.tengu/agentic-memory/raw/` (the compiled `wiki/` stays tracked — human-reviewable per PRD) |
| `agentic_memory/mod.rs` (chunks + recall) | `ingest_source` now embeds chunks (`memory_chunks.embedding` populated, fail-soft text-only fallback); agent-facing `recall` is now hybrid (pgvector → FTS) instead of FTS-only; tool schema gained `session_id`/`agent`/`role`/`reason`/`evidence` properties (the dispatch code already read them); new `env_embedder` / `embed_query` helpers; `insert_chunks` / `PostgresMemoryStore::recall` signatures gained params (one call site + one ignored smoke test updated) |
| `agentic_memory/mod.rs` (Phase 4 wiki compiler) | `compile_wiki` rewritten: runs an LLM over promoted memories → cited Markdown page (`[mem:<kind>/<id>]` inline + a deterministic `## Sources` footer). New module-scope helpers `compile_wiki_prompt` / `render_wiki_page_llm` / `wiki_compiler_model` / `chat_complete` (a minimal direct-OpenRouter chat call, the chat-side sibling of `Embedder`). Fail-soft: no `OPENROUTER_API_KEY` or an API error falls back to the old deterministic bullet dump. Model via `TENGU_WIKI_COMPILER_MODEL` env (default `anthropic/claude-sonnet-4-6`). |
| `metrics.rs` | New `MetricsKind::WikiCompiler` variant (+ `as_str` arm) so the wiki-compiler LLM call emits a `MetricsRecord` like every other LLM/embedding call. |
| `mcp_bridge.rs` + `main.rs` (Phase 5 MCP server) | New `tengu agentic-memory-server` subcommand — a standalone MCP stdio server exposing **only** `agentic_memory` to non-Tengu agents. Refactor: extracted `serve_mcp_stdio` (shared by `run_mcp_bridge` + the new `run_agentic_memory_mcp_server`); `handle_initialize` gained a `server_name` param (`tengu-tools` vs `tengu-agentic-memory`). CLI: new `Commands::AgenticMemoryServer` variant + early-return (stderr-only tracing, JSON-clean stdout) + feature-gated match arms, mirroring `McpBridge` / `RunAgent`. Operator doc: `docs/mcp-bridge.md`. |
| Phase 6 — full Qdrant removal (~10 files) | `adapters/inbound/webhooks.rs` persist repointed from `rag::RagStore` → `agentic_memory::write_step_summary_with_embedding` (the landmine: `webhooks` had an undeclared `qdrant` dep, so `--features webhooks` alone never compiled). `compress_and_store.rs` gutted to just `definition()` (Qdrant plugin/handler/`write_summary` gone). `src/adapters/rag/` orphaned — `pub mod rag` removed from `adapters/mod.rs`. `tengu registry` + `tengu memory inspect` CLIs deleted (`Commands` variants, `RegistryAction`/`MemoryAction` enums, all four command fns, dispatch arms, `try_persist_step_summary` + its call sites). `memory/vector.rs` qdrant `VectorStore` impl orphaned; `bootstrap/` `build_vector_stack_async` is disk-only and `resolve_qdrant_collection` removed. `Cargo.toml`: `qdrant` feature + `qdrant-client` dep gone. `docker-compose.yml`: qdrant service/profile/volume gone. `.env.example` / `config.example.toml`: qdrant blocks replaced with Postgres-memory equivalents. |
| `CLAUDE.md` | Was stale (pre-rework: "RAG = brain", Qdrant, auto-reindex). Rewritten to match `AGENTS.md` substance + new required-reading entry for the agentic-memory docs + corrected stuck-recipe |
| `AGENTS.md` | Fixed find-replace corruption — a blanket `Claude`→`Codex` had mangled code identifiers (`engine = "Codex"`, `Codex-sonnet-4-6`, `--features Codex`). Restored to `claude_code` / `claude-sonnet-4-6` / `--features claude_code`. `CLAUDE.md` + `AGENTS.md` are now twins |
| `docs/SESSION_HANDOFF.md` | Restored (this file) |

---

## Open items

### Phase 4–6 (per `docs/agentic-memory-implementation-2026-05-13.md`)

| Item | State |
|---|---|
| Phase 4 — wiki compiler | **Landed** (2026-05-14). `compile_wiki` LLM-synthesises a cited Markdown page from promoted memories; deterministic fallback when no LLM is available. Not yet exercised end-to-end against a real LLM — see verification block. |
| Phase 5 — MCP surface | **Landed** (2026-05-14). `tengu agentic-memory-server` is a standalone MCP stdio server exposing only `agentic_memory`; non-Tengu agents (ChatGPT/Codex/Claude) wire it into their MCP client config. Not yet exercised against a real external client — see verification block. |
| Phase 6 — migration cleanup | **Landed** (2026-05-14). Full Qdrant removal: `rag/` module orphaned, `qdrant` feature + `qdrant-client` dep gone, `compress_and_store` Qdrant plugin gone, `tengu registry` / `tengu memory inspect` CLIs gone, qdrant `VectorStore` orphaned, webhook persist migrated to `agentic_memory`. **Not compiled** — see verification block. Orphaned files (`src/adapters/rag/*.rs`, `src/application/memory/vector/qdrant.rs`) are still physically present — `git rm` them (the sandbox couldn't unlink). |

### Smaller gaps

| Item | Note |
|---|---|
| `lint` is shallow | Returns counts only — no duplicate / contradiction / stale-page / missing-citation detection (PRD R6). |
| `propose_behavior` | In the implementation-doc Tool API table; not in the operation enum. Deliberately deferred per the PRD "Correction" (MVP = capture/recall/ingest/promote/compile/lint). |
| `[agentic_memory]` config section | Implementation doc "Config Sketch" (enabled / raw_root / wiki_root / recall knobs) is **not** implemented. Plugin uses `TENGU_MEMORY_DATABASE_URL` env + hardcoded `RAW_ROOT`/`WIKI_ROOT` consts. |
| Graph tables | `memory_claims` / `memory_links` are "planned" in the doc; `ensure_schema` does not create them. Promotion → claim/link flow not built. |
| `capture` session scoping | **Closed 2026-09-12** — `capture` falls back to `TENGU_SESSION_ID` / `TENGU_AGENT_NAME` exported by the `run-agent` child. |
| Embedding dim coupling | Schema hardcodes `vector(1536)`; embedder pinned to `DEFAULT_EMBEDDING_MODEL` (`text-embedding-3-small`). Since 2026-09-12 a non-1536 vector warns and degrades to text-only on every path (event insert, hybrid recall, chunks). |
| Chunk embedding throughput | `insert_chunks` embeds chunks sequentially (one API call each). Batching is a follow-up for large sources. |
| Postgres inspect CLI | `tengu memory inspect` was removed with the Qdrant path. No Postgres-native "did the write land?" diagnostic yet — query the DB directly or run the `postgres_*_smoke` tests. |
| Orphaned files | **Closed 2026-09-12** — `git rm`'d. |
| Schema file | `.tengu/agentic-memory/AGENTS.md` (data-layers table) is not created or read by anything yet. |

---

## Phase 6 — completed (compile-verify still required)

Full Qdrant removal landed across ~10 files (see the "what landed" table). Key
notes for whoever runs the compiler next:

- **`rag/` is cleanly isolated** — it was already fully behind
  `#[cfg(feature = "qdrant")]`, so nothing outside it referenced `RagStore`
  except `webhook_builder`, `compress_and_store`, and `main.rs`'s CLI — all
  handled. A post-removal grep for `crate::adapters::rag` / `QdrantVectorStore`
  / `qdrant_client` in compiled code is clean.
- **The landmine, fixed** — `webhook_builder::persist_webhook_output` used
  `rag::RagStore` directly while gated only on `webhooks`, so
  `cargo build --features webhooks` never actually compiled. It now writes via
  `agentic_memory` under `#[cfg(feature = "postgres_memory")]`, with a
  graceful no-op (warn) when that feature is off.
- **Vestigial `MemoryConfig` qdrant fields and the orphaned `rag/` +
  `vector/qdrant.rs` files** — both removed 2026-09-12 (compiler-verified).

---

## Verify before declaring done (run on a machine with cargo + Docker)

```sh
# 1. Build — every feature combo must compile (Phase 6 removed the `qdrant`
#    feature entirely; `webhooks` was the latent-break combo).
cargo build                                        # default (openrouter, telegram)
cargo build --features postgres_memory             # new memory path
cargo build --features webhooks,postgres_memory    # the Phase 6 landmine combo
cargo build --features claude_code,postgres_memory # mixed-engine

# 2. Unit tests (no DB needed)
cargo test --features postgres_memory agentic_memory

# 3. Postgres + pgvector smoke (DB needed)
docker compose --profile postgres-memory up -d postgres-memory
export TENGU_MEMORY_DATABASE_URL=postgres://tengu:tengu@localhost:5432/tengu_memory
cargo test --features postgres_memory postgres_capture_and_recall_smoke -- --ignored
cargo test --features postgres_memory postgres_vector_recall_smoke      -- --ignored

# 4. End-to-end — trace one turn, confirm recall block appears
#    sandbox config needs [memory] within_session_output_top_k = 3 (or similar)
RUST_LOG=tengu=info cargo run --release --features postgres_memory -- chat --sandbox aura

# 5. Wiki compiler (Phase 4) — via an agent opted into tools = ["agentic_memory"]:
#    capture + promote a memory, then run compile_wiki, then inspect the page.
#    agentic_memory(operation="capture", kind="preference", content="...")
#    agentic_memory(operation="promote", target_kind="event", target_id="<id>")
#    agentic_memory(operation="compile_wiki", title="...")
#    -> expect .tengu/agentic-memory/wiki/<slug>.md with inline [mem:...] cites
#       + a ## Sources footer. Tool result reports mode=llm (or mode=fallback
#       if OPENROUTER_API_KEY is unset — that path must still write a page).

# 6. Standalone MCP server (Phase 5) — handshake + tools/list over stdio
printf '%s\n%s\n' \
  '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}' \
  '{"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}' \
  | TENGU_MEMORY_DATABASE_URL=postgres://tengu:tengu@localhost:5432/tengu_memory \
    cargo run --features postgres_memory -- agentic-memory-server
#    -> id=1: result.serverInfo.name == "tengu-agentic-memory"
#    -> id=2: result.tools[0].name == "agentic_memory"
#    (server exits cleanly when stdin closes; build a release binary for real use)
```

Watch for: `agentic_memory: ...` warn lines (embed/connection/LLM fail-soft),
`persisted user message to agentic_memory`, `agentic_memory: backstop wrote
final_text summary to Postgres`, and one `metrics` line with
`kind=wiki_compiler` per `compile_wiki` call. After ingesting a source,
confirm `memory_chunks.embedding` is non-null for at least one row.

---

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
