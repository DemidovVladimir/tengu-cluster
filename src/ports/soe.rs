//! SOE cycle ports (O3 / O4; `docs/soe-2026-10-08.md` § 10): what the weekly
//! cycle (`application/soe/`) and its tools read and write besides the
//! source store — `ports::source_store::SourceStore`, the cycle's only fact
//! input, read through `application::sources::evidence_as_of`. Impls:
//! `adapters/outbound/soe/` (wired by `bootstrap/soe.rs`); the application
//! tests use in-memory fakes.
//!
//! | Port | Does | Impl |
//! |---|---|---|
//! | [`StageRunner`] | runs one model stage (`ARCHITECT` · `CHALLENGE`) as an agent of the sandbox; the agent's tools write its drafts into the run dir through `application::soe::{submit_proposal, submit_challenge}` — the runner returns only how the run went ([`StageReply`]), never a draft | a `run-agent` child per stage (`SubprocessRunner`); a cache keyed by the canonical sha256 of `{agent, model, skill_sha256, stage, goal}` that replays a recorded stage's records verbatim (an offline miss is an error naming the full key) |
//! | [`CycleStore`] | the private SOE state `<TENGU_HOME>/state/<sources.state>/` (critic C8): run dirs `cycles/<cycle id>/` (live weekly cycles), `replays/<run id>/` (replays — never `cycles/`) and `reviews/<id>/` (Operator Review #2 packets), each claimed once and frozen once; the append-only state logs ([`StateLog`]) | `FsCycleStore` |
//!
//! | `CycleStore` rule | Value |
//! |---|---|
//! | `claim` | creates the run dir once (`create_dir`, mode 0700): an existing dir — open, failed or frozen — is refused |
//! | `status` | [`RunStatus`]: `ABSENT` · `OPEN` (claimed, no `MANIFEST.json`) · `FROZEN` |
//! | `write` · `read` | one file per name ([`valid_file_name`]: one path segment, not `MANIFEST.json`), written once with one `write_all`: an existing name or a frozen dir is refused; `read` of a missing name = `None` |
//! | `append_proposal` · `append_challenge` | one canonical JSON line (`domain::canonical::canonical_json` + `\n`) to `proposals.jsonl` / `challenges.jsonl`, one `write_all`, never rewritten; refused in a frozen dir. Which stage may append is the application's rule (`submit_*`) |
//! | `proposals` · `challenges` | every line in order, parsed with `domain::soe::record::from_json` (schema + rules); a bad line is an error naming the file and the line |
//! | `files` | every regular file of the run dir but `MANIFEST.json`, sorted bytewise |
//! | `freeze` | writes `MANIFEST.json` (the bytes given; new file only), then `chmod a-w` every file and the dir (dir last); a frozen dir refuses every write |
//! | `cycles` | the run ids under `cycles/`, sorted |
//! | `append_line` · `lines` | `<state>/<log>.jsonl` ([`StateLog::file_name`]): one line (no `\n` inside) per call with one `write_all`, its 1-based number returned (a holdout read's `#n`); never rewritten; read back in order |

// Some helpers wait for their consumers: the `soe_*` tools and `tengu soe`.
#![allow(dead_code)]

use std::fmt;

use async_trait::async_trait;

use crate::domain::lineage::value::valid_id;
use crate::domain::metrics::MetricsRecord;
use crate::domain::soe::challenge::Challenge;
use crate::domain::soe::ops::Stage;
use crate::domain::soe::proposal::MechanismProposal;

/// The manifest a frozen run dir carries.
pub(crate) const MANIFEST: &str = "MANIFEST.json";
/// The Architect's appends.
pub(crate) const PROPOSALS: &str = "proposals.jsonl";
/// The Critic's appends.
pub(crate) const CHALLENGES: &str = "challenges.jsonl";

