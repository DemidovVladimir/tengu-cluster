//! Strategy ranking end to end on the built binary
//! (`docs/strategy-ranking-automation-2026-10-08.md` SR-7): `tengu ranking`
//! on the fixture sandbox `tests/fixtures/strategy_ranking/config.toml`,
//! copied to `<tmp>/sandboxes/rank-test/config.toml` beside a copy of its
//! registry (`lineage/`: the contract `rank.test.v1`, sealed), `TENGU_HOME`
//! a temp dir, `market.db` seeded by `tengu history import-json` from the
//! captured `xyz:TSLA` hours (`tests/fixtures/xlab/dataset_xyz_TSLA_1h.json`,
//! 2026-09-25T20:00Z to 2026-09-28T15:00Z). No network, no model.
//!
//! | Test | Expected |
//! |---|---|
//! | `published_rerun_is_a_noop` | 2026-09-28 ranks `rank_fade` (−27.39) below `rank_follow` (+19.79); a second `run` prints `already published` and leaves every ranking file and the run dirs byte for byte; `show` prints `latest.md` |
//! | `restart_reruns_only_the_unfinished_strategy` | a copied state whose manifest is back to `RUNNING` and one run dir lost its `report.json` (a crash): the resume reruns that strategy only, reuses the other's run, and publishes the same `content_sha256` |
//! | `incomplete_run_keeps_latest` | 2026-09-27 COMPLETE (one trade each: both ineligible, no CI); then `rank_follow` also reads a name with no bars (`xyz:NVDA`): 2026-09-28 exits 1 INCOMPLETE (`rank_follow stale`), its dated files written, `latest.*` unchanged; a rerun stays INCOMPLETE, a no-op |
//! | `no_holdout_read_is_written` | no `holdout-reads.jsonl`, no `decisions.jsonl`; every report has no split and data through the cutoff |
//! | `unsealed_contract_is_refused` | no `[[sealed]]` row ⇒ exit 1 `contract_unsealed`; a sealed contract edited since ⇒ `contract_changed`; nothing written either way |

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use serde_json::Value;

const FIXTURE: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/strategy_ranking"
);
const DATASET: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/xlab/dataset_xyz_TSLA_1h.json"
);
const ID: &str = "rank.test.v1";
/// The sandbox the contract names: the config's dir.
const SANDBOX: &str = "rank-test";
/// The captures' last bar closes at this date's cutoff (15:00 UTC).
const LAST: &str = "2026-09-28";
/// A day earlier: one trade per strategy before its cutoff.
const EARLIER: &str = "2026-09-27";

/// A sandbox root: `sandboxes/rank-test/config.toml`, `lineage/`, `home/`
/// (`TENGU_HOME`), `market.db` seeded.
struct Fx {
    _tmp: tempfile::TempDir,
    root: PathBuf,
}

fn copy_dir(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).unwrap();
    for e in std::fs::read_dir(from).unwrap().flatten() {
        let target = to.join(e.file_name());
        if e.path().is_dir() {
            copy_dir(&e.path(), &target);
        } else {
            std::fs::copy(e.path(), &target).unwrap();
        }
    }
}

fn text(o: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&o.stdout),
        String::from_utf8_lossy(&o.stderr)
    )
}

fn fx() -> Fx {
    let tmp = tempfile::tempdir().unwrap();
    let root = std::fs::canonicalize(tmp.path()).unwrap();
    let dir = root.join("sandboxes").join(SANDBOX);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::copy(
        Path::new(FIXTURE).join("config.toml"),
        dir.join("config.toml"),
    )
    .unwrap();
    copy_dir(&Path::new(FIXTURE).join("lineage"), &root.join("lineage"));
    let f = Fx { _tmp: tmp, root };
    let o = f.tengu(&["history", "import-json", "--file", DATASET]);
    assert!(
        o.status.success() && text(&o).contains("135 rows written, 0 error(s)"),
        "seeding market.db failed:\n{}",
        text(&o)
    );
    f
}

impl Fx {
    fn home(&self) -> PathBuf {
        self.root.join("home")
    }

    fn config(&self) -> PathBuf {
        self.root
            .join("sandboxes")
            .join(SANDBOX)
            .join("config.toml")
    }

