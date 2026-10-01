//! `CachedDecisionEngine` — a replay-deterministic `DecisionEngine`: every
//! answer is stored in one SQLite file, so a rerun over the same history
//! asks the model nothing and gets the same decisions. The backtest gate arm
//! (`docs/xlab-2026-10-01.md` § 7) runs it over `JevClient` on
//! `<state dir>/backtests/decision-cache.db`
//! (`bootstrap::decision::cached_decision_engine`).
//!
//! | Rule | How |
//! |---|---|
//! | Key | sha256, 64 lowercase hex chars (never shortened), of the canonical JSON `{"model","state","questions"}` ([`request_key`]): object keys sorted at every depth, so the key never depends on map insertion order (serde_json's `preserve_order` is off in `Cargo.lock` today; the key does not rely on that) |
//! | Hit | the stored `Decision`, verbatim — its `usage` is the original call's; spend comes from the counters |
//! | Miss, online | `inner.decide`, then `INSERT OR IGNORE` + read back: every caller gets the stored row (concurrent identical misses converge; a rerun returns the same parse of the same text) |
//! | Miss, offline (`inner = None`) | an error naming the key in full and the file |
//! | Live call fails | the error; nothing stored (the next run asks again) |
//! | Counters | `CacheStats` (`DecisionEngine::cache_stats`): hits · misses (= live calls when online) · errors (offline misses, failed live calls, store errors) |
//! | Store | `decisions(key PK, model, request, decision, created_at_ms)` — `request` = the hashed canonical text; WAL (`synchronous = NORMAL`) + busy_timeout 5 s, one connection, every call on `spawn_blocking` (pattern of `history_sqlite.rs`) |

// The gate arm (`application/backtest/gate.rs`) lands with the xlab wave.
#![cfg_attr(not(test), allow(dead_code))]

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::{anyhow, bail, Context, Result};
use async_trait::async_trait;
use rusqlite::{params, Connection, OptionalExtension};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use crate::domain::decision::{Decision, Question};
use crate::domain::observation::now_ms;
use crate::ports::decision::{CacheStats, DecisionEngine};

/// The cache's file name in `<state dir>/backtests/`
/// (`config::xmarket::backtests_dir`).
pub(crate) const DECISION_CACHE_DB: &str = "decision-cache.db";

const SCHEMA_SQL: &str = "
CREATE TABLE IF NOT EXISTS decisions (
  key TEXT PRIMARY KEY, model TEXT NOT NULL, request TEXT NOT NULL,
  decision TEXT NOT NULL, created_at_ms INTEGER NOT NULL);";

const GET_SQL: &str = "SELECT decision FROM decisions WHERE key = ?1";

const PUT_SQL: &str = "
INSERT OR IGNORE INTO decisions(key, model, request, decision, created_at_ms)
VALUES (?1, ?2, ?3, ?4, ?5)";

pub(crate) struct CachedDecisionEngine {
    /// The slug every key names (the inner engine's).
    model: String,
    /// Answers misses; `None` = offline (a miss is an error).
    inner: Option<Arc<dyn DecisionEngine>>,
    conn: Arc<Mutex<Connection>>,
    path: PathBuf,
    hits: AtomicU64,
    misses: AtomicU64,
    errors: AtomicU64,
}

