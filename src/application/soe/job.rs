//! The `soe_cycle` job (roadmap O4 cadence): what a `[feeds.<n>] kind =
//! "job"`, `job = "soe_cycle"` feed runs under `tengu run`
//! (`config/feeds.rs`; built by `bootstrap/runtime.rs::job_for`) — the
//! week's live cycle ([`run_cycle`]) over the sandbox's ports.
//!
//! | Rule | Value |
//! |---|---|
//! | Week | the ISO week of the slot's local date in the feed's `tz` (a `Mon 07:00` Paris tick names that Monday's week, whatever the UTC date) |
//! | Decision time | the slot time — the schedule's, not the clock's: a retry of the slot decides at the same instant |
//! | Already frozen | `cycles/<week>/` frozen, or the forecast log holds the week (`cycle_already_frozen`) ⇒ `Done`, nothing written |
//! | Profile | `<state root>/operator.toml`, loaded each run (`config::soe::load_profile_with_text`; synthetic refused in a sandbox); missing, invalid, in-repo, loose-mode or unsigned ⇒ `Failed` before anything is created |
//! | Inputs | the source store opened each run (no `sources.db` yet = an empty packet); `[soe]` agents, limits and token prices; the generation pin; no active candidate — a shadow cycle runs no experiment (O5 waits for Operator Review #2) |
//! | Failed | any other refusal or error of the cycle ⇒ `Failed` (`fatal`) with its message; the feed's at-tick window retries it for 15 min — a dir claimed and never frozen answers `cycle_unfinished` at once, so a retry never runs a stage twice |

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{anyhow, Result};
use async_trait::async_trait;
use chrono::Datelike;
use tracing::{info, warn};

use super::cycle::{run_cycle, CycleEnv, CycleParams, ProfileIn, Target};
use super::submit::GenerationPin;
use super::CYCLE_ALREADY_FROZEN;
use crate::config::soe::{load_profile_with_text, SoeConfig};
use crate::config::sources::SourcesConfig;
use crate::domain::observation::ErrorClass;
use crate::domain::soe::portfolio::IsoWeek;
use crate::domain::tz::Zone;
use crate::ports::clock::Clock;
use crate::ports::runtime::{JobOutcome, RuntimeJob};
use crate::ports::soe::{CycleStore, RunDir, RunStatus, StageRunner};
use crate::ports::source_store::SourceStore;

/// Opens the source store for one run; `None` = no `sources.db` yet.
pub(crate) type SourceOpener = Arc<dyn Fn() -> Result<Option<Arc<dyn SourceStore>>> + Send + Sync>;

/// One sandbox's weekly cycle (module table).
pub(crate) struct SoeCycleJob {
    pub store: Arc<dyn CycleStore>,
    pub sources: SourceOpener,
    pub registry: Arc<SourcesConfig>,
    /// `None` = no model stage: the cycle decides what is carried.
    pub runner: Option<Arc<dyn StageRunner>>,
    /// Latency measurement only (`ops.json`); the decision time is the slot.
    pub clock: Arc<dyn Clock>,
    pub profile_path: PathBuf,
    /// Accept a `synthetic = true` profile (tests only).
    pub allow_synthetic: bool,
    /// The feed's `tz`: which week a slot names.
    pub zone: Zone,
    pub generation: GenerationPin,
    pub soe: SoeConfig,
}

impl SoeCycleJob {
    /// The ISO week of `slot_ms`'s local date in the feed's zone.
    pub(crate) fn week_of(&self, slot_ms: i64) -> Result<IsoWeek> {
        let w = self.zone.local_date(slot_ms).iso_week();
        IsoWeek::new(w.year(), w.week()).map_err(|e| anyhow!("{e}"))
    }

    fn params(&self, week: IsoWeek, slot_ms: i64) -> CycleParams {
        CycleParams {
            target: Target::Cycle,
            week,
            decided_at_ms: slot_ms,
            generation: self.generation.clone(),
            architect: self.soe.architect.clone(),
            critic: self.soe.critic.clone(),
            max_proposals: self.soe.max_proposals,
            forecast_max_weeks: self.soe.forecast_max_weeks,
            active: BTreeMap::new(),
            token_prices: self.soe.token_prices,
        }
    }

