//! Strategy ranking coordinator (`docs/strategy-ranking-automation-2026-10-08.md`
//! SR-5): one ranking date of one sealed contract — freshness → backtest →
//! evaluate → rank → publish — under a lease, resumable from its manifest.
//! The contract is `domain/lineage/ranking.rs`, selection and rating
//! `domain/backtest/ranking.rs` (pure), the runs `application/backtest/`, the
//! files [`store`]. IO is injected ([`RankingEnv`]), so tests run on a temp
//! state dir. Callers: `tengu ranking run` (`adapters/inbound/cli/ranking.rs`)
//! and the `strategy_ranking` tool (`adapters/outbound/tools/xlab/rank.rs`,
//! a `[feeds.*]` of `tengu run` too).
//!
//! | Step | Rule |
//! |---|---|
//! | 1 Contract | listed in `[strategy_ranking]` (`contract_not_listed`); the registry reloaded from disk (a seal appended after start counts); its newest `[[sealed]]` row `ranking:<id>` hashes the file now — else `contract_unsealed` / `contract_changed`; nothing is written before this passes |
//! | 2 Date | [`ranking_date`]: the given date, else the newest local date in `tz` whose cutoff has passed; a weekday of `days` (none = every day) — else `not_a_ranking_day`; cutoff = `Zone::at(date, cutoff)` (DST-correct), ≤ now — else `cutoff_not_reached` — and after `from` — else `before_from` |
//! | 3 Idempotency | a `COMPLETE` / `INCOMPLETE` manifest: the published ranking comes back (`published = false`), nothing runs or is written — checked before and after the lease; only the operator reruns a published date (by deleting its `<date>/` dir) |
//! | 4 Lease | `ranking:<contract id>` in the state dir's `runtime.db`, TTL 15 min, renewed before each strategy, before each manifest write (each strategy's, the terminal one, a `FAILED` one) and before each publish write (the dated ranking, `latest`); another holder ⇒ `ranking_busy` (named); a lost renewal — another holder has it, or it lapsed and another had it since (`acquired_at_ms` changed: a step outlived the TTL) — ⇒ `ranking_lease_lost`, and this run writes nothing more; released at the end |
//! | 5 Manifest | a `RUNNING` / `FAILED` one of the same contract sha256 is resumed: a `DONE` strategy whose `report.json` still hashes its `report_sha256` is reused, the rest run again; rewritten after each strategy |
//! | 6 Freshness | per instrument the strategy reads: its newest stored bar at the spec's interval closes ≥ cutoff − `max_lag_bars` × interval, else `STALE` (no run) |
//! | 7 Backtest | one strategy at a time: `prepare` (from = `from`, to = data through = the cutoff, no split — never a holdout read), `evaluate` (the rules arms), `write_run_dir` (retention also keeps this date's `DONE` runs) |
//! | 8 Evaluate | `report.json` read back: its sha256, `RunFacts`, the standing from the reloaded registry as of the cutoff (a PASS decided after it lifts nothing: a grade landed since lifts only later dates, a rerun of an earlier one too); evaluation `NOT_GATED`; skips and data notes are never failures |
//! | 9 Rank | `select` → `rank`, stamped with the contract sha256, the date and now |
//! | 10 Publish | lease renewed ⇒ `<date>/ranking.json` + `ranking.md`; `COMPLETE` and no newer date in `latest` ⇒ renewed ⇒ `latest.md`, then `latest.json`; renewed ⇒ the manifest's final status. A failure after step 5 marks the manifest `FAILED` (resumable) once the lease is renewed — a lost lease excepted: the manifest is the new holder's (a store error there: no `FAILED` either, ownership unproven) |
//!
//! A failed strategy's `error` names the state dir `<state>` (never its
//! path), so a copy of the state ranks to the same `content_sha256`.

pub(crate) mod store;

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{anyhow, bail, Context, Result};
use chrono::{Datelike, Days, NaiveDate};
use tracing::{info, warn};

use self::store::{
    contract_dir, date_dir, json_bytes, latest_date, read_manifest, read_ranking, write_atomic,
    write_manifest, Manifest, ManifestStatus, StrategyEntry, StrategyStatus, LATEST_JSON,
    LATEST_MD, MANIFEST_SCHEMA, RANKING_JSON, RANKING_MD,
};
use crate::application::backtest::{
    evaluate, prepare, resolve, write_run_dir, BacktestEnv, BacktestJob, Resolved, SpecSource,
};
use crate::config::lineage::load_registry;
use crate::config::sections::SandboxSections;
use crate::config::strategy_ranking::RankingSection;
use crate::config::xmarket::backtests_dir;
use crate::domain::backtest::ranking::{
    rank, select, Ranking, RankingStamp, RankingStatus, RunFacts, RunInput, StrategyStanding,
};
use crate::domain::backtest::report::BacktestReport;
use crate::domain::canonical::sha256_hex;
use crate::domain::lineage::ranking::RankingContract;
use crate::domain::lineage::value::{RecordKind, Time};
use crate::domain::lineage::Registry;
use crate::domain::marketdata::fmt_time;
use crate::ports::clock::Clock;
use crate::ports::market_data::MarketDataStore;
use crate::ports::runtime::RuntimeStore;

/// The ranking lease's TTL; renewed before each strategy.
pub(crate) const LEASE_TTL_MS: i64 = 15 * 60_000;
const REPORT_JSON: &str = "report.json";

/// What a ranking reads and writes, injected (module doc).
#[derive(Clone)]
pub(crate) struct RankingEnv {
    /// `<state dir>/market.db`, read only.
    pub store: Arc<dyn MarketDataStore>,
    /// `[backtest]`, `[xmarket.calendars]`, `[risk]` / `[paper]`,
    /// `[strategy_ranking]` (`SandboxSections::ranking`) of the sandbox.
    pub sections: Arc<SandboxSections>,
    /// The state dir's `runtime.db`: the ranking lease.
    pub runtime: Arc<dyn RuntimeStore>,
    pub clock: Arc<dyn Clock>,
    /// The `[xmarket]` state dir: `backtests/`, `strategy-rankings/`; its
    /// name is the `<state>` of every `run:<state>/<run id>`.
    pub state_dir: PathBuf,
}

/// One ranking date of one contract.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct RankRequest {
    /// A contract id of `[strategy_ranking] contracts` ([`pick_contract`]).
    pub contract: String,
    /// `None` = the newest date whose cutoff has passed ([`ranking_date`]).
    pub date: Option<NaiveDate>,
    /// The lease holder and the manifest's `holder`: `cli:<pid>`, a tool
    /// call id.
    pub holder: String,
}

/// What [`run_ranking`] returns.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct RankOutcome {
    pub status: RankingStatus,
    /// `false`: the date was already published — nothing ran (step 3).
    pub published: bool,
    /// Whether the date's publish replaced `latest`.
    pub latest_replaced: bool,
    /// `<state dir>/strategy-rankings/<contract id>/<date>`.
    pub dir: PathBuf,
    pub ranking: Ranking,
    pub manifest: Manifest,
}

/// A contract the publisher may run: sealed, unchanged since.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct SealedContract {
    pub contract: RankingContract,
    /// The file's digest = its newest `[[sealed]]` row.
    pub sha256: String,
    pub sealed_at: Time,
}

/// The lease a ranking of `contract` holds.
pub(crate) fn lease_resource(contract: &str) -> String {
    format!("ranking:{contract}")
}

/// `[strategy_ranking]` resolved, else `no_strategy_ranking`.
pub(crate) fn ranking_section(sections: &SandboxSections) -> Result<&RankingSection> {
    sections.ranking.as_deref().ok_or_else(|| {
        anyhow!(
            "no_strategy_ranking: this sandbox has no [strategy_ranking] section — list the \
             ranking contracts it runs there (registry, contracts)"
        )
    })
}

/// The contract a caller means: `requested` when listed, else the only one
/// listed.
pub(crate) fn pick_contract(section: &RankingSection, requested: Option<&str>) -> Result<String> {
    match requested {
        Some(id) if section.contracts.iter().any(|c| c == id) => Ok(id.to_string()),
        Some(id) => bail!(
            "contract_not_listed: `{id}` is not in [strategy_ranking] contracts ({})",
            section.contracts.join(", ")
        ),
        None => match section.contracts.as_slice() {
            [only] => Ok(only.clone()),
            all => bail!(
                "contract: [strategy_ranking] lists {} contracts ({}) — name one",
                all.len(),
                all.join(", ")
            ),
        },
    }
}

