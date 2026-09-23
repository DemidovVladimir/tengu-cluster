# Hexagonal rewrite — plan (2026-09-23)

Supersedes the 2026-09-12 "flat `src/adapters/`" rule. One branch `refactor/hexagonal`, one PR, one commit per phase. Every phase: `cargo check` (default + `--all-features`), `cargo test --bin tengu`, `cargo test --test scope_lint --test layering_lint`, `cargo fmt --check`.

## Dependency rule (enforced by `tests/layering_lint.rs`)

| Layer | May import | Must NOT import |
|---|---|---|
| `domain/` | std, serde, `domain` | everything else in crate; no IO crates (tokio fs/net, reqwest, rusqlite, postgres) |
| `ports/` | `domain`, `config` | `application`, `adapters`, `bootstrap` |
| `config/` | `domain` | `application`, `adapters`, `bootstrap` |
| `application/` | `domain`, `ports`, `config` | `adapters`, `bootstrap` (IO through std is allowed: skill files, git worktrees) |
| `adapters/outbound/` | `domain`, `ports`, `config`, `application` (types only) | `adapters/inbound`, `bootstrap` |
| `adapters/inbound/` | all of the above + `bootstrap` (to obtain wired services) | — |
| `bootstrap/` (composition root) | everything | — |
| `main.rs` | `adapters/inbound/cli`, `bootstrap` | — |

Lint starts with an explicit exception list; the list must be empty before the PR is merged.

## Target tree

```
src/
  main.rs                    thin: clap parse → adapters::inbound::cli::dispatch
  domain/                    message.rs (Role, Message, ToolCall, ToolDef, StreamEvent, Lens, Inbound*) · session.rs (ChatLoopState, PromptAssemblyReport …) · memory.rs (MemoryHit, ChunkMetadata) · plan.rs · scope.rs (ToolScope) · token.rs · usage.rs · metrics.rs (MetricsRecord/Kind/Layer, AggregatorState) · tools.rs (opt-in tool names) · secrets.rs (SecretRegistry)
  ports/                     engine.rs (Engine, EngineContext, ToolExecutor) · tool.rs (Tool, ToolPlugin, ToolCtx, PluginCtx, ToolOutput, ToolDirectory) · memory.rs (MemoryProvider, VectorStore, Embedding, MemoryService, RecallStore) · shell.rs · skill_source.rs · tool_activity.rs · orchestration.rs (Planner, PlannerVerdict, OrchestratorChatPort, WorkerHandle, ChatServiceFactory, TurnTelemetry) · judge.rs (JudgeClient, MetricKind)
  config/                    mod.rs (Config, AgentConfig, LimitsConfig, McpServerConfig …) · egress.rs (EgressConfig + validation) · skill_lifecycle.rs · paths.rs (TENGU_HOME, default config path, expand_tilde)
  application/
    chat/                    tool_loop.rs (collect_engine_response) · service.rs (chat_builder) · flow.rs · prompt_budget.rs
    orchestrator/            planner.rs · replan.rs · executor.rs · retry.rs · events.rs · shared_files.rs · wiring.rs
    memory/                  manager.rs · builtin.rs · injector.rs · fencing.rs · writer.rs
    skills/                  registry.rs (was skill_builder) · lifecycle/ (evolve = pure proposal logic, scanner, metrics, approval_gate …)
    tools/                   registry.rs (ToolRegistry, PluginToolExecutor)
    metrics.rs               global metrics bus (record / install_global_sink)
  adapters/
    inbound/                 activity.rs (tool activity lines) · eval.rs (`tengu eval`) · evolve.rs (`tengu skill evolve` driver) · cli/ (one file per subcommand, out of main.rs) · tui/ · telegram.rs · webhooks.rs · mcp_bridge.rs · run_agent.rs (IPC child)
    outbound/
      engines/               mod.rs (build_engine factory) · openrouter.rs · claude_code.rs
      tools/                 ONE DIR PER TOOL + mod.rs = the tool catalog (see below)
      memory/                builtin.rs · disk_vector.rs · embedder.rs   (Postgres agentic_memory = tools/agentic_memory/)
      mcp_client/            client.rs · protocol.rs · proxy_tool.rs
      tools/args.rs (arg + path helpers) · subprocess_runner.rs · egress.rs · secrets.rs (+ SanitizedToolExecutor) · shell.rs · noop.rs · scaffold.rs · prune.rs
  bootstrap/                 runtime.rs (build_orchestrator, build_tool_executor — was channel_runtime) · wiring.rs
```

