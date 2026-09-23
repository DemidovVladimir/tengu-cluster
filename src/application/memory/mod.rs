//! Harness-owned memory subsystem — the **in-process, file/disk** layer.
//!
//! ## Where this sits in the stack (post Phase 6, 2026-05-14)
//!
//! | Layer | Owner | Backing store |
//! |---|---|---|
//! | Durable runtime memory (planner recall, step summaries, wiki compile) | `outbound/tools/agentic_memory/` (feature `postgres_memory`) | Postgres + pgvector |
//! | Workspace memory tools (`remember`, `persistent_store`, `memory_ingest` / `memory_search`) | THIS MODULE | `vector/disk.rs` bincode file |
//! | MEMORY.md / daily logs / identity bootstrap | `builtin.rs` via `manager.rs` | workspace files |
//!
//! The legacy `rag/` facade and the Qdrant `VectorStore` impl are gone.
//!
//! ## File map
//!
//! - `builtin`       — `BuiltinMemoryProvider` (MEMORY.md, identity, daily logs,
//!                     vector). The default provider.
//! - `manager`       — `MemoryManager` holding one builtin + at most one external
//!                     provider. The handle most consumers hold.
//! - `injector`      — pre-turn fenced context injection. **One live caller:**
//!                     `orchestrator/wiring.rs::ChatOrchestratorPortImpl::run_orchestrator_turn`
//!                     (planner-side LLM call). Subagent turns do NOT use it.
//! - `writer`        — post-turn spawned non-blocking memory writes.
//!                     Same single caller as `injector` (`wiring.rs::sync_turn`).
//! - `fencing`       — `<memory-context>` block helpers.
//! - `vector`        — `DiskVectorStore` (only impl) +
//!                     `Embedder` (`DEFAULT_EMBEDDING_MODEL`).
//! - shared types (`MemoryHit`, `ChunkMetadata`, …) live in `domain/memory.rs`;
//!   the `MemoryProvider` / `VectorStore` traits in `ports/memory.rs`.

pub mod fencing;
pub mod injector;
pub mod manager;
pub mod writer;