/// Step 1 (module table): contract `id` of `reg`, sealed and unchanged.
pub(crate) fn sealed_contract(reg: &Registry, id: &str) -> Result<SealedContract> {
    let contract = reg
        .rankings
        .get(id)
        .ok_or_else(|| anyhow!("contract_unknown: no rankings/{id}.toml in the registry"))?;
    let digest = reg
        .digests
        .get(&(RecordKind::Ranking, id.to_string()))
        .ok_or_else(|| anyhow!("contract_unknown: no file digest for rankings/{id}.toml"))?;
    let key = format!("{}:{id}", RecordKind::Ranking);
    let Some(seal) = reg.locks.sealed.iter().rev().find(|s| s.record == key) else {
        bail!(
            "contract_unsealed: `{id}` has no [[sealed]] row in locks.toml — the operator reviews \
             the contract, then runs `tengu lineage seal {key}`; nothing ran"
        );
    };
    if seal.sha256 != *digest {
        bail!(
            "contract_changed: `{id}` was sealed as {} at {}; the file now hashes {digest} — a \
             changed contract is a new contract id; nothing ran",
            seal.sha256,
            seal.sealed_at
        );
    }
    Ok(SealedContract {
        contract: contract.clone(),
        sha256: digest.clone(),
        sealed_at: seal.sealed_at,
    })
}

/// Step 2 (module table): the ranking date and its cutoff (UTC ms) at
/// `now_ms`.
pub(crate) fn ranking_date(
    c: &RankingContract,
    requested: Option<NaiveDate>,
    now_ms: i64,
) -> Result<(NaiveDate, i64)> {
    let zone = c
        .zone()
        .ok_or_else(|| anyhow!("contract `{}`: tz `{}` is not supported", c.id, c.tz))?;
    let m = c
        .cutoff_minute()
        .ok_or_else(|| anyhow!("contract `{}`: cutoff `{}` is not HH:MM", c.id, c.cutoff))?;
    let days = c
        .weekdays()
        .map_err(|d| anyhow!("contract `{}`: days `{d}` is not a weekday", c.id))?;
    let start = c
        .start_ms()
        .ok_or_else(|| anyhow!("contract `{}`: from `{}` is not a UTC day", c.id, c.from))?;
    let cutoff_of = |d: NaiveDate| zone.at(d, m / 60, m % 60);
    let ranks_on = |d: NaiveDate| days.is_empty() || days.contains(&d.weekday());
    let local = |ms: i64| format!("{} {}", zone.to_local(ms).format("%Y-%m-%d %H:%M"), c.tz);
    let date = match requested {
        Some(d) => {
            if !ranks_on(d) {
                bail!(
                    "not_a_ranking_day: {d} is a {:?}; `{}` ranks on {}",
                    d.weekday(),
                    c.id,
                    c.days.join(", ")
                );
            }
            if cutoff_of(d) > now_ms {
                bail!(
                    "cutoff_not_reached: {d}'s cutoff is {} ({}); now is {}",
                    local(cutoff_of(d)),
                    fmt_time(cutoff_of(d)),
                    fmt_time(now_ms)
                );
            }
            d
        }
        None => {
            let today = zone.local_date(now_ms);
            (0..=7u64)
                .filter_map(|k| today.checked_sub_days(Days::new(k)))
                .find(|d| ranks_on(*d) && cutoff_of(*d) <= now_ms)
                .ok_or_else(|| anyhow!("no ranking date of `{}` within a week of now", c.id))?
        }
    };
    let cutoff = cutoff_of(date);
    if cutoff <= start {
        bail!(
            "before_from: {date}'s cutoff {} is not after the contract's from {}",
            fmt_time(cutoff),
            fmt_time(start)
        );
    }
    Ok((date, cutoff))
}

/// The state dir's name: the `<state>` of `run:<state>/<run id>`.
fn state_name(state_dir: &Path) -> Result<String> {
    state_dir
        .file_name()
        .and_then(|n| n.to_str())
        .map(str::to_string)
        .ok_or_else(|| anyhow!("state dir {} has no name", state_dir.display()))
}

/// The published ranking of `dir` (step 3), `None` unless its manifest is
/// `COMPLETE` / `INCOMPLETE`.
fn published(dir: &Path) -> Result<Option<RankOutcome>> {
    let Some(manifest) = read_manifest(dir)? else {
        return Ok(None);
    };
    if !manifest.status.is_published() {
        return Ok(None);
    }
    let ranking = read_ranking(&dir.join(RANKING_JSON)).with_context(|| {
        format!(
            "{} says {:?}, but its ranking does not read",
            dir.join(store::MANIFEST_JSON).display(),
            manifest.status
        )
    })?;
    Ok(Some(RankOutcome {
        status: ranking.status,
        published: false,
        latest_replaced: manifest.latest_replaced,
        dir: dir.to_path_buf(),
        ranking,
        manifest,
    }))
}

/// Run (or resume, or return) one ranking date (module table).
pub(crate) async fn run_ranking(env: &RankingEnv, req: RankRequest) -> Result<RankOutcome> {
    let section = ranking_section(&env.sections)?;
    pick_contract(section, Some(&req.contract))?;
    let reg = load_registry(&section.registry_dir).map_err(|es| {
        anyhow!(
            "the lineage registry {} does not load: {}",
            section.registry_dir.display(),
            es.join("; ")
        )
    })?;
    let sealed = sealed_contract(&reg, &req.contract)?;
    let c = &sealed.contract;
    if c.sandbox != env.sections.owner() {
        bail!(
            "contract_sandbox: `{}` ranks sandbox `{}`, this is `{}`",
            c.id,
            c.sandbox,
            env.sections.owner()
        );
    }
    let (date, cutoff_ms) = ranking_date(c, req.date, env.clock.now_ms())?;
    let dir = date_dir(&env.state_dir, &c.id, date);
    if let Some(done) = published(&dir)? {
        return Ok(done);
    }
    let resource = lease_resource(&c.id);
    let lease = env
        .runtime
        .acquire_lease(&resource, &req.holder, LEASE_TTL_MS, env.clock.now_ms())
        .await?;
    if !lease.granted {
        bail!(
            "ranking_busy: `{resource}` is held by `{}` until {} — one ranking of a contract at a \
             time",
            lease.current_holder,
            fmt_time(lease.expires_at_ms)
        );
    }
    let run = Run {
        env,
        reg: &reg,
        sealed: &sealed,
        date,
        cutoff_ms,
        dir,
        holder: &req.holder,
        resource: &resource,
        acquired_at_ms: lease.acquired_at_ms,
    };
    let out = run.go().await;
    if let Err(e) = env.runtime.release_lease(&resource, &req.holder).await {
        warn!(lease = %resource, error = %format!("{e:#}"), "strategy ranking: lease not released (it expires)");
    }
    out
}

/// One date's run, the lease held.
struct Run<'a> {
    env: &'a RankingEnv,
    reg: &'a Registry,
    sealed: &'a SealedContract,
    date: NaiveDate,
    cutoff_ms: i64,
    dir: PathBuf,
    holder: &'a str,
    resource: &'a str,
    /// The lease's `acquired_at_ms` when this run took it: a renewal granted
    /// with another one lapsed in between (step 4).
    acquired_at_ms: i64,
}

/// `ranking_lease_lost`: the lease expired during a step and another holder
/// took it (it may hold it still, or have released it since) — this run
/// writes nothing more (no FAILED manifest over the new holder's).
#[derive(Debug)]
struct LeaseLost {
    resource: String,
    /// `passed to `<holder>`` · `lapsed …`.
    how: String,
}

impl std::fmt::Display for LeaseLost {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        write!(
            f,
            "ranking_lease_lost: `{}` {} — this run stops and writes nothing more; \
             that holder runs the date (a later run resumes its manifest)",
            self.resource, self.how
        )
    }
}

impl std::error::Error for LeaseLost {}

