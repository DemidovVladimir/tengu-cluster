//! Allocate (O3 stage "Allocate"; PRD § 7.2, § 8 step 7; roadmap O3): the one
//! portfolio builder (C4). The week's candidates — each through the O1
//! gates and economics (`rank::assess`), a Critic's challenges merged
//! (`challenge::apply`) — become a validated `WeeklyPortfolio` under the
//! signed profile's weekly owner hours and validation tranche. Pure and
//! deterministic: the same inputs give the same bytes. No `LAUNCH` action
//! exists; one or zero new tests is a normal week.
//!
//! | Gate verdict | Action |
//! |---|---|
//! | `REJECT` | `REPRICE { min_price }` when every rejection is `CONTRIBUTION_BELOW_TARGET` / `PAYBACK_ABOVE_MAX` and a higher price clears them ([`min_price`]); else `REJECT` |
//! | `HOLD`, a candidate with a `[deal]` and a `LEGAL_UNRESOLVED` / `DILIGENCE_OPEN` failure | `DILIGENCE { questions, max_next_tranche }` |
//! | `HOLD` only on `BELOW_TARGET_WITH_PATH` / `SINGLE_DEMAND_SIGNAL` (C5: a bounded path, one demand event) | `CHEAP_TEST` of its next stage while the budget has room, else `HOLD` |
//! | any other `HOLD` | `HOLD` |
//! | `PASS` | ranked by the profile's `rank_order` (`rank::rank`, ties by id); in rank order an active candidate gets `CONTINUE_ACTIVE`, a new one `CHEAP_TEST`, while the budget has room; else `HOLD` |
//!
//! | Rule | Value |
//! |---|---|
//! | Budget ([`Budget::of`]) | the profile's `weekly_owner_hours` and `max_validation_tranche`, for the whole week — no other parameter |
//! | Cost of a test | the experiment stage it runs ([`Track`]): a new candidate stage 0, an active one its `next_stage` (none left: free); cash in the profile currency, ceiled; no experiment or no rate ⇒ it never fits |
//! | Fill | passing candidates in rank order, then the testable holds in the same order (`rank::compare`); a test that does not fit holds and the next one may still fit |
//! | `REPRICE` | `min_price` = the least base price — RECURRING `price_per_month`, ONE_OFF `contract_value` — at which no gate rejects and the base contribution reaches the target (no `BELOW_TARGET_WITH_PATH`): a binary search on the O1 gates, then in the profile currency (ceiled); `ACQUISITION` / `REVENUE_SHARE` have no price ⇒ `REJECT` |
//! | `DILIGENCE` | `questions` = those failures as `<field>: <detail>`; `max_next_tranche` = the experiment's first cash stage, capped at `max_validation_tranche` (none, or no rate ⇒ 0.00); costs no budget — the operator approves any tranche |
//! | Novelty | never an input: no rank key, budget or action reads a proposal's `novelty` |
//! | Nothing ranked | `hold_rationale` = `rank::hold_rationale` (every candidate's failed gates stay on its row) |
//! | Rows · next information | held / rejected by id with every gate label · `rank::week_next_information` |
//! | `inputs_sha256` | canonical sha256 of each candidate's id, version, `inputs_sha256`, gate labels and track, by id (the Critic's blocks and the active set included) |
//!
//! [`decide_week`] runs the stages after the models: the packet's views
//! (`observe`), every proposal and challenge checked, challenges merged,
//! each candidate assessed and blocked, then [`allocate`].

// Consumers land with the cycle (application `run_cycle`) and `tengu soe`.
#![allow(dead_code)]

use std::collections::BTreeMap;

use serde::Serialize;
use serde_json::json;

use super::challenge::{apply, Applied, Challenge};
use super::economics::{fields, to_currency, ECONOMICS_VERSION};
use super::gates::{gates, CitedRecord, GateCode, GateVerdict};
use super::observe::{cited_views, EvidenceIndex};
use super::opportunity::{Input, InputMut, Opportunity, RevenueModel};
use super::portfolio::{Allocation, GatedRow, PortfolioAction, RankedRow, WeeklyPortfolio};
use super::profile::OperatorProfile;
use super::proposal::MechanismProposal;
use super::rank::{
    assess, compare, hold_rationale, rank, ranked_row, same_decision, week_next_information,
    Assessment, WeekHead,
};
use super::record::{validate, Problems, Verdict};
use super::value::{codes, Est, Flow, Minor, Money, SchemaTag, ValueError};
use crate::domain::canonical::canonical_sha256;
use crate::domain::lineage::value::Time;
use crate::domain::source::EvidencePacket;

/// Where a candidate stands (module table: cost of a test).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "SCREAMING_SNAKE_CASE")]
pub enum Track {
    New,
    /// A test is under way; `next_stage` indexes its experiment's stages.
    Active {
        next_stage: usize,
    },
}

/// One candidate of the week.
#[derive(Debug, Clone)]
pub struct Candidate<'a> {
    /// As challenged: the figures the gates read.
    pub opportunity: &'a Opportunity,
    /// `rank::assess` of it, the Critic's blocks applied.
    pub assessment: Assessment,
    pub track: Track,
}

/// The week's budget (module table).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct Budget {
    pub owner_hours: u32,
    pub cash: Minor,
}

impl Budget {
    pub fn of(p: &OperatorProfile) -> Budget {
        Budget {
            owner_hours: p.weekly_owner_hours,
            cash: p.max_validation_tranche,
        }
    }
}

/// Why a candidate got its action.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Note {
    pub id: String,
    pub action: &'static str,
    pub why: String,
}

/// The allocated week and a note per candidate (by id).
#[derive(Debug, Clone, PartialEq)]
pub struct Allocated {
    pub portfolio: WeeklyPortfolio,
    pub notes: Vec<Note>,
}

/// Rejections a higher price can clear.
const PRICE_FIXABLE: [GateCode; 2] = [GateCode::ContributionBelowTarget, GateCode::PaybackAboveMax];
/// Holds a cheap test of the candidate's own experiment can resolve (C5).
const TESTABLE: [GateCode; 2] = [GateCode::BelowTargetWithPath, GateCode::SingleDemandSignal];
/// Holds diligence answers.
const DILIGENCE: [GateCode; 2] = [GateCode::LegalUnresolved, GateCode::DiligenceOpen];

/// `opp` with the base of its price input at `p` (the range widened to
/// hold it).
fn with_base_price(opp: &Opportunity, field: &str, p: Minor) -> Opportunity {
    let mut o = opp.clone();
    if let Some(InputMut::Amount(a)) = o.economics.input_mut(field) {
        if let Est::Range { low, high, .. } = a.value {
            a.value = Est::Range {
                low: low.min(p),
                base: p,
                high: high.max(p),
            };
        }
    }
    o
}

