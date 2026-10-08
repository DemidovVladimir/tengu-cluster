//! `tengu soe` black-box (`src/adapters/inbound/cli/soe.rs`): the built
//! binary on the synthetic fixtures (`tests/fixtures/soe/`), `TENGU_HOME` a
//! temp dir — never `~/.tengu`.
//!
//! | Case | Expected |
//! |---|---|
//! | `eval` on the 16 fixture cases (synthetic profile allowed) | exit 0, every case ok, no class without a case |
//! | `eval` with one wrong expectation | exit 1, the case FAILs with its diff |
//! | `check` with no profile | exit 1, `operator_profile_missing` |
//! | `check` on the synthetic profile without `--allow-synthetic` | exit 1, `synthetic_profile_refused` |
//! | `init`, then `check`; `init` again; `init` inside a git work tree | template 0600 unsigned; `operator_profile_unsigned`; `profile_exists` (file kept); `profile_in_repo` |
//! | `portfolio` on the example week, twice | byte-identical canonical JSON; 3 ranked (each `HOLD`), 1 held, 1 rejected; full hashes |
//! | `check --format json` | three scenarios, the verdict, full 64-hex hashes |
//! | `sensitivity` | 8 tornado rows |
//! | the example week | its files equal the candidates and `[[cited]]` views of the three eval cases it comes from |

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use serde_json::Value;

const AS_OF: &str = "2026-09-21T18:00:00Z";
/// The eval cases the example week is cut from (all decided at `AS_OF`).
const WEEK_CASES: [&str; 3] = [
    "contradictory_sources",
    "boundary_cash_exactly_cap",
    "good_high_ticket_integration",
];

fn fixtures() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/soe")
}

fn arg(p: &Path) -> &str {
    p.to_str().unwrap()
}

fn tengu(home: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_tengu"))
        .args(args)
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .env("TENGU_HOME", home)
        .env_remove("TENGU_CONFIG")
        .output()
        .expect("run tengu")
}

fn text(o: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&o.stdout),
        String::from_utf8_lossy(&o.stderr)
    )
}

