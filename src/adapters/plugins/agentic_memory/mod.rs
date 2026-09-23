//! `agentic_memory` — Postgres-backed Open Brain + LLM Wiki memory surface.
//!
//! MVP scope:
//! - Open Brain-style live memory in Postgres (`memory_events`, `memory_sources`,
//!   `memory_chunks`, `memory_promotions`).
//! - Karpathy LLM Wiki-style compiled Markdown under
//!   `<workspace>/.tengu/agentic-memory/wiki`.
//! - One agent-visible tool with operation dispatch.
//!
//! Runtime contract:
//! - Scope gate: `execute` checks `env_reads` for `TENGU_MEMORY_DATABASE_URL`
//!   first; `ingest_source` / `compile_wiki` additionally gate fs writes.
//! - `capture` threading: `session_id` = arg > `TENGU_SESSION_ID`;
//!   `agent` = arg > `TENGU_AGENT_NAME` > model slug.
//! - Fail-soft: a rejected embedding (wrong dim / non-finite) warns and
//!   degrades to text-only insert / FTS-only recall — never errors.
//! - Schema DDL runs once per process (`SCHEMA_READY`); connections are per call.

use anyhow::{anyhow, bail, Context, Result};
use async_trait::async_trait;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio_postgres::{Client, NoTls};
use uuid::Uuid;

use crate::adapters::memory::vector::embedder::DEFAULT_EMBEDDING_MODEL;
use crate::adapters::memory::vector::Embedder;
use crate::adapters::tool_utils::require_str;
use crate::domain::message::ToolDef;
use crate::ports::tool::{PluginCtx, Tool, ToolCtx, ToolOutput, ToolPlugin};

pub(crate) const AGENTIC_MEMORY_TOOL_NAME: &str = "agentic_memory";

const DEFAULT_DATABASE_URL_ENV: &str = "TENGU_MEMORY_DATABASE_URL";
const RAW_ROOT: &str = ".tengu/agentic-memory/raw";
const WIKI_ROOT: &str = ".tengu/agentic-memory/wiki";

pub(crate) fn tool_defs() -> Vec<ToolDef> {
    vec![AgenticMemoryTool::definition_static()]
}

pub(crate) async fn write_step_summary_with_embedding(
    session_id: &str,
    step_id: &str,
    summary: &str,
    embedding: Option<&[f32]>,
) -> Result<String> {
    let metadata = json!({
        "session_id": session_id,
        "step_id": step_id,
        "source": "subagent_summary"
    });
    let store = PostgresMemoryStore::connect_from_env().await?;
    store.ensure_schema().await?;
    let id = store
        .insert_event_with_embedding(
            Some(session_id),
            Some("subagent"),
            "assistant",
            "step_output",
            summary,
            &metadata,
            embedding,
        )
        .await?;
    Ok(id.to_string())
}

pub(crate) async fn write_user_message_with_embedding(
    session_id: &str,
    message: &str,
    embedding: Option<&[f32]>,
) -> Result<String> {
    let metadata = json!({
        "session_id": session_id,
        "source": "planner_user_message"
    });
    let store = PostgresMemoryStore::connect_from_env().await?;
    store.ensure_schema().await?;
    let id = store
        .insert_event_with_embedding(
            Some(session_id),
            Some("planner"),
            "user",
            "message",
            message,
            &metadata,
            embedding,
        )
        .await?;
    Ok(id.to_string())
}

pub(crate) async fn recall_user_messages_with_vec(
    query: &str,
    embedding: Option<&[f32]>,
    top_k: usize,
    exclude_exact: &str,
) -> Result<Vec<AgenticMemoryHit>> {
    let store = PostgresMemoryStore::connect_from_env().await?;
    store.ensure_schema().await?;
    let hits = store
        .recall_events_hybrid(query, embedding, Some("message"), None, top_k as i64 + 2)
        .await?
        .into_iter()
        .filter(|h| h.content.trim() != exclude_exact.trim())
        .take(top_k)
        .collect();
    Ok(hits)
}

pub(crate) async fn recall_step_outputs_for_session_with_vec(
    session_id: &str,
    query: &str,
    embedding: Option<&[f32]>,
    top_k: usize,
) -> Result<Vec<AgenticMemoryHit>> {
    let store = PostgresMemoryStore::connect_from_env().await?;
    store.ensure_schema().await?;
    store
        .recall_events_hybrid(
            query,
            embedding,
            Some("step_output"),
            Some(session_id),
            top_k as i64,
        )
        .await
}

pub(crate) async fn recall_step_outputs_with_vec(
    query: &str,
    embedding: Option<&[f32]>,
    top_k: usize,
) -> Result<Vec<AgenticMemoryHit>> {
    let store = PostgresMemoryStore::connect_from_env().await?;
    store.ensure_schema().await?;
    store
        .recall_events_hybrid(query, embedding, Some("step_output"), None, top_k as i64)
        .await
}

pub(crate) struct AgenticMemoryHit {
    pub id: String,
    pub kind: String,
    pub content: String,
    pub score: f32,
    pub step_id: Option<String>,
}

pub(crate) struct AgenticMemoryPlugin;

#[async_trait]
impl ToolPlugin for AgenticMemoryPlugin {
    fn name(&self) -> &'static str {
        "agentic_memory"
    }

    async fn tools(&self, _ctx: &PluginCtx<'_>) -> Result<Vec<Arc<dyn Tool>>> {
        Ok(vec![Arc::new(AgenticMemoryTool {
            def: AgenticMemoryTool::definition_static(),
        })])
    }
}

pub(crate) struct AgenticMemoryTool {
    def: ToolDef,
}