/// Module table: the least base price (profile currency) that clears `v`'s
/// rejections; `None` when no price can.
pub fn min_price(
    opp: &Opportunity,
    v: &GateVerdict,
    cited: &[CitedRecord],
    profile: &OperatorProfile,
) -> Result<Option<Minor>, ValueError> {
    if v.verdict != Verdict::Reject
        || v.failures
            .iter()
            .any(|f| f.outcome == Verdict::Reject && !PRICE_FIXABLE.contains(&f.code))
    {
        return Ok(None);
    }
    let field = match opp.economics.revenue {
        RevenueModel::Recurring { .. } => fields::PRICE,
        RevenueModel::OneOff { .. } => fields::CONTRACT_VALUE,
        RevenueModel::Acquisition { .. } | RevenueModel::RevenueShare { .. } => return Ok(None),
    };
    let Some(Input::Amount(a)) = opp.economics.input(field) else {
        return Ok(None);
    };
    let Some(&base) = a.value.base() else {
        return Ok(None);
    };
    let clears = |p: Minor| -> Result<bool, ValueError> {
        let g = gates(&with_base_price(opp, field, p), cited, profile, v.as_of)?;
        Ok(g.verdict != Verdict::Reject
            && !g
                .failures
                .iter()
                .any(|f| f.code == GateCode::BelowTargetWithPath))
    };
    // `lo` never clears, `hi` does: double, then halve the gap.
    let mut lo = base;
    let mut hi = if base.0 > 0 { base } else { Minor(1) };
    loop {
        hi = match hi.checked_mul(2) {
            Ok(h) => h,
            Err(_) => return Ok(None),
        };
        if clears(hi)? {
            break;
        }
        lo = hi;
    }
    while hi.0 - lo.0 > 1 {
        let mid = Minor(lo.0 + (hi.0 - lo.0) / 2);
        if clears(mid)? {
            hi = mid;
        } else {
            lo = mid;
        }
    }
    to_currency(&opp.economics, profile.currency, hi, Flow::Outflow)
}

/// The stage `c` would run and its cost in the profile currency;
/// `Err(why)` when it cannot be priced.
fn stage_cost(
    c: &Candidate,
    p: &OperatorProfile,
) -> Result<Result<(String, Minor, u32), String>, ValueError> {
    let Some(x) = &c.opportunity.experiment else {
        return Ok(Err("no experiment to run".into()));
    };
    let i = match c.track {
        Track::New => 0,
        Track::Active { next_stage } => next_stage,
    };
    let Some(stage) = x.stages.get(i) else {
        return Ok(Ok(("(no stage left)".into(), Minor::ZERO, 0)));
    };
    Ok(
        match to_currency(
            &c.opportunity.economics,
            p.currency,
            stage.cash,
            Flow::Outflow,
        )? {
            Some(cash) => Ok((stage.name.clone(), cash, stage.owner_hours)),
            None => Err(format!("stage `{}`: no rate to {}", stage.name, p.currency)),
        },
    )
}

/// The week's spend so far against the budget.
struct Fill {
    budget: Budget,
    used: Allocation,
}

impl Fill {
    /// Takes `c`'s next stage when it fits: `Ok((stage, cash, hours))`, else
    /// `Err(why)`.
    fn take(
        &mut self,
        c: &Candidate,
        p: &OperatorProfile,
    ) -> Result<Result<(String, Minor, u32), String>, ValueError> {
        let (name, cash, hours) = match stage_cost(c, p)? {
            Ok(x) => x,
            Err(why) => return Ok(Err(why)),
        };
        let money = |m: Minor| Money::new(m, p.currency);
        let h = self.used.owner_hours.saturating_add(hours);
        let m = self.used.cash.checked_add(cash)?;
        if h > self.budget.owner_hours || m > self.budget.cash {
            return Ok(Err(format!(
                "over budget: stage `{name}` needs {hours} h + {}; {} h + {} left of {} h + {}",
                money(cash),
                self.budget.owner_hours - self.used.owner_hours,
                money(Minor(self.budget.cash.0 - self.used.cash.0)),
                self.budget.owner_hours,
                money(self.budget.cash)
            )));
        }
        self.used = Allocation {
            owner_hours: h,
            cash: m,
        };
        Ok(Ok((name, cash, hours)))
    }
}

fn only(v: &GateVerdict, set: &[GateCode]) -> bool {
    !v.failures.is_empty() && v.failures.iter().all(|f| set.contains(&f.code))
}

/// The `DILIGENCE` action of a held candidate with a deal (module table).
fn diligence(c: &Candidate, p: &OperatorProfile) -> Result<Option<PortfolioAction>, ValueError> {
    if c.opportunity.deal.is_none() {
        return Ok(None);
    }
    let questions: Vec<String> = c
        .assessment
        .verdict
        .failures
        .iter()
        .filter(|f| f.outcome == Verdict::Hold && DILIGENCE.contains(&f.code))
        .map(|f| format!("{}: {}", f.field.as_deref().unwrap_or("deal"), f.detail))
        .collect();
    if questions.is_empty() {
        return Ok(None);
    }
    let next = match c
        .opportunity
        .experiment
        .as_ref()
        .and_then(|x| x.first_spend())
    {
        Some((_, stage)) => to_currency(
            &c.opportunity.economics,
            p.currency,
            stage.cash,
            Flow::Outflow,
        )?
        .map_or(Minor::ZERO, |m| m.min(p.max_validation_tranche)),
        None => Minor::ZERO,
    };
    Ok(Some(PortfolioAction::Diligence {
        questions,
        max_next_tranche: next,
    }))
}

/// Module table: the identity of the week's inputs.
fn allocation_inputs_sha256(cands: &[Candidate]) -> String {
    let mut rows: Vec<serde_json::Value> = cands
        .iter()
        .map(|c| {
            json!({
                "id": c.assessment.id,
                "version": c.assessment.version,
                "inputs_sha256": c.assessment.inputs_sha256,
                "gates": c.assessment.verdict.labels(),
                "track": serde_json::to_value(c.track).unwrap_or_default(),
            })
        })
        .collect();
    rows.sort_by(|a, b| a["id"].as_str().cmp(&b["id"].as_str()));
    canonical_sha256(&json!(rows))
}

