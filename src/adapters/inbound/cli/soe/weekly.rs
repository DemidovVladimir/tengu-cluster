//! `tengu soe cycle|replay|grade|resolve|review|verify|show` — the weekly
//! cycle (O3) and its O4 measures over the private SOE state
//! `<TENGU_HOME>/state/<sources.state>/` (`docs/soe-2026-10-08.md` §§ 10–13).
//! Every command writes the SOE state only: no contact, spend, publish or
//! deploy exists (O5–O8 wait for Operator Review #2).
//!
//! | Command | Does | Exit 1 when |
//! |---|---|---|
//! | `cycle [--week] [--at] [--offline \| --no-llm]` | one live cycle → `cycles/<week>/`, frozen, then the state logs — under the sandbox's leases (`runtime:<sandbox>`, `state:<SOE state root>`, as `tengu run` holds them); prints the week, the stages, the hashes | a lease held (a `tengu run` of the sandbox, another cycle); refused (`cycle_already_frozen`, `cycle_unfinished`, `cycle_out_of_order`, an unsigned profile, `--at` after now or outside `--week`) or failed |
//! | `replay --set <file> [--run-id] [--holdout] [--offline \| --no-llm] [--scale-bps]` | a `soe.replay_set/1` under `replays/` (never `cycles/`); holdout cases only with `--holdout` — a counted read; prints `report.md` | refused (`profile_mismatch`, `holdout_empty`, `replay_run_exists`, the set's load rules) |
//! | `grade <cycle> --file <toml>` | the operator's `soe.cycle_grade/1` → `grades.jsonl` | the file names another cycle; refused (`cycle_not_frozen`, `stale_grade`, the record's codes) |
//! | `resolve <cycle> [--file <toml>]` | forecast items → `resolutions.jsonl`: `EVIDENCE_APPEARS` from the source store (with a config), the rest from the answers file (`cycle_id`, `[[resolutions]]` `item`, `hit`, `observed_at`, `evidence`, `resolved_by`) | refused (`cycle_not_frozen`, `duplicate`, `evidence_before_freeze`, …) |
//! | `review [--from YYYY-Www] [--to YYYY-Www]` | the Operator Review #2 packet → `reviews/<day>/`; prints `packet.md` (its last line the STOP) | the state is not intact (the packet is still written) |
//! | `verify` | every frozen run dir re-hashed, the forecast chain, each cycle's log line | anything but `MATCH` / intact |
//! | `show <cycle id \| cycles/… \| replays/… \| reviews/…>` | a run's status and files; a cycle's head, portfolio, grade and memo; a replay's report; a review's packet | the run is absent |
//!
//! | Input | Rule |
//! |---|---|
//! | Config | `--sandbox <name>` (`sandboxes/<name>/config.toml`) or `tengu -c <file> soe …`: its `[sources]` state dir is the SOE state root, `[soe]` the stage agents and limits, its digest (or `[generation]`) the generation pin. `cycle` and `replay` need one; without one the other commands read `<TENGU_HOME>/state/soe/` |
//! | Profile | `--profile`, else `<state root>/operator.toml`; signed; `--allow-synthetic` for fixtures |
//! | Stages | default: the agents, each run recorded in the stage cache (`[soe]` agents; secrets from the vault); `--offline`: the stage cache only — a miss fails the stage, the week goes on; `--no-llm`: no stage — the week decides what is carried (a `HOLD` week when nothing is) |
//! | Times | `--at`: RFC 3339, a UTC day or epoch ms (default now); `--week`: default the ISO week of `--at` (UTC); the week must hold the decision (± 1 day) |
//! | Output | text (default) or `--format json`; ids and hashes in full |

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{anyhow, bail, Context, Result};
use serde_json::{json, Value};

