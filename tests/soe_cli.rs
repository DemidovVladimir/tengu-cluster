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
//! | `portfolio` on the example week, twice | byte-identical canonical JSON; 3 ranked (two `CHEAP_TEST` fill the 10 h week, the third `HOLD` over budget — `allocate.rs`), 1 held, 1 rejected; full hashes |
//! | `check --format json` | three scenarios, the verdict, full 64-hex hashes |
//! | `sensitivity` | 8 tornado rows |
//! | the example week | its files equal the candidates and `[[cited]]` views of the three eval cases it comes from |
//! | `cycle --offline` then `--no-llm` (a temp `[soe]` config, `tengu -c`) | a valid empty `HOLD` week, frozen read-only, one forecast-log line each; a rerun `cycle_already_frozen`; an earlier week `cycle_out_of_order`; `verify` OK; `show` the memo |
//! | an edited frozen `forecast.json` | `verify` exit 1: the file `MISMATCH`, the cycle's log line `MISMATCH` |
//! | `grade` then `resolve`, `review` | one grade line, a stale version refused, an unfrozen cycle refused; the packet's six sections, the STOP line last, `reviews/<day>/` frozen and verified |
//! | `replay` on the synthetic set | the holdout hidden (not run) without `--holdout`; with it read #1 counted first; a run id once; `replays/` only |

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

    // The four money values are the operator's: left REQUIRED, the file is
    // no profile yet; once set (synthetic values here) it is an unsigned one.
    // Every deciding command refuses it either way.
    assert_eq!(written.matches("= \"REQUIRED\"").count(), 4, "{written}");
    let opp = fixtures().join("week/opportunities/erp-integration.toml");
    let cases = fixtures().join("cases");
    let deciding = [
        vec!["soe", "check", arg(&opp)],
        vec!["soe", "sensitivity", arg(&opp)],
        vec!["soe", "eval", arg(&cases)],
    ];
    for args in &deciding {
        let o = tengu(home.path(), args);
        assert_eq!(o.status.code(), Some(1), "{args:?}: {}", text(&o));
        assert!(
            text(&o).contains("invalid_profile:"),
            "{args:?}: {}",
            text(&o)
        );
    }
    let set = written.replace("= \"REQUIRED\"", "= \"1.00\"");
    std::fs::write(&path, &set).unwrap();
    for args in &deciding {
        let o = tengu(home.path(), args);
        assert_eq!(o.status.code(), Some(1), "{args:?}: {}", text(&o));
        assert!(
            text(&o).contains("operator_profile_unsigned: "),
            "{args:?}: {}",
            text(&o)
        );
    }
    std::fs::write(&path, &written).unwrap();

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
            pair("final-rule-automation", "CHEAP_TEST"),
            pair("reseller-at-cap", "CHEAP_TEST"),
            pair("erp-integration", "HOLD"),
        ]
    );
    assert_eq!(ids("held"), [pair("draft-rule-automation", "HOLD")]);
    assert_eq!(ids("rejected"), [pair("reseller-over-cap", "REJECT")]);
    // allocate.rs fills the synthetic 10 h week: two 5 h interviews; the
    // third test is over budget and holds.
    assert_eq!(w["allocation"]["owner_hours"], 10);
    assert_eq!(w["allocation"]["cash"], "0.00");
    assert!(w["hold_rationale"].is_null(), "{}", w["hold_rationale"]);

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
    assert!(
        text(&o).contains("erp-integration HOLD: rank 3: over budget"),
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

// ---------------------------------------------------------------------------
// The weekly cycle and its O4 measures (`cli/soe/weekly.rs`)
// ---------------------------------------------------------------------------

/// An SOE config passing every `[soe]` load rule; its state root is
/// `<TENGU_HOME>/state/soe-test/`. The stage agents are `openrouter` ones —
/// `--no-llm` and `--offline` never call them.
const SOE_CONFIG: &str = r#"
[egress]
network = "open"
allow_hosts = ["www.sec.gov", "data.sec.gov", "api.ted.europa.eu"]

[rate_limits.sec]
per_minute = 300
[rate_limits.ted]
per_minute = 60

[sources]
state = "soe-test"

[sources.registry.sec_edgar]
kind = "sec_edgar"
class = "company_primary"
trust = "primary"
revision = "immutable"
enabled = false
hosts = ["www.sec.gov", "data.sec.gov"]
auth = "user_agent_env:SEC_USER_AGENT"
rate_limit = "sec"
store_raw = true
jurisdiction = "US"
language = "en"

[sources.registry.ted_search]
kind = "ted_search"
class = "law_regulator"
trust = "primary"
revision = "immutable"
enabled = false
hosts = ["api.ted.europa.eu"]
auth = "none"
rate_limit = "ted"
store_raw = true
jurisdiction = "EU"
language = "en"
query = "publication-date >= {from} AND publication-date <= {to}"

[soe]
architect = "soe_architect"
critic = "soe_critic"
max_proposals = 12
forecast_max_weeks = 12

[agents.soe_architect]
engine = "openrouter"
model = "anthropic/claude-sonnet-4-6"
description = "Proposes mechanisms as data"
tools = ["soe_view", "soe_propose", "source_evidence"]

[agents.soe_critic]
engine = "openrouter"
model = "anthropic/claude-sonnet-4-6"
description = "Challenges the week's proposals"
tools = ["soe_view", "soe_challenge", "source_evidence"]

[default_scopes.http_request]
[default_scopes.write_file]
[default_scopes.run_command]
[default_scopes.sign_and_send_transaction]
[default_scopes.sign_message]
"#;

/// A temp `TENGU_HOME` with the SOE config in it (`None` when the temp dir
/// sits in a git work tree: the state root must not).
fn soe_home() -> Option<(tempfile::TempDir, PathBuf)> {
    let home = tempfile::tempdir().unwrap();
    let mut p = home.path();
    loop {
        if p.join(".git").exists() {
            return None;
        }
        match p.parent() {
            Some(up) => p = up,
            None => break,
        }
    }
    let cfg = home.path().join("soe.toml");
    std::fs::write(&cfg, SOE_CONFIG).unwrap();
    Some((home, cfg))
}

/// `tengu -c <cfg> soe <args> --profile <synthetic> --allow-synthetic`.
fn soe(home: &Path, cfg: &Path, args: &[&str]) -> Output {
    let profile = fixtures().join("profile.synthetic.toml");
    let mut v = vec!["-c", arg(cfg), "soe"];
    v.extend(args);
    v.extend(["--profile", arg(&profile), "--allow-synthetic"]);
    tengu(home, &v)
}

fn state(home: &Path) -> PathBuf {
    home.join("state/soe-test")
}

const AT_W41: &str = "2026-10-05T12:00:00Z";

/// The roadmap O3 exit on the binary: a cycle with the stage cache only
/// (`--offline`: the Architect's miss fails its stage, fail-soft) decides a
/// valid empty `HOLD` week, frozen read-only, one forecast-log line; it runs
/// once; `--no-llm` decides the next week; an earlier week or a decision
/// outside its week is refused; `verify` and `show` read the state.
#[test]
fn hold_cycle_offline_end_to_end() {
    let Some((home, cfg)) = soe_home() else {
        return;
    };
    let h = home.path();
    let w40 = [
        "cycle",
        "--week",
        "2026-W40",
        "--at",
        "2026-09-28T12:00:00Z",
    ];
    let o = soe(h, &cfg, &[&w40[..], &["--offline"]].concat());
    let out = text(&o);
    assert!(o.status.success(), "{out}");
    assert!(
        out.contains("cycles/2026-W40 frozen · HOLD week · 0 ranked, 0 held, 0 rejected"),
        "{out}"
    );
    assert!(out.contains("stage_cache_miss"), "{out}");
    assert!(out.contains("FAILED") && out.contains("SKIPPED"), "{out}");
    assert!(out.contains("generation UNBOUND · sha256 "), "{out}");
    let dir = state(h).join("cycles/2026-W40");
    for f in [
        "MANIFEST.json",
        "memo.md",
        "portfolio.json",
        "forecast.json",
        "stages.json",
        "ops.json",
    ] {
        assert!(dir.join(f).is_file(), "{f}");
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(dir.join("forecast.json"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o222, 0, "a frozen file is read-only");
    }
    let log = std::fs::read_to_string(state(h).join("forecast-log.jsonl")).unwrap();
    assert_eq!(log.lines().count(), 1);

    let again = soe(h, &cfg, &[&w40[..], &["--no-llm"]].concat());
    assert!(!again.status.success());
    assert!(
        text(&again).contains("cycle_already_frozen"),
        "{}",
        text(&again)
    );

    let next = soe(
        h,
        &cfg,
        &[
            "cycle", "--week", "2026-W41", "--at", AT_W41, "--no-llm", "--format", "json",
        ],
    );
    assert!(next.status.success(), "{}", text(&next));
    let j: Value = serde_json::from_slice(&next.stdout).unwrap();
    assert_eq!(j["run"], "cycles/2026-W41");
    assert_eq!(j["hold"], true);
    assert!(hex64(&j["manifest_sha256"]) && hex64(&j["inputs_sha256"]));
    assert_eq!(j["stages"][0]["outcome"], "SKIPPED");

    // An earlier week would break the chain; a decision outside its week is refused.
    let early = soe(
        h,
        &cfg,
        &[
            "cycle",
            "--week",
            "2026-W39",
            "--at",
            "2026-09-21T12:00:00Z",
            "--no-llm",
        ],
    );
    assert!(!early.status.success());
    assert!(
        text(&early).contains("cycle_out_of_order"),
        "{}",
        text(&early)
    );
    let bad = soe(
        h,
        &cfg,
        &["cycle", "--week", "2026-W30", "--at", AT_W41, "--no-llm"],
    );
    assert!(!bad.status.success() && text(&bad).contains("does not hold the decision"));

    let v = soe(h, &cfg, &["verify"]);
    assert!(v.status.success(), "{}", text(&v));
    for cycle in ["2026-W40", "2026-W41"] {
        assert!(
            text(&v).contains(&format!("log line of {cycle}: MATCH")),
            "{}",
            text(&v)
        );
    }
    let s = soe(h, &cfg, &["show", "2026-W40"]);
    assert!(s.status.success(), "{}", text(&s));
    assert!(
        text(&s).contains("cycles/2026-W40 · FROZEN"),
        "{}",
        text(&s)
    );
    assert!(text(&s).contains("grade: none yet"), "{}", text(&s));
    assert!(text(&s).contains("## Cycle"), "the memo: {}", text(&s));
    // Without a config, a cycle is refused.
    let none = tengu(h, &["soe", "cycle", "--no-llm"]);
    assert!(
        text(&none).contains("soe_config_missing"),
        "{}",
        text(&none)
    );
}

/// An edited frozen forecast is caught: the file no longer matches the
/// manifest and the cycle's chained log line no longer matches it.
#[test]
fn verify_detects_edited_forecast() {
    let Some((home, cfg)) = soe_home() else {
        return;
    };
    let h = home.path();
    let o = soe(
        h,
        &cfg,
        &["cycle", "--week", "2026-W41", "--at", AT_W41, "--no-llm"],
    );
    assert!(o.status.success(), "{}", text(&o));
    let dir = state(h).join("cycles/2026-W41");
    let file = dir.join("forecast.json");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o600)).unwrap();
    }
    let before = std::fs::read_to_string(&file).unwrap();
    let edited = before.replace(AT_W41, "2026-10-05T12:00:01Z");
    assert_ne!(before, edited);
    std::fs::write(&file, edited).unwrap();
    let v = soe(h, &cfg, &["verify"]);
    let out = text(&v);
    assert!(!v.status.success(), "{out}");
    assert!(out.contains("forecast.json MISMATCH"), "{out}");
    assert!(out.contains("log line of 2026-W41: MISMATCH"), "{out}");
    assert!(out.contains("NOT INTACT"), "{out}");
    let j = soe(h, &cfg, &["verify", "--format", "json"]);
    let j: Value = serde_json::from_slice(&j.stdout).unwrap();
    assert_eq!(j["ok"], false);
}

