//! Forecast log (roadmap O4 "assumptions frozen before later outcomes are
//! observed"; PRD § 8 step 10): what each proposal predicts, frozen with the
//! week's portfolio, chained by hash, resolved only by evidence that came
//! later, scored by calibration. Pure; the files
//! (`<state>/soe/cycles/<cycle>/forecast.json`, `forecast-log.jsonl`) are the
//! application's.
//!
//! | [`ForecastItem`] (in a proposal) | Value |
//! |---|---|
//! | `observable` | [`Observable`]: `EVIDENCE_APPEARS` `event_key` (a record of that event becomes knowable) · `ASSUMPTION_WITHIN` `field`, `low`, `high` (an input's realised value falls inside; decimal text in the field's unit) · `OPERATOR_RESOLVES` `question` |
//! | `probability` | bps that it resolves YES by `resolve_by` |
//! | `resolve_by` | a known time surely after the decision |
//!
//! | `soe.forecast/1` | Value |
//! |---|---|
//! | header | `schema`, `id`, `version` ≥ 1 |
//! | `cycle_id`, `frozen_at` | the cycle · when it froze (known) |
//! | `portfolio_sha256`, `inputs_sha256` | the portfolio it froze with (canonical sha256 of its JSON) · that portfolio's `inputs_sha256` |
//! | `[[items]]` | `candidate`, `opportunity_version`, `item` ([`ForecastItem`]); sorted by candidate, then proposal order; `resolve_by` surely after `frozen_at` |
//!
//! | Rule | Value |
//! |---|---|
//! | Horizon | [`horizon_problems`]: `resolve_by` at most `max_weeks` after `frozen_at` (`horizon_too_long`; the cap is the caller's, from config — no built-in value) |
//! | Chain ([`LogLine`]) | `line_sha256` = canonical sha256 of every other field; `prev_sha256` = the previous line's ([`GENESIS`] first); [`verify_chain`] lists every edit, drop, reorder or repeated cycle (`chain_broken`) |
//! | Resolve ([`resolve`]) | evidence observed surely after `frozen_at` (`evidence_before_freeze`); a hit observed not after `resolve_by` (`hit_after_deadline`); a miss only once `resolve_by` has passed (`miss_before_deadline`); an item index the forecast has (`unknown_item`) |
//! | Calibration ([`calibration`]) | integer bps: equal-width bins over 0..=10 000, hits per bin, Brier = Σ (p − y)² / n in 10⁻⁸ (`brier_e8`, floored) |

// Consumers land with the cycle, `tengu soe grade|resolve|review` (O4).
#![allow(dead_code)]

use serde::{Deserialize, Serialize};
use serde_json::json;

use super::portfolio::WeeklyPortfolio;
use super::record::{validate, Problems, SoeRecord};
use super::value::{codes, Bps, Minor, SchemaTag, ValueError, BPS_FULL};
use crate::domain::canonical::canonical_sha256;
use crate::domain::lineage::value::{Time, TimeOrder};

/// What a forecast item watches (module table).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "SCREAMING_SNAKE_CASE", deny_unknown_fields)]
pub enum Observable {
    EvidenceAppears {
        event_key: String,
    },
    AssumptionWithin {
        field: String,
        low: String,
        high: String,
    },
    OperatorResolves {
        question: String,
    },
}

/// One prediction of a proposal (module table).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ForecastItem {
    pub observable: Observable,
    pub probability: Bps,
    pub resolve_by: Time,
}