use super::{list_or, print_json, Format};
use crate::adapters::outbound::backfill::text_table;
use crate::adapters::outbound::clock::SystemClock;
use crate::adapters::outbound::secrets::{process_secret_registry, secrets_file_path};
use crate::adapters::outbound::soe::store::FsCycleStore;
use crate::adapters::outbound::sources::open_source_store;
use crate::application::runtime::{LeaseTiming, Supervisor};
use crate::application::soe::cycle::{
    run_cycle, CycleEnv, CycleParams, ProfileIn, Target, MEMO, PORTFOLIO, STAGES,
};
use crate::application::soe::freeze::{verify_state, FileCheck, LogState};
use crate::application::soe::grade::{
    current_grades, frozen_forecast, grade_cycle, resolutions, resolve_cycle, Answers,
};
use crate::application::soe::replay::{run_replay, ReplayParams, SetIn, REPORT_MD};
use crate::application::soe::review::{build_review, ReviewRange, PACKET_MD};
use crate::application::soe::submit::{head, read_json};
use crate::bootstrap::runtime::{LeasePlan, OwnerLeases};
use crate::bootstrap::soe::{generation_pin, stage_runner};
use crate::config::paths::resolve_tengu_home;
use crate::config::sections::SandboxSections;
use crate::config::soe::{load_profile_with_text, load_replay_set, SoeConfig, SOE_STATE};
use crate::config::sources::{sources_db, SourcesConfig};
use crate::config::Config;
use crate::domain::lineage::value::Time;
use crate::domain::marketdata::parse_time;
use crate::domain::soe::portfolio::{IsoWeek, WeeklyPortfolio};
use crate::domain::soe::record::from_toml;
use crate::domain::soe::review::CycleGrade;
use crate::ports::clock::Clock;
use crate::ports::soe::{CycleStore, RunDir, RunStatus, StageRunner};
use crate::ports::source_store::SourceStore;

const DAY_MS: i64 = 86_400_000;

/// How the model stages run (module table: Stages).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::adapters::inbound::cli) enum StageMode {
    Online,
    Offline,
    NoLlm,
}

impl StageMode {
    pub(in crate::adapters::inbound::cli) fn of(offline: bool, no_llm: bool) -> StageMode {
        match (offline, no_llm) {
            (_, true) => StageMode::NoLlm,
            (true, false) => StageMode::Offline,
            (false, false) => StageMode::Online,
        }
    }
}

/// What every command shares (module table: Input).
pub(in crate::adapters::inbound::cli) struct Common {
    pub sandbox: Option<String>,
    /// `tengu -c <file>`.
    pub config_file: Option<PathBuf>,
    pub profile: Option<PathBuf>,
    pub allow_synthetic: bool,
    pub format: Format,
}

impl Common {
    /// The config named by `--sandbox` or `-c`; none = no config.
    fn config(&self) -> Result<Option<Config>> {
        match (&self.sandbox, &self.config_file) {
            (Some(name), _) => {
                crate::bootstrap::sandbox::load_sandbox_or(Some(name.clone()), Config::default())
                    .map(Some)
            }
            (None, Some(file)) => {
                let cfg = Config::load(file)
                    .with_context(|| format!("load config {}", file.display()))?;
                std::env::set_var("TENGU_CONFIG", crate::config::paths::absolute_path(file));
                crate::adapters::outbound::egress::install(&cfg.egress)?;
                Ok(Some(cfg))
            }
            (None, None) => Ok(None),
        }
    }

    /// `cycle` / `replay`: a config with `[sources]` and `[soe]`.
    fn soe_config(&self, cmd: &str) -> Result<(Config, SoeConfig, SourcesConfig, PathBuf)> {
        let config = self.config()?.ok_or_else(|| {
            anyhow!(
                "soe_config_missing: `tengu soe {cmd}` needs a config with [sources] and [soe] — \
                 --sandbox <name> or `tengu -c <file> soe {cmd}`"
            )
        })?;
        let soe = config.soe.clone().ok_or_else(|| {
            anyhow!("soe_config_missing: the config has no [soe] section (stage agents, limits)")
        })?;
        let sources = config.sources.clone().ok_or_else(|| {
            anyhow!("soe_config_missing: the config has no [sources] section (the SOE state root)")
        })?;
        let root = sources.state_dir(&resolve_tengu_home());
        Ok((config, soe, sources, root))
    }

    fn profile_path(&self, root: &Path) -> PathBuf {
        self.profile
            .clone()
            .unwrap_or_else(|| SoeConfig::profile_path(root))
    }
}

