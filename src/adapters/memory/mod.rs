//! Harness-owned memory subsystem — the **low-level** layer.
//!
//! ## Where this sits in the stack
//!
//! ```text
//!  src/adapters/rag/      ← v2 facade. 4 files. Owns three Qdrant
//!         │                   collections (registry/messages/outputs).
//!         │                   Indexer, query, cleanup, RagStore handle.
//!         ▼
//!  src/adapters/memory/   ← THIS MODULE. Low-level vector store +
//!                             memory-provider abstraction. Used by both
//!                             the v2 RAG facade above AND the legacy
//!                             planner-side LLM turn (ChatOrchestratorPortImpl).
//! ```
//!
//! `memory/` and `rag/` look like duplicates at first glance but they're
//! layered. If you're touching vector reads/writes against Qdrant or disk,
//! you're in `memory/`. If you're touching the v2 registry/messages/outputs
//! flow that the planner uses, you're in `rag/`.
//!
//! ## File map
//!
//! - `provider`      — `MemoryProvider` trait (Hermes-shaped). Heavy use.
//! - `builtin`       — `BuiltinMemoryProvider` (MEMORY.md, identity, daily logs,
//!                     vector). The default provider.
//! - `manager`       — `MemoryManager` holding one builtin + at most one external
//!                     provider. The handle most consumers hold.
//! - `injector`      — pre-turn fenced context injection. **Phase 7.1 note:**
//!                     `ChatWorker` (heavy user) is gone; only
//!                     `ChatOrchestratorPortImpl::run_orchestrator_turn` calls
//!                     this now, for the planner-side LLM call.
//! - `writer`        — post-turn spawned non-blocking memory writes.
//!                     Same Phase 7.1 note as `injector` — one caller left.
//! - `fencing`       — `<memory-context>` block helpers.
//! - `vector`        — `VectorStore` trait + the two implementations:
//!                     `vector/qdrant.rs` (real, used by RAG facade) and
//!                     `vector/disk.rs` (bincode fallback for non-qdrant builds).
//! - `context_block` — shared types (`MemoryHit`, `ChunkMetadata`, etc.).

pub mod builtin;
pub mod context_block;
pub mod fencing;
pub mod injector;
pub mod manager;
pub mod provider;
pub mod vector;
pub mod writer;
