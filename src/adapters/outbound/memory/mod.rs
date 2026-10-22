//! Memory adapters — `DiskVectorStore` (bincode at `<workspace>/memory/vectors.bin`)
//! and the `Embedder` client (model = `domain::memory::DEFAULT_EMBEDDING_MODEL`).
//! Durable runtime memory is the Postgres `agentic_memory` tool
//! (`outbound/tools/agentic_memory/`, feature `postgres_memory`).

pub mod disk_vector;
pub mod embedder;