impl AgenticMemoryTool {
    fn definition_static() -> ToolDef {
        ToolDef::new(
            AGENTIC_MEMORY_TOOL_NAME,
            "Postgres-backed agentic memory. Captures live memory, recalls \
             relevant context, ingests raw sources, promotes stable memories, \
             compiles a Markdown LLM Wiki, and lints memory/wiki hygiene.",
            json!({
                "type": "object",
                "properties": {
                    "operation": {
                        "type": "string",
                        "enum": ["capture", "recall", "ingest_source", "promote", "compile_wiki", "lint"],
                        "description": "Memory operation to run."
                    },
                    "kind": {
                        "type": "string",
                        "description": "Memory/source kind, e.g. preference, claim, file, youtube_transcript."
                    },
                    "content": {
                        "type": "string",
                        "description": "Text to capture or transcript/source body."
                    },
                    "query": {
                        "type": "string",
                        "description": "Recall query."
                    },
                    "path": {
                        "type": "string",
                        "description": "Workspace-relative file path for ingest_source(kind=file)."
                    },
                    "uri": {
                        "type": "string",
                        "description": "External source URI, e.g. URL or youtube link."
                    },
                    "title": {
                        "type": "string",
                        "description": "Optional source/wiki title."
                    },
                    "target_id": {
                        "type": "string",
                        "description": "Event/source/chunk id to promote."
                    },
                    "target_kind": {
                        "type": "string",
                        "description": "Kind of promoted item: event, source, chunk, claim."
                    },
                    "top_k": {
                        "type": "integer",
                        "description": "Recall result limit. Default 5."
                    },
                    "metadata": {
                        "type": "object",
                        "description": "Free-form metadata stored with the event/source.",
                        "additionalProperties": true
                    },
                    "session_id": {
                        "type": "string",
                        "description": "Session id to scope a captured memory to (capture)."
                    },
                    "agent": {
                        "type": "string",
                        "description": "Originating agent name for a captured memory (capture). Defaults to the calling agent's model."
                    },
                    "role": {
                        "type": "string",
                        "description": "Role for a captured memory, e.g. user/assistant/system (capture). Defaults to kind."
                    },
                    "reason": {
                        "type": "string",
                        "description": "Why an item is being promoted (promote)."
                    },
                    "evidence": {
                        "type": "object",
                        "description": "Supporting evidence stored with a promotion (promote).",
                        "additionalProperties": true
                    }
                },
                "required": ["operation"]
            }),
        )
    }

    async fn store() -> Result<PostgresMemoryStore> {
        PostgresMemoryStore::connect_from_env().await
    }

    async fn capture(&self, args: &Value, ctx: &ToolCtx<'_>) -> Result<String> {
        let kind = opt_str(args, "kind").unwrap_or("event");
        let content = require_str(args, AGENTIC_MEMORY_TOOL_NAME, "content")?;
        let metadata = args.get("metadata").cloned().unwrap_or_else(|| json!({}));
        // Session/agent threading: explicit tool arg > env exported by the
        // parent (`TENGU_SESSION_ID` / `TENGU_AGENT_NAME`) > model slug.
        let session_id = opt_str(args, "session_id")
            .map(str::to_string)
            .or_else(|| std::env::var("TENGU_SESSION_ID").ok());
        let agent = opt_str(args, "agent")
            .map(str::to_string)
            .or_else(|| std::env::var("TENGU_AGENT_NAME").ok())
            .or_else(|| ctx.agent_config.map(|a| a.model.clone()));
        let role = opt_str(args, "role").unwrap_or(kind);

        let store = Self::store().await?;
        store.ensure_schema().await?;
        let id = store
            .insert_event(
                session_id.as_deref(),
                agent.as_deref(),
                role,
                kind,
                content,
                &metadata,
            )
            .await?;
        Ok(format!(
            "captured: event_id={id} kind={kind} bytes={}",
            content.len()
        ))
    }

    async fn recall(&self, args: &Value) -> Result<String> {
        let query = require_str(args, AGENTIC_MEMORY_TOOL_NAME, "query")?;
        let top_k = args.get("top_k").and_then(|v| v.as_i64()).unwrap_or(5);
        // Embed the query so recall is hybrid (pgvector first, FTS fallback),
        // matching the planner-side recall lanes. Fail-soft: a missing
        // OPENROUTER_API_KEY or an embed error drops to FTS-only.
        let embedding = embed_query(query).await;
        let store = Self::store().await?;
        store.ensure_schema().await?;
        let rows = store
            .recall(query, embedding.as_deref(), top_k.clamp(1, 20))
            .await?;
        if rows.is_empty() {
            return Ok("recall: no matches".to_string());
        }
        Ok(rows
            .into_iter()
            .map(|row| {
                format!(
                    "- {} [{}] {}",
                    row.id,
                    row.kind,
                    first_line(&row.content, 500)
                )
            })
            .collect::<Vec<_>>()
            .join("\n"))
    }

    async fn ingest_source(&self, args: &Value, ctx: &ToolCtx<'_>) -> Result<String> {
        ctx.scope.check_fs_write(ctx.workspace)?;

        let kind = opt_str(args, "kind").unwrap_or("note");
        let title = opt_str(args, "title").unwrap_or(kind);
        let uri = opt_str(args, "uri");
        let content = match (opt_str(args, "content"), opt_str(args, "path")) {
            (Some(content), _) => content.to_string(),
            (None, Some(path)) => {
                let path = resolve_workspace_path(ctx.workspace, path);
                ctx.scope.check_fs_read(&path)?;
                std::fs::read_to_string(&path)
                    .with_context(|| format!("agentic_memory: read {}", path.display()))?
            }
            (None, None) => bail!("agentic_memory.ingest_source: content or path is required"),
        };

        let hash = sha256_hex(content.as_bytes());
        let raw_path = write_raw(ctx.workspace, kind, &hash, &content)?;
        let metadata = args.get("metadata").cloned().unwrap_or_else(|| json!({}));

        let store = Self::store().await?;
        store.ensure_schema().await?;
        let source_id = store
            .insert_source(kind, uri, title, &raw_path, &hash, &metadata)
            .await?;
        // Embed chunks on ingest so `memory_chunks.embedding` is populated and
        // the source is vector-recallable, not FTS-only. Fail-soft: no API key
        // (or a per-chunk embed error) stores that chunk text-only.
        let embedder = env_embedder();
        let chunk_count = store
            .insert_chunks(source_id, &content, &metadata, embedder.as_ref())
            .await?;

        Ok(format!(
            "ingested: source_id={source_id} chunks={chunk_count} raw_path={}",
            raw_path.display()
        ))
    }

