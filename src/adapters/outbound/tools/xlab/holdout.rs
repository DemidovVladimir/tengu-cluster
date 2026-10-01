//! Holdout discipline of the `backtest` tool (xlab, `docs/xlab-2026-10-01.md`
//! § 8): the Architect tunes on the in-sample half and reads the holdout
//! once, on purpose, counted. The operator's CLI (`tengu backtest --split`)
//! keeps both halves side by side and records nothing.
//!
//! | Call | Runs | Shows | Ledger |
//! |---|---|---|---|
//! | `split`, no `holdout` (default: hidden) | the in-sample half only — `time:<t>`: decisions end at `t` (`to` = min(to, t)); `instruments:<ids>`: after `prepare`, before any arm, the candidates trading a listed id (any leg) are left out (`seq` renumbered) with their skips; no holdout trade is simulated or written | the in-sample run, `holdout hidden: …`; the run's data notes say so | — |
//! | `split` + `holdout: true` (a read) | the full window, both halves | the split lines + `holdout read #n for this spec …` | one line |
//! | `run_id` + `holdout: true` on a stored split run (`rows.rs`) | nothing | its holdout rows too | one line, `via = "rows"` |
//!
//! | Ledger rule | Value |
//! |---|---|
//! | File | `<state dir>/backtests/holdout-reads.jsonl` — append-only, one `write_all` per line, never rewritten: the operator's audit |
//! | Line | `ts_ms`, `time`, `via` (`backtest` · `rows`), `run_id`, `spec_sha256`, `strategy`, `split`, `call_id` (when the call has one) — ids in full |
//! | #n | the line's place among the lines of its `spec_sha256`, counted up to its own end (the append's offset): a concurrent read never takes its number; the split's count over every spec alike |
//! | Not recorded | a run that fails, or a read with no holdout candidate (refused: nothing is shown) |
//! | Unrecordable | the run's text is refused (the holdout is never shown unrecorded) |

use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, Write};
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

use crate::application::backtest::{BacktestJob, Prepared};
use crate::domain::backtest::spec::SplitSpec;
use crate::domain::marketdata::fmt_time;
use crate::domain::tools as names;

/// The ledger's file name in the backtests dir (module table).
pub(crate) const LEDGER_FILE: &str = "holdout-reads.jsonl";
/// A split's text in the tool's lines, at most (an instruments split of
/// many ids is named by its count — the ids are the caller's own, and the
/// ledger and the run keep them in full).
pub(crate) const SPLIT_LABEL_CHARS: usize = 240;

/// `split` as the tool's lines name it ([`SPLIT_LABEL_CHARS`]).
pub(crate) fn split_label(split: &SplitSpec) -> String {
    let full = split.to_string();
    match split {
        SplitSpec::Instruments(ids) if full.chars().count() > SPLIT_LABEL_CHARS => {
            format!("instruments:<the {} ids given>", ids.len())
        }
        _ => full,
    }
}

/// `<backtests dir>/holdout-reads.jsonl`.
pub(crate) fn ledger_path(backtests_dir: &Path) -> PathBuf {
    backtests_dir.join(LEDGER_FILE)
}

/// Hide the holdout of `job`'s split (module table): the split leaves the
/// job; a time split ends its decisions at the split instant. Returns the
/// split (`None`: the job has none). A window that starts at or after a time
/// split is all holdout: refused.
pub(crate) fn hide(job: &mut BacktestJob, now_ms: i64) -> Result<Option<SplitSpec>> {
    let Some(split) = job.split.take() else {
        return Ok(None);
    };
    if let SplitSpec::Time(t) = &split {
        let t = *t;
        if let Some(from) = job.from_ms.filter(|f| *f >= t) {
            bail!(
                "{}: 'from' {} is at or after the split {split}: the whole window is holdout — \
                 tune on decisions before {}, then read the holdout once with \"holdout\": true",
                names::BACKTEST,
                fmt_time(from),
                fmt_time(t)
            );
        }
        job.to_ms = Some(job.to_ms.unwrap_or(now_ms).min(t));
    }
    Ok(Some(split))
}