/// Module table: the allocated week of `cands` decided at `head.as_of`.
/// `cited` = the views the gates read (a reprice re-runs them). `Err` when an
/// assessment was made at another time or in another currency, an id
/// repeats, or the record breaks a `WeeklyPortfolio` rule.
pub fn allocate(
    head: WeekHead,
    cands: &[Candidate],
    profile: &OperatorProfile,
    cited: &[CitedRecord],
) -> Result<Allocated, Vec<ValueError>> {
    let all: Vec<Assessment> = cands.iter().map(|c| c.assessment.clone()).collect();
    let mut errors = same_decision(&head, &all);
    let mut p = Problems::default();
    p.unique("candidates", all.iter().map(|a| a.id.as_str()));
    if let Err(e) = p.into_result() {
        errors.extend(e);
    }
    if !errors.is_empty() {
        return Err(errors);
    }
    let one = |e: ValueError| vec![e];
    let order = &profile.rank_order;
    let mut fill = Fill {
        budget: Budget::of(profile),
        used: Allocation {
            owner_hours: 0,
            cash: Minor::ZERO,
        },
    };
    let mut notes: Vec<Note> = Vec::new();
    let note = |notes: &mut Vec<Note>, id: &str, a: &PortfolioAction, why: String| {
        notes.push(Note {
            id: id.to_string(),
            action: a.kind(),
            why,
        })
    };
    let by_id: BTreeMap<&str, &Candidate> = cands
        .iter()
        .map(|c| (c.assessment.id.as_str(), c))
        .collect();

    // Ranked: the passing candidates in the profile's order.
    let mut ranked: Vec<RankedRow> = Vec::new();
    for (i, a) in rank(&all, order).into_iter().enumerate() {
        let c = by_id[a.id.as_str()];
        let action = match fill.take(c, profile).map_err(one)? {
            Ok((stage, cash, hours)) => {
                let action = match c.track {
                    Track::Active { .. } => PortfolioAction::ContinueActive,
                    Track::New => PortfolioAction::CheapTest {
                        max_cash: cash,
                        max_hours: hours,
                    },
                };
                note(
                    &mut notes,
                    &a.id,
                    &action,
                    format!("rank {}: stage `{stage}`", i + 1),
                );
                action
            }
            Err(why) => {
                note(
                    &mut notes,
                    &a.id,
                    &PortfolioAction::Hold,
                    format!("rank {}: {why}", i + 1),
                );
                PortfolioAction::Hold
            }
        };
        ranked.push(ranked_row(i as u32 + 1, a, order, action));
    }

    // Held and rejected, by id; testable holds wait for the fill below.
    let mut gated: Vec<&Candidate> = cands.iter().filter(|c| !c.assessment.passes()).collect();
    gated.sort_by(|a, b| {
        (&a.assessment.id, a.assessment.version).cmp(&(&b.assessment.id, b.assessment.version))
    });
    let mut actions: BTreeMap<&str, PortfolioAction> = BTreeMap::new();
    let mut testable: Vec<&Candidate> = Vec::new();
    for c in &gated {
        let a = &c.assessment;
        let v = &a.verdict;
        let action = match v.verdict {
            Verdict::Reject => match min_price(c.opportunity, v, cited, profile).map_err(one)? {
                Some(min) => {
                    let why = format!(
                        "a base price of {} clears the contribution and payback gates",
                        Money::new(min, profile.currency)
                    );
                    let action = PortfolioAction::Reprice { min_price: min };
                    note(&mut notes, &a.id, &action, why);
                    action
                }
                None => {
                    note(
                        &mut notes,
                        &a.id,
                        &PortfolioAction::Reject,
                        v.labels().join(", "),
                    );
                    PortfolioAction::Reject
                }
            },
            _ => match diligence(c, profile).map_err(one)? {
                Some(action) => {
                    note(
                        &mut notes,
                        &a.id,
                        &action,
                        "verify before any tranche".into(),
                    );
                    action
                }
                None if only(v, &TESTABLE) => {
                    testable.push(*c);
                    continue;
                }
                None => {
                    note(
                        &mut notes,
                        &a.id,
                        &PortfolioAction::Hold,
                        v.labels().join(", "),
                    );
                    PortfolioAction::Hold
                }
            },
        };
        actions.insert(&a.id, action);
    }
    testable.sort_by(|a, b| compare(&a.assessment, &b.assessment, order));
    for c in testable {
        let id = c.assessment.id.as_str();
        let action = match fill.take(c, profile).map_err(one)? {
            Ok((stage, cash, hours)) => {
                let action = PortfolioAction::CheapTest {
                    max_cash: cash,
                    max_hours: hours,
                };
                let why = format!(
                    "{}: test stage `{stage}`",
                    c.assessment.verdict.labels().join(", ")
                );
                note(&mut notes, id, &action, why);
                action
            }
            Err(why) => {
                note(&mut notes, id, &PortfolioAction::Hold, why);
                PortfolioAction::Hold
            }
        };
        actions.insert(id, action);
    }
    let row = |c: &Candidate| GatedRow {
        id: c.assessment.id.clone(),
        opportunity_version: c.assessment.version,
        action: actions[c.assessment.id.as_str()].clone(),
        gates: c.assessment.verdict.labels(),
    };
    let held: Vec<GatedRow> = gated
        .iter()
        .filter(|c| c.assessment.verdict.verdict == Verdict::Hold)
        .map(|&c| row(c))
        .collect();
    let rejected: Vec<GatedRow> = gated
        .iter()
        .filter(|c| c.assessment.verdict.verdict == Verdict::Reject)
        .map(|&c| row(c))
        .collect();

    let rationale = if ranked.is_empty() {
        hold_rationale(&all)
    } else {
        None
    };
    let week = WeeklyPortfolio {
        schema: SchemaTag::v1("weekly_portfolio").map_err(one)?,
        id: head.id,
        version: 1,
        week: head.week,
        as_of: head.as_of,
        currency: head.currency,
        profile_sha256: head.profile_sha256,
        inputs_sha256: allocation_inputs_sha256(cands),
        economics_version: ECONOMICS_VERSION,
        ranked,
        held,
        rejected,
        allocation: fill.used,
        hold_rationale: rationale,
        next_information: week_next_information(&all),
    };
    validate(&week)?;
    notes.sort_by(|a, b| a.id.cmp(&b.id));
    Ok(Allocated {
        portfolio: week,
        notes,
    })
}

// ---------------------------------------------------------------------------
// The week after the models
// ---------------------------------------------------------------------------

/// What [`decide_week`] reads.
#[derive(Debug, Clone, Copy)]
pub struct WeekInput<'a> {
    pub head: &'a WeekHead,
    /// The O2 packet at `head.as_of` — the cycle's only fact input.
    pub packet: &'a EvidencePacket,
    pub proposals: &'a [MechanismProposal],
    pub challenges: &'a [Challenge],
    /// Active candidates: opportunity id → its next experiment stage.
    pub active: &'a BTreeMap<String, usize>,
    pub profile: &'a OperatorProfile,
}

/// One proposal through challenge, gates and economics.
#[derive(Debug, Clone, PartialEq)]
pub struct Decided {
    pub proposal: String,
    pub applied: Applied,
    pub assessment: Assessment,
    pub track: Track,
}

/// The decided week: the portfolio, its notes, every candidate and what the
/// stages read.
#[derive(Debug, Clone, PartialEq)]
pub struct Week {
    pub portfolio: WeeklyPortfolio,
    pub notes: Vec<Note>,
    pub decided: Vec<Decided>,
    pub cited: Vec<CitedRecord>,
    pub evidence: EvidenceIndex,
}

