//! `tengu soe …` — the Software Opportunity Engine, offline
//! (`docs/soe-2026-10-08.md`): no sandbox config, secrets, egress, LLM or
//! source fetch. It reads the private operator profile, opportunity files, a
//! cited-records file and eval cases; only `init` writes (one new file).
//! stdout carries the report; logs go to stderr; ids and hashes print in full.
//!
//! | Command | Prints | Exit 1 when |
//! |---|---|---|
//! | `init` | writes the UNSIGNED profile template — the PRD § 14 proposed values, no capability — at `--profile`, mode 0600 (new dirs 0700); its path + sha256 | the file exists (never overwritten); the path sits inside a git work tree (`profile_in_repo`) |
//! | `check <opportunity.toml> [--cited F] [--as-of T]` | the verdict with every failed gate, the three scenarios, the rank keys, capability fit, the next information, the hashes | the profile or the opportunity is refused; the gates refuse (`future_leakage`, an unknown time) |
//! | `portfolio <dir> --as-of T [--week YYYY-Www] [--cited F]` | the week (`rank::unallocated_week`): ranked — each `HOLD`, nothing allocated before O3 — with why each ranks above the next, held, rejected, the next information; json = its canonical JSON | as `check`, for any `<id>.toml` in `<dir>` |
//! | `sensitivity <opportunity.toml> [--scale-bps 2000] [--cited F] [--as-of T]` | the tornado: each input at ± the scale — fields scaled, the base time-adjusted contribution, the verdict, gates added / removed | as `check` |
//! | `eval <cases dir>` | expected vs answered per case (`eval::run_case`): ok / FAIL with every diff | a case fails (a diff, or the case is refused — another profile id) |
//!
//! | Flag (every command) | Value |
//! |---|---|
//! | `--profile <path>` | the operator profile; default `<TENGU_HOME>/state/soe/operator.toml` |
//! | `--allow-synthetic` | accept a `synthetic = true` test profile (`tests/fixtures/soe/profile.synthetic.toml`) |
//! | `--format text\|json` | text (default) or JSON |
//!
//! Every command but `init` loads the profile with
//! `config::soe::load_profile`: a missing, invalid, in-repo, loose-mode or
//! unsigned profile is refused — an `init` template stays refused until the
//! operator signs it. `--as-of`: RFC 3339 UTC or a day; default the
//! opportunity's own `as_of` (`check`, `sensitivity`). `--week`: default the
//! ISO week of `--as-of` (UTC). `--cited`: a TOML file of `[[cited]]`
//! `CitedRecord` views (`config::soe::load_cited`) until the O2 as-of view
//! builds them; without it no record supports anything (the evidence gates
//! hold).

use std::io::Write as _;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Context, Result};
use clap::{Args, Subcommand, ValueEnum};
use serde::Serialize;
use serde_json::{json, Value};

use crate::adapters::outbound::backfill::text_table;
use crate::config::soe::{
    default_profile_path, git_work_tree, load_cited, load_opportunity, load_profile,
    load_record_dir, Loaded, SOE_STATE,
};
use crate::domain::canonical::{canonical_json, sha256_hex};
use crate::domain::lineage::pins::toml_digest;
use crate::domain::lineage::value::Time;
use crate::domain::soe::eval::{run_case, Answer, CaseClass, EvalCase};
use crate::domain::soe::gates::{next_information, CitedRecord};
use crate::domain::soe::opportunity::Opportunity;
use crate::domain::soe::portfolio::IsoWeek;
use crate::domain::soe::profile::{OperatorProfile, RankKey, UNSIGNED};
use crate::domain::soe::rank::{
    assess, current_versions, explain_order, rank, sensitivity, tornado, unallocated_week,
    Assessment, WeekHead,
};
use crate::domain::soe::record::from_toml;
use crate::domain::soe::value::Minor;

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub(super) enum Format {
    Text,
    Json,
}

