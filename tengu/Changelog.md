---
tags:
  - project
  - changelog
---

# Changelog — Last 5 Iterations

Development history of [[Overview|Tengu Cluster]], March 2026.

---

## Iteration 8: Builder Pattern Consolidation & Single-Crate Migration (March 19)

- **Single crate** — removed `crates/` directory (tengu-backends, tengu-channels, tengu-core, tengu-optimizer); all code in one `tengu-cluster` crate
- **Builder pattern** — consolidated ~60 files into ~30 via `*_builder.rs` modules:
  - `engine_builder.rs` — OpenRouter engine + streaming + tool loop (was: engine_factory, openrouter, engine_runtime, backend_tooling)
  - `tool_builder.rs` — all platform tools + capability filtering + ToolUseService (was: workspace_tools_catalog, platform_tools_catalog, workspace_tools, tool_use_service, tool_ui)
  - `chat_builder.rs` — chat runtime + commands + flow policy (was: chat_runtime, chat_commands, flow_policy)
  - `memory_builder.rs` — memory types + service + disk store + tool defs (was: memory, memory_service, memory_store, memory_tool_executor)
  - `skill_builder.rs` — registry + source + tool generation + system prompt (was: skill_registry, skill_commands, skill_source, skill_tool_executor, system_prompt)
  - `task_builder.rs` — plan/task creation + DAG validation (was: task, task_store, task_planner)
  - `secret_builder.rs` — encrypted vault (was: secret_store, secret_registry)
  - `telegram_builder.rs` — full Telegram adapter (was: telegram_runtime, telegram_commands, telegram_pipe)
- **Types consolidation** — Engine trait, OrchestratorEvent, Plan/Task state machine, EventBus, MemoryEntry all in `types.rs`
- **Config consolidation** — all config schema in `config.rs`
- **Removed `domain/` and `application/` directories** — everything flattened into `src/adapters/`
- **Simplified engine support** — OpenRouter only (other backends were in deleted crates)
- **Simplified feature flags** — `openrouter` (default), `telegram` (default), `qdrant` (optional)

**Impact:** ~30 source files down from ~60+. Flat, navigable structure with clear builder-pattern responsibility boundaries. No functional changes.

---

## Iteration 7: Event-Bus Orchestration Architecture (March 18)

- **Event-bus architecture** — replaced JoinSet batch execution with reactive event-bus model
- **Domain types** — `OrchestratorEvent` (7 variants), `PlanModification`, `TokenUsage` in `src/adapters/orchestrator_event.rs`
- **LivePlan state machine** — `LiveTask` lifecycle (Pending → Ready → Running → Completed/Failed/Skipped), dependency resolution, plan modifications, cycle detection
- **EventBus channels** — star topology with per-agent mpsc channels, auto-scaled orchestrator inbox
- **Agent workers** — persistent `tokio::spawn` tasks per agent, `AgentTaskExecutor` trait port
- **Event-driven orchestrator** — `run_orchestrator()` with `tokio::select!` event loop, timeout detection, cascade-skip on failure
- **Two-tier data routing** — Tier 1: artifacts + short text verbatim; Tier 2: LLM summarization for long unstructured outputs
- **Dynamic re-planning** — `PlanModificationRequest` handling with max modifications cap, self-referential add guard, cycle detection
- **Unified channel support** — both CLI and Telegram use the same `run_orchestrator()` core
- **TelegramTaskExecutor** — wraps engine + tools with typing indicator, tool observer, cancel flag
- **Storage agent** — JSONL-backed data broker (`src/adapters/storage_agent.rs`) with write/read/query/list/clear
- **Fan-out ensemble** — `FanOutPlan`, `run_fan_out()`, `EnsembleReport` with per-agent stats
- **Commands** — `handle_stop()`, `handle_purge_storage()`, `format_agents_listing()`, `format_cost_report()`
- **RunBudget** — aggregate `max_tokens_per_run` enforcement (default 2M tokens)
- **Debug logging** — comprehensive tracing throughout all new modules

**Impact:** Orchestration is now fully reactive — parallelism emerges from the DAG, no batch boundaries. Both CLI and Telegram share the same core. 318 tests, zero regressions.

---

## Iteration 6: Flat Architecture & abi_encode (March 16–17)