    fn state(&self) -> PathBuf {
        self.home().join("state").join(SANDBOX)
    }

    fn rankings(&self) -> PathBuf {
        self.state().join("strategy-rankings").join(ID)
    }

    fn backtests(&self) -> PathBuf {
        self.state().join("backtests")
    }

    /// `tengu -c <config> <args>` with this root's `TENGU_HOME`, cwd = the
    /// root (no repo `.env`).
    fn tengu(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_tengu"))
            .arg("-c")
            .arg(self.config())
            .args(args)
            .current_dir(&self.root)
            .env("TENGU_HOME", self.home())
            .env_remove("TENGU_CONFIG")
            .env_remove("TENGU_EGRESS")
            .env_remove("TENGU_SESSION_ID")
            .output()
            .expect("run tengu")
    }

    fn rank(&self, date: &str) -> Output {
        self.tengu(&["ranking", "run", "--date", date])
    }

    fn json(&self, path: &Path) -> Value {
        let raw = std::fs::read(path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
        serde_json::from_slice(&raw).unwrap()
    }

    fn manifest(&self, date: &str) -> Value {
        self.json(&self.rankings().join(date).join("manifest.json"))
    }

    /// The run dirs under `backtests/`, sorted.
    fn runs(&self) -> Vec<String> {
        let mut out: Vec<String> = std::fs::read_dir(self.backtests())
            .map(|rd| {
                rd.flatten()
                    .filter(|e| e.path().is_dir())
                    .map(|e| e.file_name().to_string_lossy().into_owned())
                    .collect()
            })
            .unwrap_or_default();
        out.sort();
        out
    }

    /// Every file under `dir` with its bytes, sorted by path.
    fn files(&self, dir: &Path) -> Vec<(PathBuf, Vec<u8>)> {
        fn walk(dir: &Path, out: &mut Vec<(PathBuf, Vec<u8>)>) {
            for e in std::fs::read_dir(dir).into_iter().flatten().flatten() {
                let p = e.path();
                if p.is_dir() {
                    walk(&p, out);
                } else {
                    out.push((p.clone(), std::fs::read(&p).unwrap()));
                }
            }
        }
        let mut out = Vec::new();
        walk(dir, &mut out);
        out.sort();
        out
    }
}

/// `strategy` → its `run:<state>/<run id>` in a manifest.
fn run_of(manifest: &Value, strategy: &str) -> String {
    manifest["strategies"][strategy]["run"]
        .as_str()
        .unwrap_or_else(|| panic!("no run of {strategy}: {manifest:#}"))
        .to_string()
}

#[test]
fn published_rerun_is_a_noop() {
    let f = fx();
    let o = f.rank(LAST);
    let out = text(&o);
    assert!(o.status.success(), "{out}");
    let line = |strategy: &str, rank: usize, ci_lo: &str, mean: &str| {
        format!("\n  {rank}. {strategy}  ci95_lo {ci_lo}  mean {mean}  n 2  run:{SANDBOX}/")
    };
    for want in [
        format!("strategy ranking `{ID}` {LAST}: COMPLETE · contract sha256 "),
        "decisions 2026-09-25T00:00:00Z → 2026-09-28T15:00:00Z (cutoff, data through it) · arm \
         research · NOT_GATED"
            .to_string(),
        line("rank_fade", 1, "-56.86", "-27.39"),
        line("rank_follow", 2, "-9.67", "+19.79"),
        "published now · latest replaced".to_string(),
    ] {
        assert!(out.contains(&want), "{want:?} not in:\n{out}");
    }
    assert_eq!(f.manifest(LAST)["status"], "COMPLETE");
    let before = f.files(&f.rankings());
    let names: Vec<String> = before
        .iter()
        .map(|(p, _)| p.strip_prefix(f.rankings()).unwrap().display().to_string())
        .collect();
    assert_eq!(
        names,
        [
            format!("{LAST}/manifest.json"),
            format!("{LAST}/ranking.json"),
            format!("{LAST}/ranking.md"),
            "latest.json".into(),
            "latest.md".into(),
        ]
    );
    let runs = f.runs();
    assert_eq!(runs.len(), 2, "{runs:?}");
    let run_files = f.files(&f.backtests());

    let again = f.rank(LAST);
    let out = text(&again);
    assert!(again.status.success(), "{out}");
    assert!(
        out.contains("already published by an earlier run — nothing ran"),
        "{out}"
    );
    assert!(
        f.files(&f.rankings()) == before,
        "a published date is rewritten"
    );
    assert!(f.files(&f.backtests()) == run_files, "a run dir changed");
    assert_eq!(f.runs(), runs, "something ran");

    let show = f.tengu(&["ranking", "show"]);
    assert!(show.status.success(), "{}", text(&show));
    assert_eq!(
        show.stdout,
        std::fs::read(f.rankings().join("latest.md")).unwrap()
    );
}

#[test]
fn restart_reruns_only_the_unfinished_strategy() {
    let f = fx();
    let o = f.rank(LAST);
    assert!(o.status.success(), "{}", text(&o));
    let first = f.manifest(LAST);
    // A copy of the state, stopped mid-run: the manifest back to RUNNING,
    // nothing of the date published, rank_follow's report lost.
    let g = fx();
    std::fs::remove_dir_all(g.state()).unwrap();
    copy_dir(&f.state(), &g.state());
    let dir = g.rankings().join(LAST);
    let mut m = g.manifest(LAST);
    m["status"] = "RUNNING".into();
    for key in ["finished_at_ms", "content_sha256"] {
        m.as_object_mut().unwrap().remove(key);
    }
    std::fs::write(dir.join("manifest.json"), m.to_string()).unwrap();
    for name in ["ranking.json", "ranking.md"] {
        std::fs::remove_file(dir.join(name)).unwrap();
    }
    let follow = run_of(&first, "rank_follow");
    let follow_id = follow.rsplit('/').next().unwrap();
    std::fs::remove_file(g.backtests().join(follow_id).join("report.json")).unwrap();

    let o = g.rank(LAST);
    assert!(o.status.success(), "{}", text(&o));
    assert!(text(&o).contains("published now"), "{}", text(&o));
    let resumed = g.manifest(LAST);
    assert_eq!(resumed["status"], "COMPLETE");
    assert_eq!(
        run_of(&resumed, "rank_fade"),
        run_of(&first, "rank_fade"),
        "rank_fade's run is reused"
    );
    assert_ne!(
        run_of(&resumed, "rank_follow"),
        follow,
        "rank_follow ran again"
    );
    assert_eq!(g.runs().len(), 3, "{:?}", g.runs());
    assert_eq!(resumed["content_sha256"], first["content_sha256"]);
    assert_eq!(resumed["started_at_ms"], first["started_at_ms"]);
}

#[test]
fn incomplete_run_keeps_latest() {
    let f = fx();
    let o = f.rank(EARLIER);
    let out = text(&o);
    assert!(o.status.success(), "{out}");
    // One trade each: no bootstrap CI, so both ineligible — still COMPLETE
    // (every strategy evaluated), so latest moves to it.
    assert!(
        out.contains(&format!("strategy ranking `{ID}` {EARLIER}: COMPLETE"))
            && out.contains(
                "\nineligible (2): rank_fade metric_missing:ci95_lo_bps, rank_follow \
                 metric_missing:ci95_lo_bps"
            )
            && out.contains("published now · latest replaced"),
        "{out}"
    );
    let latest: Vec<Vec<u8>> = ["latest.json", "latest.md"]
        .iter()
        .map(|n| std::fs::read(f.rankings().join(n)).unwrap())
        .collect();
    // rank_follow now also reads a name with no stored bars.
    let config = std::fs::read_to_string(f.config()).unwrap();
    let edited = config.replacen(
        "universe = \"@tsla\"\ninterval = \"1h\"\nlookback_bars = 1\nthreshold_bps = 25\ndirection = \"follow\"",
        "universe = [\"hyperliquid:xyz:TSLA\", \"hyperliquid:xyz:NVDA\"]\ninterval = \"1h\"\nlookback_bars = 1\nthreshold_bps = 25\ndirection = \"follow\"",
        1,
    );
    assert_ne!(edited, config, "the fixture's rank_follow block moved");
    std::fs::write(f.config(), edited).unwrap();

    let o = f.rank(LAST);
    let out = text(&o);
    assert_eq!(o.status.code(), Some(1), "{out}");
    for want in [
        format!("strategy ranking `{ID}` {LAST}: INCOMPLETE"),
        "\n  1. rank_fade  ci95_lo -56.86  mean -27.39  n 2".to_string(),
        "\nfailed (1): rank_follow stale".to_string(),
        format!("ranking {LAST} of `{ID}` is INCOMPLETE: 1 listed strateg(ies) without a run"),
    ] {
        assert!(out.contains(&want), "{want:?} not in:\n{out}");
    }
    let m = f.manifest(LAST);
    assert_eq!(m["status"], "INCOMPLETE");
    assert_eq!(m["latest_replaced"], false);
    assert_eq!(m["strategies"]["rank_follow"]["status"], "STALE");
    assert!(
        m["strategies"]["rank_follow"]["error"]
            .as_str()
            .unwrap()
            .contains("hyperliquid:xyz:NVDA: no 1h bars stored"),
        "{m:#}"
    );
    for name in ["ranking.json", "ranking.md"] {
        assert!(f.rankings().join(LAST).join(name).is_file(), "{name}");
    }
    for (n, before) in ["latest.json", "latest.md"].iter().zip(&latest) {
        assert_eq!(
            &std::fs::read(f.rankings().join(n)).unwrap(),
            before,
            "{n} moved"
        );
    }
    // INCOMPLETE is terminal for the date: a rerun is a no-op, still exit 1.
    let runs = f.runs();
    let again = f.rank(LAST);
    assert_eq!(again.status.code(), Some(1), "{}", text(&again));
    assert!(
        text(&again).contains("already published by an earlier run"),
        "{}",
        text(&again)
    );
    assert_eq!(f.runs(), runs);
}

#[test]
fn no_holdout_read_is_written() {
    let f = fx();
    for date in [EARLIER, LAST] {
        let o = f.rank(date);
        assert!(o.status.success(), "{}", text(&o));
    }
    assert!(!f.backtests().join("holdout-reads.jsonl").exists());
    let runs = f.runs();
    assert_eq!(runs.len(), 4, "{runs:?}");
    for run in &runs {
        let dir = f.backtests().join(run);
        assert!(!dir.join("decisions.jsonl").exists(), "{run}: a gate ran");
        let report = f.json(&dir.join("report.json"));
        assert!(
            report["split"].is_null(),
            "{run}: split {}",
            report["split"]
        );
        assert_eq!(report["data_through_ms"], report["to_ms"], "{run}");
        let arms: Vec<&String> = report["arms"].as_object().unwrap().keys().collect();
        assert_eq!(arms, ["research"], "{run}: rules arm only");
    }
}

#[test]
fn unsealed_contract_is_refused() {
    // No [[sealed]] row.
    let f = fx();
    let locks = f.root.join("lineage/locks.toml");
    let sealed = std::fs::read_to_string(&locks).unwrap();
    let header: String = sealed
        .lines()
        .take_while(|l| !l.starts_with("[[sealed]]"))
        .map(|l| format!("{l}\n"))
        .collect();
    std::fs::write(&locks, header).unwrap();
    let o = f.rank(LAST);
    let out = text(&o);
    assert_eq!(o.status.code(), Some(1), "{out}");
    assert!(
        out.contains(&format!(
            "contract_unsealed: `{ID}` has no [[sealed]] row in locks.toml"
        )),
        "{out}"
    );
    assert!(!f.state().join("strategy-rankings").exists());
    assert!(f.runs().is_empty());

    // Sealed, then edited: a new contract, refused.
    let g = fx();
    let path = g.root.join("lineage/rankings").join(format!("{ID}.toml"));
    let c = std::fs::read_to_string(&path).unwrap();
    std::fs::write(&path, c.replace("min_trades = 1", "min_trades = 2")).unwrap();
    let o = g.rank(LAST);
    let out = text(&o);
    assert_eq!(o.status.code(), Some(1), "{out}");
    assert!(
        out.contains(&format!("contract_changed: `{ID}` was sealed as ")),
        "{out}"
    );
    assert!(!g.state().join("strategy-rankings").exists());
    assert!(g.runs().is_empty());
}