#[derive(Args)]
pub(crate) struct SoeArgs {
    /// The operator profile (default <TENGU_HOME>/state/soe/operator.toml).
    #[arg(long, global = true)]
    profile: Option<PathBuf>,
    /// Accept a `synthetic = true` test profile (fixtures only).
    #[arg(long, global = true)]
    allow_synthetic: bool,
    /// `text` (default) or `json`.
    #[arg(long, global = true, value_enum, default_value_t = Format::Text)]
    format: Format,
    #[command(subcommand)]
    action: SoeAction,
}

#[derive(Subcommand)]
pub(super) enum SoeAction {
    /// Write the UNSIGNED profile template (the PRD § 14 proposed values) —
    /// never over an existing file, never inside a git work tree.
    Init,
    /// One opportunity: three scenarios, the hard gates, rank keys, next information.
    Check {
        /// A `soe.opportunity/1` file.
        opportunity: PathBuf,
        /// A TOML file of `[[cited]]` record views.
        #[arg(long)]
        cited: Option<PathBuf>,
        /// The decision time (default: the opportunity's `as_of`).
        #[arg(long)]
        as_of: Option<String>,
    },
    /// A week: every `<id>.toml` opportunity in a dir, gated and ranked.
    Portfolio {
        /// A dir of `soe.opportunity/1` files (file stem = id).
        dir: PathBuf,
        #[arg(long)]
        as_of: String,
        /// `YYYY-Www` (default: the ISO week of --as-of).
        #[arg(long)]
        week: Option<String>,
        #[arg(long)]
        cited: Option<PathBuf>,
    },
    /// The tornado: each input at ± --scale-bps.
    Sensitivity {
        opportunity: PathBuf,
        #[arg(long, default_value_t = 2000, value_parser = clap::value_parser!(i32).range(1..=10_000))]
        scale_bps: i32,
        #[arg(long)]
        cited: Option<PathBuf>,
        #[arg(long)]
        as_of: Option<String>,
    },
    /// The eval set: expected vs answered per case.
    Eval {
        /// A dir of `soe.eval_case/1` files.
        cases: PathBuf,
    },
}

/// `tengu soe` (module table).
pub(super) fn run_soe(args: SoeArgs) -> Result<()> {
    let path = args.profile.unwrap_or_else(default_profile_path);
    let format = args.format;
    let profile = || load_profile(&path, args.allow_synthetic).map_err(|e| anyhow!(e));
    match args.action {
        SoeAction::Init => init(&path, &today(), format),
        SoeAction::Check {
            opportunity,
            cited,
            as_of,
        } => {
            let p = profile()?;
            let views = load_views(cited.as_deref())?;
            let opp = load_opportunity(&opportunity).map_err(|e| anyhow!(e))?;
            let at = decision_time(as_of.as_deref(), opp.record.as_of)?;
            check(&p, &opp, &views, at, format)
        }
        SoeAction::Portfolio {
            dir,
            as_of,
            week,
            cited,
        } => {
            let p = profile()?;
            let views = load_views(cited.as_deref())?;
            let at = decision_time(Some(&as_of), Time::Unknown)?;
            let week = match week {
                Some(w) => w.parse::<IsoWeek>().map_err(|e| anyhow!("--week: {e}"))?,
                None => IsoWeek::of(&at).ok_or_else(|| anyhow!("--as-of {at}: no ISO week"))?,
            };
            portfolio(&p, &dir, &views, at, week, format)
        }
        SoeAction::Sensitivity {
            opportunity,
            scale_bps,
            cited,
            as_of,
        } => {
            let p = profile()?;
            let views = load_views(cited.as_deref())?;
            let opp = load_opportunity(&opportunity).map_err(|e| anyhow!(e))?;
            let at = decision_time(as_of.as_deref(), opp.record.as_of)?;
            sensitivity_report(&p, &opp, &views, at, scale_bps, format)
        }
        SoeAction::Eval { cases } => eval(&profile()?, &cases, format),
    }
}