/// The SOE state root of `config` (none: `<TENGU_HOME>/state/soe/`).
fn state_root(config: Option<&Config>) -> Result<PathBuf> {
    let home = resolve_tengu_home();
    match config {
        None => Ok(home.join("state").join(SOE_STATE)),
        Some(c) => c
            .sources
            .as_ref()
            .map(|s| s.state_dir(&home))
            .ok_or_else(|| anyhow!("the config has no [sources] section (the SOE state root)")),
    }
}

/// The source store when `sources.db` exists (a reader creates nothing).
fn open_sources(root: &Path) -> Result<Option<Arc<dyn SourceStore>>> {
    if !sources_db(root).is_file() {
        return Ok(None);
    }
    let sections = SandboxSections {
        sources_state_dir: Some(root.to_path_buf()),
        ..SandboxSections::default()
    };
    open_source_store(&sections).map(Some)
}

/// The runner of `mode` (module table: Stages).
fn runner(
    config: &Config,
    soe: &SoeConfig,
    root: &Path,
    store: Arc<dyn CycleStore>,
    mode: StageMode,
) -> Result<Option<Arc<dyn StageRunner>>> {
    match mode {
        StageMode::NoLlm => Ok(None),
        StageMode::Offline => stage_runner(config, soe, root, store, false).map(Some),
        StageMode::Online => {
            // The agents' children read their keys from the env (the vault).
            let _ = process_secret_registry(Some(&secrets_file_path(&resolve_tengu_home())));
            stage_runner(config, soe, root, store, true).map(Some)
        }
    }
}

fn time(s: &str) -> Result<i64> {
    parse_time(s).map_err(|e| anyhow!(e))
}

fn fmt_ms(ms: i64) -> String {
    Time::At(ms).to_string()
}

/// `--week` holds `at` (± 1 day: a Monday-morning slot in another zone).
fn week_of(week: Option<&str>, at_ms: i64) -> Result<IsoWeek> {
    let w = match week {
        Some(s) => s.parse::<IsoWeek>().map_err(|e| anyhow!("--week: {e}"))?,
        None => IsoWeek::of(&Time::At(at_ms))
            .ok_or_else(|| anyhow!("--at {}: no ISO week", fmt_ms(at_ms)))?,
    };
    let monday = w
        .monday()
        .and_hms_opt(0, 0, 0)
        .map(|d| d.and_utc().timestamp_millis())
        .ok_or_else(|| anyhow!("{w}: no Monday"))?;
    if !(monday - DAY_MS <= at_ms && at_ms < monday + 8 * DAY_MS) {
        bail!(
            "--week {w} does not hold the decision {} — give --at inside the week",
            fmt_ms(at_ms)
        );
    }
    Ok(w)
}

fn cell(s: &str) -> String {
    s.replace(['\n', '\r'], " ")
}

/// `f` under the sandbox's leases (`bootstrap::runtime::LeasePlan`:
/// `runtime:<sandbox>`, then `state:<SOE state root>`), renewed while it
/// runs: a live cycle never runs beside `tengu run`'s `soe_cycle` job or
/// another `tengu soe cycle` — two at once would fork the forecast chain.
/// Held ⇒ refused, naming the holder; lost while running ⇒ a warning.
async fn under_leases<T>(
    plan: &LeasePlan,
    f: impl std::future::Future<Output = Result<T>>,
) -> Result<T> {
    let timing = LeaseTiming::default();
    let owner = OwnerLeases::take(plan, timing.ttl_ms).await?;
    let mut supervisor = Supervisor::new();
    owner.keep(&mut supervisor, timing);
    let stopper = supervisor.stopper();
    let out = f.await;
    supervisor
        .shutdown(tokio::time::Instant::now() + std::time::Duration::from_secs(5))
        .await;
    owner.release().await;
    if let Some(stop) = stopper.cause().filter(|s| s.failed) {
        eprintln!(
            "warning: {} while the cycle ran — another process may own the SOE state root; run \
             `tengu soe verify`",
            stop.reason
        );
    }
    out
}

// ---------------------------------------------------------------------------
// cycle
// ---------------------------------------------------------------------------

