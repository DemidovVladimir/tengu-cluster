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

pub(crate) fn skill_dir(workspace: &Path, skill: &str) -> PathBuf {
    workspace.join("skills").join(skill)
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

/// Write the full per-run report + append to history + recompute metrics.json.
pub(crate) fn finalize_run(
    skill_dir: &Path,
    skill: &str,
    ts: &str,
    specs: &[MetricSpec],
    samples: &[RunSample],
    rolling_window: u32,
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
        let min = spec.min_pass_rate();
        let gated = min.is_some_and(|m| pass_rate < m);
        out.insert(
            name.clone(),
            MetricRollup {
                pass_rate,
                n: *per_metric_n.get(&name).unwrap_or(&0),
                min_pass_rate: min,
                gated,
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
            finalize_run(skill_dir, "s", &format!("t{i}"), &specs, &samples, 5).unwrap();
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

        finalize_run(skill_dir, "s", "t0", &specs, &samples, 10).unwrap();
        let mj: MetricsJson =
            serde_json::from_slice(&std::fs::read(metrics_json_path(skill_dir)).unwrap()).unwrap();
        assert!(!mj.metrics["m1"].gated);
        assert!(!mj.metrics["m2"].gated);
    }
}