fn print_json<T: Serialize>(v: &T) -> Result<()> {
    println!("{}", serde_json::to_string_pretty(v)?);
    Ok(())
}

/// A serialized value on one line: a string as itself, a table as
/// `key value · …`, anything else as its JSON.
fn plain<T: Serialize>(v: &T) -> String {
    fn text(v: &Value) -> String {
        match v {
            Value::String(s) => s.clone(),
            Value::Object(o) => o
                .iter()
                .map(|(k, v)| format!("{k} {}", text(v)))
                .collect::<Vec<_>>()
                .join(" · "),
            other => other.to_string(),
        }
    }
    serde_json::to_value(v).map_or_else(|e| format!("<{e}>"), |v| text(&v))
}

fn list_or(items: &[String], none: &str) -> String {
    if items.is_empty() {
        none.to_string()
    } else {
        items.join(", ")
    }
}

fn load_views(path: Option<&Path>) -> Result<Vec<CitedRecord>> {
    match path {
        Some(p) => load_cited(p).map_err(|e| anyhow!(e)),
        None => Ok(Vec::new()),
    }
}

/// `--as-of`, else `default`; known.
fn decision_time(arg: Option<&str>, default: Time) -> Result<Time> {
    let t = match arg {
        Some(s) => s.parse::<Time>().map_err(|e| anyhow!("--as-of: {e}"))?,
        None => default,
    };
    if !t.is_known() {
        bail!("--as-of: give a known decision time (RFC 3339 UTC or a YYYY-MM-DD day)");
    }
    Ok(t)
}

fn profile_line(p: &Loaded<OperatorProfile>) -> String {
    format!(
        "profile {} version {} · sha256 {} · {}{}",
        p.record.id,
        p.record.version,
        p.sha256,
        p.path.display(),
        if p.record.synthetic {
            " · SYNTHETIC (test fixture)"
        } else {
            ""
        }
    )
}

// ---------------------------------------------------------------------------
// init
// ---------------------------------------------------------------------------

/// The `init` template: the PRD § 14 proposed values, UNSIGNED. Never read as
/// a default — every deciding command refuses it until the operator signs it.
const PROFILE_TEMPLATE: &str = r#"# The SOE operator profile — PRIVATE (docs/soe-2026-10-08.md § 2).
# Written by `tengu soe init` as an UNSIGNED template. The values below are
# the PRD § 14 proposals, not decisions. Review every one, add your
# capabilities, then sign: set `signed_by` to your name and `signed_at` to
# the time you signed (RFC 3339 UTC, e.g. 2026-10-12T09:00:00Z). Every
# `tengu soe` command that decides refuses this file until then.
# Keep it here, outside any git work tree, mode 0600. There is no salary
# field: existing income enters only through `shadow_hourly_rate`.
schema = "soe.operator_profile/1"
id = "operator"
version = 1
valid_from = "{valid_from}"
signed_by = "{unsigned}"
signed_at = "UNKNOWN"
synthetic = false
# The reporting currency: every money value below is in it.
currency = "EUR"
# PRE_TAX (business cash contribution before tax) or POST_TAX.
profit_basis = "PRE_TAX"
# What min_monthly_contribution compares: TIME_ADJUSTED (cash contribution
# minus owner hours x shadow_hourly_rate) or CASH.
contribution_basis = "TIME_ADJUSTED"
# Max gross cash exposure per opportunity.
max_cash_exposure = "30000.00"
# Min net contribution per active month after the ramp.
min_monthly_contribution = "5000.00"
# What one owner hour costs.
shadow_hourly_rate = "100.00"
# Max base-case payback.
max_payback_months = 12
# Owner hours a week for research and validation.
weekly_owner_hours = 8
# Max validation tranche before reapproval.
max_validation_tranche = "1000.00"
# Max one-off delivery length.
max_one_off_delivery_weeks = 6
# Germany / EU by default; anything beyond is an explicit entry.
jurisdictions_allow = ["DE", "EU"]
# PRD § 14 names no channel, language or exclusion: list yours (an empty
# list allows none).
channels_allow = []
languages = []
exclusions = []
# NONE, WEEKLY (a weekly brief), MONTHLY or ON_EVIDENCE.
public_cadence = "WEEKLY"
# The order of the eight PRD § 7.2 criteria (ties: id ascending).
rank_order = [
  "EVIDENCE_CONFIDENCE",
  "TIME_ADJUSTED_BASE",
  "PAYBACK_BASE",
  "DAYS_TO_DECISIVE_EVIDENCE",
  "REVERSIBILITY",
  "CAPABILITY_FIT",
  "CONCENTRATION_MAX",
  "DEFENSIBILITY",
]
# What you can deliver — only you can state it. With none, every required
# skill is MISSING. Replace `capabilities = []` with one block per capability:
#
# [[capabilities]]
# id = "my-capability"
# skill = "integration"
# level = "CLAIMED"          # PROVEN needs a proof locator, e.g. "url:https://…"
# proof = []
# capacity_hours_per_week = "UNKNOWN"   # or { low = …, base = …, high = … }
# delivery_cost_per_hour = "UNKNOWN"
# dependencies = []
# as_of = "{valid_from}"
# valid_until = "UNKNOWN"
capabilities = []
"#;