/// What a failed hidden run adds to its error.
pub(crate) fn hidden_context(split: &SplitSpec) -> String {
    match split {
        SplitSpec::Time(t) => format!(
            " (split {split} hides its holdout: this run decides nothing at or after {})",
            fmt_time(*t)
        ),
        SplitSpec::Instruments(_) => format!(
            " (split {} hides its holdout: those ids' candidates are left out)",
            split_label(split)
        ),
    }
}

/// A candidate or trade on `legs`, decided at `decided_at_ms`, is in the
/// holdout of `split`.
fn on_holdout<'a>(
    split: &SplitSpec,
    decided_at_ms: i64,
    legs: impl Iterator<Item = &'a str>,
) -> bool {
    let legs: Vec<&str> = legs.collect();
    split.is_holdout(decided_at_ms, &legs)
}

/// Leave the holdout out of a prepared hidden run (module table): its
/// candidates go (the rest renumbered), with the skips on a listed id; the
/// run reports no split, and its data notes start with what was hidden.
/// Returns how many candidates were left out.
pub(crate) fn leave_out(p: &mut Prepared, split: &SplitSpec) -> usize {
    let before = p.set.candidates.len();
    p.set.candidates.retain(|c| {
        !on_holdout(
            split,
            c.decided_at_ms,
            c.legs.iter().map(|l| l.instrument.as_str()),
        )
    });
    for (i, c) in p.set.candidates.iter_mut().enumerate() {
        c.seq = i;
    }
    if let SplitSpec::Instruments(ids) = split {
        // A pair's key is `<a>/<b>`: either leg listed hides it.
        p.set.skipped.retain(|s| {
            !s.instrument
                .split('/')
                .any(|x| ids.iter().any(|id| id == x))
        });
    }
    let left_out = before - p.set.candidates.len();
    p.split = None;
    let what = match split {
        SplitSpec::Time(t) => format!("no decision at or after {}", fmt_time(*t)),
        SplitSpec::Instruments(_) => {
            format!("{left_out} candidate(s) trading a listed id left out")
        }
    };
    p.set.notes.insert(
        0,
        format!(
            "holdout hidden (backtest tool): split {} — {what}; this run holds the in-sample \
             half only",
            split_label(split)
        ),
    );
    left_out
}

/// Candidates of `p` in the holdout of `split`.
pub(crate) fn holdout_candidates(p: &Prepared, split: &SplitSpec) -> usize {
    p.set
        .candidates
        .iter()
        .filter(|c| {
            on_holdout(
                split,
                c.decided_at_ms,
                c.legs.iter().map(|l| l.instrument.as_str()),
            )
        })
        .count()
}

/// A read refused before it is run or counted: the run has no candidate in
/// the holdout.
pub(crate) fn check_readable(p: &Prepared, split: &SplitSpec) -> Result<()> {
    if holdout_candidates(p, split) == 0 {
        bail!(
            "{}: \"holdout\": true reads the holdout half of split {}, but none of the run's {} \
             candidate(s) falls in it — nothing to read, nothing counted",
            names::BACKTEST,
            split_label(split),
            p.set.candidates.len()
        );
    }
    Ok(())
}

/// The tool's line for a hidden run.
pub(crate) fn hidden_line(split: &SplitSpec, left_out: usize) -> String {
    let what = match split {
        SplitSpec::Time(t) => format!(
            "no decision at or after the split ({}): no holdout trade was simulated",
            fmt_time(*t)
        ),
        SplitSpec::Instruments(_) => {
            format!("the {left_out} candidate(s) trading a listed id were left out")
        }
    };
    format!(
        "holdout hidden: split {} — {what}; every figure above is the in-sample half. Tune on \
         it, then read the holdout ONCE: the same call + \"holdout\": true (each read is counted \
         per spec)",
        split_label(split)
    )
}

/// One ledger line (module table).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct HoldoutRead {
    pub ts_ms: i64,
    /// `ts_ms` as RFC 3339 (UTC).
    pub time: String,
    /// `backtest` (a run with both halves) · `rows` (a stored run's rows).
    pub via: String,
    pub run_id: String,
    pub spec_sha256: String,
    pub strategy: String,
    /// `SplitSpec`'s text: `time:<RFC 3339>` · `instruments:<id,…>`.
    pub split: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub call_id: Option<String>,
}