impl ForecastItem {
    /// Rules under `at`; `decided_at` = the decision it is made at;
    /// `fields` = the inputs `ASSUMPTION_WITHIN` may name.
    pub fn problems(&self, at: &str, decided_at: &Time, fields: &[String], p: &mut Problems) {
        p.known(&format!("{at}.resolve_by"), &self.resolve_by);
        p.check(
            self.resolve_by.order(decided_at) == TimeOrder::After,
            codes::INVALID_TIME,
            &format!("{at}.resolve_by"),
            format_args!(
                "{} is not surely after the decision {decided_at}",
                self.resolve_by
            ),
        );
        match &self.observable {
            Observable::EvidenceAppears { event_key } => {
                p.text(&format!("{at}.observable.event_key"), event_key)
            }
            Observable::OperatorResolves { question } => {
                p.text(&format!("{at}.observable.question"), question)
            }
            Observable::AssumptionWithin { field, low, high } => {
                let f = format!("{at}.observable.field");
                p.check(
                    fields.iter().any(|x| x == field),
                    codes::UNKNOWN_FIELD,
                    &f,
                    format_args!("`{field}` is not an input of the candidate"),
                );
                match (low.parse::<Minor>(), high.parse::<Minor>()) {
                    (Ok(l), Ok(h)) => p.check(
                        l <= h,
                        codes::INVALID_RANGE,
                        &format!("{at}.observable"),
                        format_args!("low {l} > high {h}"),
                    ),
                    _ => p.push(
                        codes::INVALID_FIELD,
                        &format!("{at}.observable"),
                        format_args!("low `{low}` / high `{high}`: decimal text"),
                    ),
                }
            }
        }
    }
}

/// One frozen prediction: whose, at which version.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FrozenItem {
    pub candidate: String,
    pub opportunity_version: u32,
    pub item: ForecastItem,
}

/// `soe.forecast/1` (module table).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Forecast {
    pub schema: SchemaTag,
    pub id: String,
    pub version: u32,
    pub cycle_id: String,
    pub frozen_at: Time,
    pub portfolio_sha256: String,
    pub inputs_sha256: String,
    pub items: Vec<FrozenItem>,
}

impl SoeRecord for Forecast {
    const RECORD: &'static str = "forecast";

    fn schema(&self) -> &SchemaTag {
        &self.schema
    }

    fn id(&self) -> &str {
        &self.id
    }

    fn version(&self) -> u32 {
        self.version
    }

    fn problems(&self, p: &mut Problems) {
        p.id("cycle_id", &self.cycle_id);
        p.known("frozen_at", &self.frozen_at);
        p.sha256("portfolio_sha256", &self.portfolio_sha256);
        p.sha256("inputs_sha256", &self.inputs_sha256);
        for (i, x) in self.items.iter().enumerate() {
            p.id(&format!("items[{i}].candidate"), &x.candidate);
            p.check(
                x.item.resolve_by.order(&self.frozen_at) == TimeOrder::After,
                codes::INVALID_TIME,
                &format!("items[{i}].item.resolve_by"),
                format_args!(
                    "{} is not surely after frozen_at {}",
                    x.item.resolve_by, self.frozen_at
                ),
            );
        }
    }
}

/// The canonical sha256 of a record's JSON.
pub fn sha256_of<T: Serialize>(v: &T) -> Result<String, ValueError> {
    serde_json::to_value(v)
        .map(|j| canonical_sha256(&j))
        .map_err(|e| ValueError::new(codes::INVALID_RECORD, e.to_string()))
}

/// Freeze the predictions of `proposals` (`(candidate, version, items)`)
/// with `portfolio` at `frozen_at`; validated.
pub fn freeze(
    cycle_id: &str,
    frozen_at: Time,
    portfolio: &WeeklyPortfolio,
    proposals: &[(&str, u32, &[ForecastItem])],
) -> Result<Forecast, Vec<ValueError>> {
    let mut sorted: Vec<&(&str, u32, &[ForecastItem])> = proposals.iter().collect();
    sorted.sort_by(|a, b| (a.0, a.1).cmp(&(b.0, b.1)));
    let items = sorted
        .into_iter()
        .flat_map(|(candidate, version, items)| {
            items.iter().map(move |item| FrozenItem {
                candidate: candidate.to_string(),
                opportunity_version: *version,
                item: item.clone(),
            })
        })
        .collect();
    let f = Forecast {
        schema: SchemaTag::v1("forecast").map_err(|e| vec![e])?,
        id: cycle_id.to_string(),
        version: 1,
        cycle_id: cycle_id.to_string(),
        frozen_at,
        portfolio_sha256: sha256_of(portfolio).map_err(|e| vec![e])?,
        inputs_sha256: portfolio.inputs_sha256.clone(),
        items,
    };
    validate(&f)?;
    Ok(f)
}