/// The template for `day` (`YYYY-MM-DD`).
fn profile_template(day: &str) -> String {
    PROFILE_TEMPLATE
        .replace("{valid_from}", day)
        .replace("{unsigned}", UNSIGNED)
}

/// Today, UTC (`YYYY-MM-DD`).
fn today() -> String {
    chrono::Utc::now()
        .date_naive()
        .format("%Y-%m-%d")
        .to_string()
}

/// `text` into a new file at `path`: mode 0600, new parent dirs 0700; an
/// existing file is never touched.
fn create_private(path: &Path, text: &str) -> Result<()> {
    if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
        let mut b = std::fs::DirBuilder::new();
        b.recursive(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            b.mode(0o700);
        }
        b.create(dir)
            .with_context(|| format!("create {}", dir.display()))?;
    }
    let mut o = std::fs::OpenOptions::new();
    o.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        o.mode(0o600);
    }
    let mut f = match o.open(path) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => bail!(
            "profile_exists: {}: never overwritten — edit it, or move it away first",
            path.display()
        ),
        Err(e) => return Err(e).with_context(|| format!("create {}", path.display())),
    };
    f.write_all(text.as_bytes())?;
    f.sync_all()?;
    Ok(())
}

fn init(path: &Path, day: &str, format: Format) -> Result<()> {
    if let Some(tree) = git_work_tree(path) {
        bail!(
            "profile_in_repo: {}: inside the git work tree {} — the operator profile is \
             private; keep it under <TENGU_HOME>/state/{SOE_STATE}/",
            path.display(),
            tree.display()
        );
    }
    let text = profile_template(day);
    // The template is a valid profile, unsigned.
    let p = from_toml::<OperatorProfile>(&text).map_err(|errs| {
        anyhow!(
            "the profile template is invalid:\n{}",
            errs.iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join("\n")
        )
    })?;
    debug_assert!(!p.is_signed() && !p.synthetic);
    create_private(path, &text)?;
    let sha256 = toml_digest(&text).map_err(|e| anyhow!(e))?;
    if format == Format::Json {
        return print_json(&json!({
            "path": path.display().to_string(),
            "sha256": sha256,
            "signed": false,
        }));
    }
    println!(
        "wrote the UNSIGNED operator profile template {} (mode 0600) · sha256 {sha256}",
        path.display()
    );
    println!(
        "Its values are the PRD § 14 proposals, not decisions: review each, add your \
         [[capabilities]], then sign (signed_by, signed_at). Until then every deciding \
         `tengu soe` command refuses it (operator_profile_unsigned)."
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// check
// ---------------------------------------------------------------------------

/// The `ScenarioMetrics` figures, in print order.
const FIGURES: [&str; 9] = [
    "monthly_revenue_collected",
    "monthly_cash_contribution",
    "owner_hours_per_month",
    "owner_time_cost",
    "time_adjusted_contribution",
    "initial_capital",
    "payback",
    "payback_time_adjusted",
    "occupied_months",
];

/// The profile's rank keys first, in its order, then the rest.
fn key_order(order: &[RankKey]) -> Vec<RankKey> {
    let mut keys = order.to_vec();
    keys.extend(RankKey::ALL.into_iter().filter(|k| !order.contains(k)));
    keys
}

fn check(
    p: &Loaded<OperatorProfile>,
    opp: &Loaded<Opportunity>,
    views: &[CitedRecord],
    at: Time,
    format: Format,
) -> Result<()> {
    let a = assess(&opp.record, views, &p.record, at).map_err(|e| anyhow!("{e}"))?;
    let next = next_information(&a.verdict);
    if format == Format::Json {
        let mut j = serde_json::to_value(&a)?;
        if let Value::Object(o) = &mut j {
            o.insert("as_of".into(), json!(at.to_string()));
            o.insert("opportunity_sha256".into(), json!(opp.sha256));
            o.insert("profile_id".into(), json!(p.record.id));
            o.insert("profile_sha256".into(), json!(p.sha256));
            o.insert("next_information".into(), json!(next));
        }
        return print_json(&j);
    }
    let v = &a.verdict;
    let sc = &a.scenarios;
    println!(
        "opportunity {} version {} · decided at {at} · file sha256 {}",
        a.id, a.version, opp.sha256
    );
    println!("{}", profile_line(p));
    println!(
        "verdict {} · economics v{} · economics inputs_sha256 {} · assessment inputs_sha256 {}",
        plain(&v.verdict),
        v.economics_version,
        v.inputs_sha256,
        a.inputs_sha256
    );
    if v.failures.is_empty() {
        println!("gates: none failed (PASS is still no permission to act)");
    } else {
        println!("gates:");
        let rows: Vec<Vec<String>> = v
            .failures
            .iter()
            .map(|f| {
                vec![
                    plain(&f.outcome),
                    f.label(),
                    f.field.clone().unwrap_or_default(),
                    f.detail.clone(),
                ]
            })
            .collect();
        print!(
            "{}",
            text_table(&["outcome", "gate", "field", "detail"], &rows)
        );
    }
    println!(
        "scenarios ({}; native {}; {}; run-rate month {}):",
        sc.currency,
        sc.native_currency,
        plain(&sc.basis),
        sc.run_rate_month
    );
    let cols = [&sc.downside, &sc.base, &sc.upside]
        .into_iter()
        .map(serde_json::to_value)
        .collect::<Result<Vec<Value>, _>>()?;
    let rows: Vec<Vec<String>> = FIGURES
        .iter()
        .filter(|f| cols.iter().any(|c| !c[**f].is_null()))
        .map(|f| {
            let label = if f.starts_with("payback") {
                format!("{f} (months)")
            } else {
                f.to_string()
            };
            std::iter::once(label)
                .chain(cols.iter().map(|c| plain(&c[*f])))
                .collect()
        })
        .collect();
    print!(
        "{}",
        text_table(&["figure", "DOWNSIDE", "BASE", "UPSIDE"], &rows)
    );
    println!(
        "expected loss: {}",
        match sc.expected_loss.known() {
            Some(r) => format!("{} to {} {}", r.low, r.high, sc.currency),
            None => plain(&sc.expected_loss),
        }
    );
    println!("rank keys:");
    let rows: Vec<Vec<String>> = key_order(&p.record.rank_order)
        .into_iter()
        .map(|k| vec![plain(&k), a.value(k).to_string()])
        .collect();
    print!("{}", text_table(&["key", "value"], &rows));
    let skills: Vec<String> = a
        .fit
        .skills
        .iter()
        .map(|s| match &s.capability {
            Some(c) => format!("{} {} ({c})", s.skill, plain(&s.level)),
            None => format!("{} {}", s.skill, plain(&s.level)),
        })
        .collect();
    println!(
        "capability fit: {}{}",
        plain(&a.fit.level),
        if skills.is_empty() {
            String::new()
        } else {
            format!(" — {}", skills.join(", "))
        }
    );
    println!("next information: {}", list_or(&next, "none"));
    Ok(())
}

// ---------------------------------------------------------------------------
// portfolio
// ---------------------------------------------------------------------------

fn portfolio(
    p: &Loaded<OperatorProfile>,
    dir: &Path,
    views: &[CitedRecord],
    at: Time,
    week: IsoWeek,
    format: Format,
) -> Result<()> {
    let loaded = load_record_dir::<Opportunity>(dir).map_err(|errs| {
        anyhow!(
            "{} does not load ({} problem(s)):\n{}",
            dir.display(),
            errs.len(),
            errs.iter()
                .map(|e| format!("- {e}"))
                .collect::<Vec<_>>()
                .join("\n")
        )
    })?;
    let opps: Vec<Opportunity> = loaded.into_iter().map(|l| l.record).collect();
    let current = current_versions(&opps, &at).map_err(|e| anyhow!("{e}"))?;
    for o in opps
        .iter()
        .filter(|o| !current.iter().any(|c| c.id == o.id))
    {
        eprintln!(
            "skipped `{}` version {}: dated {} — after the decision {at}",
            o.id, o.version, o.as_of
        );
    }
    let all = current
        .iter()
        .map(|o| assess(o, views, &p.record, at))
        .collect::<Result<Vec<Assessment>, _>>()
        .map_err(|e| anyhow!("{e}"))?;
    let order = &p.record.rank_order;
    let head = WeekHead {
        id: week.to_string(),
        week,
        as_of: at,
        currency: p.record.currency,
        profile_sha256: p.sha256.clone(),
    };
    let w = unallocated_week(head, &all, order).map_err(|errs| {
        anyhow!(
            "the week is invalid:\n{}",
            errs.iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join("\n")
        )
    })?;
    let canonical = canonical_json(&serde_json::to_value(&w)?);
    if format == Format::Json {
        println!("{canonical}");
        return Ok(());
    }
    println!(
        "week {} · decided at {} · {} · economics v{} · week sha256 {}",
        w.week,
        w.as_of,
        w.currency,
        w.economics_version,
        sha256_hex(&canonical)
    );
    println!("{}", profile_line(p));
    println!("inputs_sha256 {}", w.inputs_sha256);
    println!(
        "ranked ({}): nothing is allocated before O3 — a ranked candidate holds",
        w.ranked.len()
    );
    if !w.ranked.is_empty() {
        let mut header = vec!["rank", "id", "version", "action"];
        let names: Vec<String> = order.iter().map(plain).collect();
        header.extend(names.iter().map(String::as_str));
        let rows: Vec<Vec<String>> = w
            .ranked
            .iter()
            .map(|r| {
                [
                    r.rank.to_string(),
                    r.id.clone(),
                    r.opportunity_version.to_string(),
                    r.action.kind().to_string(),
                ]
                .into_iter()
                .chain(r.keys.iter().map(|k| k.value.clone()))
                .collect()
            })
            .collect();
        print!("{}", text_table(&header, &rows));
        let ranked = rank(&all, order);
        for pair in ranked.windows(2) {
            println!("  why: {}", explain_order(pair[0], pair[1], order));
        }
    }
    for (name, rows) in [("held", &w.held), ("rejected", &w.rejected)] {
        println!("{name} ({}):", rows.len());
        if !rows.is_empty() {
            let rows: Vec<Vec<String>> = rows
                .iter()
                .map(|r| {
                    vec![
                        r.id.clone(),
                        r.opportunity_version.to_string(),
                        r.action.kind().to_string(),
                        r.gates.join(", "),
                    ]
                })
                .collect();
            print!(
                "{}",
                text_table(&["id", "version", "action", "gates"], &rows)
            );
        }
    }
    println!("next information: {}", list_or(&w.next_information, "none"));
    if let Some(r) = &w.hold_rationale {
        println!("rationale: {r}");
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// sensitivity
// ---------------------------------------------------------------------------

fn sensitivity_report(
    p: &Loaded<OperatorProfile>,
    opp: &Loaded<Opportunity>,
    views: &[CitedRecord],
    at: Time,
    scale_bps: i32,
    format: Format,
) -> Result<()> {
    let o = &opp.record;
    let rows =
        sensitivity(o, views, &p.record, at, &tornado(scale_bps)).map_err(|e| anyhow!("{e}"))?;
    if format == Format::Json {
        return print_json(&json!({
            "id": o.id,
            "version": o.version,
            "as_of": at.to_string(),
            "opportunity_sha256": opp.sha256,
            "profile_id": p.record.id,
            "profile_sha256": p.sha256,
            "scale_bps": scale_bps,
            "rows": rows,
        }));
    }
    println!(
        "sensitivity of {} version {} · decided at {at} · ± {scale_bps} bps · file sha256 {}",
        o.id, o.version, opp.sha256
    );
    println!("{}", profile_line(p));
    if let Some(first) = rows.first() {
        println!(
            "base: time-adjusted contribution {} · verdict {}",
            plain(&first.base_time_adjusted),
            plain(&first.verdict_before)
        );
    }
    let table: Vec<Vec<String>> = rows
        .iter()
        .map(|r| {
            let change = match (
                r.base_time_adjusted.known(),
                r.perturbed_time_adjusted.known(),
            ) {
                (Some(b), Some(x)) => {
                    let d = Minor(x.0.saturating_sub(b.0));
                    format!("{x} ({}{d})", if d.0 >= 0 { "+" } else { "" })
                }
                _ => plain(&r.perturbed_time_adjusted),
            };
            vec![
                plain(&r.input),
                format!("{:+}", r.scale_bps),
                if r.fields.is_empty() {
                    "— (not in this revenue model)".to_string()
                } else {
                    r.fields.join(", ")
                },
                change,
                plain(&r.verdict_after),
                list_or(&r.gates_added, "—"),
                list_or(&r.gates_removed, "—"),
            ]
        })
        .collect();
    print!(
        "{}",
        text_table(
            &[
                "input",
                "scale_bps",
                "fields",
                "time-adjusted (change)",
                "verdict",
                "gates added",
                "gates removed",
            ],
            &table
        )
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// eval
// ---------------------------------------------------------------------------

/// One case's line of the eval report.
#[derive(Serialize)]
struct EvalRow {
    id: String,
    class: CaseClass,
    ok: bool,
    diffs: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    answer: Option<Answer>,
}

fn eval(p: &Loaded<OperatorProfile>, dir: &Path, format: Format) -> Result<()> {
    let cases = load_record_dir::<EvalCase>(dir).map_err(|errs| {
        anyhow!(
            "{} does not load ({} problem(s)):\n{}",
            dir.display(),
            errs.len(),
            errs.iter()
                .map(|e| format!("- {e}"))
                .collect::<Vec<_>>()
                .join("\n")
        )
    })?;
    let rows: Vec<EvalRow> = cases
        .iter()
        .map(|c| match run_case(&c.record, &p.record) {
            Ok(r) => EvalRow {
                id: r.id,
                class: r.class,
                ok: r.ok,
                diffs: r.diffs,
                answer: Some(r.answer),
            },
            Err(e) => EvalRow {
                id: c.record.id.clone(),
                class: c.record.class,
                ok: false,
                diffs: vec![format!("refused: {e}")],
                answer: None,
            },
        })
        .collect();
    let failed = rows.iter().filter(|r| !r.ok).count();
    if format == Format::Json {
        print_json(&json!({
            "profile_id": p.record.id,
            "profile_sha256": p.sha256,
            "cases": rows,
            "ok": rows.len() - failed,
            "failed": failed,
        }))?;
    } else {
        println!("{}", profile_line(p));
        let table: Vec<Vec<String>> = rows
            .iter()
            .map(|r| {
                vec![
                    r.id.clone(),
                    plain(&r.class),
                    if r.ok { "ok" } else { "FAIL" }.to_string(),
                    r.diffs.len().to_string(),
                ]
            })
            .collect();
        print!(
            "{}",
            text_table(&["case", "class", "result", "diffs"], &table)
        );
        for r in rows.iter().filter(|r| !r.ok) {
            for d in &r.diffs {
                println!("  {}: {d}", r.id);
            }
        }
        let missing: Vec<String> = CaseClass::ALL
            .iter()
            .filter(|k| !rows.iter().any(|r| r.class == **k))
            .map(plain)
            .collect();
        println!(
            "{} case(s): {} ok, {failed} FAIL · classes without a case: {}",
            rows.len(),
            rows.len() - failed,
            list_or(&missing, "none")
        );
    }
    if failed > 0 {
        bail!("soe eval: {failed} case(s) FAIL");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn init_template_is_a_valid_unsigned_profile() {
        let text = profile_template("2026-10-12");
        let p = from_toml::<OperatorProfile>(&text).unwrap_or_else(|e| panic!("{e:?}"));
        assert!(!p.is_signed() && !p.synthetic);
        assert_eq!(p.signed_by, UNSIGNED);
        assert_eq!(p.valid_from.to_string(), "2026-10-12");
        assert_eq!(p.rank_order, RankKey::ALL);
        assert!(p.capabilities.is_empty());
        assert!(!text.contains("{valid_from}") && !text.contains("{unsigned}"));
        assert!(!text.to_lowercase().contains("salary ="));
    }

    #[test]
    fn init_writes_once_with_private_mode() {
        let tmp = tempfile::TempDir::new().unwrap();
        if git_work_tree(tmp.path()).is_some() {
            return; // the temp dir itself sits in a repo here
        }
        let path = tmp.path().join("state/soe/operator.toml");
        init(&path, "2026-10-12", Format::Text).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert_eq!(text, profile_template("2026-10-12"));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode(&path), 0o600);
            assert_eq!(mode(path.parent().unwrap()), 0o700);
        }
        std::fs::write(&path, "edited").unwrap();
        let e = init(&path, "2026-10-13", Format::Text).unwrap_err();
        assert!(e.to_string().starts_with("profile_exists: "), "{e}");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "edited");
        // The loader refuses the template: unsigned.
        let fresh = tmp.path().join("other/operator.toml");
        init(&fresh, "2026-10-12", Format::Json).unwrap();
        let e = load_profile(&fresh, false).unwrap_err();
        assert!(e.starts_with("operator_profile_unsigned: "), "{e}");
    }

    #[test]
    fn init_refuses_a_path_inside_a_git_work_tree() {
        let tmp = tempfile::TempDir::new().unwrap();
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(repo.join(".git")).unwrap();
        let path = repo.join("private/operator.toml");
        let e = init(&path, "2026-10-12", Format::Text).unwrap_err();
        assert!(e.to_string().starts_with("profile_in_repo: "), "{e}");
        assert!(!path.exists() && !path.parent().unwrap().exists());
    }

    #[test]
    fn decision_time_is_known() {
        assert_eq!(
            decision_time(Some("2026-09-21T18:00:00Z"), Time::Unknown)
                .unwrap()
                .to_string(),
            "2026-09-21T18:00:00Z"
        );
        let day: Time = "2026-09-21".parse().unwrap();
        assert_eq!(decision_time(None, day).unwrap(), day);
        assert!(decision_time(None, Time::Unknown).is_err());
        assert!(decision_time(Some("UNKNOWN"), day).is_err());
        assert!(decision_time(Some("2026-09-21T18:00:00+02:00"), day).is_err());
    }
}
