---
tags:
  - core
  - architecture
---

# Architecture

Tengu Cluster uses **hexagonal architecture** as a **mandatory** requirement. This is not optional.

## Why Hexagonal

The system is a [[Overview|multi-agent runtime]] that must support:
- Multiple [[Channels]] without business logic changes
- Multiple LLM backends without domain changes
- [[Skills|Plug-and-play skills]] without recompilation
- [[Memory]] backends swappable via feature flags

Hexagonal architecture makes all of this possible through strict dependency inversion.

## Layers

### 1. Domain (`src/domain/`)
Pure business rules and invariants. No I/O, no network, no filesystem.

| File | Purpose |
|------|---------|
| `chat.rs` | Chat state machine, flow keys, history limits |
| `usage.rs` | Token usage accounting |
| `tool_policy.rs` | Tool risk-level and approval policies |
| `skill.rs` | Skill parsing, validation, rendering |
| `agent_role.rs` | Dynamic role wrapper (any string) |
| `task.rs` | Task lifecycle (status machine, retry) |
| `memory.rs` | Memory entry types, cosine similarity, budgeting |
| `capability.rs` | CapabilityId, EffectClass, capability filtering |
| `secret_registry.rs` | Secret redaction (pure, no I/O) |
| `tool_result.rs` | Structured tool output format |

### 2. Application (`src/application/`)
Use-case orchestration. Depends on domain + ports only.

| File | Purpose |
|------|---------|
| `ports.rs` | All port traits (FlowStore, Embedding, MemoryStore, etc.) |
| `chat_runtime.rs` | Per-turn orchestration, token budget gates |
| `engine_runtime.rs` | [[Tools|Tool]] loop (MAX_TOOL_ROUNDS=15) |
| `tool_use_service.rs` | Approval gate + activity publishing |
| `task_planner.rs` | LLM-based goal decomposition for [[Orchestrator]] |
| `memory_service.rs` | [[Memory]] embed -> store -> recall |
| `skill_catalog.rs` | [[Skills]] loading and per-agent filtering |
| `workspace_tools_catalog.rs` | Workspace [[Tools|primitives]] (read_file, write_file, etc.) |
| `platform_tools_catalog.rs` | Platform [[Tools|primitives]] (http_request, crypto signing) |

### 3. Ports (`src/application/ports.rs`)
Stable contracts between application and adapters:
- `FlowStorePort`, `ToolActivityPort`, `ToolApprovalPort`
- `ToolExecutionPort`, `SkillSourcePort`, `ShellExecutionPort`
- `EmbeddingPort`, `MemoryStorePort`, `TaskStorePort`

### 4. Adapters (`src/adapters/`)
Infrastructure implementations. See [[Channels]], [[Memory]].

| File | Purpose |
|------|---------|
| `channel_runtime.rs` | Shared logic for all [[Channels]] |
| `engine_factory.rs` | Engine construction from config |
| `orchestrator.rs` | [[Orchestrator]] with JoinSet parallel execution |
| `telegram_runtime.rs` | [[Channels|Telegram]] adapter |
| `http_tool_executor.rs` | `http_request` tool executor |
| `crypto_tool_executor.rs` | Crypto signing tool executor (Privy) |
| `memory_store.rs` | Disk vector store |
| `qdrant_memory_store.rs` | Qdrant vector store |
| `tool_ui.rs` | Generic approval UI (shared) |

## Dependency Rules

```
Adapters --> Ports --> (nothing)
Application --> Domain + Ports
Domain --> (nothing)
```

**Forbidden:**
- Application importing `cursive`, `reqwest`, `std::fs`, `tokio::process`
- Domain importing any infrastructure code
- Bypassing use-case services from adapters

## Enforcement

5 automated tests in `tests/hex_architecture_enforcement.rs` verify these rules on every build. See [[Testing]].

## Related

- [[Principles]] — DRY, KISS, Rust best practices
- [[Overview]] — system concept map
- [[Capabilities]] — how permissions flow through the architecture
