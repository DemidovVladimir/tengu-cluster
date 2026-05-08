//! Qdrant-backed `VectorStore`.
//!
//! Opt-in backend activated with `--features qdrant`. Communicates with a
//! Qdrant server over gRPC (via `qdrant-client` + tonic), providing ANN
//! search with cosine distance — same metric as `DiskVectorStore`'s
//! brute-force search.
//!
//! Ported from `src/adapters/qdrant_memory_store.rs::QdrantMemoryStore`
//! with signatures adapted to the new `VectorStore` trait.
//!
//! ## Payload mapping
//!
//! | Field           | Qdrant payload key      |
//! |-----------------|-------------------------|
//! | `text`          | `"text"` (string)       |
//! | `metadata.agent`| `"agent"` (string)      |
//! | `metadata.source` | `"source"` (string)   |
//! | `metadata.kind` | `"kind"` (string)       |
//! | `metadata.tags` | `"tags"` (list<string>) |
//! | `metadata.timestamp_utc` | `"timestamp_utc"` (string) |
//! | `metadata.extra.*` | `"extra_<key>"` (JSON)|

use anyhow::{Context, Result};
use async_trait::async_trait;
use qdrant_client::qdrant::{
    CreateCollectionBuilder, Distance, PointStruct, QueryPointsBuilder, UpsertPointsBuilder,
    VectorParamsBuilder,
};
use qdrant_client::Qdrant;

use crate::adapters::memory::context_block::{ChunkMetadata, MemoryHit};
use crate::adapters::memory::vector::VectorStore;

/// Qdrant-backed vector store. Communicates via gRPC (tonic). Auto-creates
/// the collection on startup if missing.
pub struct QdrantVectorStore {
    client: Qdrant,
    collection: String,
}

impl QdrantVectorStore {
    /// Connect to a Qdrant instance and ensure the target collection exists.
    pub async fn new(
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

        tracing::info!(collection, url, "QdrantVectorStore connected");

        Ok(Self {
            client,
            collection: collection.to_string(),
        })
    }

    fn payload_from(text: &str, metadata: &ChunkMetadata) -> serde_json::Value {
        let mut payload = serde_json::json!({ "text": text });
        if let serde_json::Value::Object(ref mut map) = payload {
            if let Some(ref a) = metadata.agent {
                map.insert("agent".into(), serde_json::Value::String(a.clone()));
            }
            if let Some(ref s) = metadata.source {
                map.insert("source".into(), serde_json::Value::String(s.clone()));
            }
            if let Some(ref k) = metadata.kind {
                map.insert("kind".into(), serde_json::Value::String(k.clone()));
            }
            if let Some(ref ts) = metadata.timestamp_utc {
                map.insert(
                    "timestamp_utc".into(),
                    serde_json::Value::String(ts.clone()),
                );
            }
            if !metadata.tags.is_empty() {
                map.insert(
                    "tags".into(),
                    serde_json::Value::Array(
                        metadata
                            .tags
                            .iter()
                            .map(|t| serde_json::Value::String(t.clone()))
                            .collect(),
                    ),
                );
            }
            for (k, v) in &metadata.extra {
                map.insert(format!("extra_{}", k), v.clone());
            }
        }
        payload
    }

    fn metadata_from_payload(
        payload: &std::collections::HashMap<String, qdrant_client::qdrant::Value>,
    ) -> ChunkMetadata {
        use qdrant_client::qdrant::value::Kind;

        let get_str = |k: &str| -> Option<String> {
            payload.get(k).and_then(|v| match &v.kind {
                Some(Kind::StringValue(s)) => Some(s.clone()),
                _ => None,
            })
        };

        let agent = get_str("agent");
        let source = get_str("source");
        let kind = get_str("kind");
        let timestamp_utc = get_str("timestamp_utc");
        let tags = payload
            .get("tags")
            .and_then(|v| match &v.kind {
                Some(Kind::ListValue(list)) => Some(
                    list.values
                        .iter()
                        .filter_map(|v| match &v.kind {
                            Some(Kind::StringValue(s)) => Some(s.clone()),
                            _ => None,
                        })
                        .collect::<Vec<_>>(),
                ),
                _ => None,
            })
            .unwrap_or_default();

        let mut extra = std::collections::HashMap::new();
        for (k, v) in payload {
            if let Some(stripped) = k.strip_prefix("extra_") {
                // Best-effort conversion — keep as JSON so we round-trip.
                let json_value = match &v.kind {
                    Some(Kind::StringValue(s)) => serde_json::Value::String(s.clone()),
                    Some(Kind::IntegerValue(i)) => serde_json::Value::Number((*i).into()),
                    Some(Kind::DoubleValue(d)) => serde_json::Number::from_f64(*d)
                        .map(serde_json::Value::Number)
                        .unwrap_or(serde_json::Value::Null),
                    Some(Kind::BoolValue(b)) => serde_json::Value::Bool(*b),
                    _ => serde_json::Value::Null,
                };
                extra.insert(stripped.to_string(), json_value);
            }
        }

        ChunkMetadata {
            agent,
            source,
            kind,
            tags,
            timestamp_utc,
            extra,
        }
    }
}