/// One run dir of the SOE state (module table).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) enum RunDir {
    /// `cycles/<cycle id>/` — a live weekly cycle (`2026-W41`).
    Cycle(String),
    /// `replays/<run id>/` — a replay (eval set, rerun); never under `cycles/`.
    Replay(String),
    /// `reviews/<id>/` — one Operator Review #2 packet as it was shown.
    Review(String),
}

impl RunDir {
    pub(crate) fn id(&self) -> &str {
        match self {
            RunDir::Cycle(id) | RunDir::Replay(id) | RunDir::Review(id) => id,
        }
    }

    pub(crate) fn is_replay(&self) -> bool {
        matches!(self, RunDir::Replay(_))
    }

    /// `cycles` · `replays` · `reviews`.
    pub(crate) fn space(&self) -> &'static str {
        match self {
            RunDir::Cycle(_) => "cycles",
            RunDir::Replay(_) => "replays",
            RunDir::Review(_) => "reviews",
        }
    }

    /// `cycles/<id>` · `replays/<id>` · `reviews/<id>`, from the text a tool
    /// is given; the id a lineage id (`valid_id`).
    pub(crate) fn parse(s: &str) -> Option<RunDir> {
        let (space, id) = s.trim_end_matches('/').split_once('/')?;
        if !valid_id(id) {
            return None;
        }
        match space {
            "cycles" => Some(RunDir::Cycle(id.to_string())),
            "replays" => Some(RunDir::Replay(id.to_string())),
            "reviews" => Some(RunDir::Review(id.to_string())),
            _ => None,
        }
    }
}

impl fmt::Display for RunDir {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}", self.space(), self.id())
    }
}

/// A run file name (module table: one path segment, not the manifest).
pub(crate) fn valid_file_name(name: &str) -> bool {
    let b = name.as_bytes();
    !b.is_empty()
        && b.len() <= 80
        && b[0].is_ascii_alphanumeric()
        && b.iter()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'.' | b'_' | b'-'))
        && name != MANIFEST
}

/// Where a run dir stands (module table).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RunStatus {
    Absent,
    /// Claimed, not frozen: running, or failed part-way.
    Open,
    Frozen,
}

/// An append-only state log under `<state>/` (module table).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) enum StateLog {
    /// One event per decided (or dropped) candidate per live cycle.
    Candidates,
    /// One `soe.opportunity_episode/1` per decided candidate per live cycle (C20).
    Episodes,
    /// One hash-chained `forecast::LogLine` per frozen live cycle.
    ForecastLog,
    /// Operator grades (`soe.cycle_grade/1`; a regrade is a later version).
    Grades,
    /// Holdout reads of a replay (`#n` = the line number).
    HoldoutReads,
    /// One resolved forecast item per line (`soe.forecast_resolution/1`).
    Resolutions,
    /// One finished replay per line: its report dir and case dirs.
    Replays,
    /// One review packet built per line: its dir and digest.
    Reviews,
}

impl StateLog {
    pub(crate) const ALL: [StateLog; 8] = [
        StateLog::Candidates,
        StateLog::Episodes,
        StateLog::ForecastLog,
        StateLog::Grades,
        StateLog::HoldoutReads,
        StateLog::Resolutions,
        StateLog::Replays,
        StateLog::Reviews,
    ];

    pub(crate) fn file_name(self) -> &'static str {
        match self {
            StateLog::Candidates => "candidates.jsonl",
            StateLog::Episodes => "episodes.jsonl",
            StateLog::ForecastLog => "forecast-log.jsonl",
            StateLog::Grades => "grades.jsonl",
            StateLog::HoldoutReads => "holdout-reads.jsonl",
            StateLog::Resolutions => "resolutions.jsonl",
            StateLog::Replays => "replays.jsonl",
            StateLog::Reviews => "reviews.jsonl",
        }
    }
}