/// What one strategy's fresh run gave (steps 6–8).
enum Fresh {
    Ran { text: String },
    Failed { stage: &'static str, error: String },
    Stale(String),
}

impl Run<'_> {
    async fn go(&self) -> Result<RankOutcome> {
        if let Some(done) = published(&self.dir)? {
            return Ok(done);
        }
        let c = &self.sealed.contract;
        let now = self.env.clock.now_ms();
        let mut m = match read_manifest(&self.dir)? {
            Some(prev) if prev.contract_sha256 == self.sealed.sha256 => {
                info!(
                    contract = %c.id, date = %self.date, status = ?prev.status,
                    "strategy ranking: resuming the date's manifest"
                );
                prev
            }
            _ => Manifest {
                schema: MANIFEST_SCHEMA.to_string(),
                contract: c.id.clone(),
                contract_sha256: self.sealed.sha256.clone(),
                sealed_at: self.sealed.sealed_at,
                sandbox: c.sandbox.clone(),
                date: self.date,
                tz: c.tz.clone(),
                from_ms: c.start_ms().unwrap_or_default(),
                cutoff_ms: self.cutoff_ms,
                status: ManifestStatus::Running,
                strategies: Default::default(),
                holder: self.holder.to_string(),
                started_at_ms: now,
                updated_at_ms: now,
                finished_at_ms: None,
                content_sha256: None,
                latest_replaced: false,
                error: None,
            },
        };
        m.status = ManifestStatus::Running;
        m.holder = self.holder.to_string();
        m.updated_at_ms = now;
        m.finished_at_ms = None;
        m.error = None;
        write_manifest(&self.dir, &m)?;
        match self.steps(&mut m).await {
            Ok(out) => Ok(out),
            // Another holder runs the date now: the manifest is its to write.
            Err(e) if e.downcast_ref::<LeaseLost>().is_some() => Err(e),
            Err(e) => {
                // The FAILED manifest only under a renewed lease (step 4).
                if let Err(r) = self.renew().await {
                    warn!(
                        error = %format!("{e:#}"), renewal = %format!("{r:#}"),
                        "strategy ranking: the run failed and its lease is not renewed — no FAILED manifest"
                    );
                    return Err(if r.downcast_ref::<LeaseLost>().is_some() {
                        r
                    } else {
                        e
                    });
                }
                m.status = ManifestStatus::Failed;
                m.error = Some(format!("{e:#}"));
                m.updated_at_ms = self.env.clock.now_ms();
                if let Err(w) = write_manifest(&self.dir, &m) {
                    warn!(error = %format!("{w:#}"), "strategy ranking: the FAILED manifest was not written");
                }
                Err(e)
            }
        }
    }

    /// Steps 5–10 (module table).
    async fn steps(&self, m: &mut Manifest) -> Result<RankOutcome> {
        let c = &self.sealed.contract;
        let state = state_name(&self.env.state_dir)?;
        let mut inputs = Vec::with_capacity(c.strategies.len());
        for name in &c.strategies {
            self.renew().await?;
            let reused = m
                .strategies
                .get(name)
                .filter(|e| e.status == StrategyStatus::Done)
                .and_then(|e| self.reusable(e));
            let fresh = match reused {
                Some(text) => Fresh::Ran { text },
                None => {
                    let keep: BTreeSet<String> = m
                        .strategies
                        .values()
                        .filter(|e| e.status == StrategyStatus::Done)
                        .filter_map(StrategyEntry::run_id)
                        .collect();
                    self.run_strategy(name, &keep).await
                }
            };
            let (entry, input) = self.input(&state, name, fresh);
            info!(
                contract = %c.id, date = %self.date, strategy = %name, status = ?entry.status,
                run = entry.run.as_deref().unwrap_or("-"),
                "strategy ranking: strategy done"
            );
            // A strategy that outlived the TTL may have lost the date to
            // another holder: this run writes nothing more then.
            self.renew().await?;
            m.strategies.insert(name.clone(), entry);
            m.updated_at_ms = self.env.clock.now_ms();
            write_manifest(&self.dir, m)?;
            inputs.push(input);
        }
        let stamp = RankingStamp {
            contract_sha256: self.sealed.sha256.clone(),
            date: self.date,
            generated_at_ms: self.env.clock.now_ms(),
        };
        let ranking = rank(c, &stamp, select(c, self.cutoff_ms, &inputs));
        let json = json_bytes(&ranking)?;
        let md = ranking.render_markdown();
        // Step 10, each write under a renewed lease (step 4): a run that
        // stalled here past the TTL never overwrites the new holder's.
        self.renew().await?;
        write_atomic(&self.dir, RANKING_JSON, &json)?;
        write_atomic(&self.dir, RANKING_MD, md.as_bytes())?;
        let latest_replaced = ranking.status == RankingStatus::Complete
            && !matches!(latest_date(&self.env.state_dir, &c.id), Some(d) if d > self.date);
        if latest_replaced {
            self.renew().await?;
            let cdir = contract_dir(&self.env.state_dir, &c.id);
            write_atomic(&cdir, LATEST_MD, md.as_bytes())?;
            write_atomic(&cdir, LATEST_JSON, &json)?;
        }
        let now = self.env.clock.now_ms();
        m.status = match ranking.status {
            RankingStatus::Complete => ManifestStatus::Complete,
            RankingStatus::Incomplete => ManifestStatus::Incomplete,
        };
        m.content_sha256 = Some(ranking.content_sha256());
        m.latest_replaced = latest_replaced;
        m.finished_at_ms = Some(now);
        m.updated_at_ms = now;
        self.renew().await?;
        write_manifest(&self.dir, m)?;
        info!(
            contract = %c.id, date = %self.date, status = ?m.status, latest_replaced,
            content_sha256 = m.content_sha256.as_deref().unwrap_or(""),
            "strategy ranking: published"
        );
        Ok(RankOutcome {
            status: ranking.status,
            published: true,
            latest_replaced,
            dir: self.dir.clone(),
            ranking,
            manifest: m.clone(),
        })
    }

    /// Step 4: renew the lease (TTL from now), else [`LeaseLost`] — also
    /// when it is granted with another `acquired_at_ms`: it lapsed and
    /// another holder had it since (the store keeps `acquired_at_ms` only
    /// across one holder).
    async fn renew(&self) -> Result<()> {
        let lease = self
            .env
            .runtime
            .acquire_lease(
                self.resource,
                self.holder,
                LEASE_TTL_MS,
                self.env.clock.now_ms(),
            )
            .await?;
        let how = match (lease.granted, lease.acquired_at_ms == self.acquired_at_ms) {
            (true, true) => return Ok(()),
            (true, false) => format!(
                "lapsed and another holder had it since this run took it (acquired at {}, now \
                 {})",
                fmt_time(self.acquired_at_ms),
                fmt_time(lease.acquired_at_ms)
            ),
            (false, _) => format!("passed to `{}`", lease.current_holder),
        };
        Err(LeaseLost {
            resource: self.resource.to_string(),
            how,
        }
        .into())
    }

    /// A DONE entry's `report.json` text while it still hashes its
    /// `report_sha256` (step 5).
    fn reusable(&self, e: &StrategyEntry) -> Option<String> {
        let path = backtests_dir(&self.env.state_dir)
            .join(e.run_id()?)
            .join(REPORT_JSON);
        let text = std::fs::read_to_string(path).ok()?;
        (Some(sha256_hex(&text)) == e.report_sha256).then_some(text)
    }

    /// `e` without the state dir's path (module doc).
    fn clean(&self, e: impl std::fmt::Display) -> String {
        format!("{e:#}").replace(&self.env.state_dir.display().to_string(), "<state>")
    }

    /// Steps 6–7 for strategy `name`; `keep` = this date's DONE run ids
    /// (never pruned meanwhile).
    async fn run_strategy(&self, name: &str, keep: &BTreeSet<String>) -> Fresh {
        let c = &self.sealed.contract;
        let bt = self.env.sections.backtest.clone().unwrap_or_default();
        let spec = SpecSource::Strategy(name.to_string());
        let r = match resolve(&bt, &spec) {
            Ok(r) => r,
            Err(e) => {
                return Fresh::Failed {
                    stage: "backtest",
                    error: self.clean(e),
                }
            }
        };
        match stale_detail(
            self.env.store.as_ref(),
            &r,
            c.freshness.max_lag_bars,
            self.cutoff_ms,
        )
        .await
        {
            Err(e) => {
                return Fresh::Failed {
                    stage: "freshness",
                    error: self.clean(e),
                }
            }
            Ok(Some(detail)) => return Fresh::Stale(detail),
            Ok(None) => {}
        }
        let env = BacktestEnv {
            store: Arc::clone(&self.env.store),
            sections: Arc::clone(&self.env.sections),
            backtests_dir: backtests_dir(&self.env.state_dir),
            now_ms: self.env.clock.now_ms(),
        };
        let job = BacktestJob {
            spec,
            from_ms: c.start_ms(),
            to_ms: Some(self.cutoff_ms),
            split: None,
            data_through_ms: Some(self.cutoff_ms),
        };
        let written = async {
            let mut p = prepare(&env, job).await?;
            p.keep_cited.extend(keep.iter().cloned());
            let mut run = evaluate(&p, Vec::new())?;
            write_run_dir(&p, &mut run)
        }
        .await;
        let dir = match written {
            Ok(dir) => dir,
            Err(e) => {
                return Fresh::Failed {
                    stage: "backtest",
                    error: self.clean(e),
                }
            }
        };
        match std::fs::read_to_string(dir.join(REPORT_JSON)) {
            Ok(text) => Fresh::Ran { text },
            Err(e) => Fresh::Failed {
                stage: "evaluate",
                error: format!("report.json does not read: {e}"),
            },
        }
    }

