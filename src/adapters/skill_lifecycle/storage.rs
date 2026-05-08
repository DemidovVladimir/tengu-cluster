//! Metric storage: rolling `metrics.json`, append-only `history.jsonl`, per-run reports.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};

use crate::adapters::skill_lifecycle::metrics::{MetricOutcome, MetricSpec};

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct MetricsJson {
    pub schema_version: u32,
    pub skill: String,
    pub last_run: String,
    pub last_run_ref: String,
    pub rolling_window: u32,
    pub metrics: BTreeMap<String, MetricRollup>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub(crate) struct MetricRollup {
    pub pass_rate: f32,
    pub n: u32,
    pub min_pass_rate: Option<f32>,
    pub gated: bool,
    /// Standard deviation of pass_rate over the rolling window.
    /// `None` when the window has < 2 entries (variance undefined).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stddev: Option<f32>,
    /// Min pass_rate observed in the rolling window.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min: Option<f32>,
    /// Max pass_rate observed in the rolling window.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max: Option<f32>,
}

#[derive(Debug, Serialize)]
pub(crate) struct HistoryLine<'a> {
    pub ts: &'a str,
    pub metric: &'a str,
    pub pass_rate: f32,
    pub n: u32,
    #[serde(rename = "ref")]
    pub run_ref: &'a str,
}

/// Per-fixture, per-metric outcome sample from a single run.
pub(crate) struct RunSample {
    pub fixture_id: String,
    pub outcomes: BTreeMap<String, MetricOutcome>,
}

pub(crate) fn per_run_dir(skill_dir: &Path, ts: &str) -> PathBuf {
    skill_dir.join("metrics").join("runs").join(ts)
}

pub(crate) fn history_path(skill_dir: &Path) -> PathBuf {
    skill_dir.join("metrics").join("history.jsonl")
}

pub(crate) fn metrics_json_path(skill_dir: &Path) -> PathBuf {
    skill_dir.join("metrics.json")
}

/// Prune per-run report directories under `<parent>/<ts>/` to at most
/// `max_runs` by keeping the lexicographically-latest entries (ISO-8601
/// timestamps sort correctly). `max_runs == 0` disables pruning. Errors
/// during individual removals are logged and swallowed — retention is a
/// best-effort cleanup, not a correctness gate.
pub(crate) fn prune_old_run_dirs(parent: &Path, max_runs: u32) {
    if max_runs == 0 || !parent.exists() {
        return;
    }
    let Ok(entries) = std::fs::read_dir(parent) else {
        return;
    };
    let mut dirs: Vec<PathBuf> = entries
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().map(|t| t.is_dir()).unwrap_or(false))
        .map(|e| e.path())
        .collect();
    dirs.sort();
    let keep = max_runs as usize;
    if dirs.len() <= keep {
        return;
    }
    let to_drop = dirs.len() - keep;
    for dir in dirs.into_iter().take(to_drop) {
        if let Err(e) = std::fs::remove_dir_all(&dir) {
            tracing::warn!(dir = %dir.display(), error = %e, "prune_old_run_dirs: remove failed");
        }
    }
}

/// Write the full per-run report + append to history + recompute metrics.json.
///
/// `max_per_run_reports` — when > 0, retains at most N most-recent per-run
/// directories under `metrics/runs/`. Older directories are deleted after
/// the new run is written.
pub(crate) fn finalize_run(
    skill_dir: &Path,
    skill: &str,
    ts: &str,
    specs: &[MetricSpec],
    samples: &[RunSample],
    rolling_window: u32,
    max_per_run_reports: u32,
) -> Result<()> {
    let run_dir = per_run_dir(skill_dir, ts);
    std::fs::create_dir_all(&run_dir)?;
    std::fs::write(
        run_dir.join("report.json"),
        serde_json::to_vec_pretty(&samples_to_report(samples))?,
    )?;

    // history.jsonl — append one line per metric for this run
    let hpath = history_path(skill_dir);
    std::fs::create_dir_all(hpath.parent().unwrap())?;
    let mut hf = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&hpath)?;
    let run_ref = format!("metrics/runs/{ts}");
    for spec in specs {
        let (n, pass_rate) = aggregate(samples, spec.name());
        let line = HistoryLine {
            ts,
            metric: spec.name(),
            pass_rate,
            n,
            run_ref: &run_ref,
        };
        writeln!(hf, "{}", serde_json::to_string(&line)?)?;
    }

    // metrics.json — compute rolling window per metric from history
    let rollups = compute_rollups(skill_dir, specs, rolling_window)?;
    let out = MetricsJson {
        schema_version: 1,
        skill: skill.to_string(),
        last_run: ts.to_string(),
        last_run_ref: run_ref.clone(),
        rolling_window,
        metrics: rollups,
    };
    std::fs::write(
        metrics_json_path(skill_dir),
        serde_json::to_vec_pretty(&out)?,
    )?;

    // Retention: keep the newest N per-run directories.
    let runs_parent = skill_dir.join("metrics").join("runs");
    prune_old_run_dirs(&runs_parent, max_per_run_reports);

    Ok(())
}

