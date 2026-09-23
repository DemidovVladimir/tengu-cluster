//! Vector backend for the in-process memory tools (`remember`,
//! `persistent_store`, `memory_ingest` / `memory_search`).
//!
//! | Piece | Status |
//! |---|---|
//! | `VectorStore` trait | port — `ports/memory.rs`; one impl |
//! | `disk::DiskVectorStore` | the only impl — bincode file at `<workspace>/.tengu/memory.bin` |
//! | `embedder::Embedder` | OpenRouter embeddings client; model = `embedder::DEFAULT_EMBEDDING_MODEL` |
//! | Qdrant impl | removed Phase 6 (2026-05-14); `memory_config.backend` is no longer branched on |
//!
//! Durable runtime memory (planner recall, step summaries, wiki) is NOT here —
//! it is the Postgres `agentic_memory` plugin (`plugins/agentic_memory/`,
//! feature `postgres_memory`).

pub mod disk;
pub mod embedder;

pub use disk::DiskVectorStore;
pub use embedder::Embedder;