/// `args` + the synthetic profile, allowed.
fn synthetic<'a>(profile: &'a Path, args: &[&'a str]) -> Vec<&'a str> {
    let mut v = args.to_vec();
    v.extend(["--profile", arg(profile), "--allow-synthetic"]);
    v
}

fn hex64(v: &Value) -> bool {
    v.as_str()
        .is_some_and(|s| s.len() == 64 && s.bytes().all(|b| b.is_ascii_hexdigit()))
}

#[test]
fn eval_on_fixture_cases_exits_0() {
    let home = tempfile::tempdir().unwrap();
    let f = fixtures();
    let (cases, profile) = (f.join("cases"), f.join("profile.synthetic.toml"));
    let o = tengu(
        home.path(),
        &synthetic(&profile, &["soe", "eval", arg(&cases)]),
    );
    assert!(o.status.success(), "{}", text(&o));
    assert!(
        text(&o).contains("16 case(s): 16 ok, 0 FAIL · classes without a case: none"),
        "{}",
        text(&o)
    );
    let o = tengu(
        home.path(),
        &synthetic(&profile, &["soe", "eval", arg(&cases), "--format", "json"]),
    );
    assert!(o.status.success(), "{}", text(&o));
    let j: Value = serde_json::from_slice(&o.stdout).unwrap();
    assert_eq!(
        (j["ok"].as_u64(), j["failed"].as_u64()),
        (Some(16), Some(0))
    );
    assert!(hex64(&j["profile_sha256"]));
}

#[test]
fn eval_with_a_wrong_expectation_exits_1() {
    let home = tempfile::tempdir().unwrap();
    let f = fixtures();
    let dir = home.path().join("cases");
    std::fs::create_dir_all(&dir).unwrap();
    let case = std::fs::read_to_string(f.join("cases/hold_unknown_price.toml")).unwrap();
    let wrong = case.replace(
        "verdicts = { filing-automation-unpriced = \"HOLD\" }",
        "verdicts = { filing-automation-unpriced = \"REJECT\" }",
    );
    assert_ne!(wrong, case);
    std::fs::write(dir.join("hold_unknown_price.toml"), wrong).unwrap();
    let profile = f.join("profile.synthetic.toml");
    let o = tengu(
        home.path(),
        &synthetic(&profile, &["soe", "eval", arg(&dir)]),
    );
    assert_eq!(o.status.code(), Some(1), "{}", text(&o));
    let t = text(&o);
    assert!(t.contains("hold_unknown_price"), "{t}");
    assert!(
        t.contains("verdict `filing-automation-unpriced`: expected REJECT, got HOLD"),
        "{t}"
    );
    assert!(t.contains("soe eval: 1 case(s) FAIL"), "{t}");
}

#[test]
fn missing_profile_exits_1_operator_profile_missing() {
    let home = tempfile::tempdir().unwrap();
    let opp = fixtures().join("week/opportunities/erp-integration.toml");
    let o = tengu(home.path(), &["soe", "check", arg(&opp)]);
    assert_eq!(o.status.code(), Some(1), "{}", text(&o));
    let t = text(&o);
    assert!(t.contains("operator_profile_missing: "), "{t}");
    // The default path is under TENGU_HOME, never ~/.tengu.
    let expected = home.path().join("state/soe/operator.toml");
    assert!(t.contains(arg(&expected)), "{t}");
    assert!(o.stdout.is_empty(), "{t}");
}

#[test]
fn synthetic_profile_refused_without_flag() {
    let home = tempfile::tempdir().unwrap();
    let f = fixtures();
    let opp = f.join("week/opportunities/erp-integration.toml");
    let profile = f.join("profile.synthetic.toml");
    let o = tengu(
        home.path(),
        &["soe", "check", arg(&opp), "--profile", arg(&profile)],
    );
    assert_eq!(o.status.code(), Some(1), "{}", text(&o));
    assert!(
        text(&o).contains("synthetic_profile_refused: "),
        "{}",
        text(&o)
    );
}

#[test]
fn init_writes_an_unsigned_private_template_once() {
    let home = tempfile::tempdir().unwrap();
    let path = home.path().join("state/soe/operator.toml");
    let o = tengu(home.path(), &["soe", "init", "--format", "json"]);
    assert!(o.status.success(), "{}", text(&o));
    let j: Value = serde_json::from_slice(&o.stdout).unwrap();
    assert_eq!(j["path"], arg(&path));
    assert_eq!(j["signed"], false);
    assert!(hex64(&j["sha256"]));
    let written = std::fs::read_to_string(&path).unwrap();
    assert!(written.contains("signed_by = \"UNSIGNED\""));
    assert!(written.contains("synthetic = false"));
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }

    // Every deciding command refuses it until it is signed.
    let opp = fixtures().join("week/opportunities/erp-integration.toml");
    let cases = fixtures().join("cases");
    for args in [
        vec!["soe", "check", arg(&opp)],
        vec!["soe", "sensitivity", arg(&opp)],
        vec!["soe", "eval", arg(&cases)],
    ] {
        let o = tengu(home.path(), &args);
        assert_eq!(o.status.code(), Some(1), "{args:?}: {}", text(&o));
        assert!(
            text(&o).contains("operator_profile_unsigned: "),
            "{args:?}: {}",
            text(&o)
        );
    }

    // Never overwritten.
    let o = tengu(home.path(), &["soe", "init"]);
    assert_eq!(o.status.code(), Some(1), "{}", text(&o));
    assert!(text(&o).contains("profile_exists: "), "{}", text(&o));
    assert_eq!(std::fs::read_to_string(&path).unwrap(), written);

    // Never inside a git work tree.
    let repo = home.path().join("repo");
    std::fs::create_dir_all(repo.join(".git")).unwrap();
    let in_repo = repo.join("operator.toml");
    let o = tengu(home.path(), &["soe", "init", "--profile", arg(&in_repo)]);
    assert_eq!(o.status.code(), Some(1), "{}", text(&o));
    assert!(text(&o).contains("profile_in_repo: "), "{}", text(&o));
    assert!(!in_repo.exists());
}