const WEEK_MS: i64 = 7 * 86_400_000;

/// Module table: items resolving more than `max_weeks` after `frozen_at`.
pub fn horizon_problems(f: &Forecast, max_weeks: u32) -> Vec<ValueError> {
    let limit = f
        .frozen_at
        .earliest()
        .map(|t| t + i64::from(max_weeks) * WEEK_MS);
    f.items
        .iter()
        .enumerate()
        .filter(|(_, x)| match (x.item.resolve_by.latest(), limit) {
            (Some(by), Some(limit)) => by > limit,
            _ => true,
        })
        .map(|(i, x)| {
            ValueError::new(
                codes::HORIZON_TOO_LONG,
                format!(
                    "items[{i}] ({}): resolve_by {} is more than {max_weeks} weeks after {}",
                    x.candidate, x.item.resolve_by, f.frozen_at
                ),
            )
        })
        .collect()
}

// ---------------------------------------------------------------------------
// The chain
// ---------------------------------------------------------------------------

/// `prev_sha256` of the first line.
pub const GENESIS: &str = "0000000000000000000000000000000000000000000000000000000000000000";

/// One line of `forecast-log.jsonl` (module table).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LogLine {
    pub cycle_id: String,
    pub frozen_at_ms: i64,
    pub forecast_sha256: String,
    pub portfolio_sha256: String,
    pub prev_sha256: String,
    pub line_sha256: String,
}

fn line_hash(l: &LogLine) -> String {
    canonical_sha256(&json!({
        "cycle_id": l.cycle_id,
        "frozen_at_ms": l.frozen_at_ms,
        "forecast_sha256": l.forecast_sha256,
        "portfolio_sha256": l.portfolio_sha256,
        "prev_sha256": l.prev_sha256,
    }))
}

/// The line that appends `f` after `prev` (none: the first line).
pub fn log_line(prev: Option<&LogLine>, f: &Forecast) -> Result<LogLine, ValueError> {
    let frozen_at_ms = f
        .frozen_at
        .earliest()
        .ok_or_else(|| ValueError::new(codes::INVALID_TIME, "frozen_at unknown"))?;
    let mut l = LogLine {
        cycle_id: f.cycle_id.clone(),
        frozen_at_ms,
        forecast_sha256: sha256_of(f)?,
        portfolio_sha256: f.portfolio_sha256.clone(),
        prev_sha256: prev.map_or(GENESIS.to_string(), |p| p.line_sha256.clone()),
        line_sha256: String::new(),
    };
    l.line_sha256 = line_hash(&l);
    Ok(l)
}

/// Module table: every break in `lines`.
pub fn verify_chain(lines: &[LogLine]) -> Result<(), Vec<ValueError>> {
    let mut errors = Vec::new();
    let mut bad = |i: usize, why: String| {
        errors.push(ValueError::new(
            codes::CHAIN_BROKEN,
            format!("line {}: {why}", i + 1),
        ))
    };
    let mut seen = std::collections::BTreeSet::new();
    for (i, l) in lines.iter().enumerate() {
        let want = line_hash(l);
        if l.line_sha256 != want {
            bad(
                i,
                format!("line_sha256 {} ≠ {want} (edited)", l.line_sha256),
            );
        }
        let prev = if i == 0 {
            GENESIS
        } else {
            lines[i - 1].line_sha256.as_str()
        };
        if l.prev_sha256 != prev {
            bad(
                i,
                format!(
                    "prev_sha256 {} ≠ {prev} (a line dropped, inserted or reordered)",
                    l.prev_sha256
                ),
            );
        }
        if i > 0 && l.frozen_at_ms < lines[i - 1].frozen_at_ms {
            bad(i, "frozen before the line above".into());
        }
        if !seen.insert(l.cycle_id.as_str()) {
            bad(i, format!("cycle `{}` twice", l.cycle_id));
        }
    }
    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors)
    }
}

