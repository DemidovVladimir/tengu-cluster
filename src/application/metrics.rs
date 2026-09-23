//! Metrics sink — the process-global broadcast bus every LLM / embedding
//! call reports to (`record`). Record types live in `domain::metrics`; the
//! doctrine and design notes are there too.

use std::sync::OnceLock;
use tokio::sync::broadcast;

use crate::domain::metrics::MetricsRecord;

static GLOBAL_SINK: OnceLock<broadcast::Sender<MetricsRecord>> = OnceLock::new();

/// Default broadcast channel capacity — same as the orchestrator event bus.
/// Subscribers that lag get `RecvError::Lagged` and skip ahead.
pub const DEFAULT_BUS_CAPACITY: usize = 256;

/// Install the process-global metrics sink. Idempotent — the first call wins,
/// subsequent calls are silent no-ops. `build_orchestrator` calls this once
/// per orchestrator construction; CLI subcommands that don't run the
/// orchestrator simply leave the sink uninstalled, in which case `record()`
/// only emits the tracing baseline.
pub fn install_global_sink() -> broadcast::Sender<MetricsRecord> {
    let new_tx = broadcast::channel::<MetricsRecord>(DEFAULT_BUS_CAPACITY).0;
    match GLOBAL_SINK.set(new_tx.clone()) {
        Ok(()) => new_tx,
        // Already installed — return a clone of the existing sender so the
        // caller can subscribe.
        Err(_) => GLOBAL_SINK
            .get()
            .expect("set failed but get returned None")
            .clone(),
    }
}

/// Subscribe to the global metrics stream. Returns `None` if the sink is not
/// yet installed (caller can fall back to logs-only mode).
///
/// Today's wiring goes through the broadcast `Sender` returned from
/// `install_global_sink` (the build_orchestrator bridge subscribes on the
/// sender directly), so this helper has no internal callers — the binary
/// has no `[lib]` target, which makes rustc treat all `pub fn` items as
/// dead. Kept for future subscribers (a `tengu metrics` CLI subcommand,
/// an HTTP exporter, an external eval harness).
#[allow(dead_code)]
pub fn subscribe() -> Option<broadcast::Receiver<MetricsRecord>> {
    GLOBAL_SINK.get().map(|tx| tx.subscribe())
}

/// Record a metrics event. Always emits a structured `tracing::info!` line;
/// additionally broadcasts on the global sink when one is installed. Never
/// returns an error — metrics are observability and must not gate progress.
pub fn record(rec: MetricsRecord) {
    // Always emit the tracing baseline. The user's primary surface per the
    // requirements doc — visible via `RUST_LOG=tengu=info`.
    tracing::info!(
        kind = rec.kind.as_str(),
        agent = %rec.agent,
        model = %rec.model,
        prompt_tokens = rec.prompt_tokens,
        completion_tokens = rec.completion_tokens,
        total_tokens = rec.total_tokens,
        prompt_chars = rec.prompt_chars,
        response_chars = rec.response_chars,
        latency_ms = rec.latency_ms,
        layer_count = rec.layers.len(),
        step_id = ?rec.step_id,
        session_id = %rec.session_id,
        "metrics"
    );
    // Per-layer detail at debug level so the info-line stays narrow but the
    // detail is reachable when investigating bloat.
    if !rec.layers.is_empty() {
        for l in &rec.layers {
            tracing::debug!(
                kind = rec.kind.as_str(),
                agent = %rec.agent,
                layer = %l.name,
                chars = l.chars,
                bytes = l.bytes,
                "metrics.layer"
            );
        }
    }

    if let Some(tx) = GLOBAL_SINK.get() {
        // `send` returns Err only when there are zero subscribers — that's
        // fine, we always wanted the tracing line above to be the
        // load-bearing surface.
        let _ = tx.send(rec);
    }
}
