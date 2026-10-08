//! Execution-trace store — one JSONL file per recording (`ports/trace.rs`,
//! `domain/trace.rs`): `<TENGU_HOME>/logs/trace/<sandbox>/<run_id>.jsonl`.
//!
//! | Piece | Rule |
//! |---|---|
//! | [`JsonlTraceSink::open`] | a fresh `run_id` (UUID v4) = a new file, first line `run.opened` (`{kind, pid}`; node `runtime:<sandbox>` for a `tengu run`, none for a decide — its `trigger.*` root names the trigger); a restart never appends to an old run |
//! | [`JsonlTraceSink::emit`](TraceSink::emit) | under one lock: next `seq`, stamp (`RunContext::stamp`), payload redacted — `domain::trace::scrub_value`: every string and object key, secrets then URLs — then bounded ([`MAX_PAYLOAD_BYTES`], whole fields dropped), one `write_all` on an append-mode file (`decision_loop::append_line`); a failed write keeps the `seq` (no gap) and only warns |
//! | [`JsonlTraceReader`] | one sandbox dir; a `run_id` must be a lowercase UUID (no path from a request reaches the disk); unparsable lines and a partial last line are skipped; `follow` tails the file every 250 ms into a bounded channel (256): a slow reader holds the tail, nothing is dropped, a reconnect resumes by `seq` |
//!
//! Nothing here deletes a run; `tengu prune` removes `<TENGU_HOME>/logs`
//! wholesale (the lab runs under its own `TENGU_HOME`).

use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use serde_json::json;
use tokio::sync::mpsc::{self, Receiver};
use tracing::warn;

use crate::application::decision_loop::append_line;
use crate::domain::observation::now_ms;
use crate::domain::secrets::SecretRegistry;
use crate::domain::trace::{
    bound_payload, order_events, scrub_value, Component, EventDraft, ExecutionEvent, RunContext,
    RunKind, RunSummary, Status, MAX_PAYLOAD_BYTES, RUN_OPENED,
};
use crate::ports::trace::{TraceReader, TraceSink};

/// Poll period of [`TraceReader::follow`].
const FOLLOW_POLL: Duration = Duration::from_millis(250);
/// Events a follower may hold unread before the tail waits.
const FOLLOW_CAPACITY: usize = 256;

/// `<home>/logs/trace`.
pub(crate) fn trace_root(home: &Path) -> PathBuf {
    home.join("logs").join("trace")
}

/// A sandbox name as a directory: letters, digits, `.`, `_`, `-`; not
/// starting with `.`.
pub(crate) fn check_sandbox_name(name: &str) -> Result<()> {
    let ok = !name.is_empty()
        && !name.starts_with('.')
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'));
    if !ok {
        bail!("sandbox name `{name}`: letters, digits, `.`, `_`, `-` only");
    }
    Ok(())
}

/// A run id as a file name: a lowercase hyphenated UUID, nothing else.
pub(crate) fn check_run_id(run_id: &str) -> Result<()> {
    match uuid::Uuid::parse_str(run_id) {
        Ok(u) if u.hyphenated().to_string() == run_id => Ok(()),
        _ => bail!(
            "run id `{run_id}`: not a run id (a lowercase UUID, as `tengu trace runs` prints)"
        ),
    }
}

/// `<root>/<sandbox>/<run_id>.jsonl`.
pub(crate) fn run_file(root: &Path, sandbox: &str, run_id: &str) -> PathBuf {
    root.join(sandbox).join(format!("{run_id}.jsonl"))
}

/// One recording (module table).
pub(crate) struct JsonlTraceSink {
    ctx: RunContext,
    path: PathBuf,
    secrets: Arc<SecretRegistry>,
    max_payload_bytes: usize,
    /// The last written `seq`; held across the write so `seq` = file order.
    last_seq: Mutex<u64>,
}

