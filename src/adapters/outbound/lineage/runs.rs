//! Backtest run dirs, read-only (`docs/xlab-2026-10-01.md` § 10 run dir,
//! `docs/lineage-2026-10-06.md` § 5): the attempts behind search accounting
//! ([`RunDirs`]) and the figures a result's `extract` recomputes
//! ([`ReportJson`]).
//!
//! | [`RunDirs`] | Reads |
//! |---|---|
//! | runs | `<TENGU_HOME>/state/<state>/backtests/<run id>/report.json` and `keep-<run id>/report.json`: `run_id` (else the dir name), `strategy`, `kind`, `spec_sha256`, `split`; a run-named dir without a readable report is a problem |
//! | holdout reads | `<…>/backtests/holdout-reads.jsonl`: `run_id`, `spec_sha256`, `strategy`, `time` per line |
//!
//! | [`ReportJson`] `extract` | Figures (`report.json` as `BacktestReport`) |
//! |---|---|
//! | `arm:<name>` | `arms.<name>.summary`: `n`, `mean_net_bps`, [`ci95_lo_bps`, `ci95_hi_bps`], `net_usd`, `t_stat` |
//! | `arm:<name>/in_sample` · `arm:<name>/holdout` | the same of `arms.<name>.split.in_sample` / `.holdout` |
//! | `gate` | jev − rules: `n` = `gate.decided`, mean = `gate.diff_ci.diff_bps`, CI = [`lo_bps`, `hi_bps`] |
//! | anything else | not this source (`None`) |

use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::domain::backtest::report::BacktestReport;
use crate::domain::backtest::stats::Summary;
use crate::domain::lineage::query::{HoldoutRead, RunAttempt};
use crate::ports::lineage::{AttemptSource, Extracted, ResultSource};

/// The holdout ledger's file name (`tools/xlab/holdout.rs`).
const HOLDOUT_READS: &str = "holdout-reads.jsonl";

/// Module table.
pub(crate) struct RunDirs {
    pub tengu_home: PathBuf,
}

impl RunDirs {
    fn backtests(&self, state: &str) -> PathBuf {
        self.tengu_home.join("state").join(state).join("backtests")
    }
}

fn str_field(v: &Value, k: &str) -> Option<String> {
    v.get(k).and_then(Value::as_str).map(String::from)
}

impl AttemptSource for RunDirs {
    fn runs(&self, state: &str) -> (Vec<RunAttempt>, Vec<String>) {
        let dir = self.backtests(state);
        let (mut runs, mut problems) = (Vec::new(), Vec::new());
        let Ok(entries) = std::fs::read_dir(&dir) else {
            return (runs, problems);
        };
        for e in entries.flatten() {
            let path = e.path();
            if !path.is_dir() {
                continue;
            }
            let name = e.file_name().to_string_lossy().to_string();
            let (kept, run_name) = match name.strip_prefix("keep-") {
                Some(rest) => (true, rest.to_string()),
                None => (false, name.clone()),
            };
            // Run ids start with their UTC stamp (`20261001T…Z-<strategy>`).
            if !run_name.starts_with(|c: char| c.is_ascii_digit()) {
                continue;
            }
            let report = path.join("report.json");
            let v: Value = match std::fs::read_to_string(&report)
                .map_err(|e| e.to_string())
                .and_then(|t| serde_json::from_str(&t).map_err(|e| e.to_string()))
            {
                Ok(v) => v,
                Err(e) => {
                    problems.push(format!("{}: {e}", report.display()));
                    continue;
                }
            };
            let Some(spec_sha256) = str_field(&v, "spec_sha256") else {
                problems.push(format!("{}: no spec_sha256", report.display()));
                continue;
            };
            runs.push(RunAttempt {
                state: state.to_string(),
                run_id: str_field(&v, "run_id").unwrap_or(run_name),
                kept,
                strategy: str_field(&v, "strategy").unwrap_or_default(),
                kind: str_field(&v, "kind"),
                spec_sha256,
                split: str_field(&v, "split"),
            });
        }
        (runs, problems)
    }

    fn holdout_reads(&self, state: &str) -> (Vec<HoldoutRead>, Vec<String>) {
        let file = self.backtests(state).join(HOLDOUT_READS);
        let (mut reads, mut problems) = (Vec::new(), Vec::new());
        let Ok(text) = std::fs::read_to_string(&file) else {
            return (reads, problems);
        };
        for (i, line) in text.lines().enumerate() {
            if line.trim().is_empty() {
                continue;
            }
            let parsed = serde_json::from_str::<Value>(line).ok().and_then(|v| {
                Some(HoldoutRead {
                    state: state.to_string(),
                    run_id: str_field(&v, "run_id")?,
                    spec_sha256: str_field(&v, "spec_sha256")?,
                    strategy: str_field(&v, "strategy"),
                    time: str_field(&v, "time"),
                })
            });
            match parsed {
                Some(r) => reads.push(r),
                None => problems.push(format!(
                    "{} line {}: not a holdout read (run_id, spec_sha256)",
                    file.display(),
                    i + 1
                )),
            }
        }
        (reads, problems)
    }
}

/// `report.json` figures (module table).
pub(crate) struct ReportJson;

fn of_summary(s: &Summary) -> Extracted {
    Extracted {
        n: Some(s.n as u64),
        mean_net_bps: s.mean_net_bps,
        ci95_bps: s.ci95_lo_bps.zip(s.ci95_hi_bps).map(|(lo, hi)| [lo, hi]),
        net_usd: Some(s.net_usd),
        t_stat: s.t_stat,
    }
}