    /// Step 8: the manifest entry and the ranker's input of `fresh`.
    fn input(&self, state: &str, name: &str, fresh: Fresh) -> (StrategyEntry, RunInput) {
        let failed = |stage: &str, error: String| {
            (
                StrategyEntry {
                    status: StrategyStatus::Failed,
                    run: None,
                    report_sha256: None,
                    stage: Some(stage.to_string()),
                    error: Some(error.clone()),
                },
                RunInput::Failed {
                    strategy: name.to_string(),
                    stage: stage.to_string(),
                    error,
                },
            )
        };
        match fresh {
            Fresh::Failed { stage, error } => failed(stage, error),
            Fresh::Stale(detail) => (
                StrategyEntry {
                    status: StrategyStatus::Stale,
                    run: None,
                    report_sha256: None,
                    stage: None,
                    error: Some(detail.clone()),
                },
                RunInput::Stale {
                    strategy: name.to_string(),
                    detail,
                },
            ),
            Fresh::Ran { text } => match serde_json::from_str::<BacktestReport>(&text) {
                Err(e) => failed("evaluate", format!("report.json does not parse: {e}")),
                Ok(report) => {
                    let sha = sha256_hex(&text);
                    let facts =
                        RunFacts::from_report(&report, state, &sha, &self.sealed.contract.arm);
                    let standing =
                        StrategyStanding::of(self.reg, &report.spec_sha256, self.cutoff_ms);
                    (
                        StrategyEntry {
                            status: StrategyStatus::Done,
                            run: Some(facts.run()),
                            report_sha256: Some(sha),
                            stage: None,
                            error: None,
                        },
                        RunInput::of(facts, standing),
                    )
                }
            },
        }
    }
}

/// Step 6 (module table): `None` when every instrument `r` reads has a bar
/// at its interval closing ≥ cutoff − `max_lag_bars` bars, else which ones
/// do not (ids in full).
async fn stale_detail(
    store: &dyn MarketDataStore,
    r: &Resolved,
    max_lag_bars: u32,
    cutoff_ms: i64,
) -> Result<Option<String>> {
    let iv = r.spec.interval;
    let need = cutoff_ms.saturating_sub(i64::from(max_lag_bars).saturating_mul(iv.ms()));
    let mut stale = Vec::new();
    for id in &r.instruments {
        let newest_close = store
            .coverage(Some(id))
            .await?
            .into_iter()
            .filter(|row| row.kind == "bars" && row.interval == Some(iv))
            .map(|row| row.last_ms.saturating_add(iv.ms()))
            .max();
        match newest_close {
            None => stale.push(format!("{id}: no {iv} bars stored")),
            Some(close) if close < need => {
                stale.push(format!("{id}: newest {iv} bar closes {}", fmt_time(close)))
            }
            Some(_) => {}
        }
    }
    Ok((!stale.is_empty()).then(|| {
        format!(
            "every {iv} bar must close by {} or later (the cutoff − {max_lag_bars} bar(s)): {}",
            fmt_time(need),
            stale.join("; ")
        )
    }))
}

#[cfg(test)]
pub(crate) mod tests {
    use std::collections::BTreeMap;

    use super::store::MANIFEST_JSON;
    use super::*;
    use crate::adapters::outbound::market_data::SqliteMarketData;
    use crate::adapters::outbound::runtime_store::SqliteRuntimeStore;
    use crate::config::strategy_ranking::tests::setup;
    use crate::config::Config;
    use crate::domain::backtest::ranking::{RankedRow, RunVerdict};
    use crate::domain::backtest::testkit::{utc, H};
    use crate::domain::evidence::EvidenceClass;
    use crate::domain::lineage::ranking::tests::contract;
    use crate::domain::marketdata::{Bar, FundingPoint, Interval};
    use crate::domain::tz::Zone;
    use crate::ports::clock::SimClock;

    const AAPL: &str = "hyperliquid:xyz:AAPL";
    const TSLA: &str = "hyperliquid:xyz:TSLA";
    /// The lineage fixture's sealed contract (`ranked`: rule_w, rule_w_top4).
    const ID: &str = "rank.fixture.v1";
    /// The tests' now: every date through 2026-10-11 is past its cutoff.
    const NOW: &str = "2026-10-12 12:00";
    const FULL: &str = "2026-10-13 00:00";

    fn day(s: &str) -> NaiveDate {
        NaiveDate::parse_from_str(s, "%Y-%m-%d").unwrap()
    }

    fn bytes(p: &Path) -> Vec<u8> {
        std::fs::read(p).unwrap_or_else(|e| panic!("{}: {e}", p.display()))
    }

    /// Hourly bars + hourly funding (~1 % APR, stamped 37 ms late) of AAPL
    /// and TSLA from 2026-02-23 up to each one's end (exclusive): a ±20 bps
    /// wave, a Sunday 12:00–24:00 UTC jump (+200 bps AAPL, −150 bps TSLA).
    /// Also the `strategy_ranking` tool's tests (`tools/xlab/rank.rs`).
    pub(crate) async fn seed(
        state_dir: &Path,
        ends: [(&str, &str); 2],
    ) -> Arc<dyn MarketDataStore> {
        let store = SqliteMarketData::open(state_dir).unwrap();
        let t0 = utc("2026-02-23 00:00");
        for (k, (id, end)) in ends.into_iter().enumerate() {
            let kf = k as f64;
            let hours = ((utc(end) - t0) / H) as usize;
            let (mut bars, mut funding) = (Vec::new(), Vec::new());
            for h in 0..hours {
                let t = t0 + h as i64 * H;
                let wave = ((h as f64) / (5.0 + kf) + kf).sin() * 0.002;
                let sunday = chrono::DateTime::from_timestamp_millis(t)
                    .unwrap()
                    .weekday()
                    == chrono::Weekday::Sun;
                let jump = match (sunday && (12..24).contains(&(h % 24)), k) {
                    (true, 0) => 0.02,
                    (true, _) => -0.015,
                    _ => 0.0,
                };
                let c = (100.0 + 10.0 * kf) * (wave + jump).exp();
                bars.push(Bar {
                    t_open_ms: t,
                    o: c,
                    h: c * 1.001,
                    l: c * 0.999,
                    c,
                    v: 10.0 + kf,
                    n: Some(5),
                });
                funding.push(FundingPoint {
                    t_ms: t + 37,
                    rate_1h: 1e-6,
                    premium: None,
                });
            }
            store
                .put_bars(id, Interval::H1, "test", &bars)
                .await
                .unwrap();
            store.put_funding(id, "test", &funding).await.unwrap();
        }
        Arc::new(store)
    }

    /// A ranked sandbox (`config::strategy_ranking::tests::CONFIG`) over a
    /// copy of the lineage fixture registry, a temp state dir `ranked`
    /// seeded through `ends`, a settable clock at [`NOW`].
    struct Fx {
        tmp: tempfile::TempDir,
        clock: Arc<SimClock>,
        runtime: Arc<SqliteRuntimeStore>,
        env: RankingEnv,
    }

    async fn fx_with(ends: [(&str, &str); 2]) -> Fx {
        fx_config(|t| t, ends).await
    }

    /// [`fx_with`] on `edit(CONFIG)`.
    async fn fx_config(edit: impl Fn(String) -> String, ends: [(&str, &str); 2]) -> Fx {
        let (tmp, path) = setup("ranked", edit);
        let cfg = Config::load(&path).unwrap_or_else(|e| panic!("{e:#}"));
        let sections = Arc::clone(&cfg.agents["architect"].sandbox);
        let state_dir = tmp.path().join("state/ranked");
        let store = seed(&state_dir, ends).await;
        let runtime = Arc::new(SqliteRuntimeStore::open(&state_dir).unwrap());
        let clock = Arc::new(SimClock::at(utc(NOW)));
        let env = RankingEnv {
            store,
            sections,
            runtime: runtime.clone(),
            clock: clock.clone(),
            state_dir,
        };
        Fx {
            tmp,
            clock,
            runtime,
            env,
        }
    }