    async fn promote(&self, args: &Value) -> Result<String> {
        let target_id = require_str(args, AGENTIC_MEMORY_TOOL_NAME, "target_id")?;
        let target_kind = opt_str(args, "target_kind").unwrap_or("event");
        let reason = opt_str(args, "reason").unwrap_or("agent selected for wiki compilation");
        let evidence = args.get("evidence").cloned().unwrap_or_else(|| json!({}));
        let store = Self::store().await?;
        store.ensure_schema().await?;
        let promotion_id = store
            .insert_promotion(target_kind, target_id, reason, &evidence)
            .await?;
        Ok(format!(
            "promoted: promotion_id={promotion_id} target={target_kind}/{target_id}"
        ))
    }

    /// Phase 4 LLM Wiki compiler. Runs an LLM over the promoted Open Brain
    /// memories to synthesise a cited Markdown page. Fail-soft: if the LLM
    /// call fails (no `OPENROUTER_API_KEY`, API error), a deterministic
    /// bullet-dump page is written instead so `compile_wiki` never hard-fails
    /// on a memory/LLM-availability problem.
    async fn compile_wiki(&self, args: &Value, ctx: &ToolCtx<'_>) -> Result<String> {
        ctx.scope.check_fs_write(ctx.workspace)?;

        let title = opt_str(args, "title").unwrap_or("agentic-memory");
        let wiki_path = wiki_page_path(ctx.workspace, title);
        if let Some(parent) = wiki_path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        let store = Self::store().await?;
        store.ensure_schema().await?;
        let promoted = store.promoted_items(30).await?;

        if promoted.is_empty() {
            std::fs::write(&wiki_path, render_wiki_page(title, &promoted))
                .with_context(|| format!("agentic_memory: write {}", wiki_path.display()))?;
            return Ok(format!(
                "compiled_wiki: page={} items=0 (no promoted memories yet)",
                wiki_path.display()
            ));
        }

        let (system, user) = compile_wiki_prompt(title, &promoted);
        let (body, mode) = match chat_complete(&system, &user).await {
            Ok(llm_body) => (render_wiki_page_llm(title, &llm_body, &promoted), "llm"),
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    "agentic_memory: wiki compiler LLM call failed; writing deterministic fallback page"
                );
                (render_wiki_page(title, &promoted), "fallback")
            }
        };
        std::fs::write(&wiki_path, body)
            .with_context(|| format!("agentic_memory: write {}", wiki_path.display()))?;
        Ok(format!(
            "compiled_wiki: page={} items={} mode={}",
            wiki_path.display(),
            promoted.len(),
            mode
        ))
    }

    async fn lint(&self, ctx: &ToolCtx<'_>) -> Result<String> {
        let store = Self::store().await?;
        store.ensure_schema().await?;
        let stats = store.stats().await?;
        let wiki_root = ctx.workspace.join(WIKI_ROOT);
        let wiki_pages = count_markdown_files(&wiki_root);
        Ok(format!(
            "agentic_memory lint: events={} sources={} chunks={} promotions={} wiki_pages={}",
            stats.events, stats.sources, stats.chunks, stats.promotions, wiki_pages
        ))
    }
}

#[async_trait]
impl Tool for AgenticMemoryTool {
    fn definition(&self) -> &ToolDef {
        &self.def
    }

    async fn execute(&self, args: &Value, ctx: &ToolCtx<'_>) -> Result<ToolOutput> {
        // The DB URL env var is the resource this tool consumes; `ingest_source`
        // / `compile_wiki` additionally gate their fs writes.
        ctx.scope.check_env_read(DEFAULT_DATABASE_URL_ENV)?;
        let operation = require_str(args, AGENTIC_MEMORY_TOOL_NAME, "operation")?;
        let text = match operation {
            "capture" => self.capture(args, ctx).await?,
            "recall" => self.recall(args).await?,
            "ingest_source" => self.ingest_source(args, ctx).await?,
            "promote" => self.promote(args).await?,
            "compile_wiki" => self.compile_wiki(args, ctx).await?,
            "lint" => self.lint(ctx).await?,
            other => bail!("agentic_memory: unknown operation '{other}'"),
        };
        Ok(ToolOutput::from(text))
    }
}

/// Process-wide "schema DDL already applied" marker — see `ensure_schema`.
static SCHEMA_READY: tokio::sync::OnceCell<()> = tokio::sync::OnceCell::const_new();

struct PostgresMemoryStore {
    client: Client,
}

impl PostgresMemoryStore {
    async fn connect_from_env() -> Result<Self> {
        let url = std::env::var(DEFAULT_DATABASE_URL_ENV)
            .map_err(|_| anyhow!("{DEFAULT_DATABASE_URL_ENV} must be set for agentic_memory"))?;
        let (client, connection) = tokio_postgres::connect(&url, NoTls).await?;
        tokio::spawn(async move {
            if let Err(e) = connection.await {
                tracing::warn!(error = %e, "agentic_memory postgres connection ended");
            }
        });
        Ok(Self { client })
    }