- **Context window update** — Claude Sonnet 4.6 / Opus 4.6 recognized as 1M context (was 200K)
- **`abi_encode` platform tool** — dynamic ABI encoding via `alloy::dyn_abi`, supports all Solidity types
- **IP-NFT skill rewrite** — `ipnft-mint` SKILL.md expanded from 2 steps to full 10-step pipeline including Molecule GraphQL metadata flow (previously hardcoded in `desci_tools.rs`)
- **`desci_tools.rs` removed** — all DeSci logic now in [[Skills]] + platform primitives
- **`api_skill_executor.rs` removed** — replaced by `http_request` primitive
- New platform tools: `http_tool_executor.rs`, `crypto_tool_executor.rs`
- Per-skill Obsidian documentation pages created

**Impact:** Flat module structure. DeSci workflow entirely skill-driven, no domain-specific Rust code.

---

## Iteration 5: DeSci + Privy Integration (March 13–16)

**Commits:** `1bfea88`, `2dfc67d`, `298b023`

- Full [[DeSci]] minting pipeline inlined in `desci_tools.rs` (1100+ lines)
- Privy agentic wallet integration — `/wallet` command in Telegram
- Generic `tool_ui.rs` for shared approval dialogs across [[Channels]]
- Task planner with 380+ lines for [[Orchestrator]] goal decomposition
- New domain types: `capability.rs`, `run_state.rs`, `tool_result.rs`
- Cleaned up prune logic, validated end-to-end

**Impact:** Production-ready DeSci + Privy. Telegram fully integrated.

---

## Iteration 4: Parallel Execution & Orchestration (March 9–13)

**Commits:** `913aede`, `aea8a67`, `5c125d5`, `c97297b`

- **Parallel batch execution** via `JoinSet` in [[Orchestrator]]
- LLM-based task planner with dependency tracking (`resolve_execution_order()`)
- **Inline data passing** between dependent [[Agents]] (not file paths)
- Auto-summarize results into [[Memory]] as `topic_overview`
- **RAG planner recall** — prior overviews injected before planning
- Removed dead code: EventBus, heartbeat
- Added `tengu prune` CLI and `/purge` Telegram command
- Memory architecture validation docs

**Impact:** Orchestrator engine complete. Memory-backed coordination proven.

---

## Iteration 3: Sandboxes & Multi-Agent Cooperation (March 6–9)

**Commits:** `4a05301`, `2fb6d1b`, `09cb86b`

- **Sandboxes framework** — `sandboxes/<name>/config.toml` for domain-specific teams
- Dynamic role-based routing, per-agent [[Tools|tool]] allowlists
- Per-user-per-agent state in Telegram
- `/stop` cancellation support (TUI, Telegram, CLI)
- Docker/cloud-init deployment scripts
- `DEPLOYMENT.md` guide

**Impact:** Full multi-agent sandbox system. Production deployment ready.

---

## Iteration 2: DeSci Minting & File Upload (March 4–6)

**Commits:** `3b44f93`, `a607124`, `93eb824`, `d9037e3`

- Token budget gates (80% warning, 100% hard limit)
- [[DeSci]] minting pipeline: POI registration, IP-NFT minting, file upload
- Inline EVM signing via alloy + `EVM_PRIVATE_KEY`
- Removed external `tools/` directory — all logic native in adapters
- Environment variable handling fixes

**Impact:** DeSci toolkit operational. Transpiler workarounds cleaned up.

---

## Iteration 1: Foundation — Skills & Memory RAG (March 2–4)

**Commits:** `ea29896`, `4691b46`, `d864d81`, `1da2332`, `21ddb18`

- [[Skills]] system introduced — markdown-based agent capabilities
- [[Memory]] RAG subsystem with embedding + cosine similarity
- Qdrant support (feature-gated)
- Secrets vault (AES-256-GCM at `~/.tengu/secrets.vault`)
- JS/TS/Python transpiler (later removed in favor of explicit Rust)
- Security hardening and architecture cleanup

**Impact:** Skill composability and memory-backed agents established.

---

## Summary by Theme

| Theme | Iteration | Status |
|-------|-----------|--------|
| [[Skills]] system | 1 | Production (transpiler removed) |
| [[Memory]] RAG | 1 | Production (metadata filtering validated) |
| [[DeSci]] minting | 2–6 | Production (skill-driven, `desci_tools.rs` removed) |
| Multi-agent sandboxes | 3 | Production (role-based routing) |
| [[Orchestrator|Event-bus orchestration]] | 4, 7, 8 | Production (event-bus, DAG dispatch, builder consolidation) |
| Privy wallets | 5 | Production (Telegram integrated) |
| [[Deployment]] | 3 | Production (Docker + cloud-init) |

## Related

- [[Overview]] — system goals
- [[Architecture]] — how it all fits together