    async fn fx() -> Fx {
        fx_with([(AAPL, FULL), (TSLA, FULL)]).await
    }

    impl Fx {
        fn registry(&self) -> PathBuf {
            self.tmp.path().join("registry")
        }

        fn backtests(&self) -> PathBuf {
            backtests_dir(&self.env.state_dir)
        }

        fn dir(&self, d: &str) -> PathBuf {
            date_dir(&self.env.state_dir, ID, day(d))
        }

        fn contract_dir(&self) -> PathBuf {
            contract_dir(&self.env.state_dir, ID)
        }

        fn latest(&self) -> PathBuf {
            self.contract_dir().join(LATEST_JSON)
        }

        async fn run(&self, d: &str) -> Result<RankOutcome> {
            self.run_as(d, "test:a").await
        }

        async fn run_as(&self, d: &str, holder: &str) -> Result<RankOutcome> {
            let req = RankRequest {
                contract: ID.into(),
                date: Some(day(d)),
                holder: holder.into(),
            };
            run_ranking(&self.env, req).await
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

        /// Rewrite `<registry>/<rel>` with `f`.
        fn edit(&self, rel: &str, f: impl Fn(&str) -> String) {
            let path = self.registry().join(rel);
            let text = std::fs::read_to_string(&path).unwrap();
            let new = f(&text);
            assert_ne!(new, text, "{rel}: nothing changed");
            std::fs::write(&path, new).unwrap();
        }
    }

    /// strategy → its run in the manifest.
    fn run_of(o: &RankOutcome, strategy: &str) -> String {
        o.manifest.strategies[strategy].run.clone().unwrap()
    }

    /// `strategy`'s ranked row, any cohort.
    fn row(o: &RankOutcome, strategy: &str) -> RankedRow {
        o.ranking
            .cohorts
            .iter()
            .flat_map(|c| &c.rows)
            .find(|r| r.strategy == strategy)
            .cloned()
            .unwrap_or_else(|| panic!("{strategy} not ranked:\n{}", o.ranking.render_compact()))
    }

    #[tokio::test]
    async fn a_published_date_reruns_as_a_noop() {
        let f = fx().await;
        let first = f.run("2026-10-09").await.unwrap();
        let r = &first.ranking;
        assert_eq!(r.status, RankingStatus::Complete, "{}", r.render_compact());
        assert!(first.published && first.latest_replaced);
        assert_eq!(first.manifest.status, ManifestStatus::Complete);
        assert_eq!(r.cutoff_ms, Zone::NewYork.at(day("2026-10-09"), 0, 0));
        // rule_w reads AAPL + TSLA, its top-4 cut AAPL only: two cohorts
        // (`instruments_sha256`), one row each; run locators whole.
        assert_eq!(r.cohorts.len(), 2);
        let ranked: BTreeMap<&str, &str> = r
            .cohorts
            .iter()
            .flat_map(|c| &c.rows)
            .map(|x| (x.strategy.as_str(), x.run.as_str()))
            .collect();
        assert_eq!(
            ranked,
            BTreeMap::from([
                ("rule_w", "run:ranked/20261012T120000Z-rule_w"),
                ("rule_w_top4", "run:ranked/20261012T120000Z-rule_w_top4"),
            ])
        );
        assert_eq!(run_of(&first, "rule_w"), ranked["rule_w"]);
        let dir = f.dir("2026-10-09");
        let latest = f.latest();
        let files: Vec<PathBuf> = [MANIFEST_JSON, RANKING_JSON, RANKING_MD]
            .iter()
            .map(|n| dir.join(n))
            .chain([latest.clone(), f.contract_dir().join(LATEST_MD)])
            .collect();
        let before: Vec<Vec<u8>> = files.iter().map(|p| bytes(p)).collect();
        assert_eq!(bytes(&latest), bytes(&dir.join(RANKING_JSON)));
        let runs = f.runs();
        assert_eq!(runs.len(), 2);

        f.clock.advance(H);
        let again = f.run("2026-10-09").await.unwrap();
        assert!(!again.published);
        assert_eq!(again.ranking, first.ranking);
        assert_eq!(again.manifest, first.manifest);
        let after: Vec<Vec<u8>> = files.iter().map(|p| bytes(p)).collect();
        assert!(before == after, "a published date is never rewritten");
        assert_eq!(f.runs(), runs, "nothing ran");
    }

    #[tokio::test]
    async fn a_restart_resumes_without_rerunning_done_strategies() {
        let f = fx().await;
        let first = f.run("2026-10-09").await.unwrap();
        let dir = f.dir("2026-10-09");
        // Stopped after rule_w (shutdown grace, a crash): RUNNING, no
        // rule_w_top4 entry, nothing published yet.
        let mut m = read_manifest(&dir).unwrap().unwrap();
        m.status = ManifestStatus::Running;
        m.strategies.remove("rule_w_top4");
        m.finished_at_ms = None;
        write_manifest(&dir, &m).unwrap();
        for n in [RANKING_JSON, RANKING_MD] {
            std::fs::remove_file(dir.join(n)).unwrap();
        }
        f.clock.advance(H);
        let resumed = f.run_as("2026-10-09", "test:b").await.unwrap();
        assert!(resumed.published);
        assert_eq!(
            run_of(&resumed, "rule_w"),
            run_of(&first, "rule_w"),
            "reused"
        );
        assert_ne!(
            run_of(&resumed, "rule_w_top4"),
            run_of(&first, "rule_w_top4")
        );
        assert_eq!(f.runs().len(), 3);
        assert_eq!(resumed.manifest.holder, "test:b");
        assert_eq!(resumed.manifest.started_at_ms, first.manifest.started_at_ms);
        assert_eq!(
            resumed.ranking.content_sha256(),
            first.ranking.content_sha256()
        );

        // FAILED resumes too; a DONE run whose report.json changed since runs
        // again.
        let mut m = read_manifest(&dir).unwrap().unwrap();
        m.status = ManifestStatus::Failed;
        write_manifest(&dir, &m).unwrap();
        let report = f
            .backtests()
            .join(m.strategies["rule_w"].run_id().unwrap())
            .join(REPORT_JSON);
        let text = std::fs::read_to_string(&report).unwrap();
        std::fs::write(&report, text + "\n").unwrap();
        f.clock.advance(H);
        let again = f.run("2026-10-09").await.unwrap();
        assert_ne!(run_of(&again, "rule_w"), run_of(&resumed, "rule_w"));
        assert_eq!(
            run_of(&again, "rule_w_top4"),
            run_of(&resumed, "rule_w_top4")
        );
        assert_eq!(f.runs().len(), 4);
        assert_eq!(again.manifest.status, ManifestStatus::Complete);
    }

    #[tokio::test]
    async fn a_failed_strategy_keeps_the_prior_latest_and_writes_an_incomplete_manifest() {
        let mut f = fx().await;
        let d1 = f.run("2026-10-08").await.unwrap();
        assert_eq!(d1.status, RankingStatus::Complete);
        let latest = bytes(&f.latest());
        // rule_w (two names, a candidate each weekend) passes a 40-candidate
        // guard: its backtest fails; the top-4 cut (one name) does not.
        let mut s = (*f.env.sections).clone();
        s.backtest.as_mut().unwrap().max_candidates = 40;
        f.env.sections = Arc::new(s);
        let d2 = f.run("2026-10-09").await.unwrap();
        assert_eq!(d2.status, RankingStatus::Incomplete);
        assert!(d2.published && !d2.latest_replaced);
        assert_eq!(bytes(&f.latest()), latest, "the prior latest is kept");
        let m = read_manifest(&f.dir("2026-10-09")).unwrap().unwrap();
        assert_eq!(m.status, ManifestStatus::Incomplete);
        let e = &m.strategies["rule_w"];
        assert_eq!(
            (e.status, e.stage.as_deref(), e.run.as_deref()),
            (StrategyStatus::Failed, Some("backtest"), None)
        );
        let error = e.error.as_deref().unwrap();
        assert!(error.contains("max_candidates guard"), "{error}");
        assert_eq!(m.strategies["rule_w_top4"].status, StrategyStatus::Done);
        // The dated ranking surfaces the failure and ranks the rest.
        let r = read_ranking(&f.dir("2026-10-09").join(RANKING_JSON)).unwrap();
        let failed: Vec<(&str, &str)> = r
            .failed
            .iter()
            .map(|x| (x.strategy.as_str(), x.reason.as_str()))
            .collect();
        assert_eq!(failed, [("rule_w", "failed:backtest")]);
        assert_eq!(r.cohorts[0].rows.len(), 1);
        assert_eq!(r.cohorts[0].rows[0].strategy, "rule_w_top4");
        // Published as INCOMPLETE: a rerun returns it.
        let again = f.run("2026-10-09").await.unwrap();
        assert!(!again.published && again.status == RankingStatus::Incomplete);
    }