    /// Idempotent DDL, guarded by `SCHEMA_READY` so the advisory-lock + DDL
    /// batch runs once per process. Connections are still opened per call.
    /// A failed attempt leaves the cell unset, so the next call retries.
    async fn ensure_schema(&self) -> Result<()> {
        SCHEMA_READY
            .get_or_try_init(|| self.run_schema_ddl())
            .await
            .map(|_| ())
    }

    async fn run_schema_ddl(&self) -> Result<()> {
        self.client
            .batch_execute("SELECT pg_advisory_lock(778390130512);")
            .await?;
        let result = self
            .client
            .batch_execute(
                r#"
                CREATE EXTENSION IF NOT EXISTS vector;

                CREATE TABLE IF NOT EXISTS memory_events (
                    id uuid PRIMARY KEY,
                    session_id text,
                    agent text,
                    role text NOT NULL,
                    kind text NOT NULL,
                    content text NOT NULL,
                    summary text,
                    embedding vector(1536),
                    metadata jsonb NOT NULL DEFAULT '{}',
                    created_at timestamptz NOT NULL DEFAULT now()
                );

                CREATE TABLE IF NOT EXISTS memory_sources (
                    id uuid PRIMARY KEY,
                    kind text NOT NULL,
                    uri text,
                    title text NOT NULL,
                    raw_path text NOT NULL,
                    hash text NOT NULL,
                    metadata jsonb NOT NULL DEFAULT '{}',
                    created_at timestamptz NOT NULL DEFAULT now()
                );

                CREATE TABLE IF NOT EXISTS memory_chunks (
                    id uuid PRIMARY KEY,
                    source_id uuid REFERENCES memory_sources(id) ON DELETE CASCADE,
                    event_id uuid REFERENCES memory_events(id) ON DELETE CASCADE,
                    content text NOT NULL,
                    embedding vector(1536),
                    metadata jsonb NOT NULL DEFAULT '{}',
                    created_at timestamptz NOT NULL DEFAULT now()
                );

                CREATE TABLE IF NOT EXISTS memory_promotions (
                    id uuid PRIMARY KEY,
                    target_kind text NOT NULL,
                    target_id text NOT NULL,
                    status text NOT NULL DEFAULT 'pending',
                    reason text NOT NULL,
                    evidence jsonb NOT NULL DEFAULT '{}',
                    created_at timestamptz NOT NULL DEFAULT now()
                );

                CREATE INDEX IF NOT EXISTS memory_events_content_fts
                    ON memory_events USING gin (to_tsvector('english', content));
                CREATE INDEX IF NOT EXISTS memory_chunks_content_fts
                    ON memory_chunks USING gin (to_tsvector('english', content));
                CREATE INDEX IF NOT EXISTS memory_events_metadata_gin
                    ON memory_events USING gin (metadata);
                CREATE INDEX IF NOT EXISTS memory_sources_metadata_gin
                    ON memory_sources USING gin (metadata);

                ALTER TABLE memory_events
                    ADD COLUMN IF NOT EXISTS embedding vector(1536);
                CREATE INDEX IF NOT EXISTS memory_events_embedding_hnsw
                    ON memory_events USING hnsw (embedding vector_cosine_ops);
                CREATE INDEX IF NOT EXISTS memory_chunks_embedding_hnsw
                    ON memory_chunks USING hnsw (embedding vector_cosine_ops);
                "#,
            )
            .await;
        let unlock = self
            .client
            .batch_execute("SELECT pg_advisory_unlock(778390130512);")
            .await;
        result?;
        unlock?;
        Ok(())
    }

    async fn insert_event(
        &self,
        session_id: Option<&str>,
        agent: Option<&str>,
        role: &str,
        kind: &str,
        content: &str,
        metadata: &Value,
    ) -> Result<Uuid> {
        self.insert_event_with_embedding(session_id, agent, role, kind, content, metadata, None)
            .await
    }

    async fn insert_event_with_embedding(
        &self,
        session_id: Option<&str>,
        agent: Option<&str>,
        role: &str,
        kind: &str,
        content: &str,
        metadata: &Value,
        embedding: Option<&[f32]>,
    ) -> Result<Uuid> {
        let id = Uuid::new_v4();
        // Fail-soft: a rejected embedding (wrong dimension / non-finite)
        // degrades to the text-only insert, mirroring `insert_chunks`.
        let literal = embedding.and_then(|e| match pgvector_literal(e) {
            Ok(l) => Some(l),
            Err(err) => {
                tracing::warn!(
                    error = %err,
                    "agentic_memory: event embedding rejected; storing text-only"
                );
                None
            }
        });
        if let Some(embedding) = literal {
            self.client
                .execute(
                    "INSERT INTO memory_events
                     (id, session_id, agent, role, kind, content, metadata, embedding)
                     VALUES ($1, $2, $3, $4, $5, $6, $7, $8::text::vector)",
                    &[
                        &id,
                        &session_id,
                        &agent,
                        &role,
                        &kind,
                        &content,
                        &metadata,
                        &embedding,
                    ],
                )
                .await?;
        } else {
            self.client
                .execute(
                    "INSERT INTO memory_events
                     (id, session_id, agent, role, kind, content, metadata)
                     VALUES ($1, $2, $3, $4, $5, $6, $7)",
                    &[&id, &session_id, &agent, &role, &kind, &content, &metadata],
                )
                .await?;
        }
        Ok(id)
    }