pub(in crate::adapters::inbound::cli) async fn cycle(
    c: &Common,
    week: Option<&str>,
    at: Option<&str>,
    mode: StageMode,
) -> Result<()> {
    let (config, soe, registry, root) = c.soe_config("cycle")?;
    let clock = SystemClock;
    let now = clock.now_ms();
    let at_ms = match at {
        Some(s) => time(s)?,
        None => now,
    };
    if at_ms > now {
        bail!(
            "--at {}: after now ({}) — a live cycle decides on what was read by then",
            fmt_ms(at_ms),
            fmt_ms(now)
        );
    }
    let week = week_of(week, at_ms)?;
    let (profile, text) = load_profile_with_text(&c.profile_path(&root), c.allow_synthetic)
        .map_err(|e| anyhow!(e))?;
    let store: Arc<dyn CycleStore> = Arc::new(FsCycleStore::new(&root));
    let sources = open_sources(&root)?;
    let runner = runner(&config, &soe, &root, Arc::clone(&store), mode)?;
    let env = CycleEnv {
        sources: sources.as_deref(),
        registry: &registry,
        store: &*store,
        runner: runner.as_deref(),
        clock: &clock,
        profile: ProfileIn {
            record: &profile.record,
            sha256: &profile.sha256,
            text: &text,
        },
    };
    let params = CycleParams {
        target: Target::Cycle,
        week,
        decided_at_ms: at_ms,
        generation: generation_pin(&config)?,
        architect: soe.architect.clone(),
        critic: soe.critic.clone(),
        max_proposals: soe.max_proposals,
        forecast_max_weeks: soe.forecast_max_weeks,
        active: BTreeMap::new(),
        token_prices: soe.token_prices,
    };
    let out = under_leases(&LeasePlan::of(&config), run_cycle(&env, &params)).await?;
    let stages: Value = read_json(&*store, &out.dir, STAGES)?.unwrap_or(Value::Null);
    let p = &out.portfolio;
    if c.format == Format::Json {
        return print_json(&json!({
            "run": out.dir.to_string(),
            "cycle_id": out.cycle_id,
            "state_root": root.display().to_string(),
            "hold": p.is_hold(),
            "portfolio": p,
            "generation": params.generation,
            "profile_sha256": profile.sha256,
            "inputs_sha256": out.inputs_sha256,
            "decision_sha256": out.decision_sha256,
            "manifest_sha256": out.manifest_sha256,
            "episodes": out.episodes,
            "stages": stages,
        }));
    }
    println!(
        "{} frozen · {} · {} ranked, {} held, {} rejected · decided at {}",
        out.dir,
        if p.is_hold() {
            "HOLD week"
        } else {
            "RANKED week"
        },
        p.ranked.len(),
        p.held.len(),
        p.rejected.len(),
        p.as_of
    );
    println!("state root {}", root.display());
    println!(
        "profile {} v{} · sha256 {}{}",
        profile.record.id,
        profile.record.version,
        profile.sha256,
        if profile.record.synthetic {
            " · SYNTHETIC (test fixture)"
        } else {
            ""
        }
    );
    println!(
        "generation {} · sha256 {}",
        params.generation.id, params.generation.sha256
    );
    print_stages(&stages);
    print_portfolio(p);
    println!("inputs_sha256 {}", out.inputs_sha256);
    println!("decision_sha256 {}", out.decision_sha256);
    println!("manifest_sha256 {}", out.manifest_sha256);
    println!("episodes {}", out.episodes);
    Ok(())
}

fn print_stages(stages: &Value) {
    let rows: Vec<Vec<String>> = stages
        .as_array()
        .into_iter()
        .flatten()
        .map(|s| {
            vec![
                s["stage"].as_str().unwrap_or_default().to_string(),
                s["agent"].as_str().unwrap_or_default().to_string(),
                s["outcome"].as_str().unwrap_or_default().to_string(),
                cell(s["note"].as_str().unwrap_or_default()),
            ]
        })
        .collect();
    if !rows.is_empty() {
        print!(
            "{}",
            text_table(&["stage", "agent", "outcome", "note"], &rows)
        );
    }
}