    #[tokio::test]
    async fn a_stale_instrument_marks_its_strategy_stale() {
        let f = fx_with([(AAPL, FULL), (TSLA, "2026-10-06 00:00")]).await;
        let out = f.run("2026-10-09").await.unwrap();
        assert_eq!(out.status, RankingStatus::Incomplete);
        assert!(!out.latest_replaced && !f.latest().exists());
        let e = &out.manifest.strategies["rule_w"];
        assert_eq!(e.status, StrategyStatus::Stale);
        let detail = e.error.as_deref().unwrap();
        assert!(
            detail.starts_with(
                "every 1h bar must close by 2026-10-09T02:00:00Z or later (the cutoff − 2 \
                 bar(s)): hyperliquid:xyz:TSLA: newest 1h bar closes 2026-10-06T00:00:00Z"
            ),
            "{detail}"
        );
        assert!(!detail.contains(AAPL), "{detail}");
        assert_eq!(
            (
                out.ranking.failed[0].strategy.as_str(),
                out.ranking.failed[0].reason.as_str()
            ),
            ("rule_w", "stale")
        );
        // rule_w never ran; its top-4 cut (AAPL only) did.
        assert_eq!(f.runs(), ["20261012T120000Z-rule_w_top4"]);
    }

    #[tokio::test]
    async fn latest_is_replaced_only_after_both_files_are_written() {
        let f = fx().await;
        f.run("2026-10-08").await.unwrap();
        let latest = bytes(&f.latest());
        let latest_md = f.contract_dir().join(LATEST_MD);
        let latest_md_bytes = bytes(&latest_md);
        // ranking.md cannot be written (a directory holds its name): the run
        // fails FAILED, latest untouched.
        let d2 = f.dir("2026-10-09");
        std::fs::create_dir_all(d2.join(RANKING_MD)).unwrap();
        let e = format!("{:#}", f.run("2026-10-09").await.unwrap_err());
        assert!(e.contains("ranking.md"), "{e}");
        assert_eq!(bytes(&f.latest()), latest);
        assert_eq!(bytes(&latest_md), latest_md_bytes);
        let m = read_manifest(&d2).unwrap().unwrap();
        assert_eq!(m.status, ManifestStatus::Failed);
        assert!(m.error.as_deref().unwrap().contains("ranking.md"));
        // latest.md cannot be replaced: neither is latest.json, the commit point.
        std::fs::remove_dir(d2.join(RANKING_MD)).unwrap();
        std::fs::remove_file(&latest_md).unwrap();
        std::fs::create_dir(&latest_md).unwrap();
        assert!(f.run("2026-10-09").await.is_err());
        assert_eq!(bytes(&f.latest()), latest);
        assert!(d2.join(RANKING_JSON).is_file() && d2.join(RANKING_MD).is_file());
        // Unblocked: the resumed run (no backtest again) replaces both.
        std::fs::remove_dir(&latest_md).unwrap();
        let out = f.run("2026-10-09").await.unwrap();
        assert!(out.published && out.latest_replaced);
        assert_eq!(bytes(&f.latest()), bytes(&d2.join(RANKING_JSON)));
        assert_eq!(bytes(&latest_md), bytes(&d2.join(RANKING_MD)));
        assert_eq!(f.runs().len(), 4, "the failed publishes reran no backtest");
        // An older date publishes its own files, never latest.
        let old = f.run("2026-10-07").await.unwrap();
        assert!(old.status == RankingStatus::Complete && !old.latest_replaced);
        assert_eq!(latest_date(&f.env.state_dir, ID), Some(day("2026-10-09")));
        // No temp file left.
        for dir in [d2, f.contract_dir()] {
            let temps: Vec<String> = std::fs::read_dir(&dir)
                .unwrap()
                .flatten()
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .filter(|n| n.starts_with('.'))
                .collect();
            assert!(temps.is_empty(), "{}: {temps:?}", dir.display());
        }
    }

    #[tokio::test]
    async fn the_daily_run_never_writes_a_holdout_read() {
        let f = fx().await;
        let out = f.run("2026-10-09").await.unwrap();
        assert!(!f.backtests().join("holdout-reads.jsonl").exists());
        let cutoff = out.ranking.cutoff_ms;
        for run in f.runs() {
            let text = std::fs::read_to_string(f.backtests().join(&run).join(REPORT_JSON)).unwrap();
            let r: BacktestReport = serde_json::from_str(&text).unwrap();
            assert!(
                r.split.is_none() && r.arms.values().all(|a| a.split.is_none()),
                "{run}"
            );
            assert_eq!(
                (r.from_ms, r.to_ms, r.data_through_ms),
                (utc("2026-03-01 00:00"), cutoff, Some(cutoff)),
                "{run}"
            );
        }
        assert!(
            out.ranking.ineligible.is_empty(),
            "{:?}",
            out.ranking.ineligible
        );
    }

    #[tokio::test]
    async fn a_rerun_on_the_same_data_has_the_same_content_sha256() {
        let f = fx().await;
        let first = f.run("2026-10-09").await.unwrap();
        std::fs::remove_dir_all(f.dir("2026-10-09")).unwrap();
        f.clock.advance(H);
        let second = f.run("2026-10-09").await.unwrap();
        assert!(second.published);
        for s in ["rule_w", "rule_w_top4"] {
            assert_ne!(run_of(&first, s), run_of(&second, s), "new run ids");
        }
        assert_ne!(
            first.ranking.generated_at_ms,
            second.ranking.generated_at_ms
        );
        assert_eq!(
            first.ranking.content_sha256(),
            second.ranking.content_sha256()
        );
        assert_eq!(
            first.manifest.content_sha256,
            second.manifest.content_sha256
        );
        assert_eq!(f.runs().len(), 4);
    }

    #[test]
    fn the_ranking_date_and_cutoff_follow_new_york_across_both_dst_switches() {
        let daily = contract(); // America/New_York, cutoff 00:00, every day
        let later = utc("2027-06-01 00:00");
        let cutoff = |c: &RankingContract, d: &str| ranking_date(c, Some(day(d)), later).unwrap().1;
        // Fall back on Sun 2026-11-01: that midnight is still EDT, the next EST.
        assert_eq!(cutoff(&daily, "2026-10-31"), utc("2026-10-31 04:00"));
        assert_eq!(cutoff(&daily, "2026-11-01"), utc("2026-11-01 04:00"));
        assert_eq!(cutoff(&daily, "2026-11-02"), utc("2026-11-02 05:00"));
        // Spring forward on Sun 2027-03-14.
        assert_eq!(cutoff(&daily, "2027-03-14"), utc("2027-03-14 05:00"));
        assert_eq!(cutoff(&daily, "2027-03-15"), utc("2027-03-15 04:00"));
        // No date: the newest local date whose cutoff has passed.
        let default = |c: &RankingContract, now: &str| ranking_date(c, None, utc(now)).unwrap();
        // 2026-11-01 23:30 EST.
        assert_eq!(
            default(&daily, "2026-11-02 04:30"),
            (day("2026-11-01"), utc("2026-11-01 04:00"))
        );
        assert_eq!(
            default(&daily, "2026-11-02 05:00"),
            (day("2026-11-02"), utc("2026-11-02 05:00"))
        );
        // 2027-03-14 23:59 EDT.
        assert_eq!(
            default(&daily, "2027-03-15 03:59"),
            (day("2027-03-14"), utc("2027-03-14 05:00"))
        );
        assert_eq!(
            default(&daily, "2027-03-15 04:00"),
            (day("2027-03-15"), utc("2027-03-15 04:00"))
        );
        // A cutoff still ahead; a cutoff not after `from`.
        let e = ranking_date(&daily, Some(day("2026-11-02")), utc("2026-11-02 04:59"))
            .unwrap_err()
            .to_string();
        assert!(
            e.starts_with(
                "cutoff_not_reached: 2026-11-02's cutoff is 2026-11-02 00:00 America/New_York \
                 (2026-11-02T05:00:00Z)"
            ),
            "{e}"
        );
        let e = ranking_date(&daily, Some(day("2026-02-28")), later)
            .unwrap_err()
            .to_string();
        assert!(e.starts_with("before_from: 2026-02-28's cutoff"), "{e}");
        // A weekend contract: Mondays, 12:00 New York.
        let mut weekend = contract();
        weekend.days = vec!["Mon".into()];
        weekend.cutoff = "12:00".into();
        assert_eq!(cutoff(&weekend, "2026-11-02"), utc("2026-11-02 17:00"));
        assert_eq!(cutoff(&weekend, "2027-03-15"), utc("2027-03-15 16:00"));
        assert_eq!(
            default(&weekend, "2026-11-04 12:00"),
            (day("2026-11-02"), utc("2026-11-02 17:00"))
        );
        // Monday 11:59 EST: the Monday before.
        assert_eq!(default(&weekend, "2026-11-02 16:59").0, day("2026-10-26"));
        let e = ranking_date(&weekend, Some(day("2026-11-01")), later)
            .unwrap_err()
            .to_string();
        assert!(
            e.starts_with("not_a_ranking_day: 2026-11-01 is a Sun; `rank.t` ranks on Mon"),
            "{e}"
        );
    }