    /// One run (module table): what was done, or why not.
    async fn cycle(&self, slot_ms: i64) -> Result<String> {
        let week = self.week_of(slot_ms)?;
        let dir = RunDir::Cycle(week.to_string());
        if self.store.status(&dir)? == RunStatus::Frozen {
            return Ok(format!("{dir} is frozen already: nothing to do"));
        }
        let (profile, text) = load_profile_with_text(&self.profile_path, self.allow_synthetic)
            .map_err(|e| anyhow!(e))?;
        let sources = (self.sources)()?;
        let env = CycleEnv {
            sources: sources.as_deref(),
            registry: &self.registry,
            store: &*self.store,
            runner: self.runner.as_deref(),
            clock: &*self.clock,
            profile: ProfileIn {
                record: &profile.record,
                sha256: &profile.sha256,
                text: &text,
            },
        };
        match run_cycle(&env, &self.params(week, slot_ms)).await {
            Ok(o) => Ok(format!(
                "{} frozen: {} ranked, {} held, {} rejected{}; manifest {}",
                o.dir,
                o.portfolio.ranked.len(),
                o.portfolio.held.len(),
                o.portfolio.rejected.len(),
                if o.portfolio.is_hold() {
                    " (HOLD week)"
                } else {
                    ""
                },
                o.manifest_sha256
            )),
            Err(e) => {
                let message = format!("{e:#}");
                if message.starts_with(CYCLE_ALREADY_FROZEN) {
                    Ok(message)
                } else {
                    Err(e)
                }
            }
        }
    }
}

