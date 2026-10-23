//! Harness-owned memory subsystem — the **in-process, file/disk** layer.
//!
//! ## Where this sits in the stack
//!
//! | Layer | Owner | Backing store |
//! |---|---|---|
//! | Durable runtime memory (planner recall, step summaries, wiki compile) | `outbound/tools/agentic_memory/` (feature `postgres_memory`) | Postgres + pgvector |
//! | Workspace memory tools (`remember`, `persistent_store`, `memory_ingest` / `memory_search`) | THIS MODULE | `vector/disk.rs` bincode file |
//!
//! The legacy `rag/` facade and the Qdrant `VectorStore` impl are gone.
//!
//! ## File map
//!
//! - `manager`       — `MemoryManager`: the workspace vector backend
//!                     (embedder + store) behind the memory tools.
//! - `fencing`       — the recalled-memory block (`domain::untrusted` fence).
//! - `vector`        — `DiskVectorStore` (only impl) +
//!                     `Embedder` (`DEFAULT_EMBEDDING_MODEL`).
//! - shared types (`MemoryHit`, `ChunkMetadata`, …) live in `domain/memory.rs`;
//!   the `VectorStore` trait in `ports/memory.rs`. The system prompt's
//!   workspace files (IDENTITY / PROFILE / CONTEXT) are loaded by
//!   `application/skills/registry.rs`, not here.

pub mod fencing;
pub mod manager;