#[async_trait]
impl VectorStore for QdrantVectorStore {
    async fn write(
        &self,
        embedding: Vec<f32>,
        text: &str,
        metadata: ChunkMetadata,
    ) -> Result<String> {
        let payload_json = Self::payload_from(text, &metadata);
        let id = uuid::Uuid::new_v4().to_string();
        let point = PointStruct::new(
            id.clone(),
            embedding,
            qdrant_client::Payload::try_from(payload_json).unwrap_or_default(),
        );
        self.client
            .upsert_points(UpsertPointsBuilder::new(&self.collection, vec![point]).wait(true))
            .await
            .context("failed to upsert point to Qdrant")?;
        Ok(id)
    }

    async fn search(
        &self,
        embedding: &[f32],
        top_k: usize,
        _filter: Option<&ChunkMetadata>,
    ) -> Result<Vec<MemoryHit>> {
        // NOTE: server-side filtering not yet wired; we do a raw top-k then
        // client-filter (mirrors the disk store fetch-k * 3 pattern used in
        // the legacy `MemoryService::recall_filtered`). When `_filter` is
        // `Some`, callers should fetch extra and post-filter themselves, or
        // a later task can push the filter down into the Qdrant query.
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

        let hits = response
            .result
            .into_iter()
            .map(|scored| {
                let payload = scored.payload;
                let text = payload
                    .get("text")
                    .and_then(|v| match &v.kind {
                        Some(qdrant_client::qdrant::value::Kind::StringValue(s)) => Some(s.clone()),
                        _ => None,
                    })
                    .unwrap_or_default();
                let metadata = Self::metadata_from_payload(&payload);
                MemoryHit {
                    text,
                    score: scored.score,
                    metadata,
                }
            })
            .collect();

        Ok(hits)
    }

    async fn delete(&self, id: &str) -> Result<bool> {
        use qdrant_client::qdrant::{DeletePointsBuilder, PointsIdsList};

        self.client
            .delete_points(
                DeletePointsBuilder::new(&self.collection)
                    .points(PointsIdsList {
                        ids: vec![id.into()],
                    })
                    .wait(true),
            )
            .await
            .context("Qdrant delete failed")?;
        Ok(true)
    }