impl JsonlTraceSink {
    /// Start a recording of `sandbox` under `root` ([`trace_root`]): a new
    /// `run_id`, its file created with the `run.opened` event.
    pub(crate) fn open(
        root: &Path,
        sandbox: &str,
        config_hash: Option<&str>,
        runtime_id: Option<&str>,
        kind: RunKind,
        secrets: Arc<SecretRegistry>,
    ) -> Result<Self> {
        check_sandbox_name(sandbox)?;
        let run_id = uuid::Uuid::new_v4().to_string();
        let path = run_file(root, sandbox, &run_id);
        let dir = path.parent().unwrap_or(root);
        std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
        let sink = Self {
            ctx: RunContext {
                sandbox: sandbox.to_string(),
                config_hash: config_hash.map(str::to_string),
                runtime_id: runtime_id.map(str::to_string),
                run_id,
            },
            path,
            secrets,
            max_payload_bytes: MAX_PAYLOAD_BYTES,
            last_seq: Mutex::new(0),
        };
        // A decide run's trigger node (`trigger:decide` or
        // `trigger:map/<sha256>`) is named by its `trigger.*` root event.
        let node = match kind {
            RunKind::Run => Some(crate::domain::workflow::node_id::runtime(sandbox)),
            RunKind::Decide | RunKind::Studio => None,
        };
        let mut opened = EventDraft::new(Component::Runtime, RUN_OPENED, Status::Ok)
            .payload(json!({"kind": kind.as_str(), "pid": std::process::id()}));
        opened.node_id = node;
        sink.write(opened)
            .with_context(|| format!("write {}", sink.path.display()))?;
        Ok(sink)
    }

    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    fn write(&self, draft: EventDraft) -> Result<String> {
        let mut last = self.last_seq.lock().unwrap_or_else(|p| p.into_inner());
        let seq = *last + 1;
        let mut ev = self.ctx.stamp(seq, now_ms(), draft);
        scrub_value(&mut ev.payload, &self.secrets);
        ev.payload = bound_payload(std::mem::take(&mut ev.payload), self.max_payload_bytes).0;
        let line = format!("{}\n", serde_json::to_string(&ev)?);
        append_line(&self.path, &line)?;
        *last = seq;
        Ok(ev.event_id)
    }
}

impl TraceSink for JsonlTraceSink {
    fn emit(&self, draft: EventDraft) -> Option<String> {
        match self.write(draft) {
            Ok(id) => Some(id),
            Err(e) => {
                warn!(path = %self.path.display(), error = %format!("{e:#}"), "trace write failed");
                None
            }
        }
    }

    fn run_id(&self) -> Option<&str> {
        Some(&self.ctx.run_id)
    }
}

/// Complete lines of `bytes` as events (unparsable lines skipped); the
/// bytes after the last newline stay in `bytes`.
fn drain_events(bytes: &mut Vec<u8>) -> Vec<ExecutionEvent> {
    let mut out = Vec::new();
    while let Some(i) = bytes.iter().position(|b| *b == b'\n') {
        let line: Vec<u8> = bytes.drain(..=i).collect();
        if let Ok(ev) = serde_json::from_slice::<ExecutionEvent>(&line) {
            out.push(ev);
        }
    }
    out
}

/// One sandbox's recordings (module table).
pub(crate) struct JsonlTraceReader {
    dir: PathBuf,
    poll: Duration,
    capacity: usize,
}

impl JsonlTraceReader {
    /// The reader of `<root>/<sandbox>/`.
    pub(crate) fn new(root: &Path, sandbox: &str) -> Result<Self> {
        check_sandbox_name(sandbox)?;
        Ok(Self {
            dir: root.join(sandbox),
            poll: FOLLOW_POLL,
            capacity: FOLLOW_CAPACITY,
        })
    }

    #[cfg(test)]
    fn tuned(mut self, poll: Duration, capacity: usize) -> Self {
        self.poll = poll;
        self.capacity = capacity;
        self
    }

    fn file(&self, run_id: &str) -> Result<PathBuf> {
        check_run_id(run_id)?;
        Ok(self.dir.join(format!("{run_id}.jsonl")))
    }

    fn read_all(path: &Path) -> Result<Vec<ExecutionEvent>> {
        let mut bytes = std::fs::read(path).with_context(|| format!("read {}", path.display()))?;
        let mut evs = drain_events(&mut bytes);
        order_events(&mut evs);
        Ok(evs)
    }
}

impl TraceReader for JsonlTraceReader {
    fn runs(&self) -> Result<Vec<RunSummary>> {
        let entries = match std::fs::read_dir(&self.dir) {
            Ok(e) => e,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(e).with_context(|| format!("read {}", self.dir.display())),
        };
        let mut out = Vec::new();
        for entry in entries.flatten() {
            let path = entry.path();
            let Some(stem) = path
                .file_name()
                .and_then(|n| n.to_str())
                .and_then(|n| n.strip_suffix(".jsonl"))
            else {
                continue;
            };
            if check_run_id(stem).is_err() {
                continue;
            }
            if let Some(s) = RunSummary::of(&Self::read_all(&path)?) {
                out.push(s);
            }
        }
        out.sort_by(|a, b| (a.started_ms, &a.run_id).cmp(&(b.started_ms, &b.run_id)));
        Ok(out)
    }