    async fn insert_source(
        &self,
        kind: &str,
        uri: Option<&str>,
        title: &str,
        raw_path: &Path,
        hash: &str,
        metadata: &Value,
    ) -> Result<Uuid> {
        let id = Uuid::new_v4();
        let raw_path = raw_path.to_string_lossy().to_string();
        self.client
            .execute(
                "INSERT INTO memory_sources
                 (id, kind, uri, title, raw_path, hash, metadata)
                 VALUES ($1, $2, $3, $4, $5, $6, $7)",
                &[&id, &kind, &uri, &title, &raw_path, &hash, &metadata],
            )
            .await?;
        Ok(id)
    }

    async fn insert_chunks(
        &self,
        source_id: Uuid,
        content: &str,
        metadata: &Value,
        embedder: Option<&Embedder>,
    ) -> Result<usize> {
        let chunks = chunk_text(content, 1800, 200);
        for chunk in &chunks {
            let id = Uuid::new_v4();
            // Best-effort embed: a missing embedder, an embed error, or a
            // wrong-dimension vector all fall back to a text-only row so the
            // chunk is still FTS-recallable.
            let literal: Option<String> = match embedder {
                Some(e) => match e.embed(chunk.as_str()).await {
                    Ok(v) => match pgvector_literal(&v) {
                        Ok(l) => Some(l),
                        Err(err) => {
                            tracing::warn!(
                                error = %err,
                                "agentic_memory: chunk embedding rejected; storing text-only"
                            );
                            None
                        }
                    },
                    Err(err) => {
                        tracing::warn!(
                            error = %err,
                            "agentic_memory: chunk embed failed; storing text-only"
                        );
                        None
                    }
                },
                None => None,
            };
            match literal {
                Some(literal) => {
                    self.client
                        .execute(
                            "INSERT INTO memory_chunks
                             (id, source_id, content, metadata, embedding)
                             VALUES ($1, $2, $3, $4, $5::text::vector)",
                            &[&id, &source_id, chunk, &metadata, &literal],
                        )
                        .await?;
                }
                None => {
                    self.client
                        .execute(
                            "INSERT INTO memory_chunks
                             (id, source_id, content, metadata)
                             VALUES ($1, $2, $3, $4)",
                            &[&id, &source_id, chunk, &metadata],
                        )
                        .await?;
                }
            }
        }
        Ok(chunks.len())
    }

    /// Hybrid recall over `memory_events` + `memory_chunks`. When an embedding
    /// is supplied and matches at least one row, pgvector cosine ranking wins;
    /// otherwise (no embedding, embedding rejected, or zero vector hits) the
    /// query falls back to full-text search. Mirrors `recall_events_hybrid`.
    async fn recall(
        &self,
        query: &str,
        embedding: Option<&[f32]>,
        top_k: i64,
    ) -> Result<Vec<RecallRow>> {
        if let Some(literal) = embedding.and_then(|e| pgvector_literal(e).ok()) {
            let rows = self
                .client
                .query(
                    r#"
                    SELECT id::text, kind, content
                    FROM (
                        SELECT id, kind, content, embedding
                        FROM memory_events
                        WHERE embedding IS NOT NULL
                        UNION ALL
                        SELECT id, 'chunk' AS kind, content, embedding
                        FROM memory_chunks
                        WHERE embedding IS NOT NULL
                    ) hybrid
                    ORDER BY embedding <=> $1::text::vector
                    LIMIT $2
                    "#,
                    &[&literal, &top_k],
                )
                .await?;
            if !rows.is_empty() {
                return Ok(rows
                    .into_iter()
                    .map(|row| RecallRow {
                        id: row.get(0),
                        kind: row.get(1),
                        content: row.get(2),
                    })
                    .collect());
            }
        }

        let rows = self
            .client
            .query(
                r#"
                SELECT id::text, kind, content,
                       ts_rank_cd(to_tsvector('english', content), plainto_tsquery('english', $1)) AS rank
                FROM memory_events
                WHERE to_tsvector('english', content) @@ plainto_tsquery('english', $1)
                UNION ALL
                SELECT id::text, 'chunk' AS kind, content,
                       ts_rank_cd(to_tsvector('english', content), plainto_tsquery('english', $1)) AS rank
                FROM memory_chunks
                WHERE to_tsvector('english', content) @@ plainto_tsquery('english', $1)
                ORDER BY rank DESC
                LIMIT $2
                "#,
                &[&query, &top_k],
            )
            .await?;

        Ok(rows
            .into_iter()
            .map(|row| RecallRow {
                id: row.get(0),
                kind: row.get(1),
                content: row.get(2),
            })
            .collect())
    }

    async fn recall_events(
        &self,
        query: &str,
        kind: Option<&str>,
        session_id: Option<&str>,
        top_k: i64,
    ) -> Result<Vec<AgenticMemoryHit>> {
        let rows = self
            .client
            .query(
                r#"
                SELECT id::text,
                       kind,
                       content,
                       ts_rank_cd(to_tsvector('english', content), plainto_tsquery('english', $1)) AS rank,
                       metadata->>'step_id' AS step_id
                FROM memory_events
                WHERE to_tsvector('english', content) @@ plainto_tsquery('english', $1)
                  AND ($2::text IS NULL OR kind = $2)
                  AND ($3::text IS NULL OR session_id = $3)
                ORDER BY rank DESC, created_at DESC
                LIMIT $4
                "#,
                &[&query, &kind, &session_id, &top_k],
            )
            .await?;

        Ok(rows
            .into_iter()
            .map(|row| AgenticMemoryHit {
                id: row.get(0),
                kind: row.get(1),
                content: row.get(2),
                score: row.get::<_, f32>(3),
                step_id: row.get(4),
            })
            .collect())
    }