impl HoldoutRead {
    pub(crate) fn new(
        via: &str,
        run_id: &str,
        spec_sha256: &str,
        strategy: &str,
        split: &SplitSpec,
        call_id: Option<&str>,
        ts_ms: i64,
    ) -> Self {
        Self {
            ts_ms,
            time: fmt_time(ts_ms),
            via: via.to_string(),
            run_id: run_id.to_string(),
            spec_sha256: spec_sha256.to_string(),
            strategy: strategy.to_string(),
            split: split.to_string(),
            call_id: call_id.map(str::to_string),
        }
    }
}

/// A read's numbers (module table).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ReadCount {
    /// Holdout reads of this spec, this one included (1 = the first).
    pub spec: usize,
    /// Holdout reads of this split, every spec, this one included.
    pub split: usize,
}

/// Append `read` to the ledger and count it (module table).
pub(crate) fn record(backtests_dir: &Path, read: &HoldoutRead) -> Result<ReadCount> {
    std::fs::create_dir_all(backtests_dir)
        .with_context(|| format!("create {}", backtests_dir.display()))?;
    let path = ledger_path(backtests_dir);
    let mut line = serde_json::to_string(read)?;
    line.push('\n');
    let mut f = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .with_context(|| format!("open {}", path.display()))?;
    f.write_all(line.as_bytes())
        .with_context(|| format!("append to {}", path.display()))?;
    // O_APPEND: this handle's offset is now the end of our own line.
    let end = f.stream_position()?;
    let mut text = String::new();
    File::open(&path)
        .with_context(|| format!("read {}", path.display()))?
        .take(end)
        .read_to_string(&mut text)
        .with_context(|| format!("read {}", path.display()))?;
    let mut count = ReadCount { spec: 0, split: 0 };
    for row in text
        .lines()
        .filter_map(|l| serde_json::from_str::<HoldoutRead>(l).ok())
    {
        count.spec += usize::from(row.spec_sha256 == read.spec_sha256);
        count.split += usize::from(row.split == read.split);
    }
    // Our own line is in the text read; never report a read as #0.
    count.spec = count.spec.max(1);
    count.split = count.split.max(1);
    Ok(count)
}