fn print_portfolio(p: &WeeklyPortfolio) {
    if !p.ranked.is_empty() {
        let rows: Vec<Vec<String>> = p
            .ranked
            .iter()
            .map(|r| {
                vec![
                    r.rank.to_string(),
                    r.id.clone(),
                    r.opportunity_version.to_string(),
                    serde_json::to_string(&r.action).unwrap_or_default(),
                ]
            })
            .collect();
        print!(
            "{}",
            text_table(&["rank", "id", "version", "action"], &rows)
        );
    }
    for (name, rows) in [("held", &p.held), ("rejected", &p.rejected)] {
        if rows.is_empty() {
            continue;
        }
        println!("{name} ({}):", rows.len());
        let rows: Vec<Vec<String>> = rows
            .iter()
            .map(|r| {
                vec![
                    r.id.clone(),
                    r.opportunity_version.to_string(),
                    serde_json::to_string(&r.action).unwrap_or_default(),
                    r.gates.join(", "),
                ]
            })
            .collect();
        print!(
            "{}",
            text_table(&["id", "version", "action", "gates"], &rows)
        );
    }
    println!(
        "allocation: {} owner h · {} cash",
        p.allocation.owner_hours, p.allocation.cash
    );
    println!("next information: {}", list_or(&p.next_information, "none"));
    if let Some(r) = &p.hold_rationale {
        println!("rationale: {r}");
    }
}

// ---------------------------------------------------------------------------
// replay
// ---------------------------------------------------------------------------

/// The replay's options (module table).
pub(in crate::adapters::inbound::cli) struct ReplayArgs<'a> {
    pub set: &'a Path,
    pub run_id: Option<&'a str>,
    pub holdout: bool,
    pub mode: StageMode,
    pub scale_bps: i32,
}