    async fn recall_events_hybrid(
        &self,
        query: &str,
        embedding: Option<&[f32]>,
        kind: Option<&str>,
        session_id: Option<&str>,
        top_k: i64,
    ) -> Result<Vec<AgenticMemoryHit>> {
        // Fail-soft: a rejected embedding (wrong dimension / non-finite)
        // degrades to FTS-only recall, mirroring `recall`.
        let literal = embedding.and_then(|e| match pgvector_literal(e) {
            Ok(l) => Some(l),
            Err(err) => {
                tracing::warn!(
                    error = %err,
                    "agentic_memory: query embedding rejected; falling back to FTS-only recall"
                );
                None
            }
        });
        if let Some(embedding) = literal {
            let rows = self
                .client
                .query(
                    r#"
                    SELECT id::text,
                           kind,
                           content,
                           (1.0 - (embedding <=> $1::text::vector))::real AS score,
                           metadata->>'step_id' AS step_id
                    FROM memory_events
                    WHERE embedding IS NOT NULL
                      AND ($2::text IS NULL OR kind = $2)
                      AND ($3::text IS NULL OR session_id = $3)
                    ORDER BY embedding <=> $1::text::vector, created_at DESC
                    LIMIT $4
                    "#,
                    &[&embedding, &kind, &session_id, &top_k],
                )
                .await?;

            let hits = rows
                .into_iter()
                .map(|row| AgenticMemoryHit {
                    id: row.get(0),
                    kind: row.get(1),
                    content: row.get(2),
                    score: row.get::<_, f32>(3),
                    step_id: row.get(4),
                })
                .collect::<Vec<_>>();
            if !hits.is_empty() {
                return Ok(hits);
            }
        }

        self.recall_events(query, kind, session_id, top_k).await
    }

    async fn insert_promotion(
        &self,
        target_kind: &str,
        target_id: &str,
        reason: &str,
        evidence: &Value,
    ) -> Result<Uuid> {
        let id = Uuid::new_v4();
        self.client
            .execute(
                "INSERT INTO memory_promotions
                 (id, target_kind, target_id, reason, evidence)
                 VALUES ($1, $2, $3, $4, $5)",
                &[&id, &target_kind, &target_id, &reason, &evidence],
            )
            .await?;
        Ok(id)
    }

    async fn promoted_items(&self, limit: i64) -> Result<Vec<PromotedItem>> {
        let rows = self
            .client
            .query(
                r#"
                SELECT p.target_kind, p.target_id, p.reason,
                       COALESCE(e.content, c.content, s.title, '') AS content
                FROM memory_promotions p
                LEFT JOIN memory_events e ON p.target_kind = 'event' AND e.id::text = p.target_id
                LEFT JOIN memory_chunks c ON p.target_kind = 'chunk' AND c.id::text = p.target_id
                LEFT JOIN memory_sources s ON p.target_kind = 'source' AND s.id::text = p.target_id
                ORDER BY p.created_at DESC
                LIMIT $1
                "#,
                &[&limit],
            )
            .await?;
        Ok(rows
            .into_iter()
            .map(|row| PromotedItem {
                target_kind: row.get(0),
                target_id: row.get(1),
                reason: row.get(2),
                content: row.get(3),
            })
            .collect())
    }

    async fn stats(&self) -> Result<MemoryStats> {
        let events = self.count("memory_events").await?;
        let sources = self.count("memory_sources").await?;
        let chunks = self.count("memory_chunks").await?;
        let promotions = self.count("memory_promotions").await?;
        Ok(MemoryStats {
            events,
            sources,
            chunks,
            promotions,
        })
    }

    async fn count(&self, table: &str) -> Result<i64> {
        let row = self
            .client
            .query_one(&format!("SELECT count(*)::bigint FROM {table}"), &[])
            .await?;
        Ok(row.get(0))
    }
}

struct RecallRow {
    id: String,
    kind: String,
    content: String,
}

#[derive(Clone)]
struct PromotedItem {
    target_kind: String,
    target_id: String,
    reason: String,
    content: String,
}

struct MemoryStats {
    events: i64,
    sources: i64,
    chunks: i64,
    promotions: i64,
}

fn opt_str<'a>(args: &'a Value, key: &str) -> Option<&'a str> {
    args.get(key)
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
}

/// Construct an embedder from the environment, mirroring `main.rs` and
/// `RagPlanner::embedder`. Returns `None` when `OPENROUTER_API_KEY` is unset
/// so callers fall back to FTS / text-only paths. The model is pinned to
/// `DEFAULT_EMBEDDING_MODEL` because the Postgres schema hardcodes
/// `vector(1536)` — see `ensure_schema` and `pgvector_literal`.
fn env_embedder() -> Option<Embedder> {
    std::env::var("OPENROUTER_API_KEY")
        .ok()
        .map(|api_key| Embedder::new(api_key, DEFAULT_EMBEDDING_MODEL.to_string()))
}

/// Embed a single query string. Fail-soft: a missing key or an embed error
/// logs at warn and returns `None`, dropping the caller to FTS / text-only.
async fn embed_query(text: &str) -> Option<Vec<f32>> {
    let embedder = env_embedder()?;
    match embedder.embed(text).await {
        Ok(v) => Some(v),
        Err(e) => {
            tracing::warn!(
                error = %e,
                "agentic_memory: query embed failed; falling back to FTS / text-only"
            );
            None
        }
    }
}

fn resolve_workspace_path(workspace: &Path, path: &str) -> PathBuf {
    let p = PathBuf::from(path);
    if p.is_absolute() {
        p
    } else {
        workspace.join(p)
    }
}