#[test]
fn portfolio_rerun_is_byte_identical() {
    let home = tempfile::tempdir().unwrap();
    let f = fixtures();
    let profile = f.join("profile.synthetic.toml");
    let (dir, cited) = (f.join("week/opportunities"), f.join("week/cited.toml"));
    let args = synthetic(
        &profile,
        &[
            "soe",
            "portfolio",
            arg(&dir),
            "--cited",
            arg(&cited),
            "--as-of",
            AS_OF,
            "--format",
            "json",
        ],
    );
    let first = tengu(home.path(), &args);
    assert!(first.status.success(), "{}", text(&first));
    let second = tengu(home.path(), &args);
    assert_eq!(first.stdout, second.stdout, "a rerun changes the bytes");

    let w: Value = serde_json::from_slice(&first.stdout).unwrap();
    assert_eq!(w["schema"], "soe.weekly_portfolio/1");
    assert_eq!(
        (w["id"].as_str(), w["week"].as_str()),
        (Some("2026-W39"), Some("2026-W39"))
    );
    for h in ["profile_sha256", "inputs_sha256"] {
        assert!(hex64(&w[h]), "{h}: {}", w[h]);
    }
    assert_eq!(w["economics_version"], 1);
    let ids = |list: &str| -> Vec<(String, String)> {
        w[list]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| {
                (
                    r["id"].as_str().unwrap().to_string(),
                    r["action"]["kind"].as_str().unwrap().to_string(),
                )
            })
            .collect()
    };
    let pair = |id: &str, kind: &str| (id.to_string(), kind.to_string());
    assert_eq!(
        ids("ranked"),
        [
            pair("final-rule-automation", "HOLD"),
            pair("reseller-at-cap", "HOLD"),
            pair("erp-integration", "HOLD"),
        ]
    );
    assert_eq!(ids("held"), [pair("draft-rule-automation", "HOLD")]);
    assert_eq!(ids("rejected"), [pair("reseller-over-cap", "REJECT")]);
    assert_eq!(w["allocation"]["cash"], "0.00");
    assert!(w["hold_rationale"]
        .as_str()
        .unwrap()
        .starts_with("not allocated"));

    // The text view names why each ranks above the next.
    let args: Vec<&str> = args
        .iter()
        .filter(|a| !["--format", "json"].contains(*a))
        .copied()
        .collect();
    let o = tengu(home.path(), &args);
    assert!(o.status.success(), "{}", text(&o));
    assert!(
        text(&o).contains(
            "why: `reseller-at-cap` before `erp-integration`: EVIDENCE_CONFIDENCE HIGH vs MEDIUM"
        ),
        "{}",
        text(&o)
    );
}