pub(in crate::adapters::inbound::cli) async fn replay(
    c: &Common,
    a: &ReplayArgs<'_>,
) -> Result<()> {
    let (config, soe, registry, root) = c.soe_config("replay")?;
    let set = load_replay_set(a.set, c.allow_synthetic).map_err(|e| anyhow!(e))?;
    let (profile, text) = load_profile_with_text(&c.profile_path(&root), c.allow_synthetic)
        .map_err(|e| anyhow!(e))?;
    let clock = SystemClock;
    let run_id = match a.run_id {
        Some(r) => r.to_string(),
        None => format!(
            "r-{}",
            chrono::DateTime::from_timestamp_millis(clock.now_ms())
                .context("the clock is out of range")?
                .format("%Y%m%dT%H%M%SZ")
        ),
    };
    let store: Arc<dyn CycleStore> = Arc::new(FsCycleStore::new(&root));
    let sources = open_sources(&root)?;
    let runner = runner(&config, &soe, &root, Arc::clone(&store), a.mode)?;
    let env = CycleEnv {
        sources: sources.as_deref(),
        registry: &registry,
        store: &*store,
        runner: runner.as_deref(),
        clock: &clock,
        profile: ProfileIn {
            record: &profile.record,
            sha256: &profile.sha256,
            text: &text,
        },
    };
    let params = ReplayParams {
        run_id,
        generation: generation_pin(&config)?,
        architect: soe.architect.clone(),
        critic: soe.critic.clone(),
        max_proposals: soe.max_proposals,
        forecast_max_weeks: soe.forecast_max_weeks,
        token_prices: soe.token_prices,
        holdout: a.holdout,
        scale_bps: a.scale_bps,
    };
    let out = run_replay(
        &env,
        SetIn {
            record: &set.record,
            sha256: &set.sha256,
        },
        &params,
    )
    .await?;
    if c.format == Format::Json {
        return print_json(&json!({
            "run": out.dir.to_string(),
            "state_root": root.display().to_string(),
            "report": out.report,
            "manifest_sha256": out.manifest_sha256,
            "replays_line": out.line,
        }));
    }
    print!("{}", out.markdown);
    println!(
        "\n{} frozen · manifest sha256 {} · replays.jsonl line {} · state root {}",
        out.dir,
        out.manifest_sha256,
        out.line,
        root.display()
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// grade · resolve
// ---------------------------------------------------------------------------

fn store_of(c: &Common) -> Result<(Option<Config>, PathBuf, FsCycleStore)> {
    let config = c.config()?;
    let root = state_root(config.as_ref())?;
    let store = FsCycleStore::new(&root);
    Ok((config, root, store))
}

fn refused(what: &str, errors: &[crate::domain::soe::value::ValueError]) -> anyhow::Error {
    anyhow!(
        "{what} refused ({} problem(s)):\n{}",
        errors.len(),
        errors
            .iter()
            .map(|e| format!("- {e}"))
            .collect::<Vec<_>>()
            .join("\n")
    )
}

pub(in crate::adapters::inbound::cli) fn grade(c: &Common, cycle: &str, file: &Path) -> Result<()> {
    let (_, root, store) = store_of(c)?;
    let text = std::fs::read_to_string(file).with_context(|| format!("read {}", file.display()))?;
    if let Ok(g) = from_toml::<CycleGrade>(&text) {
        if g.cycle_id != cycle {
            bail!(
                "{}: grades cycle `{}`, not `{cycle}`",
                file.display(),
                g.cycle_id
            );
        }
    }
    let graded = grade_cycle(&store, &text)?
        .map_err(|e| refused(&format!("grade of {}", file.display()), &e))?;
    if c.format == Format::Json {
        return print_json(&json!({
            "cycle_id": cycle,
            "state_root": root.display().to_string(),
            "grade": graded.grade,
            "line": graded.line,
            "supersedes": graded.supersedes,
        }));
    }
    let g = &graded.grade;
    println!(
        "grade `{}` v{} of cycles/{} appended · grades.jsonl line {}{}",
        g.id,
        g.version,
        g.cycle_id,
        graded.line,
        graded
            .supersedes
            .map_or(String::new(), |v| format!(" · supersedes v{v}"))
    );
    println!(
        "{} · correction {} min · research {} h · changed a decision: {} · misses {}",
        g.scores()
            .iter()
            .map(|(k, v)| format!("{k} {v}"))
            .collect::<Vec<_>>()
            .join(" · "),
        g.correction_minutes,
        g.research_hours,
        if g.changed_decision { "yes" } else { "no" },
        g.misses.len()
    );
    Ok(())
}

pub(in crate::adapters::inbound::cli) async fn resolve(
    c: &Common,
    cycle: &str,
    file: Option<&Path>,
) -> Result<()> {
    let (config, root, store) = store_of(c)?;
    let answers: Option<Answers> = match file {
        None => None,
        Some(f) => {
            let text =
                std::fs::read_to_string(f).with_context(|| format!("read {}", f.display()))?;
            Some(toml::from_str(&text).with_context(|| format!("{}", f.display()))?)
        }
    };
    let registry = config.as_ref().and_then(|c| c.sources.clone());
    let opened = match &registry {
        Some(_) => open_sources(&root)?,
        None => None,
    };
    let sources: Option<(&dyn SourceStore, &SourcesConfig)> =
        match (opened.as_deref(), registry.as_ref()) {
            (Some(s), Some(r)) => Some((s, r)),
            _ => None,
        };
    let r = resolve_cycle(
        &store,
        sources,
        cycle,
        answers.as_ref(),
        SystemClock.now_ms(),
    )
    .await?
    .map_err(|e| refused(&format!("resolution of {cycle}"), &e))?;
    if c.format == Format::Json {
        return print_json(&r);
    }
    println!(
        "cycles/{} · forecast sha256 {} · {} item(s): {} resolved now, {} before, {} pending",
        r.cycle_id,
        r.forecast_sha256,
        r.items,
        r.added.len(),
        r.already.len(),
        r.pending.len()
    );
    if !r.added.is_empty() {
        let rows: Vec<Vec<String>> = r
            .added
            .iter()
            .map(|l| {
                vec![
                    l.item.to_string(),
                    l.candidate.clone(),
                    l.probability.to_string(),
                    if l.hit { "HIT" } else { "MISS" }.to_string(),
                    l.observed_at.to_string(),
                    l.resolved_by.clone(),
                    list_or(&l.evidence, "-"),
                ]
            })
            .collect();
        print!(
            "{}",
            text_table(
                &[
                    "item",
                    "candidate",
                    "probability bps",
                    "outcome",
                    "observed",
                    "by",
                    "evidence"
                ],
                &rows
            )
        );
    }
    for (item, why) in &r.pending {
        println!("pending item {item}: {why}");
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// review · verify · show
// ---------------------------------------------------------------------------

pub(in crate::adapters::inbound::cli) fn review(
    c: &Common,
    from: Option<&str>,
    to: Option<&str>,
) -> Result<()> {
    let (_, root, store) = store_of(c)?;
    let week = |s: Option<&str>, flag: &str| -> Result<Option<IsoWeek>> {
        s.map(|w| w.parse::<IsoWeek>().map_err(|e| anyhow!("{flag}: {e}")))
            .transpose()
    };
    let range = ReviewRange {
        from: week(from, "--from")?,
        to: week(to, "--to")?,
    };
    let out = build_review(&store, &SystemClock, range)?;
    if c.format == Format::Json {
        print_json(&json!({
            "run": out.dir.to_string(),
            "state_root": root.display().to_string(),
            "packet": out.json,
            "manifest_sha256": out.manifest_sha256,
            "reviews_line": out.line,
        }))?;
    } else {
        print!("{}", out.markdown);
        println!(
            "\n{} frozen · manifest sha256 {} · reviews.jsonl line {} · state root {}",
            out.dir,
            out.manifest_sha256,
            out.line,
            root.display()
        );
    }
    if !out.integrity_ok {
        bail!(
            "the SOE state is not intact — see the packet's Integrity section (`tengu soe verify`)"
        );
    }
    Ok(())
}

fn check_name(c: FileCheck) -> &'static str {
    match c {
        FileCheck::Match => "MATCH",
        FileCheck::Mismatch => "MISMATCH",
        FileCheck::Absent => "ABSENT",
        FileCheck::Extra => "EXTRA",
    }
}

fn log_name(s: LogState) -> &'static str {
    match s {
        LogState::Match => "MATCH",
        LogState::Mismatch => "MISMATCH",
        LogState::Missing => "MISSING",
        LogState::Orphan => "ORPHAN",
    }
}

pub(in crate::adapters::inbound::cli) fn verify(c: &Common) -> Result<()> {
    let (_, root, store) = store_of(c)?;
    let s = verify_state(&store)?;
    if c.format == Format::Json {
        print_json(&json!({
            "state_root": root.display().to_string(),
            "ok": s.ok(),
            "check": s,
        }))?;
    } else {
        println!("state root {}", root.display());
        let rows: Vec<Vec<String>> = s
            .runs
            .iter()
            .map(|r| {
                let bad: Vec<String> = r
                    .files
                    .iter()
                    .filter(|(_, c)| *c != FileCheck::Match)
                    .map(|(f, c)| format!("{f} {}", check_name(*c)))
                    .collect();
                vec![
                    r.run.clone(),
                    r.manifest_sha256
                        .clone()
                        .unwrap_or_else(|| "NO MANIFEST".into()),
                    if r.ok() {
                        format!("MATCH ({} files)", r.files.len())
                    } else if r.manifest_sha256.is_none() {
                        "NOT FROZEN".to_string()
                    } else {
                        bad.join(", ")
                    },
                ]
            })
            .collect();
        if rows.is_empty() {
            println!("no frozen run");
        } else {
            print!(
                "{}",
                text_table(&["run", "manifest sha256", "files"], &rows)
            );
        }
        for o in &s.open {
            println!("open (claimed, never frozen): {o}");
        }
        println!(
            "forecast chain: {} line(s) · {}",
            s.log_lines,
            if s.chain.is_empty() {
                "intact".to_string()
            } else {
                s.chain.join("; ")
            }
        );
        for (cycle, state) in &s.log {
            println!("log line of {cycle}: {}", log_name(*state));
        }
        println!("{}", if s.ok() { "OK" } else { "NOT INTACT" });
    }
    if !s.ok() {
        bail!("soe verify: the SOE state is not intact");
    }
    Ok(())
}

pub(in crate::adapters::inbound::cli) fn show(c: &Common, run: &str) -> Result<()> {
    let (_, root, store) = store_of(c)?;
    let dir = RunDir::parse(run).unwrap_or_else(|| RunDir::Cycle(run.to_string()));
    let status = store.status(&dir)?;
    if status == RunStatus::Absent {
        bail!("{dir}: no such run in {}", root.display());
    }
    let files = store.files(&dir)?;
    let text_of = |name: &str| -> Result<Option<String>> {
        Ok(store
            .read(&dir, name)?
            .map(|b| String::from_utf8_lossy(&b).into_owned()))
    };
    let status_name = match status {
        RunStatus::Frozen => "FROZEN",
        RunStatus::Open => "OPEN (claimed, never frozen)",
        RunStatus::Absent => "ABSENT",
    };
    let (grade, resolved) = match &dir {
        RunDir::Cycle(id) => (
            current_grades(&store)?.remove(id),
            resolutions(&store)?
                .into_iter()
                .filter(|l| &l.cycle_id == id)
                .collect::<Vec<_>>(),
        ),
        _ => (None, Vec::new()),
    };
    if c.format == Format::Json {
        let portfolio: Option<Value> = read_json(&store, &dir, PORTFOLIO)?;
        return print_json(&json!({
            "run": dir.to_string(),
            "state_root": root.display().to_string(),
            "status": status_name,
            "files": files,
            "head": head(&store, &dir).ok(),
            "portfolio": portfolio,
            "grade": grade,
            "resolutions": resolved,
        }));
    }
    println!("{dir} · {status_name} · {} file(s)", files.len());
    println!("state root {}", root.display());
    if let RunDir::Cycle(id) = &dir {
        if let Ok(h) = head(&store, &dir) {
            println!(
                "decided at {} ({} clock) · packet sha256 {} · generation {} {}",
                h.decided_at(),
                serde_json::to_value(h.mode)
                    .ok()
                    .and_then(|v| v.as_str().map(str::to_lowercase))
                    .unwrap_or_default(),
                h.packet_sha256,
                h.generation.id,
                h.generation.sha256
            );
        }
        if status == RunStatus::Frozen {
            if let Ok((f, sha)) = frozen_forecast(&store, id) {
                println!(
                    "forecast sha256 {sha} · {} item(s), {} resolved",
                    f.items.len(),
                    resolved.len()
                );
            }
        }
        match &grade {
            Some(g) => println!(
                "grade `{}` v{} by {} at {} · correction {} min · changed a decision: {}",
                g.id,
                g.version,
                g.graded_by,
                g.graded_at,
                g.correction_minutes,
                if g.changed_decision { "yes" } else { "no" }
            ),
            None => println!("grade: none yet (`tengu soe grade {id} --file <toml>`)"),
        }
    }
    let body = match &dir {
        RunDir::Cycle(_) => text_of(MEMO)?,
        RunDir::Replay(_) => text_of(REPORT_MD)?,
        RunDir::Review(_) => text_of(PACKET_MD)?,
    };
    match body {
        Some(b) => print!("\n{b}"),
        None => println!("files: {}", list_or(&files, "none")),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A live cycle takes the sandbox's leases: refused while a `tengu run`
    /// of the sandbox holds them (nothing runs), run once they are free —
    /// and freed again after.
    #[tokio::test]
    async fn cycle_waits_for_no_runner() {
        let dir = tempfile::tempdir().unwrap();
        let plan = LeasePlan {
            sandbox: "soe".into(),
            state_dir: dir.path().to_path_buf(),
            ledger: false,
            soe_state: Some(dir.path().to_path_buf()),
        };
        let held = OwnerLeases::take(&plan, 60_000).await.unwrap();
        let ran = std::sync::atomic::AtomicBool::new(false);
        let e = under_leases(&plan, async {
            ran.store(true, std::sync::atomic::Ordering::SeqCst);
            Ok(())
        })
        .await
        .unwrap_err();
        assert!(
            format!("{e:#}").contains("sandbox `soe` is already running"),
            "{e:#}"
        );
        assert!(!ran.load(std::sync::atomic::Ordering::SeqCst));
        assert!(held.release().await);
        let n = under_leases(&plan, async { Ok(7) }).await.unwrap();
        assert_eq!(n, 7);
        // Released: a runner takes them again.
        let again = OwnerLeases::take(&plan, 60_000).await.unwrap();
        assert_eq!(
            again.resources(),
            ["runtime:soe", "state:{}"]
                .map(|r| r.replace("{}", &dir.path().file_name().unwrap().to_string_lossy()))
        );
        assert!(again.release().await);
    }
}