// ---------------------------------------------------------------------------
// Resolution and calibration
// ---------------------------------------------------------------------------

/// The operator's (or a later packet's) answer to one frozen item.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Resolution {
    /// Index into `Forecast.items`.
    pub item: usize,
    pub hit: bool,
    /// When the deciding evidence became knowable (a miss: when it was checked).
    pub observed_at: Time,
    /// Source record ids, in full.
    pub evidence: Vec<String>,
    pub resolved_by: String,
}

/// A resolved item: its probability and outcome.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Resolved {
    pub candidate: String,
    pub item: usize,
    pub probability: Bps,
    pub hit: bool,
}

/// Module table: `r` against `f`.
pub fn resolve(f: &Forecast, r: &Resolution) -> Result<Resolved, ValueError> {
    let x = f.items.get(r.item).ok_or_else(|| {
        ValueError::new(
            codes::UNKNOWN_ITEM,
            format!("item {} — the forecast has {}", r.item, f.items.len()),
        )
    })?;
    if r.observed_at.order(&f.frozen_at) != TimeOrder::After {
        return Err(ValueError::new(
            codes::EVIDENCE_BEFORE_FREEZE,
            format!(
                "observed {} is not surely after frozen_at {}: the forecast could have seen it",
                r.observed_at, f.frozen_at
            ),
        ));
    }
    let by = &x.item.resolve_by;
    if r.hit && r.observed_at.order(by) == TimeOrder::After {
        return Err(ValueError::new(
            codes::HIT_AFTER_DEADLINE,
            format!("a hit observed {} after resolve_by {by}", r.observed_at),
        ));
    }
    if !r.hit && by.order(&r.observed_at) != TimeOrder::NotAfter {
        return Err(ValueError::new(
            codes::MISS_BEFORE_DEADLINE,
            format!(
                "a miss checked {} before resolve_by {by} passed",
                r.observed_at
            ),
        ));
    }
    Ok(Resolved {
        candidate: x.candidate.clone(),
        item: r.item,
        probability: x.item.probability,
        hit: r.hit,
    })
}

/// One calibration bin, `[lo_bps, hi_bps)` (the last one closed).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CalBin {
    pub lo_bps: u32,
    pub hi_bps: u32,
    pub n: usize,
    pub hits: usize,
    /// Σ predicted bps (mean = this / n).
    pub predicted_bps_sum: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Calibration {
    pub n: usize,
    pub bins: Vec<CalBin>,
    /// Brier score × 10⁸, floored; none without a point.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub brier_e8: Option<u64>,
}