impl CachedDecisionEngine {
    /// Open (creating it and its directory) the cache at `path` for
    /// `model`. `inner` answers misses (`None` = offline) and must serve
    /// `model`: the key names the model that was asked.
    pub(crate) fn open(
        path: &Path,
        model: &str,
        inner: Option<Arc<dyn DecisionEngine>>,
    ) -> Result<Self> {
        if let Some(engine) = &inner {
            if engine.model() != model {
                bail!(
                    "decision cache for model `{model}` over an engine for `{}`",
                    engine.model()
                );
            }
        }
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
        }
        let conn = Connection::open(path).with_context(|| format!("open {}", path.display()))?;
        conn.execute_batch(
            "PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL; PRAGMA busy_timeout=5000;",
        )?;
        conn.execute_batch(SCHEMA_SQL)?;
        Ok(Self {
            model: model.to_string(),
            inner,
            conn: Arc::new(Mutex::new(conn)),
            path: path.to_path_buf(),
            hits: AtomicU64::new(0),
            misses: AtomicU64::new(0),
            errors: AtomicU64::new(0),
        })
    }

    pub(crate) fn stats(&self) -> CacheStats {
        CacheStats {
            hits: self.hits.load(Ordering::Relaxed),
            misses: self.misses.load(Ordering::Relaxed),
            errors: self.errors.load(Ordering::Relaxed),
        }
    }

    async fn with_conn<T, F>(&self, f: F) -> Result<T>
    where
        T: Send + 'static,
        F: FnOnce(&Connection) -> Result<T> + Send + 'static,
    {
        let conn = Arc::clone(&self.conn);
        tokio::task::spawn_blocking(move || {
            let guard = conn
                .lock()
                .map_err(|e| anyhow!("decision cache lock: {e}"))?;
            f(&guard)
        })
        .await
        .context("decision cache task")?
    }

    async fn lookup(&self, key: &str) -> Result<Option<Decision>> {
        let key = key.to_string();
        self.with_conn(move |c| {
            let text: Option<String> = c
                .query_row(GET_SQL, params![key], |r| r.get(0))
                .optional()?;
            text.map(|t| parse_row(&key, &t)).transpose()
        })
        .await
    }

    /// Store `d` under `key` unless a row is there already; returns the
    /// stored row either way.
    async fn store(&self, key: &str, request: String, d: &Decision) -> Result<Decision> {
        let (key, model, text) = (
            key.to_string(),
            self.model.clone(),
            serde_json::to_string(d)?,
        );
        self.with_conn(move |c| {
            c.execute(PUT_SQL, params![key, model, request, text, now_ms()])
                .with_context(|| format!("decision cache write {key}"))?;
            let stored: String = c.query_row(GET_SQL, params![key], |r| r.get(0))?;
            parse_row(&key, &stored)
        })
        .await
    }

    async fn answer(
        &self,
        state: &Value,
        questions: &BTreeMap<String, Question>,
    ) -> Result<Decision> {
        let (key, request) = request_key(&self.model, state, questions);
        if let Some(d) = self.lookup(&key).await? {
            self.hits.fetch_add(1, Ordering::Relaxed);
            return Ok(d);
        }
        self.misses.fetch_add(1, Ordering::Relaxed);
        let Some(inner) = &self.inner else {
            bail!(
                "decision cache miss (offline): key {key} for model `{}` is not in {} — \
                 run without --offline to ask the model",
                self.model,
                self.path.display()
            );
        };
        let fresh = inner
            .decide(state, questions)
            .await
            .with_context(|| format!("decision cache miss {key}: live call"))?;
        self.store(&key, request, &fresh).await
    }
}

fn parse_row(key: &str, text: &str) -> Result<Decision> {
    serde_json::from_str(text).with_context(|| format!("decision cache row {key} does not parse"))
}

/// `v` as JSON text with object keys sorted at every depth; arrays keep
/// their order, scalars print as serde_json prints them.
pub(crate) fn canonical_json(v: &Value) -> String {
    let mut out = String::new();
    write_canonical(v, &mut out);
    out
}

fn write_canonical(v: &Value, out: &mut String) {
    match v {
        Value::Object(o) => {
            let mut keys: Vec<&String> = o.keys().collect();
            keys.sort();
            out.push('{');
            for (i, k) in keys.into_iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                out.push_str(&Value::String(k.clone()).to_string());
                out.push(':');
                write_canonical(&o[k.as_str()], out);
            }
            out.push('}');
        }
        Value::Array(a) => {
            out.push('[');
            for (i, x) in a.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_canonical(x, out);
            }
            out.push(']');
        }
        scalar => out.push_str(&scalar.to_string()),
    }
}

/// The cache key of one decisions request — sha256 hex (64 chars) of the
/// canonical `{"model","state","questions"}` — and that canonical text.
pub(crate) fn request_key(
    model: &str,
    state: &Value,
    questions: &BTreeMap<String, Question>,
) -> (String, String) {
    let request = canonical_json(&json!({
        "model": model,
        "state": state,
        "questions": questions,
    }));
    (format!("{:x}", Sha256::digest(request.as_bytes())), request)
}

#[async_trait]
impl DecisionEngine for CachedDecisionEngine {
    fn model(&self) -> &str {
        &self.model
    }

    async fn decide(
        &self,
        state: &Value,
        questions: &BTreeMap<String, Question>,
    ) -> Result<Decision> {
        let answered = self.answer(state, questions).await;
        if answered.is_err() {
            self.errors.fetch_add(1, Ordering::Relaxed);
        }
        answered
    }

