//! Vector backend for memory retrieval and ingestion.
//!
//! Two swappable stores, selected by config:
//! - `disk`   — bincode file at `<workspace>/.tengu/memory.bin`
//! - `qdrant` — REST client against a Qdrant instance
//!
//! Both expose the `VectorStore` trait below.

use anyhow::Result;
use async_trait::async_trait;

use crate::adapters::memory::context_block::{ChunkMetadata, MemoryHit};

#[async_trait]
pub trait VectorStore: Send + Sync {
    async fn write(
        &self,
        embedding: Vec<f32>,
        text: &str,
        metadata: ChunkMetadata,
    ) -> Result<()>;

    async fn search(
        &self,
        embedding: &[f32],
        top_k: usize,
        filter: Option<&ChunkMetadata>,
    ) -> Result<Vec<MemoryHit>>;
}

pub mod disk;
pub mod embedder;
#[cfg(feature = "qdrant")]
pub mod qdrant;

pub use disk::DiskVectorStore;
pub use embedder::Embedder;
#[cfg(feature = "qdrant")]
pub use qdrant::QdrantVectorStore;