/// The tool's line for a recorded read of `split` (the spec is the
/// `spec_sha256` the text prints).
pub(crate) fn read_line(count: ReadCount, split: &SplitSpec) -> String {
    let verdict = if count.spec > 1 {
        "a variant picked after reading this holdout is fitted to it: say so"
    } else {
        "this first read is the spec's out-of-sample test"
    };
    format!(
        "holdout read #{} for this spec · {} read(s) of split {} in this sandbox, every spec — \
         {verdict}",
        count.spec,
        count.split,
        split_label(split)
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::application::backtest::SpecSource;
    use crate::domain::backtest::testkit::utc;

    const AAA: &str = "hyperliquid:xyz:AAA";

    fn job(split: Option<&str>, from: Option<i64>, to: Option<i64>) -> BacktestJob {
        BacktestJob {
            spec: SpecSource::Strategy("w".into()),
            from_ms: from,
            to_ms: to,
            split: split.map(|s| SplitSpec::parse(s).unwrap()),
        }
    }

    /// A time split ends the decisions at it (or earlier); the split leaves
    /// the job; a window inside the holdout is refused; an instruments split
    /// keeps the window.
    #[test]
    fn hiding_a_split_ends_the_window_at_it() {
        let now = utc("2026-10-01 12:00");
        let t = utc("2026-07-01 00:00");
        for (to, want) in [
            (None, t),
            (Some(utc("2026-09-01 00:00")), t),
            (Some(t - 1), t - 1),
        ] {
            let mut j = job(Some("time:2026-07-01"), None, to);
            let split = hide(&mut j, now).unwrap();
            assert_eq!(split, Some(SplitSpec::Time(t)));
            assert_eq!((j.split.clone(), j.to_ms), (None, Some(want)), "{to:?}");
        }
        let mut j = job(Some("time:2026-07-01"), Some(t), None);
        let e = hide(&mut j, now).unwrap_err().to_string();
        assert!(
            e.starts_with("backtest: 'from' 2026-07-01T00:00:00Z is at or after the split"),
            "{e}"
        );
        let mut j = job(Some(&format!("instruments:{AAA}")), None, None);
        assert!(matches!(
            hide(&mut j, now).unwrap(),
            Some(SplitSpec::Instruments(_))
        ));
        assert_eq!((j.split.clone(), j.to_ms), (None, None));
        let mut j = job(None, None, None);
        assert_eq!(hide(&mut j, now).unwrap(), None);
        assert_eq!(j.to_ms, None);
    }

    fn read(spec: &str, split: &str, run: &str) -> HoldoutRead {
        HoldoutRead::new(
            "backtest",
            run,
            spec,
            "w",
            &SplitSpec::parse(split).unwrap(),
            Some("mcp:0123:2"),
            utc("2026-10-01 12:00"),
        )
    }

    /// Every read is one appended line; #n counts the spec's lines up to
    /// this one, the split's alike; the file is never rewritten.
    #[test]
    fn reads_are_appended_and_counted_per_spec_and_split() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("backtests");
        let (a, b) = ("a".repeat(64), "b".repeat(64));
        let steps = [
            (&a, "time:2026-07-01", (1, 1)),
            (&b, "time:2026-07-01", (1, 2)),
            (&a, "time:2026-07-01", (2, 3)),
            (&a, "time:2026-08-01", (3, 1)),
        ];
        for (i, (spec, split, (n_spec, n_split))) in steps.iter().enumerate() {
            let r = read(spec, split, &format!("20261001T120000Z-w-{i}"));
            let c = record(&dir, &r).unwrap();
            assert_eq!((c.spec, c.split), (*n_spec, *n_split), "step {i}");
        }
        let text = std::fs::read_to_string(ledger_path(&dir)).unwrap();
        let lines: Vec<HoldoutRead> = text
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        assert_eq!(lines.len(), 4);
        assert_eq!(lines[2].run_id, "20261001T120000Z-w-2");
        assert_eq!(lines[2].spec_sha256, a);
        assert_eq!(lines[0].split, "time:2026-07-01T00:00:00Z");
        assert_eq!(lines[0].time, "2026-10-01T12:00:00Z");
        assert_eq!(lines[0].call_id.as_deref(), Some("mcp:0123:2"));
        // A garbled line (an editor, a crash) is skipped, never fatal.
        std::fs::write(ledger_path(&dir), format!("{text}not json\n")).unwrap();
        let c = record(&dir, &read(&a, "time:2026-07-01", "x")).unwrap();
        assert_eq!((c.spec, c.split), (4, 4));
    }

    #[test]
    fn the_lines_name_the_split_and_the_count() {
        let split = SplitSpec::parse("time:2026-07-01").unwrap();
        let first = read_line(ReadCount { spec: 1, split: 3 }, &split);
        assert_eq!(
            first,
            "holdout read #1 for this spec · 3 read(s) of split time:2026-07-01T00:00:00Z in this \
             sandbox, every spec — this first read is the spec's out-of-sample test"
        );
        let again = read_line(ReadCount { spec: 2, split: 4 }, &split);
        assert!(
            again.starts_with("holdout read #2 for this spec · 4 read(s)")
                && again.ends_with("fitted to it: say so"),
            "{again}"
        );
        // An instruments split of many ids is named by its count (the ids
        // are the caller's own; the ledger keeps them whole).
        let ids: Vec<String> = (0..40)
            .map(|i| format!("hyperliquid:xyz:N{i:03}"))
            .collect();
        let many = SplitSpec::parse(&format!("instruments:{}", ids.join(","))).unwrap();
        assert_eq!(split_label(&many), "instruments:<the 40 ids given>");
        let few = SplitSpec::parse(&format!("instruments:{AAA}")).unwrap();
        assert_eq!(split_label(&few), format!("instruments:{AAA}"));
        let line = hidden_line(&many, 7);
        assert!(line.chars().count() < 400, "{line}");
        assert_eq!(
            HoldoutRead::new("backtest", "r", "s", "w", &many, None, 0).split,
            many.to_string()
        );
        let hidden = hidden_line(&SplitSpec::parse("time:2026-07-01").unwrap(), 0);
        assert!(
            hidden.starts_with(
                "holdout hidden: split time:2026-07-01T00:00:00Z — no decision at or after the split \
                 (2026-07-01T00:00:00Z)"
            ),
            "{hidden}"
        );
        assert!(hidden.contains("\"holdout\": true"), "{hidden}");
    }
}