/// Module doc: the stages after the models, then [`allocate`]. `Err` lists
/// every problem: a packet at another time, a refused proposal or
/// challenge, an opportunity id proposed twice.
pub fn decide_week(w: &WeekInput) -> Result<Week, Vec<ValueError>> {
    let at = w.head.as_of;
    let mut errors: Vec<ValueError> = Vec::new();
    if at != Time::At(w.packet.as_of_ms) {
        errors.push(ValueError::new(
            codes::INVALID_TIME,
            format!(
                "the packet is at {} ms, the week at {at}",
                w.packet.as_of_ms
            ),
        ));
    }
    let evidence = EvidenceIndex::of(w.packet);
    let cited = cited_views(w.packet);
    let mut p = Problems::default();
    p.unique(
        "proposals.opportunity.id",
        w.proposals.iter().map(|x| x.opportunity().id.as_str()),
    );
    if let Err(e) = p.into_result() {
        errors.extend(e);
    }
    let prefixed = |what: &str, id: &str, e: Vec<ValueError>| {
        e.into_iter()
            .map(|x| ValueError::new(x.code, format!("{what} `{id}`: {}", x.message)))
            .collect::<Vec<_>>()
    };
    for x in w.proposals {
        if let Err(e) = x.draft.check(&evidence, &at) {
            errors.extend(prefixed("proposal", &x.id, e));
        }
    }
    let targets: Vec<&Opportunity> = w.proposals.iter().map(|x| x.opportunity()).collect();
    for c in w.challenges {
        if let Err(e) = c.draft.check(&evidence, &targets) {
            errors.extend(prefixed("challenge", &c.id, e));
        }
    }
    if !errors.is_empty() {
        return Err(errors);
    }
    let challenges: Vec<&Challenge> = w.challenges.iter().collect();
    let mut decided = Vec::new();
    for x in w.proposals {
        let applied = apply(x.opportunity(), &challenges).map_err(|e| vec![e])?;
        let mut assessment =
            assess(&applied.opportunity, &cited, w.profile, at).map_err(|e| vec![e])?;
        assessment.verdict = applied.held(assessment.verdict);
        let track = match w.active.get(&x.opportunity().id) {
            Some(&next_stage) => Track::Active { next_stage },
            None => Track::New,
        };
        decided.push(Decided {
            proposal: x.id.clone(),
            applied,
            assessment,
            track,
        });
    }
    let cands: Vec<Candidate> = decided
        .iter()
        .map(|d| Candidate {
            opportunity: &d.applied.opportunity,
            assessment: d.assessment.clone(),
            track: d.track,
        })
        .collect();
    let Allocated { portfolio, notes } = allocate(w.head.clone(), &cands, w.profile, &cited)?;
    Ok(Week {
        portfolio,
        notes,
        decided,
        cited,
        evidence,
    })
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::domain::canonical::canonical_json;
    use crate::domain::lineage::pins::toml_digest;
    use crate::domain::soe::challenge::tests::{challenge, widen};
    use crate::domain::soe::challenge::{ChallengeKind, Effect};
    use crate::domain::soe::economics::tests::{economics, with_economics, VERIFIED_DEAL};
    use crate::domain::soe::gates::tests::{
        acquisition, at, cited, complete, passing, revenue_share,
    };
    use crate::domain::soe::observe::tests::{filing, packet, report, t_ms};
    use crate::domain::soe::opportunity::tests::recurring;
    use crate::domain::soe::profile::tests::{synthetic, SYNTHETIC};
    use crate::domain::soe::proposal::tests::{draft_on, provenance};
    use crate::domain::soe::record::Tier;
    use crate::domain::soe::risk::{Risk, RiskKind, RiskStatus};
    use crate::domain::soe::value::{Bps, Currency};
    use crate::domain::source::testkit::{copy_of, rec, Src, H};
    use crate::domain::source::SourceRecord;

    pub(crate) fn head() -> WeekHead {
        WeekHead {
            id: "2026-W41".into(),
            week: "2026-W41".parse().unwrap(),
            as_of: Time::At(t_ms()),
            currency: Currency::Eur,
            profile_sha256: toml_digest(SYNTHETIC).unwrap(),
        }
    }

    /// A passing recurring automation (as `rank.rs`'s): 10 leads × 20 %,
    /// churn 5 %, ramp 2, `price` a month, variable 9 %, fixed 40.00, 9 h.
    pub(crate) fn automation(id: &str, price: &str) -> Opportunity {
        let mut o = complete(with_economics(
            "AUTOMATE",
            &economics(
                &[
                    ("variable_cost", "900"),
                    ("fixed_costs_per_month", "\"40.00\""),
                    ("owner_hours_per_month", "9"),
                    ("ramp_months", "2"),
                ],
                "RECURRING",
                &[
                    ("leads_per_month", "10"),
                    ("conversion", "2000"),
                    ("churn_per_month", "500"),
                    ("price_per_month", price),
                    ("collection_loss", "0"),
                ],
                ["\"0.00\"", "\"900.00\"", "\"300.00\"", "\"0.00\""],
            ),
            "",
        ));
        o.id = id.into();
        o
    }

    /// `o`'s experiment stages replaced: `(cash, hours)` each, with stop rules.
    fn stages(o: &mut Opportunity, costs: &[(&str, u32)]) {
        let x = o.experiment.as_mut().unwrap();
        x.stages = costs
            .iter()
            .enumerate()
            .map(|(i, (cash, hours))| crate::domain::soe::experiment::Stage {
                name: format!("stage-{i}"),
                cash: cash.parse().unwrap(),
                recoverable: Minor::ZERO,
                owner_hours: *hours,
                stop_rule: Some("no signal by the deadline".into()),
            })
            .collect();
    }

    fn assessed<'a>(o: &'a Opportunity, track: Track) -> Candidate<'a> {
        Candidate {
            opportunity: o,
            assessment: assess(o, &cited(), &synthetic(), head().as_of)
                .unwrap_or_else(|e| panic!("{e}")),
            track,
        }
    }

    /// `opps` (passing-style, cited `SIGNAL`) at the test decision time.
    pub(crate) fn week_of(opps: &[(&Opportunity, Track)]) -> Allocated {
        let cands: Vec<Candidate> = opps.iter().map(|(o, t)| assessed(o, *t)).collect();
        allocate(head(), &cands, &synthetic(), &cited()).unwrap_or_else(|e| panic!("{e:?}"))
    }

    /// Proposals stamped from `(opportunity, records, novelty)`.
    pub(crate) fn proposals(
        items: Vec<(Opportunity, Vec<&SourceRecord>, Tier)>,
    ) -> Vec<MechanismProposal> {
        items
            .into_iter()
            .map(|(o, records, novelty)| {
                let mut d = draft_on(o, &records);
                d.novelty = novelty;
                let id = format!("p-{}", d.opportunity.id);
                MechanismProposal::stamp(&id, "2026-W41", d, provenance("architect")).unwrap()
            })
            .collect()
    }

    pub(crate) fn decide(
        records: &[SourceRecord],
        props: &[MechanismProposal],
        challenges: &[Challenge],
        active: &BTreeMap<String, usize>,
    ) -> Week {
        let pk = packet(records, t_ms());
        let h = head();
        decide_week(&WeekInput {
            head: &h,
            packet: &pk,
            proposals: props,
            challenges,
            active,
            profile: &synthetic(),
        })
        .unwrap_or_else(|e| panic!("{e:?}"))
    }

    fn kinds(rows: &[GatedRow]) -> Vec<(&str, &str)> {
        rows.iter()
            .map(|r| (r.id.as_str(), r.action.kind()))
            .collect()
    }

    /// Days before the decision, as ms.
    pub(crate) fn ago(days: i64) -> i64 {
        t_ms() - days * 24 * H
    }

    /// A buyer-forum post (customer demand), its own event.
    pub(crate) fn post(native: &str, days: i64) -> SourceRecord {
        rec(
            Src::FORUM,
            native,
            ago(days),
            ago(days) + 60_000,
            ago(days) + 120_000,
        )
    }

    #[test]
    fn strong_news_weak_demand_holds() {
        // News: three articles copying one wire release (one origin — a
        // trigger, however loud); demand: one forum post.
        let f = filing("0000000001-26-000101", 4);
        let w1 = copy_of(&f, Src::WIRE, "w-1", ago(3), ago(3) + 60_000);
        let w2 = copy_of(&f, Src::WIRE, "w-2", ago(3), ago(3) + 60_000);
        let d1 = copy_of(&f, Src::DAILY, "d-1", ago(2), ago(2) + 60_000);
        let p1 = post("p-1", 2);
        let records = vec![w1.clone(), w2.clone(), d1.clone(), p1.clone()];
        let o = automation("news-automation", "\"600.00\"");
        let props = proposals(vec![(o.clone(), vec![&w1, &w2, &d1, &p1], Tier::High)]);
        let w = decide(&records, &props, &[], &BTreeMap::new());
        let pf = &w.portfolio;
        // No launch, nothing ranked: one demand event is held, and a cheap
        // demand test (the interviews stage: no cash, 5 h) fits the week.
        assert!(pf.ranked.is_empty() && pf.is_hold());
        assert_eq!(pf.held[0].gates, ["SINGLE_DEMAND_SIGNAL"]);
        assert_eq!(
            pf.held[0].action,
            PortfolioAction::CheapTest {
                max_cash: Minor::ZERO,
                max_hours: 5
            }
        );
        assert_eq!(
            (pf.allocation.owner_hours, pf.allocation.cash),
            (5, Minor::ZERO)
        );
        assert_eq!(
            pf.hold_rationale.as_deref(),
            Some("no candidate passed the hard gates: 1 held, 0 rejected")
        );
        // The news counted once: one origin behind three articles.
        assert_eq!(w.evidence.get(&w1.record_id).unwrap().confirmations, 1);

        // News only, no demand at all: held, and nothing to test.
        let props = proposals(vec![(o.clone(), vec![&w1, &w2, &d1], Tier::High)]);
        let w = decide(&records, &props, &[], &BTreeMap::new());
        assert_eq!(kinds(&w.portfolio.held), [("news-automation", "HOLD")]);
        assert_eq!(w.portfolio.held[0].gates, ["NO_TESTABLE_EVIDENCE"]);
        assert_eq!(w.portfolio.allocation.owner_hours, 0);

        // Corroborated news (a second, independent outlet) + the one post:
        // it passes — and still only gets a cheap test, never more.
        let own = report(&f, Src::DAILY, "d-own", ago(2));
        let mut records = records.clone();
        records.push(own.clone());
        let props = proposals(vec![(o, vec![&w1, &own, &p1], Tier::High)]);
        let w = decide(&records, &props, &[], &BTreeMap::new());
        assert_eq!(w.portfolio.ranked.len(), 1);
        assert_eq!(w.portfolio.ranked[0].action.kind(), "CHEAP_TEST");
    }

    /// 12000.00 a month on a signed one-off, 160 owner hours, nothing else.
    fn twelve_k_one_off(contract: &str) -> Opportunity {
        complete(with_economics(
            "HIGH_TICKET_DELIVERY",
            &economics(
                &[
                    ("variable_cost", "0"),
                    ("fixed_costs_per_month", "\"0.00\""),
                    ("owner_hours_per_month", "0"),
                    ("ramp_months", "0"),
                ],
                "ONE_OFF",
                &[
                    ("contract_value", contract),
                    ("win_probability", "10000"),
                    ("delivery_weeks", "4"),
                    ("owner_hours_total", "160"),
                    ("collection_loss", "0"),
                ],
                ["\"0.00\"", "\"0.00\"", "\"0.00\"", "\"0.00\""],
            ),
            "",
        ))
    }

    #[test]
    fn revenue_12k_160h_rejected_or_repriced() {
        // 12000.00 collected, 160 h × 70.00 = 11200.00 of owner time:
        // 800.00 time-adjusted against the synthetic 3000.00 target.
        let mut one_off = twelve_k_one_off("\"12000.00\"");
        one_off.id = "one-off-12k".into();
        let mut share = revenue_share("\"16000.00\"", "7500", "160");
        share.id = "share-12k".into();
        let w = week_of(&[(&one_off, Track::New), (&share, Track::New)]).portfolio;
        assert!(w.ranked.is_empty() && w.held.is_empty());
        // The one-off has a price: 11200.00 + 3000.00 = 14200.00 clears the
        // target. The revenue share has none: rejected.
        let rows: Vec<(&str, &PortfolioAction, &[String])> = w
            .rejected
            .iter()
            .map(|r| (r.id.as_str(), &r.action, r.gates.as_slice()))
            .collect();
        let below = ["CONTRIBUTION_BELOW_TARGET".to_string()];
        assert_eq!(
            rows,
            [
                (
                    "one-off-12k",
                    &PortfolioAction::Reprice {
                        min_price: "14200.00".parse().unwrap()
                    },
                    &below[..]
                ),
                ("share-12k", &PortfolioAction::Reject, &below[..]),
            ]
        );
        // The boundary: one cent less is still rejected, the price passes.
        let v = |c: &str| gates(&twelve_k_one_off(c), &cited(), &synthetic(), at()).unwrap();
        assert_eq!(v("\"14199.99\"").verdict, Verdict::Reject);
        assert_eq!(v("\"14200.00\"").verdict, Verdict::Pass);
        // Another rejection a price cannot fix (unbounded scope) rejects.
        let mut scope = one_off.clone();
        scope.risk.bounds.scope = crate::domain::soe::risk::Boundedness::Unbounded;
        let w = week_of(&[(&scope, Track::New)]).portfolio;
        assert_eq!(w.rejected[0].action, PortfolioAction::Reject);
        // Reprice works in the profile currency: the action never states
        // less than the native price needs.
        let vb = gates(&one_off, &cited(), &synthetic(), at()).unwrap();
        assert_eq!(
            min_price(&one_off, &vb, &cited(), &synthetic()).unwrap(),
            Some("14200.00".parse().unwrap())
        );
    }

    #[test]
    fn acquisition_unverifiable_transfer_holds_with_diligence() {
        // Relative to the synthetic profile: 15000.00 to buy, under its
        // 20000.00 cap; ownership only the seller's word, the hand-over
        // unverified, costs unknown, transfer of the payment account
        // unresolved.
        let mut o = acquisition(
            &VERIFIED_DEAL
                .replace(
                    "ownership = { status = \"VERIFIED\"",
                    "ownership = { status = \"SELLER_CLAIM\"",
                )
                .replace(
                    "transition = { status = \"VERIFIED\"",
                    "transition = { status = \"UNVERIFIED\"",
                )
                .replace(
                    "costs = { status = \"VERIFIED\"",
                    "costs = { status = \"UNKNOWN\"",
                ),
        );
        o.risk.risks.push(Risk {
            kind: RiskKind::Transferability,
            probability: Est::range(
                Bps::new(1000).unwrap(),
                Bps::new(3000).unwrap(),
                Bps::new(6000).unwrap(),
            )
            .unwrap(),
            impact: Est::range(
                "0.00".parse().unwrap(),
                "6000.00".parse().unwrap(),
                "15000.00".parse().unwrap(),
            )
            .unwrap(),
            control: "the payment processor confirms the account transfer in writing".into(),
            status: RiskStatus::Unresolved,
        });
        let verified = acquisition(VERIFIED_DEAL);
        let mut verified = verified;
        verified.id = "verified-acquisition".into();
        let w = week_of(&[(&o, Track::New), (&verified, Track::New)]).portfolio;
        let PortfolioAction::Diligence {
            questions,
            max_next_tranche,
        } = &w.held[0].action
        else {
            panic!("{:?}", w.held[0].action)
        };
        let fields: Vec<&str> = questions
            .iter()
            .map(|q| q.split(": ").next().unwrap())
            .collect();
        assert_eq!(
            fields,
            [
                "deal.ownership",
                "deal.transition",
                "risk.risks[0]",
                "deal.costs"
            ]
        );
        assert!(questions[0].contains("SELLER_CLAIM"), "{questions:?}");
        // Only the next tranche: the pilot stage's 750.00, under the
        // 1500.00 tranche — and no budget taken: the operator approves it.
        assert_eq!(*max_next_tranche, "750.00".parse().unwrap());
        assert!(*max_next_tranche <= synthetic().max_validation_tranche);
        assert_eq!(w.held[0].gates, ["LEGAL_UNRESOLVED", "DILIGENCE_OPEN"]);
        // The verified acquisition passes and ranks; the unverified one never does.
        assert_eq!(w.ranked.len(), 1);
        assert_eq!(w.ranked[0].id, "verified-acquisition");
        assert!(w.ranked.iter().all(|r| r.id != o.id));
        // A diligence question that is a red flag is a rejection, not diligence.
        let flagged = acquisition(
            &VERIFIED_DEAL.replace("red_flags = []", "red_flags = [\"escrow refused\"]"),
        );
        let w = week_of(&[(&flagged, Track::New)]).portfolio;
        assert_eq!(w.rejected[0].action, PortfolioAction::Reject);
    }

    #[test]
    fn two_articles_copying_one_source_are_one_confirmation() {
        // `copied` rests on two articles that copy one filing; `confirmed`
        // on an article and an independent outlet's own report.
        let f = filing("0000000001-26-000201", 5);
        let g = filing("0000000001-26-000202", 5);
        let c1 = copy_of(&f, Src::WIRE, "w-11", ago(4), ago(4) + 60_000);
        let c2 = copy_of(&f, Src::DAILY, "d-11", ago(4), ago(4) + 60_000);
        let c3 = copy_of(&f, Src::WIRE, "w-12", ago(3), ago(3) + 60_000);
        let g1 = copy_of(&g, Src::WIRE, "w-21", ago(4), ago(4) + 60_000);
        let g2 = report(&g, Src::DAILY, "d-21", ago(3));
        let records = vec![c1.clone(), c2.clone(), c3.clone(), g1.clone(), g2.clone()];
        let copied = automation("copied", "\"600.00\"");
        let confirmed = automation("confirmed", "\"600.00\"");
        let props = proposals(vec![
            (copied.clone(), vec![&c1, &c2], Tier::Medium),
            (confirmed.clone(), vec![&g1, &g2], Tier::Medium),
        ]);
        let w = decide(&records, &props, &[], &BTreeMap::new());
        // One origin behind the copies (domain/source counts it): a trigger.
        for r in [&c1, &c2, &c3] {
            assert_eq!(w.evidence.get(&r.record_id).unwrap().confirmations, 1);
        }
        for r in [&g1, &g2] {
            assert_eq!(w.evidence.get(&r.record_id).unwrap().confirmations, 2);
        }
        assert_eq!(kinds(&w.portfolio.held), [("copied", "HOLD")]);
        assert_eq!(w.portfolio.held[0].gates, ["NO_TESTABLE_EVIDENCE"]);
        assert_eq!(w.portfolio.ranked.len(), 1);
        assert_eq!(w.portfolio.ranked[0].id, "confirmed");
        // A third copy changes nothing: still one confirmation, still held.
        let props3 = proposals(vec![
            (copied, vec![&c1, &c2, &c3], Tier::Medium),
            (confirmed, vec![&g1, &g2], Tier::Medium),
        ]);
        let w3 = decide(&records, &props3, &[], &BTreeMap::new());
        assert_eq!(w3.portfolio.held, w.portfolio.held);
        assert_eq!(w3.portfolio.ranked, w.portfolio.ranked);
    }

    /// Two equal automations: `act` (active, improving — a primary filing
    /// confirms it) and `new` (two independent demand posts); each stage
    /// 6 h, so the 10 h week holds one test.
    pub(crate) fn active_vs_new(novelty_new: Tier, novelty_act: Tier) -> Week {
        let f = filing("0000000001-26-000301", 3);
        let (p1, p2) = (post("p-31", 3), post("p-32", 2));
        let mut act = automation("act", "\"600.00\"");
        act.version = 2;
        let mut new = automation("new", "\"600.00\"");
        for o in [&mut act, &mut new] {
            stages(o, &[("0.00", 6), ("750.00", 6)]);
        }
        let props = proposals(vec![
            (new, vec![&p1, &p2], novelty_new),
            (act, vec![&f], novelty_act),
        ]);
        let active = BTreeMap::from([("act".to_string(), 1)]);
        decide(&[f, p1, p2], &props, &[], &active)
    }

    #[test]
    fn active_improving_beats_novel_equal() {
        let w = active_vs_new(Tier::High, Tier::Low);
        let ranked: Vec<(&str, &str)> = w
            .portfolio
            .ranked
            .iter()
            .map(|r| (r.id.as_str(), r.action.kind()))
            .collect();
        // Evidence decides (HIGH: a primary fact; MEDIUM: two demand
        // events); the active test continues its pilot stage, the new one waits.
        assert_eq!(ranked, [("act", "CONTINUE_ACTIVE"), ("new", "HOLD")]);
        assert_eq!(w.portfolio.ranked[0].keys[0].value, "HIGH");
        assert_eq!(w.portfolio.ranked[1].keys[0].value, "MEDIUM");
        assert_eq!(
            (
                w.portfolio.allocation.owner_hours,
                w.portfolio.allocation.cash
            ),
            (6, "750.00".parse().unwrap())
        );
        let held = w.notes.iter().find(|n| n.id == "new").unwrap();
        assert!(held.why.contains("over budget"), "{held:?}");
        // The track is not a rank key: an equal active candidate behind on
        // evidence waits for the new one.
        let f = filing("0000000001-26-000302", 3);
        let (p1, p2) = (post("p-33", 3), post("p-34", 2));
        let mut behind = automation("behind", "\"600.00\"");
        let mut ahead = automation("ahead", "\"600.00\"");
        for o in [&mut behind, &mut ahead] {
            stages(o, &[("0.00", 6), ("750.00", 6)]);
        }
        let props = proposals(vec![
            (behind, vec![&p1, &p2], Tier::Low),
            (ahead, vec![&f], Tier::Low),
        ]);
        let active = BTreeMap::from([("behind".to_string(), 1)]);
        let w = decide(&[f, p1, p2], &props, &[], &active);
        let ranked: Vec<(&str, &str)> = w
            .portfolio
            .ranked
            .iter()
            .map(|r| (r.id.as_str(), r.action.kind()))
            .collect();
        assert_eq!(ranked, [("ahead", "CHEAP_TEST"), ("behind", "HOLD")]);
    }

    #[test]
    fn novelty_flag_does_not_change_rank() {
        let json = |w: &Week| canonical_json(&serde_json::to_value(&w.portfolio).unwrap());
        let base = active_vs_new(Tier::Medium, Tier::Medium);
        for (n, a) in [
            (Tier::High, Tier::Low),
            (Tier::Low, Tier::High),
            (Tier::Unknown, Tier::Unknown),
        ] {
            let w = active_vs_new(n, a);
            assert_eq!(json(&w), json(&base), "novelty {n:?} / {a:?}");
            assert_eq!(w.notes, base.notes);
        }
    }

    #[test]
    fn no_pass_gives_valid_empty_portfolio() {
        let mut h1 = complete(recurring()); // price UNKNOWN
        h1.id = "h1".into();
        let mut h3 = passing();
        h3.id = "h3".into();
        h3.economics.owner_hours_per_month.value = Est::unknown("no time log");
        let mut r = revenue_share("\"16000.00\"", "7500", "160"); // hidden labour, no price
        r.id = "r".into();
        let w = week_of(&[(&r, Track::New), (&h3, Track::New), (&h1, Track::New)]).portfolio;
        assert!(validate(&w).is_ok());
        assert!(w.ranked.is_empty() && w.is_hold());
        assert_eq!(
            w.hold_rationale.as_deref(),
            Some("no candidate passed the hard gates: 2 held, 1 rejected")
        );
        // Every failed gate stays on its row.
        assert_eq!(kinds(&w.held), [("h1", "HOLD"), ("h3", "HOLD")]);
        assert_eq!(
            w.held[0].gates,
            ["UNKNOWN_INPUT:economics.revenue.price_per_month"]
        );
        assert_eq!(
            w.held[1].gates,
            [
                "UNBOUNDED_OWNER_TIME",
                "UNKNOWN_INPUT:economics.owner_hours_per_month"
            ]
        );
        assert_eq!(kinds(&w.rejected), [("r", "REJECT")]);
        assert_eq!(
            w.next_information,
            [
                "economics.owner_hours_per_month",
                "economics.revenue.price_per_month"
            ]
        );
        assert_eq!(
            (w.allocation.owner_hours, w.allocation.cash),
            (0, Minor::ZERO)
        );
        // The same rows, rationale and next information as the O1 HOLD week.
        let all: Vec<Assessment> = [&r, &h3, &h1]
            .map(|o| assess(o, &cited(), &synthetic(), head().as_of).unwrap())
            .to_vec();
        let hold = crate::domain::soe::rank::hold_week(head(), &all).unwrap();
        assert_eq!(
            (&w.held, &w.rejected, &w.hold_rationale, &w.next_information),
            (
                &hold.held,
                &hold.rejected,
                &hold.hold_rationale,
                &hold.next_information
            )
        );
        // No candidate at all: a valid empty week.
        let empty = allocate(head(), &[], &synthetic(), &cited())
            .unwrap()
            .portfolio;
        assert!(validate(&empty).is_ok());
        assert_eq!(
            empty.hold_rationale.as_deref(),
            Some("no candidate this week")
        );
    }

    #[test]
    fn budget_never_exceeds_hours_or_tranche() {
        // Six passing automations, better price ranks first; stage-0 costs
        // vary. Week (synthetic): 10 h, 1500.00.
        let costs = [
            ("0.00", 6),
            ("500.00", 3),
            ("900.00", 2),
            ("400.00", 1),
            ("1500.00", 0),
            ("0.00", 1),
        ];
        let mut opps: Vec<Opportunity> = costs
            .iter()
            .enumerate()
            .map(|(i, (cash, hours))| {
                let mut o = automation(&format!("c{i}"), &format!("\"{}.00\"", 700 - i * 10));
                stages(&mut o, &[(cash, *hours), ("100.00", 1)]);
                o
            })
            .collect();
        // A testable hold competes for the same budget after them.
        let mut path = passing();
        path.id = "path".into();
        if let RevenueModel::RevenueShare {
            partner_monthly_revenue,
            ..
        } = &mut path.economics.revenue
        {
            partner_monthly_revenue.value = Est::range(
                "7000.00".parse().unwrap(),
                "8750.00".parse().unwrap(),
                "11000.00".parse().unwrap(),
            )
            .unwrap();
        }
        opps.push(path);
        let p = synthetic();
        let check = |w: &WeeklyPortfolio, p: &OperatorProfile| {
            assert!(w.allocation.owner_hours <= p.weekly_owner_hours);
            assert!(w.allocation.cash <= p.max_validation_tranche);
            let (mut h, mut c) = (0u32, Minor::ZERO);
            for a in w
                .ranked
                .iter()
                .map(|r| &r.action)
                .chain(w.held.iter().map(|r| &r.action))
            {
                if let PortfolioAction::CheapTest {
                    max_cash,
                    max_hours,
                } = a
                {
                    h += max_hours;
                    c = c.checked_add(*max_cash).unwrap();
                }
            }
            assert_eq!((h, c), (w.allocation.owner_hours, w.allocation.cash));
        };
        let refs: Vec<(&Opportunity, Track)> = opps.iter().map(|o| (o, Track::New)).collect();
        let w = week_of(&refs).portfolio;
        check(&w, &p);
        let got: Vec<(&str, &str)> = w
            .ranked
            .iter()
            .map(|r| (r.id.as_str(), r.action.kind()))
            .collect();
        // c0 6 h; c1 3 h + 500 (9 h); c2 would be 11 h — waits; c3 1 h + 400
        // (10 h, 900.00); c4, c5 and the testable hold no longer fit.
        assert_eq!(
            got,
            [
                ("c0", "CHEAP_TEST"),
                ("c1", "CHEAP_TEST"),
                ("c2", "HOLD"),
                ("c3", "CHEAP_TEST"),
                ("c4", "HOLD"),
                ("c5", "HOLD"),
            ]
        );
        assert_eq!(w.held[0].gates, ["BELOW_TARGET_WITH_PATH"]);
        assert_eq!(kinds(&w.held), [("path", "HOLD")]);
        assert_eq!(
            (w.allocation.owner_hours, w.allocation.cash),
            (10, "900.00".parse().unwrap())
        );
        // Under every smaller week the totals stay inside it.
        for hours in 1..=12 {
            for tranche in ["100.00", "450.00", "1000.00"] {
                let mut small = synthetic();
                small.weekly_owner_hours = hours;
                small.max_validation_tranche = tranche.parse().unwrap();
                let cands: Vec<Candidate> = refs
                    .iter()
                    .map(|(o, t)| Candidate {
                        opportunity: o,
                        assessment: assess(o, &cited(), &small, head().as_of).unwrap(),
                        track: *t,
                    })
                    .collect();
                let w = allocate(head(), &cands, &small, &cited())
                    .unwrap()
                    .portfolio;
                check(&w, &small);
            }
        }
    }

    #[test]
    fn capability_version_active_at_decided_at() {
        // Equal automations; `a-migrate` needs `migration`, whose PROVEN
        // version expired 2026-06-30 — STALE at the decision, so the fit
        // decides against it (the id alone would put it first).
        let mut a = automation("a-migrate", "\"600.00\"");
        a.requires_skills = vec!["migration".into()];
        let b = automation("b-integrate", "\"600.00\"");
        let ranked = |p: &OperatorProfile| -> Vec<(String, String)> {
            let cands: Vec<Candidate> = [&a, &b]
                .iter()
                .map(|o| Candidate {
                    opportunity: o,
                    assessment: assess(o, &cited(), p, head().as_of).unwrap(),
                    track: Track::New,
                })
                .collect();
            let w = allocate(head(), &cands, p, &cited()).unwrap().portfolio;
            w.ranked
                .iter()
                .map(|r| {
                    let fit = r
                        .keys
                        .iter()
                        .find(|k| k.key == crate::domain::soe::profile::RankKey::CapabilityFit);
                    (r.id.clone(), fit.unwrap().value.clone())
                })
                .collect()
        };
        let pair = |x: &str, fx: &str, y: &str, fy: &str| {
            vec![
                (x.to_string(), fx.to_string()),
                (y.to_string(), fy.to_string()),
            ]
        };
        assert_eq!(
            ranked(&synthetic()),
            pair("b-integrate", "PROVEN", "a-migrate", "STALE")
        );
        // A new version active before the decision: equal again, the id decides.
        let mut p = synthetic();
        let mut fresh = p.capabilities[2].clone();
        fresh.id = "migration-2026".into();
        fresh.as_of = "2026-10-01".parse().unwrap();
        fresh.valid_until = "2027-10-01".parse().unwrap();
        p.capabilities.push(fresh.clone());
        assert_eq!(
            ranked(&p),
            pair("a-migrate", "PROVEN", "b-integrate", "PROVEN")
        );
        // One that starts after the decision is not in force yet.
        let mut later = synthetic();
        fresh.as_of = "2026-10-06".parse().unwrap();
        later.capabilities.push(fresh);
        assert_eq!(
            ranked(&later),
            pair("b-integrate", "PROVEN", "a-migrate", "STALE")
        );
    }

    #[test]
    fn challenges_and_blocks_reach_the_allocation() {
        // A Critic finds hidden labour (14 h base) on one candidate and a
        // transferability question on another: the first is rejected (the
        // shadow time eats the margin), the second held; inputs differ.
        let f = filing("0000000001-26-000401", 3);
        let (a, b) = (automation("a", "\"600.00\""), automation("b", "\"600.00\""));
        let props = proposals(vec![
            (a, vec![&f], Tier::Medium),
            (b, vec![&f], Tier::Medium),
        ]);
        let hours = "economics.owner_hours_per_month";
        let cs = [
            challenge(
                "k1",
                "a",
                ChallengeKind::HiddenLabor,
                widen(hours, Some("60"), Some("60"), Some("80")),
            ),
            challenge(
                "k2",
                "b",
                ChallengeKind::Transferability,
                Effect::BlockGate {
                    gate: GateCode::LegalUnresolved,
                },
            ),
        ];
        let w = decide(std::slice::from_ref(&f), &props, &cs, &BTreeMap::new());
        assert!(w.portfolio.ranked.is_empty());
        assert_eq!(kinds(&w.portfolio.rejected), [("a", "REPRICE")]);
        assert_eq!(kinds(&w.portfolio.held), [("b", "HOLD")]);
        assert_eq!(w.portfolio.held[0].gates, ["LEGAL_UNRESOLVED"]);
        let none = decide(&[f], &props, &[], &BTreeMap::new());
        assert_ne!(none.portfolio.inputs_sha256, w.portfolio.inputs_sha256);
        assert_eq!(none.portfolio.ranked.len(), 2);
        // A challenge on a candidate nobody proposed is refused with the week.
        let ghost = challenge("k3", "ghost", ChallengeKind::BaseRate, Effect::None);
        let pk = packet(&[filing("0000000001-26-000401", 3)], t_ms());
        let h = head();
        let e = decide_week(&WeekInput {
            head: &h,
            packet: &pk,
            proposals: &props,
            challenges: &[ghost],
            active: &BTreeMap::new(),
            profile: &synthetic(),
        })
        .unwrap_err();
        assert_eq!(e[0].code, codes::UNKNOWN_TARGET);
        assert!(e[0].message.starts_with("challenge `k3`"), "{e:?}");
    }
}