    #[tokio::test]
    async fn a_held_lease_refuses_an_overlapping_run() {
        let f = fx().await;
        let feed = "feed:strategy_ranking_daily:1791460800000:0";
        let held = f
            .runtime
            .acquire_lease(&lease_resource(ID), feed, LEASE_TTL_MS, utc(NOW))
            .await
            .unwrap();
        assert!(held.granted);
        let e = f.run("2026-10-09").await.unwrap_err().to_string();
        assert!(
            e.starts_with(&format!(
                "ranking_busy: `ranking:rank.fixture.v1` is held by `{feed}` until"
            )),
            "{e}"
        );
        assert!(!f.dir("2026-10-09").exists() && f.runs().is_empty());
        // Expired (its holder died): the next run takes it, and releases it.
        f.clock.advance(LEASE_TTL_MS);
        f.run("2026-10-09").await.unwrap();
        let next = f
            .runtime
            .acquire_lease(&lease_resource(ID), "other", LEASE_TTL_MS, f.clock.now_ms())
            .await
            .unwrap();
        assert!(next.granted, "released at the end");
    }

    /// The runtime store, except that `holder`'s `steal_at`-th acquire finds
    /// the lease taken by `thief` — as when a step outlives the TTL and
    /// another process takes the date (`release`: and has released it again
    /// since, `clock` a second later); its `fail_at`-th acquire is a store
    /// error (0 = never).
    struct Thief {
        inner: Arc<SqliteRuntimeStore>,
        holder: &'static str,
        steal_at: usize,
        calls: std::sync::atomic::AtomicUsize,
        release: Option<Arc<SimClock>>,
        fail_at: usize,
    }

    impl Thief {
        fn at(inner: &Arc<SqliteRuntimeStore>, steal_at: usize) -> Thief {
            Thief {
                inner: Arc::clone(inner),
                holder: "test:a",
                steal_at,
                calls: Default::default(),
                release: None,
                fail_at: 0,
            }
        }
    }

    #[async_trait::async_trait]
    impl RuntimeStore for Thief {
        async fn acquire_lease(
            &self,
            resource: &str,
            holder: &str,
            ttl_ms: i64,
            now_ms: i64,
        ) -> Result<crate::domain::runtime::RunnerLease> {
            use std::sync::atomic::Ordering;
            let n = if holder == self.holder {
                self.calls.fetch_add(1, Ordering::SeqCst) + 1
            } else {
                0
            };
            if n > 0 && n == self.steal_at {
                let expired = now_ms + ttl_ms;
                let stolen = self
                    .inner
                    .acquire_lease(resource, "thief", ttl_ms, expired)
                    .await?;
                assert!(stolen.granted);
                if let Some(clock) = &self.release {
                    self.inner.release_lease(resource, "thief").await?;
                    clock.advance(1_000);
                    return self
                        .inner
                        .acquire_lease(resource, holder, ttl_ms, clock.now_ms())
                        .await;
                }
            }
            if n > 0 && n == self.fail_at {
                anyhow::bail!("database is locked");
            }
            self.inner
                .acquire_lease(resource, holder, ttl_ms, now_ms)
                .await
        }
        async fn release_lease(&self, resource: &str, holder: &str) -> Result<()> {
            self.inner.release_lease(resource, holder).await
        }
        async fn write_heartbeat(&self, hb: &crate::domain::runtime::Heartbeat) -> Result<()> {
            self.inner.write_heartbeat(hb).await
        }
    }

    /// A step that outlived the lease: the renewal before its manifest write
    /// fails `ranking_lease_lost`, and this run writes nothing more — no
    /// FAILED manifest, no entry — over the date the new holder now runs.
    #[tokio::test]
    async fn a_lost_lease_writes_nothing_more() {
        let f = fx().await;
        let mut env = f.env.clone();
        // Acquires of `test:a`: the run's, the renewal before rule_w, the
        // one after it (stolen).
        env.runtime = Arc::new(Thief::at(&f.runtime, 3));
        let req = RankRequest {
            contract: ID.into(),
            date: Some(day("2026-10-09")),
            holder: "test:a".into(),
        };
        let e = format!("{:#}", run_ranking(&env, req).await.unwrap_err());
        assert!(
            e.starts_with("ranking_lease_lost: `ranking:rank.fixture.v1` passed to `thief`"),
            "{e}"
        );
        let m = read_manifest(&f.dir("2026-10-09")).unwrap().unwrap();
        assert_eq!(m.status, ManifestStatus::Running, "{m:#?}");
        assert!(m.strategies.is_empty() && m.error.is_none(), "{m:#?}");
        assert!(!f.dir("2026-10-09").join(RANKING_JSON).exists());
        assert_eq!(f.runs().len(), 1, "rule_w ran before the loss");
        let other = f
            .runtime
            .acquire_lease(&lease_resource(ID), "other", LEASE_TTL_MS, f.clock.now_ms())
            .await
            .unwrap();
        assert_eq!(other.current_holder, "thief", "not released by test:a");
        // The thief gone, the next run resumes the manifest and publishes.
        f.clock.advance(3 * LEASE_TTL_MS);
        let out = f.run("2026-10-09").await.unwrap();
        assert_eq!(out.manifest.status, ManifestStatus::Complete);
    }