fn sha256_hex(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn pgvector_literal(embedding: &[f32]) -> Result<String> {
    if embedding.len() != 1536 {
        bail!(
            "agentic_memory: expected 1536-dim embedding, got {}",
            embedding.len()
        );
    }
    let mut out = String::with_capacity(embedding.len() * 10);
    out.push('[');
    for (i, value) in embedding.iter().enumerate() {
        if !value.is_finite() {
            bail!("agentic_memory: embedding contains non-finite value");
        }
        if i > 0 {
            out.push(',');
        }
        out.push_str(&value.to_string());
    }
    out.push(']');
    Ok(out)
}

fn write_raw(workspace: &Path, kind: &str, hash: &str, content: &str) -> Result<PathBuf> {
    let ext = if kind.contains("transcript") {
        "txt"
    } else {
        "md"
    };
    let path = workspace
        .join(RAW_ROOT)
        .join(kind)
        .join(format!("{hash}.{ext}"));
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&path, content)?;
    Ok(path)
}

fn chunk_text(content: &str, size: usize, overlap: usize) -> Vec<String> {
    if content.is_empty() {
        return vec![];
    }
    let chars: Vec<char> = content.chars().collect();
    let mut chunks = Vec::new();
    let mut start = 0;
    while start < chars.len() {
        let end = (start + size).min(chars.len());
        chunks.push(chars[start..end].iter().collect());
        if end == chars.len() {
            break;
        }
        start = end.saturating_sub(overlap);
    }
    chunks
}

fn wiki_page_path(workspace: &Path, title: &str) -> PathBuf {
    let slug = title
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect::<String>()
        .split('-')
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join("-");
    workspace.join(WIKI_ROOT).join(format!("{slug}.md"))
}

fn render_wiki_page(title: &str, items: &[PromotedItem]) -> String {
    let mut out = format!("# {title}\n\n");
    out.push_str("> LLM Wiki draft compiled from promoted Open Brain memories.\n\n");
    out.push_str("## Promoted Memory\n\n");
    if items.is_empty() {
        out.push_str("_No promoted items yet._\n");
        return out;
    }
    for item in items {
        out.push_str(&format!(
            "- `{}/{}`: {}  \n  Source: {}\n",
            item.target_kind,
            item.target_id,
            first_line(&item.content, 300),
            item.reason
        ));
    }
    out
}

/// Wrap an LLM-synthesised wiki body with a header and a deterministic
/// `## Sources` section. The Sources block lists every promoted memory token
/// so provenance is complete even if the model dropped an inline citation.
fn render_wiki_page_llm(title: &str, body: &str, items: &[PromotedItem]) -> String {
    let mut out = format!("# {title}\n\n");
    out.push_str(
        "> LLM Wiki page compiled from promoted Open Brain memories. Generated — \
         do not hand-edit; re-run `agentic_memory(compile_wiki)`.\n\n",
    );
    out.push_str(body.trim());
    out.push_str("\n\n## Sources\n\n");
    for item in items {
        out.push_str(&format!(
            "- `[mem:{}/{}]` — {}\n",
            item.target_kind,
            item.target_id,
            first_line(&item.reason, 200)
        ));
    }
    out
}

/// Build the (system, user) prompt pair for the wiki compiler. Each promoted
/// memory is rendered with its citation token `[mem:<kind>/<id>]` so the model
/// can cite it inline; long contents are capped to keep the prompt bounded.
fn compile_wiki_prompt(title: &str, items: &[PromotedItem]) -> (String, String) {
    let system = "You are the LLM Wiki compiler for the Tengu agentic-memory \
system. You are given a set of promoted memories — facts an agent or human \
marked as stable and high-value. Synthesise them into ONE coherent Markdown \
wiki page.\n\n\
Rules:\n\
- Output ONLY the Markdown body. Do NOT emit an H1 title — the harness adds it.\n\
- Cite every claim inline with its exact memory token, e.g. `[mem:event/<id>]`. \
Only use tokens from the input; never invent one.\n\
- Group related memories under `##` / `###` headings.\n\
- If two memories conflict, keep both and add a `> Contradiction:` note that \
cites both tokens.\n\
- Be terse and factual. No preamble, no conclusion."
        .to_string();

    let mut user = format!(
        "# Page title: {title}\n\n## Promoted memories ({})\n\n",
        items.len()
    );
    for item in items {
        let content: String = item.content.chars().take(1500).collect();
        let truncated = item.content.chars().count() > 1500;
        user.push_str(&format!(
            "### [mem:{}/{}]\n- promotion reason: {}\n- content:\n{}{}\n\n",
            item.target_kind,
            item.target_id,
            item.reason,
            content,
            if truncated { "\n…(truncated)" } else { "" },
        ));
    }
    user.push_str("Compile these into the wiki page body now.");
    (system, user)
}

/// Model slug for the wiki compiler chat call. Env-overridable; defaults to
/// the orchestrator's model. OpenRouter slug format (`anthropic/...`).
fn wiki_compiler_model() -> String {
    std::env::var("TENGU_WIKI_COMPILER_MODEL")
        .unwrap_or_else(|_| "anthropic/claude-sonnet-4-6".to_string())
}