/// The operator grades a frozen cycle (append-only; a stale version is
/// refused), resolves it, and builds the Review #2 packet: six sections, the
/// STOP line last, frozen under `reviews/`.
#[test]
fn grade_then_review() {
    let Some((home, cfg)) = soe_home() else {
        return;
    };
    let h = home.path();
    let o = soe(
        h,
        &cfg,
        &["cycle", "--week", "2026-W41", "--at", AT_W41, "--no-llm"],
    );
    assert!(o.status.success(), "{}", text(&o));
    let grade = h.join("grade.toml");
    std::fs::write(
        &grade,
        r#"schema = "soe.cycle_grade/1"
id = "g-2026-W41"
version = 1
cycle_id = "2026-W41"
graded_by = "operator"
graded_at = "2026-10-06T09:00:00Z"
relevance = 2
evidence = 3
economics = 3
hidden_labor = 3
novelty_bias = 4
actionability = 2
correction_minutes = 15
research_hours = 1
changed_decision = false
misses = []
"#,
    )
    .unwrap();
    let g = soe(h, &cfg, &["grade", "2026-W41", "--file", arg(&grade)]);
    assert!(g.status.success(), "{}", text(&g));
    assert!(text(&g).contains("grades.jsonl line 1"), "{}", text(&g));
    let stale = soe(h, &cfg, &["grade", "2026-W41", "--file", arg(&grade)]);
    assert!(
        !stale.status.success() && text(&stale).contains("stale_grade"),
        "{}",
        text(&stale)
    );
    let other = soe(h, &cfg, &["grade", "2026-W42", "--file", arg(&grade)]);
    assert!(!other.status.success() && text(&other).contains("grades cycle `2026-W41`"));
    let unfrozen = std::fs::read_to_string(&grade)
        .unwrap()
        .replace("2026-W41", "2026-W43");
    std::fs::write(&grade, unfrozen).unwrap();
    let u = soe(h, &cfg, &["grade", "2026-W43", "--file", arg(&grade)]);
    assert!(
        !u.status.success() && text(&u).contains("cycle_not_frozen"),
        "{}",
        text(&u)
    );

    let r = soe(h, &cfg, &["resolve", "2026-W41"]);
    assert!(r.status.success(), "{}", text(&r));
    assert!(text(&r).contains("0 item(s)"), "{}", text(&r));

    let rv = soe(h, &cfg, &["review"]);
    let out = String::from_utf8_lossy(&rv.stdout).to_string();
    assert!(rv.status.success(), "{}", text(&rv));
    for heading in [
        "## 1. Weekly portfolios and grades",
        "## 2. Forecast vs later evidence",
        "## 3. Missed / false opportunities",
        "## 4. Cost and operator hours",
        "## 5. Source coverage and rights",
        "## 6. Best candidate lane",
        "## Stop flags (roadmap § 14)",
    ] {
        assert!(out.contains(heading), "{heading}:\n{out}");
    }
    let stop = "STOP — Operator Review #2 decides";
    let (md_out, trailer) = out
        .rsplit_once("\nreviews/")
        .expect("the review's trailer line");
    assert!(
        md_out.trim_end().lines().last().unwrap().starts_with(stop),
        "{out}"
    );
    assert!(trailer.contains(" frozen · manifest sha256 "), "{out}");
    let reviews: Vec<_> = std::fs::read_dir(state(h).join("reviews"))
        .unwrap()
        .flatten()
        .collect();
    assert_eq!(reviews.len(), 1);
    let md = std::fs::read_to_string(reviews[0].path().join("packet.md")).unwrap();
    assert!(
        md.trim_end().lines().last().unwrap().starts_with(stop),
        "{md}"
    );
    assert!(md.contains("| `2026-W41` | HOLD |"), "{md}");
    let j = soe(h, &cfg, &["review", "--format", "json"]);
    assert!(j.status.success(), "{}", text(&j));
    let j: Value = serde_json::from_slice(&j.stdout).unwrap();
    assert_eq!(j["packet"]["packet"]["graded"], 1);
    assert_eq!(j["packet"]["integrity"]["ok"], true);
    // The review dirs verify too.
    let v = soe(h, &cfg, &["verify"]);
    assert!(v.status.success(), "{}", text(&v));
    assert!(text(&v).contains("reviews/"), "{}", text(&v));
}