    fn cache_stats(&self) -> Option<CacheStats> {
        Some(self.stats())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MODEL: &str = "~typesafe/jev-latest";

    /// Counts calls; answers `take` (the probed wire shape, a new id per
    /// call) or fails like an out-of-credits endpoint. With a barrier, each
    /// call waits for the others before answering.
    #[derive(Default)]
    struct Counting {
        calls: AtomicU64,
        fail: bool,
        barrier: Option<tokio::sync::Barrier>,
    }

    impl Counting {
        fn calls(&self) -> u64 {
            self.calls.load(Ordering::SeqCst)
        }
    }

    #[async_trait]
    impl DecisionEngine for Counting {
        fn model(&self) -> &str {
            MODEL
        }
        async fn decide(&self, _s: &Value, _q: &BTreeMap<String, Question>) -> Result<Decision> {
            let n = self.calls.fetch_add(1, Ordering::SeqCst) + 1;
            if let Some(b) = &self.barrier {
                b.wait().await;
            }
            if self.fail {
                bail!("decisions endpoint HTTP 402: insufficient credits");
            }
            Ok(serde_json::from_value(json!({
                "id": format!("gen-dec-{n}"),
                "model": "typesafe/jev-1.13-20260917",
                "answers": {"next_action": {"type": "choice", "choice": "take",
                    "probabilities": {"take": 0.83, "skip": 0.15, "ask_architect": 0.02},
                    "confidence": 0.74}},
                "usage": {"input_tokens": 406, "output_tokens": 43, "cost": 0.000017052}
            }))?)
        }
    }

    fn questions() -> BTreeMap<String, Question> {
        BTreeMap::from([(
            "next_action".to_string(),
            Question::Choice {
                instructions: "pick".into(),
                criteria: BTreeMap::from([
                    ("take".into(), "trade".into()),
                    ("skip".into(), "no trade".into()),
                ]),
            },
        )])
    }

    fn state(signal_bps: f64) -> Value {
        json!({
            "goal": "fade the weekend move",
            "event": {"instrument": "hyperliquid:xyz:TSLA", "side": "sell",
                      "signal_bps": signal_bps, "decided_at": "2026-09-27T22:00:00Z"},
            "history": [],
            "step": 0
        })
    }

    fn db(dir: &tempfile::TempDir) -> PathBuf {
        dir.path().join("backtests").join(DECISION_CACHE_DB)
    }

    fn stats(hits: u64, misses: u64, errors: u64) -> Option<CacheStats> {
        Some(CacheStats {
            hits,
            misses,
            errors,
        })
    }

    #[tokio::test]
    async fn same_request_asks_the_inner_engine_once() {
        let dir = tempfile::tempdir().unwrap();
        let inner = Arc::new(Counting::default());
        let cache = CachedDecisionEngine::open(&db(&dir), MODEL, Some(inner.clone())).unwrap();
        let first = cache.decide(&state(-182.5), &questions()).await.unwrap();
        let again = cache.decide(&state(-182.5), &questions()).await.unwrap();
        assert_eq!(again, first);
        assert_eq!(first.id, "gen-dec-1");
        assert_eq!(first.answers["next_action"].probabilities["take"], 0.83);
        assert_eq!(inner.calls(), 1);
        // Another state is another key.
        let other = cache.decide(&state(95.0), &questions()).await.unwrap();
        assert_eq!(other.id, "gen-dec-2");
        assert_eq!(inner.calls(), 2);
        assert_eq!(cache.cache_stats(), stats(1, 2, 0));
    }

    /// The key is a function of the value, never of map insertion order;
    /// pinned so a format change (every cached decision lost) is noticed.
    #[test]
    fn key_ignores_map_insertion_order() {
        let mut inner_a = serde_json::Map::new();
        inner_a.insert("d".into(), json!(3));
        inner_a.insert("e".into(), Value::Null);
        let mut inner_b = serde_json::Map::new();
        inner_b.insert("e".into(), Value::Null);
        inner_b.insert("d".into(), json!(3));
        let mut a = serde_json::Map::new();
        a.insert("a".into(), json!(1));
        a.insert("b".into(), json!({"c": [2, Value::Object(inner_a)]}));
        let mut b = serde_json::Map::new();
        b.insert("b".into(), json!({"c": [2, Value::Object(inner_b)]}));
        b.insert("a".into(), json!(1));
        let (key_a, text_a) = request_key(MODEL, &Value::Object(a), &questions());
        let (key_b, text_b) = request_key(MODEL, &Value::Object(b), &questions());
        assert_eq!((&key_a, &text_a), (&key_b, &text_b));
        assert_eq!(
            text_a,
            r#"{"model":"~typesafe/jev-latest","questions":{"next_action":{"criteria":{"skip":"no trade","take":"trade"},"instructions":"pick","type":"choice"}},"state":{"a":1,"b":{"c":[2,{"d":3,"e":null}]}}}"#
        );
        assert_eq!(
            key_a,
            "54067e7423c6b106f590147b30f72b2499fa4a9aabe3586c3a442a4d46d2e21b"
        );
        // Keys sort and escape like JSON; arrays keep their order.
        assert_eq!(
            canonical_json(&json!({"z": [3, 1], "a\"b": {"y": 1.5, "x": "é"}})),
            r#"{"a\"b":{"x":"é","y":1.5},"z":[3,1]}"#
        );
        // The model is part of the key.
        let other = request_key("typesafe/jev-1.13-20260917", &json!({}), &questions());
        assert_ne!(other.0, request_key(MODEL, &json!({}), &questions()).0);
    }

    #[tokio::test]
    async fn offline_miss_errors_naming_the_key() {
        let dir = tempfile::tempdir().unwrap();
        let cache = CachedDecisionEngine::open(&db(&dir), MODEL, None).unwrap();
        let err = cache
            .decide(&state(-182.5), &questions())
            .await
            .unwrap_err();
        let (key, _) = request_key(MODEL, &state(-182.5), &questions());
        assert_eq!(key.len(), 64);
        let msg = format!("{err:#}");
        assert!(msg.contains("offline") && msg.contains(&key), "{msg}");
        assert!(msg.contains(&db(&dir).display().to_string()), "{msg}");
        assert_eq!(cache.cache_stats(), stats(0, 1, 1));
    }

    #[tokio::test]
    async fn decisions_persist_across_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let inner = Arc::new(Counting::default());
        let online = CachedDecisionEngine::open(&db(&dir), MODEL, Some(inner.clone())).unwrap();
        let first = online.decide(&state(-182.5), &questions()).await.unwrap();
        drop(online);
        let offline = CachedDecisionEngine::open(&db(&dir), MODEL, None).unwrap();
        let again = offline.decide(&state(-182.5), &questions()).await.unwrap();
        assert_eq!(again, first, "the stored decision, verbatim");
        assert_eq!(inner.calls(), 1);
        assert_eq!(offline.cache_stats(), stats(1, 0, 0));
    }