    /// Review P2: a run that stalled after its last strategy past the TTL
    /// never publishes over the new holder. Acquires of `test:a`: the run's
    /// (1), before and after each of the two strategies (2–5), then before
    /// the dated ranking (6), `latest` (7) and the terminal manifest (8).
    /// Stolen at 6: no ranking, no `latest`, no terminal manifest; at 7: the
    /// dated ranking only; at 8: no terminal manifest — each time the
    /// manifest stays the last one written under the lease (`RUNNING`, both
    /// strategies `DONE`) and the run fails `ranking_lease_lost`. A thief
    /// that has released the lease again is a loss too (it lapsed). A step
    /// failing at the publish writes `FAILED` only under a renewed lease.
    #[tokio::test]
    async fn a_lease_lost_before_publish_publishes_nothing() {
        let req = || RankRequest {
            contract: ID.into(),
            date: Some(day("2026-10-09")),
            holder: "test:a".into(),
        };
        for (steal_at, ranking, latest) in [(6, false, false), (7, true, false), (8, true, true)] {
            let f = fx().await;
            let mut env = f.env.clone();
            env.runtime = Arc::new(Thief::at(&f.runtime, steal_at));
            let e = format!("{:#}", run_ranking(&env, req()).await.unwrap_err());
            assert!(
                e.starts_with("ranking_lease_lost: `ranking:rank.fixture.v1` passed to `thief`"),
                "{steal_at}: {e}"
            );
            let dir = f.dir("2026-10-09");
            let m = read_manifest(&dir).unwrap().unwrap();
            assert_eq!(m.status, ManifestStatus::Running, "{steal_at}: {m:#?}");
            assert!(m.finished_at_ms.is_none() && m.content_sha256.is_none() && m.error.is_none());
            assert_eq!(m.strategies.len(), 2, "{steal_at}");
            assert!(m
                .strategies
                .values()
                .all(|e| e.status == StrategyStatus::Done));
            for name in [RANKING_JSON, RANKING_MD] {
                assert_eq!(dir.join(name).exists(), ranking, "{steal_at}: {name}");
            }
            for name in [LATEST_JSON, LATEST_MD] {
                assert_eq!(
                    f.contract_dir().join(name).exists(),
                    latest,
                    "{steal_at}: {name}"
                );
            }
            // The thief gone, the next run resumes the manifest and publishes.
            f.clock.advance(3 * LEASE_TTL_MS);
            let out = f.run("2026-10-09").await.unwrap();
            assert_eq!(out.manifest.status, ManifestStatus::Complete, "{steal_at}");
            assert!(f.latest().exists());
        }

        // The thief took the date and released it again: lapsed, lost.
        let f = fx().await;
        let mut env = f.env.clone();
        env.runtime = Arc::new(Thief {
            release: Some(f.clock.clone()),
            ..Thief::at(&f.runtime, 6)
        });
        let e = format!("{:#}", run_ranking(&env, req()).await.unwrap_err());
        assert!(
            e.starts_with(
                "ranking_lease_lost: `ranking:rank.fixture.v1` lapsed and another holder had it"
            ),
            "{e}"
        );
        assert!(!f.dir("2026-10-09").join(RANKING_JSON).exists() && !f.latest().exists());

        // A store error at the publish's renewal fails the run; the FAILED
        // manifest only after its own renewal: renewed ⇒ written …
        let f = fx().await;
        let mut env = f.env.clone();
        env.runtime = Arc::new(Thief {
            fail_at: 6,
            ..Thief::at(&f.runtime, 0)
        });
        let e = format!("{:#}", run_ranking(&env, req()).await.unwrap_err());
        assert!(e.contains("database is locked"), "{e}");
        let m = read_manifest(&f.dir("2026-10-09")).unwrap().unwrap();
        assert_eq!(m.status, ManifestStatus::Failed, "{m:#?}");
        assert!(!f.dir("2026-10-09").join(RANKING_JSON).exists());
        // … lost meanwhile ⇒ none, and the loss is the answer.
        let f = fx().await;
        let mut env = f.env.clone();
        env.runtime = Arc::new(Thief {
            fail_at: 6,
            ..Thief::at(&f.runtime, 7)
        });
        let e = format!("{:#}", run_ranking(&env, req()).await.unwrap_err());
        assert!(e.starts_with("ranking_lease_lost: "), "{e}");
        let m = read_manifest(&f.dir("2026-10-09")).unwrap().unwrap();
        assert_eq!(m.status, ManifestStatus::Running, "{m:#?}");
        assert!(m.error.is_none());
    }

    #[tokio::test]
    async fn an_unsealed_or_changed_contract_is_refused() {
        let f = fx().await;
        let rel = "rankings/rank.fixture.v1.toml";
        f.edit(rel, |t| t.replace("min_trades = 20", "min_trades = 21"));
        let e = f.run("2026-10-09").await.unwrap_err().to_string();
        assert!(
            e.starts_with(
                "contract_changed: `rank.fixture.v1` was sealed as \
                 4047867b8b8ddc928fbc0cffdfb333f331e02b9dc5608c734c998b1a863ec040 at \
                 2026-10-08T17:03:53Z; the file now hashes "
            ),
            "{e}"
        );
        // Unsealed: no [[sealed]] row for it.
        f.edit(rel, |t| t.replace("min_trades = 21", "min_trades = 20"));
        f.edit("locks.toml", |t| {
            let cut = t
                .find("[[sealed]]\nrecord = \"ranking:rank.fixture.v1\"")
                .unwrap();
            t[..cut].to_string()
        });
        let e = f.run("2026-10-09").await.unwrap_err().to_string();
        assert!(
            e.starts_with(
                "contract_unsealed: `rank.fixture.v1` has no [[sealed]] row in locks.toml"
            ),
            "{e}"
        );
        // Not listed in [strategy_ranking].
        let req = RankRequest {
            contract: "rank.other".into(),
            date: Some(day("2026-10-09")),
            holder: "test:a".into(),
        };
        let e = run_ranking(&f.env, req).await.unwrap_err().to_string();
        assert!(
            e.starts_with("contract_not_listed: `rank.other` is not in [strategy_ranking]"),
            "{e}"
        );
        assert!(
            !f.dir("2026-10-09").exists() && f.runs().is_empty(),
            "nothing written"
        );
    }

    /// UNCOVERED 3 (critic): a forward grade lifts a strategy's evidence
    /// tier in the rankings published after it; a published date's files
    /// are never rewritten.
    #[tokio::test]
    async fn a_forward_grade_changes_only_later_dates() {
        // The top cut reads both names (top_n = 1): one cohort with rule_w.
        let f = fx_config(
            |t| {
                t.replace(
                    "universe = [\"hyperliquid:xyz:AAPL\"]\n",
                    "universe = [\"hyperliquid:xyz:AAPL\", \"hyperliquid:xyz:TSLA\"]\ntop_n = 1\n",
                )
            },
            [(AAPL, FULL), (TSLA, FULL)],
        )
        .await;
        // Variant rule_w.top4 (ACTIVE; a PENDING forward) carries this
        // sandbox's top cut.
        let bt = f.env.sections.backtest.clone().unwrap();
        let spec = SpecSource::Strategy("rule_w_top4".into());
        let top4 = resolve(&bt, &spec).unwrap().spec_sha256;
        f.edit("variants/rule_w.top4.toml", |t| {
            t.replace(
                "f4dd0eb2fac4ed4cb8c9b045367fdf83ad2f84228227304f46f1f99a26ff395f",
                &top4,
            )
        });
        let top = |o: &RankOutcome| row(o, "rule_w_top4");
        let d1 = f.run("2026-10-08").await.unwrap();
        assert_eq!(
            d1.ranking.cohorts.len(),
            1,
            "{}",
            d1.ranking.render_compact()
        );
        assert_eq!(
            (top(&d1).evidence_tier, top(&d1).verdict, top(&d1).variants),
            (
                EvidenceClass::None,
                RunVerdict::Active,
                vec!["rule_w.top4".to_string()]
            )
        );
        // rule_w carries no variant here: UNREGISTERED (1) < ACTIVE (2).
        assert_eq!(&top(&d1).rating[..2], &[0, 2]);
        assert_eq!(&row(&d1, "rule_w").rating[..2], &[0, 1]);
        let dir1 = f.dir("2026-10-08");
        let files = || [MANIFEST_JSON, RANKING_JSON, RANKING_MD].map(|n| bytes(&dir1.join(n)));
        let before = files();
        // The grade: the forward PASSes, decided Thu 2026-10-08 14:00Z —
        // after 2026-10-08's cutoff (04:00Z), before 2026-10-09's.
        f.edit("experiments/rule_w.forward.toml", |t| {
            t.replace("value = \"PENDING\"", "value = \"PASS\"")
                .replace(
                    "decided_at = \"UNKNOWN\"",
                    "decided_at = \"2026-10-08T14:00:00Z\"",
                )
        });
        f.clock.advance(H);
        let d2 = f.run("2026-10-09").await.unwrap();
        assert_eq!(top(&d2).evidence_tier, EvidenceClass::ForwardPaper);
        assert_eq!(&top(&d2).rating[..2], &[3, 2]);
        let order: Vec<&str> = d2.ranking.cohorts[0]
            .rows
            .iter()
            .map(|r| r.strategy.as_str())
            .collect();
        assert_eq!(order, ["rule_w", "rule_w_top4"], "the stronger from now on");
        // The published date stays as it was: its files, and a rerun of it.
        assert!(files() == before, "2026-10-08 rewritten");
        let again = f.run("2026-10-08").await.unwrap();
        assert!(!again.published);
        assert_eq!(top(&again).evidence_tier, EvidenceClass::None);
        // Ranked anew (its dir deleted, as an INCOMPLETE date is rerun), the
        // earlier date still reads the registry as of its own cutoff: the
        // grade decided after it lifts nothing, the content is the same.
        std::fs::remove_dir_all(&dir1).unwrap();
        f.clock.advance(H);
        let rerun = f.run("2026-10-08").await.unwrap();
        assert!(rerun.published);
        assert_eq!(top(&rerun).evidence_tier, EvidenceClass::None);
        assert_eq!(rerun.ranking.content_sha256(), d1.ranking.content_sha256());
    }
}
