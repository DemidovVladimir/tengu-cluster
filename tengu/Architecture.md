---
tags:
  - core
  - architecture
---

# Architecture

Tengu Cluster uses a **flat module structure** — everything lives under `src/adapters/` alongside `src/main.rs`.

## Why Flat

The system is a [[Overview|multi-agent runtime]] that must support:
- Multiple [[Channels]] without business logic changes
- Multiple LLM backends without code changes
- [[Skills|Plug-and-play skills]] without recompilation
- [[Memory]] backends swappable via feature flags

A flat structure makes it easy to navigate, modify, and reason about the codebase — every file is a sibling in `src/adapters/`.

## Structure

All source files live in `src/adapters/` (single crate, no sub-crates):

| Category | Files | Purpose |
|----------|-------|---------|
| **Types & config** | `types.rs`, `config.rs`, `ports.rs`, `token.rs` | Engine trait, Message, ToolCall, OrchestratorEvent, Plan/Task state machine, EventBus, MemoryEntry, config schema, port traits |
| **Builders** | `engine_builder.rs`, `tool_builder.rs`, `chat_builder.rs`, `memory_builder.rs`, `skill_builder.rs`, `task_builder.rs`, `agent_builder.rs`, `secret_builder.rs`, `flow_builder.rs` | Factory + configuration modules that compose subsystems |
| **Orchestration** | `event_orchestrator.rs`, `orchestrator.rs` | Event-bus core (event loop, dispatch, data routing, RunBudget) and CLI wiring |
| **Tool executors** | `composite_tool_executor.rs`, `http_tool_executor.rs`, `crypto_tool_executor.rs`, `cache_tool_executor.rs` | Tool execution adapters |
| **Channels** | `tui/`, `telegram_builder.rs`, `channel_runtime.rs` | User interfaces and shared channel logic |
| **Storage** | `qdrant_memory_store.rs`, `embedding.rs` | Qdrant adapter (feature-gated), OpenRouter embedding |
| **Infra** | `scaffold.rs`, `prune.rs`, `shell_executor.rs`, `approval.rs`, `prompt_budget.rs`, `usage.rs` | Infrastructure utilities |

## Related

- [[Principles]] — DRY, KISS, Rust best practices
- [[Overview]] — system concept map
- [[Capabilities]] — how permissions flow through the architecture