    #[tokio::test]
    async fn a_failed_live_call_is_not_stored() {
        let dir = tempfile::tempdir().unwrap();
        let failing = Arc::new(Counting {
            fail: true,
            ..Default::default()
        });
        let cache = CachedDecisionEngine::open(&db(&dir), MODEL, Some(failing)).unwrap();
        let err = cache
            .decide(&state(-182.5), &questions())
            .await
            .unwrap_err();
        let (key, _) = request_key(MODEL, &state(-182.5), &questions());
        let msg = format!("{err:#}");
        assert!(msg.contains(&key) && msg.contains("HTTP 402"), "{msg}");
        assert_eq!(cache.cache_stats(), stats(0, 1, 1));
        drop(cache);
        // The next run asks again.
        let inner = Arc::new(Counting::default());
        let cache = CachedDecisionEngine::open(&db(&dir), MODEL, Some(inner.clone())).unwrap();
        cache.decide(&state(-182.5), &questions()).await.unwrap();
        assert_eq!(inner.calls(), 1);
    }

    /// Two identical misses in flight: both call the model, the first
    /// answer stored wins, both callers get it.
    #[tokio::test]
    async fn concurrent_identical_misses_converge() {
        let dir = tempfile::tempdir().unwrap();
        let inner = Arc::new(Counting {
            barrier: Some(tokio::sync::Barrier::new(2)),
            ..Default::default()
        });
        let cache = CachedDecisionEngine::open(&db(&dir), MODEL, Some(inner.clone())).unwrap();
        let (s, q) = (state(-182.5), questions());
        let (a, b) = tokio::join!(cache.decide(&s, &q), cache.decide(&s, &q));
        assert_eq!(inner.calls(), 2);
        assert_eq!(a.unwrap(), b.unwrap());
        assert_eq!(cache.cache_stats(), stats(0, 2, 0));
    }

    #[test]
    fn the_inner_engine_must_serve_the_model() {
        let dir = tempfile::tempdir().unwrap();
        let inner: Arc<dyn DecisionEngine> = Arc::new(Counting::default());
        let err = CachedDecisionEngine::open(&db(&dir), "typesafe/jev-1.13-20260917", Some(inner))
            .err()
            .unwrap();
        assert!(format!("{err}").contains(MODEL), "{err}");
    }
}