/// Minimal OpenRouter chat-completion call — the chat-side sibling of
/// `Embedder` (direct HTTP, OpenAI-compatible, emits a `MetricsRecord`).
/// Used only by the wiki compiler; not a general-purpose engine.
async fn chat_complete(system: &str, user: &str) -> Result<String> {
    let api_key = std::env::var("OPENROUTER_API_KEY")
        .map_err(|_| anyhow!("OPENROUTER_API_KEY must be set for agentic_memory.compile_wiki"))?;
    let base_url = std::env::var("OPENROUTER_BASE_URL")
        .unwrap_or_else(|_| "https://openrouter.ai/api".to_string());
    let model = wiki_compiler_model();
    let client = crate::adapters::egress::policy().llm_api_client(
        reqwest::Client::builder()
            .connect_timeout(std::time::Duration::from_secs(30))
            .timeout(std::time::Duration::from_secs(120)),
    )?;

    let request = json!({
        "model": model.clone(),
        "messages": [
            {"role": "system", "content": system},
            {"role": "user", "content": user},
        ],
        "stream": false,
        "max_tokens": 4000,
    });

    let started = std::time::Instant::now();
    let prompt_chars = (system.chars().count() + user.chars().count()) as u32;
    let prompt_bytes = (system.len() + user.len()) as u32;

    let resp = client
        .post(format!(
            "{}/v1/chat/completions",
            base_url.trim_end_matches('/')
        ))
        .header("Authorization", format!("Bearer {api_key}"))
        .header("Content-Type", "application/json")
        .json(&request)
        .send()
        .await
        .context("agentic_memory: wiki compiler chat request failed")?;

    let status = resp.status();
    if !status.is_success() {
        let error_body = resp.text().await.unwrap_or_default();
        bail!("agentic_memory: wiki compiler chat API returned {status}: {error_body}");
    }

    let json: Value = resp
        .json()
        .await
        .context("agentic_memory: failed to parse wiki compiler chat response")?;
    let content = json["choices"][0]["message"]["content"]
        .as_str()
        .ok_or_else(|| anyhow!("agentic_memory: chat response missing choices[0].message.content"))?
        .to_string();

    let prompt_tokens = json["usage"]["prompt_tokens"]
        .as_u64()
        .map(|v| v as u32)
        .unwrap_or(prompt_chars / 4);
    let completion_tokens = json["usage"]["completion_tokens"]
        .as_u64()
        .map(|v| v as u32)
        .unwrap_or((content.chars().count() / 4) as u32);

    crate::adapters::metrics::record(crate::adapters::metrics::MetricsRecord {
        ts_unix: crate::adapters::metrics::now_unix(),
        session_id: std::env::var("TENGU_SESSION_ID").unwrap_or_else(|_| "-".to_string()),
        kind: crate::adapters::metrics::MetricsKind::WikiCompiler,
        agent: "wiki_compiler".to_string(),
        model: model.clone(),
        prompt_tokens,
        completion_tokens,
        total_tokens: prompt_tokens + completion_tokens,
        prompt_chars,
        prompt_bytes,
        response_chars: content.chars().count() as u32,
        latency_ms: started.elapsed().as_millis() as u64,
        layers: Vec::new(),
        step_id: None,
    });

    Ok(content)
}

fn first_line(s: &str, max_chars: usize) -> String {
    let line = s.lines().next().unwrap_or("").trim();
    let mut out = String::new();
    for c in line.chars().take(max_chars) {
        out.push(c);
    }
    if line.chars().count() > max_chars {
        out.push_str("...");
    }
    out
}

fn count_markdown_files(root: &Path) -> usize {
    let Ok(entries) = std::fs::read_dir(root) else {
        return 0;
    };
    entries
        .filter_map(|entry| entry.ok())
        .filter(|entry| entry.path().extension().and_then(|e| e.to_str()) == Some("md"))
        .count()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chunks_overlap() {
        let chunks = chunk_text("abcdefghij", 4, 1);
        assert_eq!(chunks, vec!["abcd", "defg", "ghij"]);
    }

    #[test]
    fn wiki_slug_is_stable() {
        let path = wiki_page_path(Path::new("/tmp/ws"), "Agent Memory / Open Brain");
        assert_eq!(
            path,
            PathBuf::from("/tmp/ws/.tengu/agentic-memory/wiki/agent-memory-open-brain.md")
        );
    }

    #[test]
    fn tool_def_uses_expected_name() {
        assert_eq!(tool_defs()[0].name, AGENTIC_MEMORY_TOOL_NAME);
    }

    #[test]
    fn pgvector_literal_rejects_wrong_dim() {
        assert!(pgvector_literal(&[1.0, 2.0]).is_err());
    }

    #[tokio::test]
    #[ignore = "requires TENGU_MEMORY_DATABASE_URL and a running Postgres/pgvector DB"]
    async fn postgres_capture_and_recall_smoke() {
        let store = PostgresMemoryStore::connect_from_env().await.unwrap();
        store.ensure_schema().await.unwrap();
        let marker = format!("agentic memory smoke {}", Uuid::new_v4());
        let id = store
            .insert_event(
                Some("smoke-session"),
                Some("smoke-agent"),
                "user",
                "preference",
                &marker,
                &json!({"smoke": true}),
            )
            .await
            .unwrap();
        let rows = store.recall(&marker, None, 5).await.unwrap();
        assert!(
            rows.iter().any(|row| row.id == id.to_string()),
            "expected inserted row in recall results"
        );
    }

    #[tokio::test]
    #[ignore = "requires TENGU_MEMORY_DATABASE_URL and a running Postgres/pgvector DB"]
    async fn postgres_vector_recall_smoke() {
        let store = PostgresMemoryStore::connect_from_env().await.unwrap();
        store.ensure_schema().await.unwrap();

        let marker = format!("agentic memory vector smoke {}", Uuid::new_v4());
        let mut embedding = vec![0.0; 1536];
        embedding[7] = 1.0;

        let id = store
            .insert_event_with_embedding(
                Some("smoke-session"),
                Some("smoke-agent"),
                "user",
                "message",
                &marker,
                &json!({"smoke": true, "mode": "vector"}),
                Some(&embedding),
            )
            .await
            .unwrap();

        let rows = store
            .recall_events_hybrid(
                "unrelated lexical query",
                Some(&embedding),
                Some("message"),
                None,
                5,
            )
            .await
            .unwrap();
        assert!(
            rows.first().is_some_and(|row| row.id == id.to_string()),
            "expected vector recall to find inserted row without lexical overlap"
        );
    }
}