## Tools: fix "where do they live / how do I add one"

| Today | After |
|---|---|
| `plugins/` name hides that these are tools | `adapters/outbound/tools/<name>/` |
| Opt-in tool = 3 edits (`register_core_plugins`, `WORKSPACE_TOOLS_ALLOWLIST`, `valid_workspace_tools`) | ✅ 1 `ToolEntry` row in `tools/mod.rs::catalog()` (+ name in `domain/tools.rs` if opt-in, test-enforced); replaced `register_core_plugins`, `compute_base_tools`/`compute_bridge_tools` bodies, `WORKSPACE_TOOLS_ALLOWLIST`, `valid_workspace_tools`. Parity test: identical defs for all 64 opt-in combos × memory on/off |
| ✅ No guide | `docs/tools.md` |
| ✅ `[[mcp_servers]]` absent from examples | Documented block in `config.example.toml` |
| Found: MCP server tools not advertised to plan-step subagents | documented, not fixed (behaviour-neutral rewrite) — `SESSION_HANDOFF.md` |

## Phases

| # | Commit | Main work | Hard part |
|---|---|---|---|
| 0 ✅ | lint + skeleton | branch, empty layer dirs, `tests/layering_lint.rs` with exception list | — |
| 1 ✅ | domain + ports | split `types.rs`, `ports.rs`, `tool_plugin.rs`, `orchestrator/plan.rs`; pull traits out of `planner.rs`, `executor.rs`, `wiring.rs`, `provider.rs`, `vector.rs`, `engine_builder.rs` | `ToolCtx`/`PluginCtx` still hold concrete `MemoryManager` + `SecretRegistry` (lint exceptions; cleared in 4) |
| 2 ✅ | config | `config.rs` → `config/`; `EgressConfig` → `config/egress.rs`, `SkillLifecycleConfig` → `config/skill_lifecycle.rs`, path helpers → `config/paths.rs`, `DEFAULT_EMBEDDING_MODEL` → `domain/memory.rs` | `load_sandbox_or` installs egress → stays in `main.rs` until `bootstrap/` (5) |
| 3 ✅ | outbound adapters + tool catalog | engines, tools, memory stores, mcp client, egress, secrets, shell, runner; metrics record → `domain/metrics.rs`, sink → outbound | catalog replaces the 3-place registration |
| 4 ✅ | application | chat, flow, prompt_budget, orchestrator, memory manager, skills, eval | new ports: `Embedding`, `MemoryService`, `RecallStore` (memory), `ToolDirectory` (tool); `SecretRegistry` → `domain/secrets.rs`; `eval` + evolve driver → `adapters/inbound/` (they compose runtimes); lint EXCEPTIONS empty |
| 5 | inbound + bootstrap | `main.rs` (2.6k lines) → `inbound/cli/*`; `channel_runtime.rs` → `bootstrap/`; telegram/webhooks/tui/mcp_bridge | inbound adapters constructing runtime themselves |
| 6 | zero exceptions + docs | empty the lint exception list; update every doc in CLAUDE.md's "REQUIRED updates" table (arch md/svg/html, context-management, SESSION_HANDOFF, CLAUDE.md + AGENTS.md) + `docs/tools.md` | doc volume (html inline FILE_MAP arrays) |

## Not changing

| Item | Why |
|---|---|
| Behaviour, config TOML schema, CLI flags, IPC JSON | pure restructure; `tests/run_agent_ipc.rs` must pass unchanged |
| Single crate (no workspace split) | crate split is a separate decision; lint gives the boundary now |
| Doctrine (planner strips tools, Tor default, fail-soft memory) | unaffected |

## Risks

| Risk | Mitigation |
|---|---|
| Feature-gated modules (`claude_code`, `postgres_memory`, `webhooks`, `telegram`) break silently | `cargo check --all-features` + default every phase |
| Stale branches (`event-bus-memory-switch`, `feature/phase-b-orchestration-collapse`, …) become unmergeable | accepted — user: ignore them |
| Hard-coded `src/adapters/...` paths in tests/docs/skills | `rg 'src/adapters'` sweep in phase 6 |

## Tooling

| Tool | Use |
|---|---|
| `tests/layering_lint.rs` | dependency rule + `EXCEPTIONS` (must shrink; stale entries fail) |
| path rewrite | a mapping `{modules: {old: new}, symbols: {mod: {Sym: new_mod}}}` applied to `src/**/*.rs`; splits grouped `use crate::m::{A, B}` by destination. No re-export shims left behind. |