/// The private SOE state (module table). Synchronous: short file IO.
pub(crate) trait CycleStore: Send + Sync {
    /// The state dir, for messages.
    fn root_display(&self) -> String;
    fn status(&self, dir: &RunDir) -> anyhow::Result<RunStatus>;
    /// Create `dir` once; `Err` when it exists.
    fn claim(&self, dir: &RunDir) -> anyhow::Result<()>;
    /// Run ids under `cycles/`, sorted.
    fn cycles(&self) -> anyhow::Result<Vec<String>>;
    /// A new file `name` of `dir`.
    fn write(&self, dir: &RunDir, name: &str, bytes: &[u8]) -> anyhow::Result<()>;
    fn read(&self, dir: &RunDir, name: &str) -> anyhow::Result<Option<Vec<u8>>>;
    /// Every file of `dir` but `MANIFEST.json`, sorted.
    fn files(&self, dir: &RunDir) -> anyhow::Result<Vec<String>>;
    fn append_proposal(&self, dir: &RunDir, p: &MechanismProposal) -> anyhow::Result<()>;
    fn proposals(&self, dir: &RunDir) -> anyhow::Result<Vec<MechanismProposal>>;
    fn append_challenge(&self, dir: &RunDir, c: &Challenge) -> anyhow::Result<()>;
    fn challenges(&self, dir: &RunDir) -> anyhow::Result<Vec<Challenge>>;
    /// Write `MANIFEST.json`, then make `dir` read-only.
    fn freeze(&self, dir: &RunDir, manifest: &[u8]) -> anyhow::Result<()>;
    /// Append one line; its 1-based number.
    fn append_line(&self, log: StateLog, line: &str) -> anyhow::Result<u64>;
    fn lines(&self, log: StateLog) -> anyhow::Result<Vec<String>>;
}

/// One model stage of a run (the runner's input).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct StageRequest {
    /// `ARCHITECT` or `CHALLENGE`.
    pub stage: Stage,
    /// The `[agents.<name>]` that runs it (one with a `description`).
    pub agent: String,
    /// Where its tools write.
    pub dir: RunDir,
    pub cycle_id: String,
    /// The user turn: the run dir, cycle id, decision time, the packet's
    /// sha256 and candidate ids — never source text or a later outcome.
    pub goal: String,
}

/// How one stage run went.
#[derive(Debug, Clone, Default)]
pub(crate) struct StageReply {
    pub ok: bool,
    /// The agent's final summary — model text: untrusted, fenced when shown.
    pub summary: String,
    pub error: Option<String>,
    pub latency_ms: u64,
    /// One per LLM call (model, tokens) — `AgentIpcOutput.metrics`; kept on
    /// a failed run.
    pub metrics: Vec<MetricsRecord>,
}

#[async_trait]
pub(crate) trait StageRunner: Send + Sync {
    /// Run `req` to its end. `Err` = the runner itself failed (no child, an
    /// offline cache miss): the cycle records a failed stage and goes on.
    async fn run(&self, req: &StageRequest) -> anyhow::Result<StageReply>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn run_dirs_parse_and_print() {
        for (text, dir) in [
            ("cycles/2026-W41", RunDir::Cycle("2026-W41".into())),
            (
                "replays/r-2026-10-08.1/",
                RunDir::Replay("r-2026-10-08.1".into()),
            ),
            ("reviews/2026-11-02", RunDir::Review("2026-11-02".into())),
        ] {
            assert_eq!(RunDir::parse(text), Some(dir.clone()));
            assert_eq!(RunDir::parse(&dir.to_string()), Some(dir));
        }
        for bad in [
            "cycles",
            "cycles/",
            "cycles/../x",
            "cycles/a/b",
            "evidence/2026-W41",
            "/cycles/2026-W41",
        ] {
            assert_eq!(RunDir::parse(bad), None, "{bad}");
        }
        assert!(valid_file_name("portfolio.json"));
        for bad in ["", MANIFEST, "../x", "a/b", ".hidden"] {
            assert!(!valid_file_name(bad), "{bad}");
        }
    }
}