/// Module table: `points` = (probability, hit) per resolved item, `bins` ≥ 1.
pub fn calibration(points: &[(Bps, bool)], bins: u16) -> Calibration {
    let k = u32::from(bins.max(1));
    let full = u32::from(BPS_FULL);
    let mut out: Vec<CalBin> = (0..k)
        .map(|i| CalBin {
            lo_bps: i * full / k,
            hi_bps: (i + 1) * full / k,
            n: 0,
            hits: 0,
            predicted_bps_sum: 0,
        })
        .collect();
    let mut sq: u128 = 0;
    for &(p, hit) in points {
        let p = u32::from(p.get());
        let i = (p * k / full).min(k - 1) as usize;
        out[i].n += 1;
        out[i].hits += usize::from(hit);
        out[i].predicted_bps_sum += u64::from(p);
        let y = if hit { full } else { 0 };
        sq += u128::from(p.abs_diff(y)).pow(2);
    }
    let n = points.len();
    Calibration {
        n,
        bins: out,
        brier_e8: (n > 0).then(|| (sq / n as u128) as u64),
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub(crate) fn t(s: &str) -> Time {
        s.parse().unwrap()
    }

    pub(crate) fn item(event: &str, bps: i64, by: &str) -> ForecastItem {
        ForecastItem {
            observable: Observable::EvidenceAppears {
                event_key: event.into(),
            },
            probability: Bps::new(bps).unwrap(),
            resolve_by: t(by),
        }
    }

    const H: &str = "0000000000000000000000000000000000000000000000000000000000000003";

    fn forecast(cycle: &str, frozen: &str) -> Forecast {
        Forecast {
            schema: SchemaTag::v1("forecast").unwrap(),
            id: cycle.into(),
            version: 1,
            cycle_id: cycle.into(),
            frozen_at: t(frozen),
            portfolio_sha256: H.into(),
            inputs_sha256: H.into(),
            items: vec![
                FrozenItem {
                    candidate: "a".into(),
                    opportunity_version: 1,
                    item: item("ted:procedure:7", 6000, "2026-11-02"),
                },
                FrozenItem {
                    candidate: "a".into(),
                    opportunity_version: 1,
                    item: ForecastItem {
                        observable: Observable::OperatorResolves {
                            question: "does the pilot firm pay".into(),
                        },
                        probability: Bps::new(3000).unwrap(),
                        resolve_by: t("2026-12-21"),
                    },
                },
            ],
        }
    }

    #[test]
    fn items_rules_and_horizon() {
        let f = forecast("2026-W41", "2026-10-05T18:00:00Z");
        assert!(validate(&f).is_ok());
        // 2026-12-21 ends 11 weeks after the freeze: inside 12, outside 8.
        assert!(horizon_problems(&f, 12).is_empty());
        let e = horizon_problems(&f, 8);
        assert_eq!(e.len(), 1);
        assert_eq!(e[0].code, codes::HORIZON_TOO_LONG);
        // An item due at (or before) the freeze is refused.
        let mut early = f.clone();
        early.items[0].item.resolve_by = t("2026-10-05");
        assert_eq!(validate(&early).unwrap_err()[0].code, codes::INVALID_TIME);
        // In a proposal: the field must be an input, low ≤ high, decimal.
        let fields = vec!["economics.revenue.price_per_month".to_string()];
        let mut p = Problems::default();
        let at = t("2026-10-05T12:00:00Z");
        for (field, low, high) in [
            ("economics.revenue.price_per_month", "400.00", "650.00"),
            ("economics.revenue.price", "400.00", "650.00"),
            ("economics.revenue.price_per_month", "650.00", "400.00"),
            ("economics.revenue.price_per_month", "lots", "650.00"),
        ] {
            ForecastItem {
                observable: Observable::AssumptionWithin {
                    field: field.into(),
                    low: low.into(),
                    high: high.into(),
                },
                probability: Bps::new(5000).unwrap(),
                resolve_by: t("2026-11-01"),
            }
            .problems("forecast[0]", &at, &fields, &mut p);
        }
        let got: Vec<&str> = p
            .into_result()
            .unwrap_err()
            .iter()
            .map(|e| e.code)
            .collect();
        assert_eq!(
            got,
            [
                codes::UNKNOWN_FIELD,
                codes::INVALID_RANGE,
                codes::INVALID_FIELD
            ]
        );
    }

    #[test]
    fn chain_detects_edit() {
        let f1 = forecast("2026-W41", "2026-10-05T18:00:00Z");
        let f2 = forecast("2026-W42", "2026-10-12T18:00:00Z");
        let f3 = forecast("2026-W43", "2026-10-19T18:00:00Z");
        let l1 = log_line(None, &f1).unwrap();
        let l2 = log_line(Some(&l1), &f2).unwrap();
        let l3 = log_line(Some(&l2), &f3).unwrap();
        assert_eq!(l1.prev_sha256, GENESIS);
        assert_eq!(l1.forecast_sha256, sha256_of(&f1).unwrap());
        let chain = vec![l1.clone(), l2.clone(), l3.clone()];
        assert!(verify_chain(&chain).is_ok());
        // Rebuilt from the same forecasts: the same bytes.
        assert_eq!(log_line(None, &f1).unwrap(), l1);

        // An edited field: its own hash no longer matches.
        let mut edited = chain.clone();
        edited[1].portfolio_sha256 = GENESIS.into();
        let e = verify_chain(&edited).unwrap_err();
        assert_eq!(e.len(), 1);
        assert!(e[0].message.starts_with("line 2: line_sha256"), "{e:?}");
        // Rehashed to hide the edit: the next line's link breaks.
        edited[1].line_sha256 = line_hash(&edited[1]);
        let e = verify_chain(&edited).unwrap_err();
        assert!(e[0].message.starts_with("line 3: prev_sha256"), "{e:?}");
        // A dropped line and a reordered pair break the links too.
        assert!(verify_chain(&[l1.clone(), l3.clone()]).is_err());
        let e = verify_chain(&[l2.clone(), l1.clone()]).unwrap_err();
        assert!(e.iter().all(|x| x.code == codes::CHAIN_BROKEN));
        // A forecast edited after it froze no longer matches its line.
        let mut later = f2.clone();
        later.items[0].item.probability = Bps::new(9000).unwrap();
        assert_ne!(sha256_of(&later).unwrap(), l2.forecast_sha256);
    }

    #[test]
    fn resolve_refuses_evidence_before_freeze() {
        let f = forecast("2026-W41", "2026-10-05T18:00:00Z");
        let r = |hit: bool, observed: &str| Resolution {
            item: 0,
            hit,
            observed_at: t(observed),
            evidence: vec![],
            resolved_by: "operator".into(),
        };
        // Evidence from before (or the day of) the freeze resolves nothing.
        for before in ["2026-10-05T17:59:59Z", "2026-10-05T18:00:00Z", "2026-10-05"] {
            assert_eq!(
                resolve(&f, &r(true, before)).unwrap_err().code,
                codes::EVIDENCE_BEFORE_FREEZE,
                "{before}"
            );
        }
        let ok = resolve(&f, &r(true, "2026-10-20T09:00:00Z")).unwrap();
        assert_eq!((ok.candidate.as_str(), ok.hit), ("a", true));
        assert_eq!(ok.probability, Bps::new(6000).unwrap());
        // A hit after the deadline, a miss before it, an unknown item.
        assert_eq!(
            resolve(&f, &r(true, "2026-11-03T00:00:00Z"))
                .unwrap_err()
                .code,
            codes::HIT_AFTER_DEADLINE
        );
        assert_eq!(
            resolve(&f, &r(false, "2026-11-02T12:00:00Z"))
                .unwrap_err()
                .code,
            codes::MISS_BEFORE_DEADLINE
        );
        assert!(resolve(&f, &r(false, "2026-11-03T00:00:00Z")).is_ok());
        let mut far = r(true, "2026-10-20T09:00:00Z");
        far.item = 9;
        assert_eq!(resolve(&f, &far).unwrap_err().code, codes::UNKNOWN_ITEM);
    }

    #[test]
    fn calibration_is_exact_in_bps() {
        let b = |v: i64| Bps::new(v).unwrap();
        let c = calibration(&[(b(8000), true), (b(8000), false), (b(1000), false)], 5);
        assert_eq!(c.n, 3);
        assert_eq!(c.bins.len(), 5);
        assert_eq!(
            (
                c.bins[4].lo_bps,
                c.bins[4].hi_bps,
                c.bins[4].n,
                c.bins[4].hits
            ),
            (8000, 10000, 2, 1)
        );
        assert_eq!(c.bins[0].n, 1);
        // (0.2² + 0.8² + 0.1²) / 3 = 0.23 exactly.
        assert_eq!(c.brier_e8, Some(23_000_000));
        // 100 % lands in the last bin; no point, no score.
        assert_eq!(calibration(&[(Bps::FULL, true)], 4).bins[3].n, 1);
        assert_eq!(calibration(&[], 4).brier_e8, None);
    }
}