fn samples_to_report(samples: &[RunSample]) -> serde_json::Value {
    serde_json::json!({
        "fixtures": samples.iter().map(|s| serde_json::json!({
            "id": s.fixture_id,
            "outcomes": s.outcomes,
        })).collect::<Vec<_>>(),
    })
}

fn aggregate(samples: &[RunSample], metric: &str) -> (u32, f32) {
    let mut n = 0u32;
    let mut passes = 0u32;
    for s in samples {
        if let Some(o) = s.outcomes.get(metric) {
            n += 1;
            if o.pass {
                passes += 1;
            }
        }
    }
    let rate = if n == 0 {
        0.0
    } else {
        passes as f32 / n as f32
    };
    (n, rate)
}

fn compute_rollups(
    skill_dir: &Path,
    specs: &[MetricSpec],
    window: u32,
) -> Result<BTreeMap<String, MetricRollup>> {
    let hpath = history_path(skill_dir);
    let mut per_metric: BTreeMap<String, Vec<f32>> = BTreeMap::new();
    let mut per_metric_n: BTreeMap<String, u32> = BTreeMap::new();

    if hpath.exists() {
        let f = std::fs::File::open(&hpath).with_context(|| format!("open {:?}", hpath))?;
        for line in BufReader::new(f).lines() {
            let line = line?;
            let v: serde_json::Value = serde_json::from_str(&line)?;
            let metric = v
                .get("metric")
                .and_then(|x| x.as_str())
                .unwrap_or_default()
                .to_string();
            let rate = v.get("pass_rate").and_then(|x| x.as_f64()).unwrap_or(0.0) as f32;
            let n = v.get("n").and_then(|x| x.as_u64()).unwrap_or(0) as u32;
            per_metric.entry(metric.clone()).or_default().push(rate);
            per_metric_n.insert(metric, n);
        }
    }

    let mut out = BTreeMap::new();
    for spec in specs {
        let name = spec.name().to_string();
        let mut series = per_metric.get(&name).cloned().unwrap_or_default();
        if series.len() > window as usize {
            series.drain(..series.len() - window as usize);
        }
        let pass_rate = if series.is_empty() {
            0.0
        } else {
            series.iter().sum::<f32>() / series.len() as f32
        };
        // Sample stddev (n-1 denominator) only when the window has >= 2
        // entries; variance over a single point is undefined.
        let stddev = if series.len() >= 2 {
            let mean = pass_rate;
            let sumsq: f32 = series.iter().map(|x| (x - mean).powi(2)).sum();
            Some((sumsq / (series.len() as f32 - 1.0)).sqrt())
        } else {
            None
        };
        let win_min = series
            .iter()
            .copied()
            .min_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let win_max = series
            .iter()
            .copied()
            .max_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let min_floor = spec.min_pass_rate();
        let gated = min_floor.is_some_and(|m| pass_rate < m);
        out.insert(
            name.clone(),
            MetricRollup {
                pass_rate,
                n: *per_metric_n.get(&name).unwrap_or(&0),
                min_pass_rate: min_floor,
                gated,
                stddev,
                min: win_min,
                max: win_max,
            },
        );
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tempfile::TempDir;

    fn sample(metric: &str, pass: bool) -> RunSample {
        let mut map = BTreeMap::new();
        map.insert(
            metric.to_string(),
            MetricOutcome {
                pass,
                score: if pass { 1.0 } else { 0.0 },
                notes: None,
                raw: json!({}),
            },
        );
        RunSample {
            fixture_id: "f1".into(),
            outcomes: map,
        }
    }

    fn shell_spec(name: &str, min: Option<f32>) -> MetricSpec {
        MetricSpec::ShellCheck {
            name: name.into(),
            cmd: "x".into(),
            expect_stdout_matches: None,
            expect_exit_code: Some(0),
            min_pass_rate: min,
        }
    }

    #[test]
    fn writes_report_history_and_metrics_json() {
        let dir = TempDir::new().unwrap();
        let skill_dir = dir.path();
        let specs = vec![shell_spec("m1", Some(0.8))];
        let samples = vec![sample("m1", true), sample("m1", false)];

        finalize_run(
            skill_dir,
            "mint-ipnft",
            "2026-04-22T14-03-11Z",
            &specs,
            &samples,
            10,
            0, // no retention pruning in this test
        )
        .unwrap();

        let mj: MetricsJson =
            serde_json::from_slice(&std::fs::read(metrics_json_path(skill_dir)).unwrap()).unwrap();
        assert_eq!(mj.skill, "mint-ipnft");
        let r = mj.metrics.get("m1").unwrap();
        assert!((r.pass_rate - 0.5).abs() < 1e-4);
        assert_eq!(r.n, 2);
        assert!(r.gated, "0.5 < 0.8 should be gated");

        let h = std::fs::read_to_string(history_path(skill_dir)).unwrap();
        assert_eq!(h.lines().count(), 1);
        assert!(std::fs::metadata(
            per_run_dir(skill_dir, "2026-04-22T14-03-11Z").join("report.json")
        )
        .is_ok());
    }

    #[test]
    fn rolling_window_trims_series() {
        let dir = TempDir::new().unwrap();
        let skill_dir = dir.path();
        let specs = vec![shell_spec("m1", Some(0.8))];

        // 12 runs alternating pass/fail
        for i in 0..12 {
            let ok = i % 2 == 0;
            let samples = vec![sample("m1", ok)];
            finalize_run(skill_dir, "s", &format!("t{i}"), &specs, &samples, 5, 0).unwrap();
        }
        let mj: MetricsJson =
            serde_json::from_slice(&std::fs::read(metrics_json_path(skill_dir)).unwrap()).unwrap();
        let r = mj.metrics.get("m1").unwrap();
        // Last 5 runs: t7(fail=0), t8(pass=1), t9(fail=0), t10(pass=1), t11(fail=0)
        // Average of [0.0, 1.0, 0.0, 1.0, 0.0] == 0.4
        assert!((r.pass_rate - 0.4).abs() < 1e-4, "got {}", r.pass_rate);
    }

    #[test]
    fn gated_only_when_min_present_and_below() {
        let dir = TempDir::new().unwrap();
        let skill_dir = dir.path();
        let specs = vec![
            shell_spec("m1", None),      // no min -> never gated
            shell_spec("m2", Some(0.5)), // pass_rate 1.0 -> not gated
        ];
        let mut outs = BTreeMap::new();
        outs.insert(
            "m1".into(),
            MetricOutcome {
                pass: true,
                score: 1.0,
                notes: None,
                raw: json!({}),
            },
        );
        outs.insert(
            "m2".into(),
            MetricOutcome {
                pass: true,
                score: 1.0,
                notes: None,
                raw: json!({}),
            },
        );
        let samples = vec![RunSample {
            fixture_id: "f".into(),
            outcomes: outs,
        }];

        finalize_run(skill_dir, "s", "t0", &specs, &samples, 10, 0).unwrap();
        let mj: MetricsJson =
            serde_json::from_slice(&std::fs::read(metrics_json_path(skill_dir)).unwrap()).unwrap();
        assert!(!mj.metrics["m1"].gated);
        assert!(!mj.metrics["m2"].gated);
    }

    #[test]
    fn retention_prunes_old_per_run_dirs() {
        let dir = TempDir::new().unwrap();
        let skill_dir = dir.path();
        let specs = vec![shell_spec("m1", Some(0.8))];

        // 7 runs with max_per_run_reports = 3 — should retain runs [t4, t5, t6].
        for i in 0..7 {
            let samples = vec![sample("m1", true)];
            finalize_run(skill_dir, "s", &format!("t{i:02}"), &specs, &samples, 10, 3).unwrap();
        }

        let runs_dir = skill_dir.join("metrics").join("runs");
        let mut dirs: Vec<String> = std::fs::read_dir(&runs_dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().to_string())
            .collect();
        dirs.sort();
        assert_eq!(dirs, vec!["t04", "t05", "t06"]);

        // history.jsonl still has all 7 lines — retention only touches run dirs.
        let h = std::fs::read_to_string(history_path(skill_dir)).unwrap();
        assert_eq!(h.lines().count(), 7);
    }

    #[test]
    fn compute_rollups_emits_stddev_when_n_ge_2() {
        let dir = TempDir::new().unwrap();
        let skill_dir = dir.path();
        let specs = vec![shell_spec("m1", None)];
        // Five runs with pass rates: 1, 0, 1, 0, 1 (alternating).
        // Mean = 0.6, sample stddev = sqrt((4 * 0.16 + 1 * 0.36 - wait)
        //   deviations: 0.4, -0.6, 0.4, -0.6, 0.4
        //   sq devs: 0.16, 0.36, 0.16, 0.36, 0.16  → sum = 1.20
        //   sample var = 1.20 / 4 = 0.30 → stddev ≈ 0.5477226
        for i in 0..5 {
            let ok = i % 2 == 0;
            let samples = vec![sample("m1", ok)];
            finalize_run(skill_dir, "s", &format!("t{i}"), &specs, &samples, 10, 0).unwrap();
        }
        let mj: MetricsJson =
            serde_json::from_slice(&std::fs::read(metrics_json_path(skill_dir)).unwrap()).unwrap();
        let r = mj.metrics.get("m1").unwrap();
        let s = r.stddev.expect("stddev should be Some when n >= 2");
        assert!(
            (s - 0.547_722_6).abs() < 1e-3,
            "expected ~0.5477, got {}",
            s
        );
    }

    #[test]
    fn compute_rollups_omits_stddev_when_n_lt_2() {
        let dir = TempDir::new().unwrap();
        let skill_dir = dir.path();
        let specs = vec![shell_spec("m1", None)];
        let samples = vec![sample("m1", true)];
        finalize_run(skill_dir, "s", "t0", &specs, &samples, 10, 0).unwrap();
        let mj: MetricsJson =
            serde_json::from_slice(&std::fs::read(metrics_json_path(skill_dir)).unwrap()).unwrap();
        let r = mj.metrics.get("m1").unwrap();
        assert!(r.stddev.is_none(), "stddev should be None for n < 2");
    }

    #[test]
    fn compute_rollups_emits_min_max_over_window() {
        let dir = TempDir::new().unwrap();
        let skill_dir = dir.path();
        // Use llm_judge-style multi-fixture rates by writing aggregated
        // history.jsonl entries directly through finalize_run with crafted
        // sample sets: 2/5, 5/5 pass; for shell_check we get 0 or 1 only,
        // so build three runs hitting 0.4, 0.7, 0.9 by varying fixture mix.
        let specs = vec![shell_spec("m1", None)];
        let make = |passes: u32, total: u32| -> Vec<RunSample> {
            (0..total).map(|i| sample("m1", i < passes)).collect()
        };
        // 0.4 = 2/5, 0.7 ≈ 7/10, 0.9 = 9/10
        finalize_run(skill_dir, "s", "t0", &specs, &make(2, 5), 10, 0).unwrap();
        finalize_run(skill_dir, "s", "t1", &specs, &make(7, 10), 10, 0).unwrap();
        finalize_run(skill_dir, "s", "t2", &specs, &make(9, 10), 10, 0).unwrap();
        let mj: MetricsJson =
            serde_json::from_slice(&std::fs::read(metrics_json_path(skill_dir)).unwrap()).unwrap();
        let r = mj.metrics.get("m1").unwrap();
        assert!((r.min.unwrap() - 0.4).abs() < 1e-4, "min={:?}", r.min);
        assert!((r.max.unwrap() - 0.9).abs() < 1e-4, "max={:?}", r.max);
    }

    #[test]
    fn metrics_json_back_compat_for_old_files_without_variance() {
        // Old shape — no stddev/min/max keys.
        let raw = r#"{
            "pass_rate": 0.75,
            "n": 4,
            "min_pass_rate": 0.5,
            "gated": false
        }"#;
        let r: MetricRollup = serde_json::from_str(raw).expect("must deserialize old shape");
        assert!((r.pass_rate - 0.75).abs() < 1e-4);
        assert_eq!(r.n, 4);
        assert!(r.stddev.is_none());
        assert!(r.min.is_none());
        assert!(r.max.is_none());
    }

    #[test]
    fn retention_zero_disables_pruning() {
        let dir = TempDir::new().unwrap();
        let runs = dir.path().join("runs");
        std::fs::create_dir_all(runs.join("a")).unwrap();
        std::fs::create_dir_all(runs.join("b")).unwrap();
        std::fs::create_dir_all(runs.join("c")).unwrap();

        prune_old_run_dirs(&runs, 0);
        let count = std::fs::read_dir(&runs).unwrap().count();
        assert_eq!(count, 3);
    }
}