#[async_trait]
impl RuntimeJob for SoeCycleJob {
    async fn run(&self, slot_ms: i64, run_id: &str) -> JobOutcome {
        match self.cycle(slot_ms).await {
            Ok(note) => {
                info!(job = "soe_cycle", %run_id, slot_ms, %note, "soe cycle");
                JobOutcome::Done { note }
            }
            Err(e) => {
                let message = format!("{e:#}");
                warn!(job = "soe_cycle", %run_id, slot_ms, error = %message, "soe cycle failed");
                JobOutcome::Failed {
                    class: ErrorClass::Fatal,
                    message,
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;
    use crate::application::soe::tests::{
        generation, registry, MemCycleStore, MemSourceStore, ScriptedRunner, CLOCK_MS,
    };
    use crate::ports::clock::SimClock;
    use crate::ports::soe::StateLog;

    fn utc(s: &str) -> i64 {
        chrono::DateTime::parse_from_rfc3339(s)
            .unwrap()
            .timestamp_millis()
    }

    fn fixture_profile() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/soe/profile.synthetic.toml")
    }

    fn job(store: &Arc<MemCycleStore>, profile_path: PathBuf) -> SoeCycleJob {
        let sources: Arc<dyn SourceStore> = Arc::new(MemSourceStore::default());
        let runner = ScriptedRunner::new(Arc::clone(store), Vec::new(), Vec::new());
        SoeCycleJob {
            store: Arc::clone(store) as Arc<dyn CycleStore>,
            sources: Arc::new(move || Ok(Some(Arc::clone(&sources)))),
            registry: Arc::new(registry()),
            runner: Some(Arc::new(runner)),
            clock: Arc::new(SimClock::at(CLOCK_MS)),
            profile_path,
            allow_synthetic: true,
            zone: Zone::Paris,
            generation: generation(),
            soe: toml::from_str(
                "architect = \"soe-architect\"\ncritic = \"soe-critic\"\nmax_proposals = 12\nforecast_max_weeks = 12",
            )
            .unwrap(),
        }
    }

    fn failed(o: JobOutcome) -> String {
        match o {
            JobOutcome::Failed { class, message } => {
                assert_eq!(class, ErrorClass::Fatal);
                message
            }
            other => panic!("not failed: {other:?}"),
        }
    }

    /// One frozen cycle per week: the slot's Paris week, decided at the
    /// slot; a second fire that week does nothing; the next week runs.
    #[tokio::test]
    async fn job_runs_each_week_once() {
        let store = Arc::new(MemCycleStore::default());
        let j = job(&store, fixture_profile());
        let mon = utc("2026-10-05T05:00:00Z"); // Mon 07:00 CEST
        let JobOutcome::Done { note } = j.run(mon, "feed:soe_week:1").await else {
            panic!("the week's cycle runs");
        };
        assert!(
            note.starts_with("cycles/2026-W41 frozen: 0 ranked") && note.contains("HOLD week"),
            "{note}"
        );
        let w41 = RunDir::Cycle("2026-W41".into());
        assert_eq!(store.status(&w41).unwrap(), RunStatus::Frozen);
        let head: serde_json::Value =
            serde_json::from_slice(&store.dir(&w41)["head.json"]).unwrap();
        assert_eq!(head["decided_at_ms"], mon);
        assert_eq!(store.lines(StateLog::ForecastLog).unwrap().len(), 1);

        // Again that week (a retry, a restart, an hourly feed): a no-op.
        let before = store.snapshot();
        let JobOutcome::Done { note } = j.run(mon + 3_600_000, "feed:soe_week:2").await else {
            panic!("a frozen week is done");
        };
        assert_eq!(note, "cycles/2026-W41 is frozen already: nothing to do");
        assert_eq!(store.snapshot(), before);

        // The next Monday runs its own week; the first stays as it was.
        let next = utc("2026-10-12T05:00:00Z");
        assert!(matches!(
            j.run(next, "feed:soe_week:3").await,
            JobOutcome::Done { .. }
        ));
        assert_eq!(store.cycles().unwrap(), ["2026-W41", "2026-W42"]);
        assert_eq!(store.dir(&w41), before.dirs[&w41]);
        assert_eq!(store.lines(StateLog::ForecastLog).unwrap().len(), 2);
    }

    /// The week is the feed zone's: Sunday 23:30 UTC is Monday in Paris.
    #[test]
    fn week_follows_the_feed_zone() {
        let store = Arc::new(MemCycleStore::default());
        let mut j = job(&store, fixture_profile());
        let t = utc("2026-10-11T23:30:00Z");
        assert_eq!(j.week_of(t).unwrap().to_string(), "2026-W42");
        j.zone = Zone::Utc;
        assert_eq!(j.week_of(t).unwrap().to_string(), "2026-W41");
    }

    /// Refusals fail the run before anything is created: an unsigned
    /// profile, a synthetic one in a sandbox, a dir claimed and never frozen.
    #[tokio::test]
    async fn job_refusals_create_nothing() {
        let tmp = tempfile::TempDir::new().unwrap();
        let text = std::fs::read_to_string(fixture_profile()).unwrap();
        let unsigned = tmp.path().join("operator.toml");
        std::fs::write(
            &unsigned,
            text.replace("signed_by = \"fixture\"", "signed_by = \"UNSIGNED\"")
                .replace(
                    "signed_at = \"2026-10-01T09:00:00Z\"",
                    "signed_at = \"UNKNOWN\"",
                ),
        )
        .unwrap();
        let store = Arc::new(MemCycleStore::default());
        let mon = utc("2026-10-05T05:00:00Z");
        let e = failed(job(&store, unsigned).run(mon, "r").await);
        assert!(e.starts_with("operator_profile_unsigned: "), "{e}");
        let mut sandbox = job(&store, fixture_profile());
        sandbox.allow_synthetic = false;
        let e = failed(sandbox.run(mon, "r").await);
        assert!(e.starts_with("synthetic_profile_refused: "), "{e}");
        assert!(store.snapshot().dirs.is_empty());

        let w41 = RunDir::Cycle("2026-W41".into());
        store.claim(&w41).unwrap();
        let e = failed(job(&store, fixture_profile()).run(mon, "r").await);
        assert!(e.starts_with("cycle_unfinished: cycles/2026-W41"), "{e}");
        assert_eq!(store.status(&w41).unwrap(), RunStatus::Open);
    }
}