/// A replay set's holdout stays unread until a counted read; a run id runs
/// once; a synthetic set needs `--allow-synthetic`.
#[test]
fn replay_holdout_needs_a_counted_read() {
    let Some((home, cfg)) = soe_home() else {
        return;
    };
    let h = home.path();
    let set = fixtures().join("replay.synthetic.toml");
    let r1 = soe(
        h,
        &cfg,
        &["replay", "--set", arg(&set), "--run-id", "r1", "--no-llm"],
    );
    let out = text(&r1);
    assert!(r1.status.success(), "{out}");
    assert!(out.contains("1 case(s) hidden"), "{out}");
    assert!(
        !out.contains("late-holdout") && !out.contains("the buyer never paid"),
        "{out}"
    );
    assert!(!state(h).join("holdout-reads.jsonl").exists());
    assert!(
        !state(h).join("cycles").exists(),
        "a replay never writes cycles/"
    );
    let r2 = soe(
        h,
        &cfg,
        &[
            "replay",
            "--set",
            arg(&set),
            "--run-id",
            "r2",
            "--no-llm",
            "--holdout",
        ],
    );
    let out = text(&r2);
    assert!(r2.status.success(), "{out}");
    assert!(out.contains("holdout read #1 of this set"), "{out}");
    assert!(out.contains("the buyer never paid"), "{out}");
    let reads = std::fs::read_to_string(state(h).join("holdout-reads.jsonl")).unwrap();
    assert_eq!(reads.lines().count(), 1);
    let again = soe(
        h,
        &cfg,
        &["replay", "--set", arg(&set), "--run-id", "r1", "--no-llm"],
    );
    assert!(!again.status.success() && text(&again).contains("replay_run_exists"));
    let strict = tengu(
        h,
        &[
            "-c",
            arg(&cfg),
            "soe",
            "replay",
            "--set",
            arg(&set),
            "--no-llm",
        ],
    );
    assert!(!strict.status.success());
}
