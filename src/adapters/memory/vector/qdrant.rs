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
        filter: Option<&ChunkMetadata>,
    ) -> Result<Vec<MemoryHit>> {
        // Fix F (2026-05-09) — server-side filtering on string-valued
        // payload fields. Pre Fix-F this argument was ignored and callers
        // had to over-fetch and post-filter (e.g. `search_outputs_for_session`
        // fetched 5× and dropped non-matches). With server-side push-down,
        // we ask Qdrant for exactly top_k matching points.
        //
        // Translation rules: each `Some(_)` structured field on
        // `ChunkMetadata` (agent / source / kind) becomes one
        // `FieldCondition` matching by string keyword. Each string-valued
        // entry in `extra` becomes one condition keyed `extra_<k>`
        // (the same prefix used at write time — see top-of-file mapping
        // table). Non-string and empty filters fall through with no
        // condition added, so a partially-populated `ChunkMetadata` still
        // applies the constraints it can.
        //
        // If no conditions emerge (no Some-fields, no string-extras), we
        // skip the Filter entirely — equivalent to the unfiltered query.
        let query_vec = embedding.to_vec();
        let mut builder = QueryPointsBuilder::new(&self.collection)
            .query(query_vec)
            .limit(top_k as u64)
            .with_payload(true);
        if let Some(f) = filter {
            if let Some(qf) = build_search_filter(f) {
                builder = builder.filter(qf);
            }
        }
        let response = self
            .client
            .query(builder)
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

/// Fix F (2026-05-09) — translate a `ChunkMetadata` filter into a Qdrant
/// `Filter`. Returns `None` when nothing actionable is present (so the
/// caller can skip the filter entirely and fall through to an unfiltered
/// query).
///
/// Supported today: structured string fields (`agent`, `source`, `kind`)
/// and string-valued entries in `extra`. The most important use case is
/// `extra.rag_session_id` for within-session output recall (`Fix A`).
/// Non-string filter values are silently dropped — this is a deliberately
/// limited shape, not a general query DSL.
fn build_search_filter(filter: &ChunkMetadata) -> Option<qdrant_client::qdrant::Filter> {
    use qdrant_client::qdrant::r#match::MatchValue;
    use qdrant_client::qdrant::{
        condition::ConditionOneOf, Condition, FieldCondition, Filter, Match,
    };

    let make_keyword_eq = |key: &str, value: &str| Condition {
        condition_one_of: Some(ConditionOneOf::Field(FieldCondition {
            key: key.to_string(),
            r#match: Some(Match {
                match_value: Some(MatchValue::Keyword(value.to_string())),
            }),
            ..Default::default()
        })),
    };

    let mut must = Vec::new();
    let structured = [
        ("agent", filter.agent.as_deref()),
        ("source", filter.source.as_deref()),
        ("kind", filter.kind.as_deref()),
    ];
    for (key, value) in structured {
        if let Some(s) = value.filter(|s| !s.is_empty()) {
            must.push(make_keyword_eq(key, s));
        }
    }
    for (k, v) in &filter.extra {
        if let Some(s) = v.as_str().filter(|s| !s.is_empty()) {
            must.push(make_keyword_eq(&format!("extra_{}", k), s));
        }
    }

    if must.is_empty() {
        None
    } else {
        Some(Filter {
            must,
            ..Default::default()
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Fix F — empty `ChunkMetadata` produces no filter.
    #[test]
    fn build_filter_empty_returns_none() {
        let m = ChunkMetadata::default();
        assert!(build_search_filter(&m).is_none());
    }

    /// Fix F — `extra.rag_session_id = "smoke-1"` becomes a single
    /// keyword-match condition on payload key `extra_rag_session_id`.
    /// This is the most-used path for Fix A's within-session recall.
    #[test]
    fn build_filter_session_extra_emits_condition() {
        use qdrant_client::qdrant::{condition::ConditionOneOf, r#match::MatchValue};

        let mut m = ChunkMetadata::default();
        m.extra.insert(
            "rag_session_id".to_string(),
            serde_json::Value::String("smoke-1".to_string()),
        );
        let filter = build_search_filter(&m).expect("filter");
        assert_eq!(filter.must.len(), 1);
        let cond = &filter.must[0];
        let ConditionOneOf::Field(fc) = cond.condition_one_of.as_ref().unwrap() else {
            panic!("expected FieldCondition");
        };
        assert_eq!(fc.key, "extra_rag_session_id");
        let m_inner = fc.r#match.as_ref().unwrap();
        match m_inner.match_value.as_ref().unwrap() {
            MatchValue::Keyword(s) => assert_eq!(s, "smoke-1"),
            other => panic!("expected Keyword match, got {:?}", other),
        }
    }

    /// Fix F — non-string extras are skipped silently.
    #[test]
    fn build_filter_skips_non_string_extras() {
        let mut m = ChunkMetadata::default();
        m.extra
            .insert("count".to_string(), serde_json::Value::from(42));
        m.extra
            .insert("rag_session_id".to_string(), serde_json::Value::from("x"));
        let filter = build_search_filter(&m).expect("filter");
        // Only the string extra survives. The integer is dropped.
        assert_eq!(filter.must.len(), 1);
    }

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
