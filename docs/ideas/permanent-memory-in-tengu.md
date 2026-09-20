---
tags: [idea, memory, architecture, tengu]
status: draft
created: 2026-03-28
---

> Idea note, moved from `tengu/ideas/memory-idea/Permanent Memory in Tengu.md` on 2026-09-18.

# Permanent Memory in Tengu

## The Problem

Tengu's current memory system is powerful but ephemeral in spirit — memories are stored per-session or wiped on `prune`. There's no concept of **permanent, cross-session memory** that persists across restarts, agents, and even separate deployments. The agent forgets who you are every time.

The goal: make Tengu genuinely remember things the way a person does — long-term facts, user preferences, learned patterns — without any manual intervention from the user.

---

## What Already Exists

- `DiskVectorMemoryStore` — brute-force cosine similarity, bincode-serialized to `vectors.bin`. Survives restarts in theory, but gets pruned with everything else.
- `QdrantMemoryStore` — Qdrant-backed ANN search, opt-in via feature flag. The right foundation for scale.
- `MemoryService` — embedding + store abstraction (`MemoryStorePort`, `EmbeddingPort`). Clean ports/adapters design.
- `remember` tool — agents can already write memories. Recall happens at query time via `recall_filtered`.

The architecture is ready. What's missing is the **policy layer** that decides what survives, what decays, and what never gets pruned.

---

## The Idea: Permanent Memory as a First-Class Concept

Introduce a memory **tier system** with three levels:

| Tier | Name | Lifetime | Stored In | Prunable? |
|---|---|---|---|---|
| 0 | Session | Current run only | In-process `RwLock<Vec>` | Yes (always) |
| 1 | Persistent | Until explicit delete | `vectors.bin` / Qdrant | Only with `--permanent` flag |
| 2 | **Permanent** | Forever | Separate `permanent.bin` or dedicated Qdrant collection | Never by default |

### Key Design Choices

**Permanent memories never get pruned.** The `prune` command skips them unless the user passes `--wipe-permanent`. This is the single most important invariant.

**Permanent memories are promoted, not created directly.** The agent writes to tier 1 (persistent) by default. A fact becomes permanent either:
- Manually: user runs `tengu memory promote <id>`
- Automatically: after a fact is recalled N times (reinforcement heuristic)
- Via a new tool call: `remember_permanently` — reserved for things like user name, language preference, key project facts

**Separate storage namespace.** Permanent memories live in `~/.tengu/memory/permanent/` (disk) or a `tengu_permanent` Qdrant collection — completely separate from the ephemeral store so `clear_all` can never accidentally touch them.

---

## Architecture Constraints

- **Rust only.** All implementation — `MemoryTier`, `PermanentDiskStore`, the new tool — is Rust code compiled into the Tengu binary. No scripts, no side processes.
- **Tools require a rebuild.** `remember_permanently` is a new `ToolDef` exposed via `ToolExecutionPort`. Adding it means `cargo build`. This is a deliberate one-time cost — once compiled in, it's available across all agents and sandboxes without further changes.
- **Skills are hot-plug.** Any skill that wants to *instruct* agents to use permanent memory (e.g. "prefer `remember_permanently` for user preferences") can do so by updating its `SKILL.md` — no rebuild needed. The tool just has to already be compiled in.

---

## How to Plug This Into Tengu

### 1. Extend `MemoryEntry` with a tier field

```rust
#[derive(Serialize, Deserialize, Clone)]
pub struct MemoryEntry {
    // ... existing fields ...
    pub tier: MemoryTier,  // NEW
}

#[derive(Serialize, Deserialize, Clone, PartialEq)]
pub enum MemoryTier {
    Session,
    Persistent,
    Permanent,
}

impl Default for MemoryTier {
    fn default() -> Self { MemoryTier::Persistent }
}
```

### 2. Add a `PermanentMemoryStore` (separate store path)

Implement `MemoryStorePort` for a new `PermanentDiskStore` that reads/writes from `~/.tengu/memory/permanent/permanent.bin`. The key difference: `clear_all` is a no-op (or returns an error).

### 3. Add `remember_permanently` tool

A new `ToolDef` next to `remember`:
```json
{
  "name": "remember_permanently",
  "description": "Store a fact in permanent long-term memory. Use for user preferences, names, key project facts — things that should never be forgotten across restarts.",
  "parameters": { "content": "string", "metadata": "object" }
}
```

### 4. Update `prune` to skip permanent tier

In `prune.rs`, scope the clear operation to only tier 0 and tier 1. Add a `--wipe-permanent` flag with an explicit confirmation prompt.

### 5. Recall always merges both stores

In `MemoryService::recall_filtered`, query both the persistent store and the permanent store, merge results, re-rank by score, and budget-trim. Permanent memories could get a small score boost (e.g. `+0.05`) so they surface reliably even when slightly less semantically relevant.

### 6. (Optional) Qdrant: use separate collections

If using the Qdrant backend, keep a `tengu_memory` collection for persistent and a `tengu_permanent` collection for permanent. Collection-level separation means zero risk of accidental deletion.

---

## Open Questions

- **Auto-promotion heuristic**: how many recalls before a memory is promoted? Should recall count be tracked per entry? Adds a field to `MemoryEntry`.
- **Memory decay for persistent tier**: should persistent memories age out after N days of no recall? Could help keep the store clean without full prune.
- **Multi-agent permanent memory**: is permanent memory global (shared by all agents) or per-agent-id? Probably global for user facts, per-agent for task-specific knowledge.
- **Sync across deployments**: if Tengu runs on multiple machines (e.g. desktop + telegram bot), how does permanent memory stay in sync? Qdrant cloud is the natural answer here.
- **Privacy**: permanent memory survives everything. Should there be encryption at rest separate from the secrets vault?

---

## Next Steps

- [ ] Add `MemoryTier` enum to `types.rs`
- [ ] Implement `PermanentDiskStore` in `memory_builder.rs`
- [ ] Add `remember_permanently` to `memory_tool_defs()`
- [ ] Update `prune.rs` to respect tier boundaries
- [ ] Update `MemoryService::recall_filtered` to merge both stores
- [ ] Add `tengu memory list --permanent` CLI subcommand for inspection
- [ ] Write integration test: store permanent → prune → recall → still present