    fn events(&self, run_id: &str, after_seq: u64, limit: usize) -> Result<Vec<ExecutionEvent>> {
        let path = self.file(run_id)?;
        Ok(Self::read_all(&path)?
            .into_iter()
            .filter(|e| e.seq > after_seq)
            .take(limit)
            .collect())
    }

    fn follow(&self, run_id: &str, after_seq: u64) -> Result<Receiver<ExecutionEvent>> {
        let path = self.file(run_id)?;
        let (tx, rx) = mpsc::channel(self.capacity);
        let poll = self.poll;
        tokio::spawn(async move {
            let (mut pos, mut last, mut pending) = (0u64, after_seq, Vec::<u8>::new());
            loop {
                let read = tokio::task::spawn_blocking({
                    let path = path.clone();
                    move || -> std::io::Result<(u64, Vec<u8>)> {
                        let mut f = std::fs::File::open(&path)?;
                        let len = f.metadata()?.len();
                        let start = if len < pos { 0 } else { pos };
                        let mut buf = Vec::new();
                        f.seek(SeekFrom::Start(start))?;
                        f.take(len - start).read_to_end(&mut buf)?;
                        Ok((start, buf))
                    }
                })
                .await;
                if let Ok(Ok((start, buf))) = read {
                    if start < pos {
                        // Replaced or truncated: read again from the top;
                        // `last` keeps already-sent events from repeating.
                        pending.clear();
                    }
                    pos = start + buf.len() as u64;
                    pending.extend_from_slice(&buf);
                    for ev in drain_events(&mut pending) {
                        if ev.seq <= last {
                            continue;
                        }
                        last = ev.seq;
                        if tx.send(ev).await.is_err() {
                            return;
                        }
                    }
                }
                if tx.is_closed() {
                    return;
                }
                tokio::time::sleep(poll).await;
            }
        });
        Ok(rx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::trace::EventDraft;
    use serde_json::Value;

    fn sink(root: &Path, secrets: SecretRegistry) -> JsonlTraceSink {
        JsonlTraceSink::open(
            root,
            "control-loop-lab",
            Some(&"c".repeat(64)),
            Some("host:4242:0f0e0d0c-0b0a-4908-8706-050403020100"),
            RunKind::Run,
            Arc::new(secrets),
        )
        .unwrap()
    }

    fn step(i: u64) -> EventDraft {
        EventDraft::new(Component::Loop, "loop.completed", Status::Ok)
            .session(format!("tick:{i}"))
            .node("loop:demo")
            .payload(json!({"i": i}))
    }

    fn file_lines(path: &Path) -> Vec<Value> {
        std::fs::read_to_string(path)
            .unwrap()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect()
    }

    fn reader(root: &Path) -> JsonlTraceReader {
        JsonlTraceReader::new(root, "control-loop-lab")
            .unwrap()
            .tuned(Duration::from_millis(10), FOLLOW_CAPACITY)
    }

    async fn recv(rx: &mut Receiver<ExecutionEvent>) -> ExecutionEvent {
        tokio::time::timeout(Duration::from_secs(5), rx.recv())
            .await
            .expect("follow timed out")
            .expect("follow ended")
    }

    /// The ids `emit` returns are the ids read back by page and by follow.
    #[tokio::test]
    async fn ids_stable_across_write_and_replay() {
        let tmp = tempfile::tempdir().unwrap();
        let s = sink(tmp.path(), SecretRegistry::new());
        let run = s.run_id().unwrap().to_string();
        let mut written = vec![format!("{run}:1")];
        written.extend((0..5).map(|i| s.emit(step(i)).unwrap()));
        let r = reader(tmp.path());
        let paged: Vec<String> = r
            .events(&run, 0, usize::MAX)
            .unwrap()
            .into_iter()
            .map(|e| e.event_id)
            .collect();
        assert_eq!(paged, written);
        let mut rx = r.follow(&run, 0).unwrap();
        let mut followed = Vec::new();
        for _ in 0..written.len() {
            followed.push(recv(&mut rx).await.event_id);
        }
        assert_eq!(followed, written);
        assert_eq!(written[3], format!("{run}:4"));
    }

    /// One emit = one line; a re-read and a page boundary never repeat it.
    #[test]
    fn one_event_written_once() {
        let tmp = tempfile::tempdir().unwrap();
        let s = sink(tmp.path(), SecretRegistry::new());
        let id = s.emit(step(1)).unwrap();
        s.emit(step(2)).unwrap();
        let lines = file_lines(s.path());
        assert_eq!(
            lines.iter().filter(|l| l["event_id"] == json!(id)).count(),
            1
        );
        assert_eq!(lines.len(), 3, "run.opened + 2");
        let r = reader(tmp.path());
        let run = s.run_id().unwrap();
        let first = r.events(run, 0, 2).unwrap();
        let rest = r.events(run, first.last().unwrap().seq, 10).unwrap();
        let mut all: Vec<String> = first
            .iter()
            .chain(&rest)
            .map(|e| e.event_id.clone())
            .collect();
        assert_eq!(all.len(), 3);
        all.dedup();
        assert_eq!(all.len(), 3);
        assert_eq!(r.events(run, 0, 10).unwrap(), r.events(run, 0, 10).unwrap());
    }

    /// 8 threads emitting at once: the file's `seq` is 1..=N in line order.
    #[test]
    fn concurrent_emit_keeps_seq_order() {
        let tmp = tempfile::tempdir().unwrap();
        let s = Arc::new(sink(tmp.path(), SecretRegistry::new()));
        let threads: Vec<_> = (0..8)
            .map(|t| {
                let s = Arc::clone(&s);
                std::thread::spawn(move || {
                    for i in 0..100 {
                        s.emit(step(t * 1000 + i)).unwrap();
                    }
                })
            })
            .collect();
        threads.into_iter().for_each(|t| t.join().unwrap());
        let seqs: Vec<u64> = file_lines(s.path())
            .iter()
            .map(|l| l["seq"].as_u64().unwrap())
            .collect();
        assert_eq!(seqs, (1..=801).collect::<Vec<u64>>());
    }

    /// Secrets and URLs never reach the file; ids and the envelope stay.
    #[test]
    fn redacts_secrets_and_urls_before_write() {
        let tmp = tempfile::tempdir().unwrap();
        let mut secrets = SecretRegistry::new();
        secrets.register("sk-trace-secret-42".into());
        let s = sink(tmp.path(), secrets);
        s.emit(
            step(1)
                .call("demo:tick:1:1")
                .payload(json!({
                    "args": {"url": "https://rpc.example.com/?api-key=k-in-url", "note": "auth sk-trace-secret-42"},
                    "event": [
                        "see wss://feed.example/x?token=t1 now",
                        {"sk-trace-secret-42": 1, "https://hook.example/?sig=s1": {"ok": "x"}}
                    ],
                })),
        )
        .unwrap();
        let raw = std::fs::read_to_string(s.path()).unwrap();
        for leaked in [
            "sk-trace-secret-42",
            "k-in-url",
            "rpc.example.com",
            "token=t1",
            "hook.example",
            "sig=s1",
        ] {
            assert!(!raw.contains(leaked), "{leaked} in {raw}");
        }
        let last = file_lines(s.path()).pop().unwrap();
        assert_eq!(last["payload"]["args"]["url"], json!("<url>"));
        assert_eq!(last["payload"]["args"]["note"], json!("auth [REDACTED]"));
        assert_eq!(last["payload"]["event"][0], json!("see <url> now"));
        // Keys too: an event's keys are input like its values.
        assert_eq!(
            last["payload"]["event"][1],
            json!({"[REDACTED]": 1, "<url>": {"ok": "x"}})
        );
        assert_eq!(last["call_id"], json!("demo:tick:1:1"));
        // Bounded too: a huge payload is cut to whole fields.
        s.emit(step(2).payload(json!({"big": "b".repeat(10_000), "id": "keep"})))
            .unwrap();
        let last = file_lines(s.path()).pop().unwrap();
        assert_eq!(last["payload"]["id"], json!("keep"));
        assert_eq!(last["payload"]["_dropped"], json!(["big"]));
    }

    /// A follower from `seq` n gets n+1… then each new event; a second
    /// follower (a reconnect) resumes from its own `seq` with no gap.
    #[tokio::test]
    async fn follow_resumes_after_seq() {
        let tmp = tempfile::tempdir().unwrap();
        let s = sink(tmp.path(), SecretRegistry::new());
        for i in 0..4 {
            s.emit(step(i)).unwrap();
        }
        let run = s.run_id().unwrap().to_string();
        let r = reader(tmp.path());
        let mut rx = r.follow(&run, 3).unwrap();
        assert_eq!(recv(&mut rx).await.seq, 4);
        assert_eq!(recv(&mut rx).await.seq, 5);
        s.emit(step(9)).unwrap();
        s.emit(step(10)).unwrap();
        assert_eq!(recv(&mut rx).await.seq, 6);
        drop(rx);
        let mut again = r.follow(&run, 6).unwrap();
        assert_eq!(recv(&mut again).await.seq, 7);
        assert!(
            tokio::time::timeout(Duration::from_millis(100), again.recv())
                .await
                .is_err(),
            "nothing past the last event"
        );
    }

    /// A reader that does not read while 60 events land (channel of 4)
    /// still gets every one, in order: the tail waits, nothing is dropped.
    #[tokio::test]
    async fn follow_lagging_reader_loses_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        let s = sink(tmp.path(), SecretRegistry::new());
        let run = s.run_id().unwrap().to_string();
        let r = JsonlTraceReader::new(tmp.path(), "control-loop-lab")
            .unwrap()
            .tuned(Duration::from_millis(5), 4);
        let mut rx = r.follow(&run, 0).unwrap();
        for i in 0..60 {
            s.emit(step(i)).unwrap();
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
        let mut seqs = Vec::new();
        for _ in 0..61 {
            seqs.push(recv(&mut rx).await.seq);
        }
        assert_eq!(seqs, (1..=61).collect::<Vec<u64>>());
    }

    /// Each open is a new run: own id, own file, `seq` from 1, `run.opened`
    /// first; `runs` lists both, events never mix.
    #[test]
    fn restart_starts_new_run_file() {
        let tmp = tempfile::tempdir().unwrap();
        let a = sink(tmp.path(), SecretRegistry::new());
        a.emit(step(1)).unwrap();
        let b = JsonlTraceSink::open(
            tmp.path(),
            "control-loop-lab",
            Some(&"c".repeat(64)),
            Some("host:4343:1f0e0d0c-0b0a-4908-8706-050403020100"),
            RunKind::Run,
            Arc::new(SecretRegistry::new()),
        )
        .unwrap();
        b.emit(step(2)).unwrap();
        assert_ne!(a.run_id(), b.run_id());
        assert_ne!(a.path(), b.path());
        let r = reader(tmp.path());
        for s in [&a, &b] {
            let evs = r.events(s.run_id().unwrap(), 0, 10).unwrap();
            assert_eq!(evs[0].kind, RUN_OPENED);
            assert_eq!(evs[0].seq, 1);
            assert_eq!(evs[0].payload["kind"], json!("run"));
            assert!(evs.iter().all(|e| Some(e.run_id.as_str()) == s.run_id()));
        }
        let runs = r.runs().unwrap();
        assert_eq!(runs.len(), 2);
        let ids: Vec<Option<&str>> = runs.iter().map(|x| x.runtime_id.as_deref()).collect();
        assert!(ids.contains(&Some("host:4242:0f0e0d0c-0b0a-4908-8706-050403020100")));
        assert!(ids.contains(&Some("host:4343:1f0e0d0c-0b0a-4908-8706-050403020100")));
        assert!(runs
            .iter()
            .all(|x| x.kind.as_deref() == Some("run") && x.events == 2));
    }

    /// `run.opened` names `runtime:<sandbox>` for a `tengu run`; a decide's
    /// names no node (a `--map` run is not `trigger:decide`: its
    /// `trigger.*` root names the trigger).
    #[test]
    fn run_opened_node_by_kind() {
        let tmp = tempfile::tempdir().unwrap();
        let run = sink(tmp.path(), SecretRegistry::new());
        let decide = JsonlTraceSink::open(
            tmp.path(),
            "control-loop-lab",
            None,
            None,
            RunKind::Decide,
            Arc::new(SecretRegistry::new()),
        )
        .unwrap();
        let first = |s: &JsonlTraceSink| file_lines(s.path()).remove(0);
        assert_eq!(first(&run)["node_id"], json!("runtime:control-loop-lab"));
        assert_eq!(first(&decide)["node_id"], Value::Null);
        assert_eq!(first(&decide)["payload"]["kind"], json!("decide"));
        assert_eq!(first(&decide)["runtime_id"], Value::Null);
    }

    /// No request path reaches the disk: run ids and sandbox names are
    /// checked before any path is built.
    #[test]
    fn rejects_paths_that_are_not_ids() {
        let tmp = tempfile::tempdir().unwrap();
        let r = reader(tmp.path());
        for bad in [
            "../x",
            "5B0C7D0E-8A4E-4F0A-9D8E-2F1C3B4A5D6E",
            "",
            "x.jsonl",
        ] {
            assert!(r.events(bad, 0, 1).is_err(), "{bad}");
        }
        for bad in ["..", "../etc", "a/b", ".hidden", ""] {
            assert!(JsonlTraceReader::new(tmp.path(), bad).is_err(), "{bad}");
        }
        assert!(r.runs().unwrap().is_empty(), "no dir = no runs");
    }
}
