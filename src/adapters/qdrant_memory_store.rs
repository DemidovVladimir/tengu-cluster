//! Qdrant-backed vector memory store implementing MemoryStorePort.
//!
//! Opt-in backend activated with `--features qdrant` and `backend = "qdrant"` in
//! config. Communicates with a Qdrant server over gRPC (via `qdrant-client` +
//! tonic), providing ANN (approximate nearest-neighbor) search with cosine
//! distance — the same metric used by the disk backend's brute-force search.
//!
//! ## Data mapping
//!
//! | MemoryEntry field     | Qdrant representation                   |
//! |-----------------------|-----------------------------------------|
//! | `id`                  | Point ID (UUID string)                  |
//! | `embedding`           | Point vector (`Vec<f32>`)               |
//! | `content`             | Payload key `"content"` (string)        |
//! | `agent_id`            | Payload key `"agent_id"` (string)       |
//! | `created_at_epoch_s`  | Payload key `"created_at_epoch_s"` (i64)|
//!
//! The collection is auto-created on first connect if it doesn't exist, using
//! `Distance::Cosine` and the configured `vector_size`.

use crate::adapters::ports::MemoryStorePort;
use crate::adapters::types::{MemoryEntry, MemorySearchResult};
use anyhow::{Context, Result};
use async_trait::async_trait;
use qdrant_client::qdrant::{
    CountPointsBuilder, CreateCollectionBuilder, DeletePointsBuilder, Distance, PointStruct,
    QueryPointsBuilder, UpsertPointsBuilder, VectorParamsBuilder,
};
use qdrant_client::Qdrant;

/// Qdrant-backed vector memory store.
///
/// Communicates via gRPC (tonic). Auto-creates the collection on startup if
/// missing. Requires a running Qdrant server — see `config.example.toml` for
/// docker run instructions.
pub(crate) struct QdrantMemoryStore {
    client: Qdrant,
    collection: String,
}

impl QdrantMemoryStore {
    /// Connect to a Qdrant instance and ensure the target collection exists.
    pub(crate) async fn new(
        url: &str,
        api_key: Option<&str>,
        collection: &str,
        vector_size: u64,
    ) -> Result<Self> {
        let mut builder = Qdrant::from_url(url);
        if let Some(key) = api_key {
            builder = builder.api_key(key);
        }
        let client = builder
            .build()
            .map_err(|e| anyhow::anyhow!("failed to build Qdrant client: {}", e))?;

        // Auto-create collection if it doesn't exist.
        let exists = client
            .collection_exists(collection)
            .await
            .context("failed to check if Qdrant collection exists")?;

        if !exists {
            client
                .create_collection(
                    CreateCollectionBuilder::new(collection)
                        .vectors_config(VectorParamsBuilder::new(vector_size, Distance::Cosine)),
                )
                .await
                .context("failed to create Qdrant collection")?;
            tracing::info!(collection, vector_size, "Created Qdrant collection");
        }

        tracing::info!(collection, url, "Qdrant memory store connected");

        Ok(Self {
            client,
            collection: collection.to_string(),
        })
    }
}

#[async_trait]
impl MemoryStorePort for QdrantMemoryStore {
    async fn store(&self, entry: &MemoryEntry) -> Result<()> {
        let mut payload_json = serde_json::json!({
            "content": entry.content,
            "agent_id": entry.agent_id,
            "created_at_epoch_s": entry.created_at_epoch_s,
        });
        // Store metadata as "meta_*" prefixed payload keys.
        if let serde_json::Value::Object(ref mut map) = payload_json {
            for (k, v) in &entry.metadata {
                map.insert(format!("meta_{}", k), serde_json::Value::String(v.clone()));
            }
        }
        let point = PointStruct::new(
            entry.id.clone(),
            entry.embedding.clone(),
            qdrant_client::Payload::try_from(payload_json).unwrap_or_default(),
        );
        self.client
            .upsert_points(UpsertPointsBuilder::new(&self.collection, vec![point]).wait(true))
            .await
            .context("failed to upsert point to Qdrant")?;
        Ok(())
    }

    async fn search_by_vector(
        &self,
        embedding: &[f32],
        top_k: usize,
    ) -> Result<Vec<MemorySearchResult>> {
        let query_vec = embedding.to_vec();
        let response = self
            .client
            .query(
                QueryPointsBuilder::new(&self.collection)
                    .query(query_vec)
                    .limit(top_k as u64)
                    .with_payload(true),
            )
            .await
            .context("Qdrant search failed")?;

        let results = response
            .result
            .into_iter()
            .filter_map(|scored| {
                let payload = scored.payload;
                let content = payload
                    .get("content")
                    .and_then(|v| match &v.kind {
                        Some(qdrant_client::qdrant::value::Kind::StringValue(s)) => Some(s.clone()),
                        _ => None,
                    })
                    .unwrap_or_default();
                let agent_id = payload
                    .get("agent_id")
                    .and_then(|v| match &v.kind {
                        Some(qdrant_client::qdrant::value::Kind::StringValue(s)) => Some(s.clone()),
                        _ => None,
                    })
                    .unwrap_or_default();
                let created_at_epoch_s = payload
                    .get("created_at_epoch_s")
                    .and_then(|v| match &v.kind {
                        Some(qdrant_client::qdrant::value::Kind::IntegerValue(i)) => {
                            Some(*i as u64)
                        }
                        _ => None,
                    })
                    .unwrap_or(0);

                let id = scored
                    .id
                    .as_ref()
                    .and_then(|pid| match &pid.point_id_options {
                        Some(qdrant_client::qdrant::point_id::PointIdOptions::Uuid(uuid)) => {
                            Some(uuid.clone())
                        }
                        _ => None,
                    })?;

                // Reconstruct metadata from "meta_*" prefixed payload keys.
                let mut metadata = std::collections::HashMap::new();
                for (k, v) in &payload {
                    if let Some(stripped) = k.strip_prefix("meta_") {
                        if let Some(qdrant_client::qdrant::value::Kind::StringValue(s)) = &v.kind {
                            metadata.insert(stripped.to_string(), s.clone());
                        }
                    }
                }

                Some(MemorySearchResult {
                    entry: MemoryEntry {
                        id,
                        content,
                        embedding: Vec::new(), // Qdrant doesn't return vectors by default
                        agent_id,
                        created_at_epoch_s,
                        metadata,
                    },
                    score: scored.score,
                })
            })
            .collect();

        Ok(results)
    }

    async fn delete(&self, id: &str) -> Result<bool> {
        let result = self
            .client
            .delete_points(
                DeletePointsBuilder::new(&self.collection)
                    .points(vec![id.to_string()])
                    .wait(true),
            )
            .await;
        match result {
            Ok(_) => Ok(true),
            Err(e) => Err(anyhow::anyhow!("Qdrant delete failed: {}", e)),
        }
    }

    async fn clear_all(&self) -> Result<()> {
        // Delete all points by matching everything.
        self.client
            .delete_points(
                DeletePointsBuilder::new(&self.collection)
                    .points(qdrant_client::qdrant::Filter::default())
                    .wait(true),
            )
            .await
            .context("Qdrant clear_all failed")?;
        Ok(())
    }

    async fn entry_count(&self) -> usize {
        self.client
            .count(CountPointsBuilder::new(&self.collection).exact(true))
            .await
            .map(|r| r.result.map(|c| c.count as usize).unwrap_or(0))
            .unwrap_or(0)
    }

    async fn storage_bytes(&self) -> u64 {
        // Not meaningful for a remote database.
        0
    }
}