    /// Phase 6.3 — filter-based delete by numeric payload field.
    /// Uses a Qdrant `Filter` with a `Range { lt }` condition on the named
    /// field (typically `"extra_rag_created_at"`). Two-phase implementation
    /// because Qdrant's `delete_points(filter)` returns the operation id
    /// but not a deleted-count: we first scroll the matching points (cap
    /// at a generous limit per batch — defensive against unbounded purges),
    /// then delete by id list, returning the precise count we removed.
    /// On a clean collection (nothing to purge) this is a single empty
    /// scroll round-trip.
    async fn delete_older_than(&self, field: &str, cutoff: f64) -> Result<u64> {
        use qdrant_client::qdrant::{
            condition::ConditionOneOf, Condition, DeletePointsBuilder, FieldCondition, Filter,
            PointsIdsList, Range, ScrollPointsBuilder,
        };

        let condition = Condition {
            condition_one_of: Some(ConditionOneOf::Field(FieldCondition {
                key: field.to_string(),
                range: Some(Range {
                    lt: Some(cutoff),
                    ..Default::default()
                }),
                ..Default::default()
            })),
        };
        let filter = Filter {
            must: vec![condition],
            ..Default::default()
        };

        // Scroll matching points up to a per-call cap. 10_000 is generous
        // for a single sweep — if a deployment ever exceeds this, the
        // caller should run cleanup more often or extend this loop with
        // pagination.
        const SWEEP_LIMIT: u32 = 10_000;
        let scroll = self
            .client
            .scroll(
                ScrollPointsBuilder::new(&self.collection)
                    .filter(filter.clone())
                    .limit(SWEEP_LIMIT)
                    .with_payload(false)
                    .with_vectors(false),
            )
            .await
            .context("Qdrant scroll for TTL purge failed")?;
        let ids: Vec<qdrant_client::qdrant::PointId> = scroll
            .result
            .into_iter()
            .filter_map(|p| p.id)
            .collect();
        if ids.is_empty() {
            return Ok(0);
        }
        let count = ids.len() as u64;
        // qdrant-client 1.17: `DeletePointsBuilder::points()` takes anything
        // that implements `Into<PointsSelectorOneOf>`. `PointsIdsList`
        // satisfies that directly — the same pattern the single-id `delete`
        // method above uses. Don't wrap in a `PointsSelector` layer.
        self.client
            .delete_points(
                DeletePointsBuilder::new(&self.collection)
                    .points(PointsIdsList { ids })
                    .wait(true),
            )
            .await
            .context("Qdrant delete_points (TTL purge) failed")?;
        Ok(count)
    }

    async fn clear_all(&self) -> Result<()> {
        // Qdrant doesn't expose "delete all" per-collection as a single call;
        // delete the collection and recreate it. Preserves vector config.
        // Best-effort — recreate failure is fatal.
        let vector_size = self
            .client
            .collection_info(&self.collection)
            .await
            .context("failed to read collection info for clear_all")?
            .result
            .and_then(|info| info.config)
            .and_then(|cfg| cfg.params)
            .and_then(|params| params.vectors_config)
            .and_then(|vc| match vc.config {
                Some(qdrant_client::qdrant::vectors_config::Config::Params(p)) => Some(p.size),
                _ => None,
            })
            .ok_or_else(|| anyhow::anyhow!("could not infer vector size for clear_all"))?;

        self.client
            .delete_collection(&self.collection)
            .await
            .context("Qdrant delete_collection failed")?;
        self.client
            .create_collection(
                CreateCollectionBuilder::new(&self.collection)
                    .vectors_config(VectorParamsBuilder::new(vector_size, Distance::Cosine)),
            )
            .await
            .context("Qdrant recreate collection failed")?;
        Ok(())
    }

    async fn entry_count(&self) -> Result<usize> {
        let info = self
            .client
            .collection_info(&self.collection)
            .await
            .context("Qdrant collection_info failed")?;
        Ok(info
            .result
            .and_then(|r| r.points_count)
            .map(|c| c as usize)
            .unwrap_or(0))
    }

    async fn storage_bytes(&self) -> Result<u64> {
        // Qdrant doesn't expose storage bytes via the info endpoint reliably
        // — return 0 to signal "unknown" rather than fail the caller.
        Ok(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Smoke test: exercises Qdrant only if `QDRANT_URL` is configured.
    /// Otherwise skipped silently — matches the plan's `#[ignore]`-or-env
    /// conditional pattern.
    #[tokio::test]
    async fn write_then_search_against_live_qdrant() {
        let Ok(url) = std::env::var("QDRANT_URL") else {
            eprintln!("QDRANT_URL not set; skipping live test");
            return;
        };
        let api_key = std::env::var("QDRANT_API_KEY").ok();
        let collection = format!(
            "test_vec_{}",
            uuid::Uuid::new_v4().to_string().replace('-', "")
        );
        let store = QdrantVectorStore::new(&url, api_key.as_deref(), &collection, 3)
            .await
            .expect("connect");
        let emb = vec![1.0_f32, 0.0, 0.0];
        store
            .write(emb.clone(), "hello qdrant", ChunkMetadata::default())
            .await
            .expect("write");

        // give Qdrant a beat to index
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;

        let hits = store.search(&emb, 5, None).await.expect("search");
        assert!(!hits.is_empty());
        assert_eq!(hits[0].text, "hello qdrant");
    }
}
