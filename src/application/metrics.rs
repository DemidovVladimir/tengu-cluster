//! Metrics sink — the process-global broadcast bus every LLM / embedding
//! call reports to (`record`), and the forwarder that hands it on with a
//! barrier ([`forward_sink`] → [`MetricsBarrier`]). Record types live in
//! `domain::metrics`; the doctrine and design notes are there too.

use std::sync::OnceLock;
use tokio::sync::broadcast::error::{RecvError, TryRecvError};
use tokio::sync::{broadcast, mpsc, oneshot};

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
/// sender directly), so this helper has no internal callers — the `tengu`
/// lib exports only `run`, so rustc treats every unreached `pub fn` as
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
        cached_input_tokens = rec.cached_input_tokens,
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

/// Put a record that was already logged elsewhere on the global sink only:
/// a `run-agent` child logs its own records (forwarded into this process's
/// log), so the parent re-emits them without a second `metrics` line.
pub fn forward(rec: MetricsRecord) {
    if let Some(tx) = GLOBAL_SINK.get() {
        let _ = tx.send(rec);
    }
}

/// The barrier of one [`forward_sink`] forwarder. Dropping it ends the
/// forwarder.
pub(crate) struct MetricsBarrier(mpsc::UnboundedSender<oneshot::Sender<()>>);

impl MetricsBarrier {
    /// Resolves once the forwarder has handed on every record put on the
    /// global sink before this call (at once when the forwarder has ended).
    /// What a surface awaits before it closes its trace: a record still in
    /// the sink would land after the closing event.
    pub(crate) async fn flush(&self) {
        let (ack, acked) = oneshot::channel();
        if self.0.send(ack).is_ok() {
            let _ = acked.await;
        }
    }
}

/// Subscribe to the global sink now (installing it), then hand each record
/// to `on_record` in order, on a task of its own, until `on_record` returns
/// `false` or the returned barrier is dropped. A flush drains what the sink
/// already holds before it answers: a record sent before the flush was sent
/// is handed on before it resolves.
pub(crate) fn forward_sink(
    mut on_record: impl FnMut(MetricsRecord) -> bool + Send + 'static,
) -> MetricsBarrier {
    let mut records = install_global_sink().subscribe();
    let (tx, mut flushes) = mpsc::unbounded_channel::<oneshot::Sender<()>>();
    tokio::spawn(async move {
        loop {
            tokio::select! {
                biased;
                got = records.recv() => match got {
                    Ok(rec) => {
                        if !on_record(rec) {
                            break;
                        }
                    }
                    Err(RecvError::Lagged(n)) => {
                        tracing::warn!(dropped = n, "metrics sink subscriber lagged");
                    }
                    Err(RecvError::Closed) => break,
                },
                flush = flushes.recv() => {
                    // The barrier is gone: so is whoever reads what we hand on.
                    let Some(ack) = flush else { break };
                    loop {
                        match records.try_recv() {
                            Ok(rec) => {
                                if !on_record(rec) {
                                    let _ = ack.send(());
                                    return;
                                }
                            }
                            Err(TryRecvError::Lagged(n)) => {
                                tracing::warn!(dropped = n, "metrics sink subscriber lagged");
                            }
                            Err(_) => break,
                        }
                    }
                    let _ = ack.send(());
                }
            }
        }
    });
    MetricsBarrier(tx)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::metrics::MetricsKind;
    use std::sync::{Arc, Mutex};

    fn rec(session: &str, total_tokens: u32) -> MetricsRecord {
        MetricsRecord {
            ts_unix: 0,
            session_id: session.into(),
            kind: MetricsKind::Subagent,
            agent: "researcher".into(),
            model: "m".into(),
            prompt_tokens: 1,
            cached_input_tokens: 0,
            completion_tokens: 2,
            total_tokens,
            prompt_chars: 0,
            prompt_bytes: 0,
            response_chars: 0,
            latency_ms: 0,
            layers: Vec::new(),
            step_id: None,
        }
    }

    /// `forward` puts a child's record on the sink (the TUI panel and the
    /// event bridge still see it) without logging it a second time.
    #[test]
    fn forward_reaches_the_sink() {
        let mut rx = install_global_sink().subscribe();
        let session = format!("fwd-{}", uuid::Uuid::new_v4());
        forward(rec(&session, 3));
        // Other tests share the process-global sink: skip their records.
        let got = std::iter::from_fn(|| rx.try_recv().ok()).find(|r| r.session_id == session);
        assert_eq!(got.map(|r| r.total_tokens), Some(3));
    }

    /// The barrier: a record sent just before `flush` has been handed on
    /// when it resolves — on a current-thread runtime the forwarder has not
    /// run once before the flush, so nothing but the barrier orders them.
    /// Dropping the barrier ends the forwarder (a later flush of a clone
    /// would resolve at once; there is none).
    #[tokio::test]
    async fn flush_resolves_after_every_record_sent_before_it() {
        let session = format!("barrier-{}", uuid::Uuid::new_v4());
        let seen = Arc::new(Mutex::new(Vec::new()));
        let s = Arc::clone(&seen);
        let mine = session.clone();
        let barrier = forward_sink(move |r| {
            if r.session_id == mine {
                s.lock().unwrap().push(r.total_tokens);
            }
            true
        });
        record(rec(&session, 1));
        forward(rec(&session, 2));
        assert!(seen.lock().unwrap().is_empty(), "the forwarder has not run");
        barrier.flush().await;
        assert_eq!(*seen.lock().unwrap(), [1, 2]);
        // A forwarder whose consumer is gone ends; its flush returns at once.
        let stop = forward_sink(|_| false);
        record(rec(&session, 3));
        stop.flush().await;
        stop.flush().await;
    }
}