#[test]
fn check_json_has_three_scenarios_and_full_hashes() {
    let home = tempfile::tempdir().unwrap();
    let f = fixtures();
    let profile = f.join("profile.synthetic.toml");
    let opp = f.join("week/opportunities/erp-integration.toml");
    let cited = f.join("week/cited.toml");
    let o = tengu(
        home.path(),
        &synthetic(
            &profile,
            &[
                "soe",
                "check",
                arg(&opp),
                "--cited",
                arg(&cited),
                "--as-of",
                AS_OF,
                "--format",
                "json",
            ],
        ),
    );
    assert!(o.status.success(), "{}", text(&o));
    let j: Value = serde_json::from_slice(&o.stdout).unwrap();
    assert_eq!(j["id"], "erp-integration");
    assert_eq!(j["as_of"], AS_OF);
    assert_eq!(j["verdict"]["verdict"], "PASS");
    for s in ["downside", "base", "upside"] {
        assert!(
            j["scenarios"][s]["time_adjusted_contribution"].is_string(),
            "{s}: {}",
            j["scenarios"][s]
        );
    }
    for h in [
        &j["inputs_sha256"],
        &j["scenarios"]["inputs_sha256"],
        &j["verdict"]["inputs_sha256"],
        &j["opportunity_sha256"],
        &j["profile_sha256"],
    ] {
        assert!(hex64(h), "{h}");
    }
    assert_eq!(j["keys"]["EVIDENCE_CONFIDENCE"], "MEDIUM");

    // Without the cited views nothing supports it: it holds.
    let o = tengu(
        home.path(),
        &synthetic(&profile, &["soe", "check", arg(&opp), "--format", "json"]),
    );
    assert!(o.status.success(), "{}", text(&o));
    let j: Value = serde_json::from_slice(&o.stdout).unwrap();
    assert_eq!(j["verdict"]["verdict"], "HOLD");
    assert_eq!(j["next_information"][0], "signals");
}

#[test]
fn sensitivity_prints_a_tornado() {
    let home = tempfile::tempdir().unwrap();
    let f = fixtures();
    let profile = f.join("profile.synthetic.toml");
    let opp = f.join("week/opportunities/final-rule-automation.toml");
    let cited = f.join("week/cited.toml");
    let o = tengu(
        home.path(),
        &synthetic(
            &profile,
            &[
                "soe",
                "sensitivity",
                arg(&opp),
                "--cited",
                arg(&cited),
                "--scale-bps",
                "2500",
                "--format",
                "json",
            ],
        ),
    );
    assert!(o.status.success(), "{}", text(&o));
    let j: Value = serde_json::from_slice(&o.stdout).unwrap();
    let rows = j["rows"].as_array().unwrap();
    assert_eq!(rows.len(), 8);
    assert_eq!(
        (rows[0]["input"].as_str(), rows[0]["scale_bps"].as_i64()),
        (Some("OWNER_HOURS"), Some(-2500))
    );
    assert_eq!(j["as_of"], "2026-09-21", "default: the opportunity's as_of");
    // A scale beyond ±100 % is refused before anything runs.
    let o = tengu(
        home.path(),
        &synthetic(
            &profile,
            &["soe", "sensitivity", arg(&opp), "--scale-bps", "10001"],
        ),
    );
    assert_eq!(o.status.code(), Some(2), "{}", text(&o));
}

/// The example week is cut from three eval cases: its files must stay equal
/// to them.
#[test]
fn week_fixture_matches_its_cases() {
    let f = fixtures();
    let parse = |p: &Path| -> toml::Value {
        toml::from_str(&std::fs::read_to_string(p).unwrap())
            .unwrap_or_else(|e| panic!("{}: {e}", p.display()))
    };
    let mut ids = Vec::new();
    let mut cited = Vec::new();
    for case in WEEK_CASES {
        let c = parse(&f.join(format!("cases/{case}.toml")));
        assert_eq!(c["as_of"].as_str(), Some(AS_OF), "{case}");
        for cand in c["candidates"].as_array().unwrap() {
            let id = cand["id"].as_str().unwrap();
            let file = parse(&f.join(format!("week/opportunities/{id}.toml")));
            assert_eq!(
                &file, cand,
                "week/opportunities/{id}.toml differs from {case}"
            );
            ids.push(id.to_string());
        }
        cited.extend(c["cited"].as_array().unwrap().iter().cloned());
    }
    let week = parse(&f.join("week/cited.toml"));
    assert_eq!(week["cited"].as_array().unwrap(), &cited);
    ids.sort();
    let mut files: Vec<String> = std::fs::read_dir(f.join("week/opportunities"))
        .unwrap()
        .map(|e| {
            e.unwrap()
                .path()
                .file_stem()
                .unwrap()
                .to_string_lossy()
                .into_owned()
        })
        .collect();
    files.sort();
    assert_eq!(files, ids);
}