impl ReportJson {
    fn read(path: &Path) -> Result<BacktestReport, String> {
        let file = if path.is_dir() {
            path.join("report.json")
        } else {
            path.to_path_buf()
        };
        let text =
            std::fs::read_to_string(&file).map_err(|e| format!("{}: {e}", file.display()))?;
        serde_json::from_str(&text)
            .map_err(|e| format!("{}: not a report.json: {e}", file.display()))
    }
}

impl ResultSource for ReportJson {
    fn extract(&self, path: &Path, extract: &str) -> Option<Result<Extracted, String>> {
        if extract != "gate" && !extract.starts_with("arm:") {
            return None;
        }
        let report = match Self::read(path) {
            Ok(r) => r,
            Err(e) => return Some(Err(e)),
        };
        if extract == "gate" {
            return Some(match &report.gate {
                None => Err(format!(
                    "run {} has no Jev gate arm (report.gate)",
                    report.run_id
                )),
                Some(g) => Ok(Extracted {
                    n: Some(g.decided as u64),
                    mean_net_bps: g.diff_ci.as_ref().map(|d| d.diff_bps),
                    ci95_bps: g.diff_ci.as_ref().map(|d| [d.lo_bps, d.hi_bps]),
                    net_usd: None,
                    t_stat: None,
                }),
            });
        }
        let spec = &extract["arm:".len()..];
        let (name, half) = match spec.split_once('/') {
            Some((n, h)) => (n, Some(h)),
            None => (spec, None),
        };
        let Some(arm) = report.arms.get(name) else {
            let have: Vec<&str> = report.arms.keys().map(String::as_str).collect();
            return Some(Err(format!(
                "run {} has no arm `{name}` (arms: {})",
                report.run_id,
                have.join(", ")
            )));
        };
        Some(match half {
            None => Ok(of_summary(&arm.summary)),
            Some(h @ ("in_sample" | "holdout")) => match &arm.split {
                None => Err(format!(
                    "arm `{name}` of run {} has no split",
                    report.run_id
                )),
                Some(s) if h == "in_sample" => Ok(of_summary(&s.in_sample)),
                Some(s) => Ok(of_summary(&s.holdout)),
            },
            Some(other) => Err(format!(
                "extract `{extract}`: `/{other}` is not /in_sample or /holdout"
            )),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn home() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/lineage/home")
    }

    fn run_dir(name: &str) -> PathBuf {
        home().join("state/xlab/backtests").join(name)
    }

    #[test]
    fn run_dirs_and_holdout_reads_of_the_fixture() {
        let src = RunDirs { tengu_home: home() };
        let (mut runs, problems) = src.runs("xlab");
        runs.sort_by(|a, b| a.run_id.cmp(&b.run_id));
        assert!(problems.is_empty(), "{problems:?}");
        let ids: Vec<(&str, bool)> = runs.iter().map(|r| (r.run_id.as_str(), r.kept)).collect();
        assert_eq!(
            ids,
            vec![
                ("20260930T090000Z-rule_w", true),
                ("20261001T120034Z-rule_w", false),
                ("20261002T100000Z-rule_w_top4", false),
            ]
        );
        assert_eq!(
            runs[1].spec_sha256,
            "cba7a380444a5e6648f0d1421837ce5504c0a1e55e6b3126a8de6596ea343a7f"
        );
        assert_eq!(runs[1].split.as_deref(), Some("time:2026-07-01T00:00:00Z"));
        let (reads, problems) = src.holdout_reads("xlab");
        assert!(problems.is_empty());
        assert_eq!(reads.len(), 1);
        assert_eq!(reads[0].run_id, "20261001T120034Z-rule_w");
        assert_eq!(src.runs("no-such-state"), (vec![], vec![]));
    }

    #[test]
    fn report_json_gives_arms_halves_and_the_gate() {
        let rj = ReportJson;
        let got = rj
            .extract(&run_dir("20261001T120034Z-rule_w"), "arm:research/holdout")
            .unwrap()
            .unwrap();
        assert_eq!(
            got,
            Extracted {
                n: Some(480),
                mean_net_bps: Some(47.04),
                ci95_bps: Some([3.1, 91.0]),
                net_usd: Some(225.8),
                t_stat: Some(2.05),
            }
        );
        let report = run_dir("20261001T120034Z-rule_w").join("report.json");
        let whole = rj.extract(&report, "arm:research").unwrap().unwrap();
        assert_eq!((whole.n, whole.mean_net_bps), (Some(1480), Some(41.27)));
        let gate = rj
            .extract(&run_dir("20261002T100000Z-rule_w_top4"), "gate")
            .unwrap()
            .unwrap();
        assert_eq!(
            (gate.n, gate.mean_net_bps, gate.ci95_bps),
            (Some(120), Some(-5.1), Some([-60.2, 48.7]))
        );
        let no_arm = rj.extract(&report, "arm:jev").unwrap().unwrap_err();
        assert!(no_arm.contains("no arm `jev` (arms: research)"), "{no_arm}");
        assert!(rj.extract(&report, "gate").unwrap().is_err());
        assert!(rj.extract(&report, "arm:research/oops").unwrap().is_err());
        assert!(rj
            .extract(&run_dir("20261002T100000Z-rule_w_top4"), "arm:jev/holdout")
            .unwrap()
            .is_err());
        assert_eq!(rj.extract(&report, "ledger:xmarket-weekend"), None);
    }
}
