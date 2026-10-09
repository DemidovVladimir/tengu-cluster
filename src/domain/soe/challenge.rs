//! Critic challenge (O3 stage "Challenge"; PRD § 8 step 6): disconfirming
//! evidence, hidden labour, dependency failure, base rates, legal access,
//! transferability, duplicated sources — as data a separate model writes
//! about one proposal. Pure. A challenge merges only toward the
//! conservative side: it can widen an input against the candidate, make it
//! unknown or hold the candidate on a gate; it can never improve an input,
//! make an unknown known or lift a gate.
//!
//! | [`ChallengeDraft`] (model-written) | Value |
//! |---|---|
//! | `target` | the challenged opportunity id |
//! | `kind` | [`ChallengeKind`] |
//! | `claim`, `evidence[]` | text · source record ids of the cycle's packet, in full (may be empty) |
//! | `effect` | [`Effect`]: `WIDEN` `field` + any of `low` `base` `high` (decimal text in the field's unit) or `unknown_reason` · `BLOCK_GATE` `gate` (a `GateCode`) · `NONE` |
//!
//! `soe.challenge/1` = header + `cycle_id` + `[provenance]` (`proposal::Provenance`, stamped by the tool) + `[draft]`.
//!
//! | Merge ([`apply`]) | Rule |
//! |---|---|
//! | direction | [`direction`]: revenue, price, volume, conversion, win probability and share are better higher — each end may only go down; costs, hours, churn, loss, ramp, delivery weeks and initial capital are better lower — each end may only go up |
//! | ends | each given end moves only the adverse way (a favourable one is ignored, noted); then `base` and the far end follow so `low ≤ base ≤ high` holds, again only adversely |
//! | `unknown_reason` | a known input becomes `UNKNOWN: <reason>` |
//! | an unknown input | stays unknown — a challenge cannot make it known |
//! | `BLOCK_GATE` | a `HOLD` failure on the gate (`gates::GateVerdict::with_holds`), field `challenge.<id>`; a `PASS` holds, a `REJECT` stays |
//! | order | challenges by id; each sees the input as the previous left it |
//!
//! | Check ([`ChallengeDraft::check`]) | Code |
//! |---|---|
//! | a computed or stamped key ([`proposal::COMPUTED_KEYS`](super::proposal::COMPUTED_KEYS)) | `computed_field` |
//! | `target` not a candidate of the cycle | `unknown_target` |
//! | an evidence id not in the packet | `unsupported_evidence` |
//! | a `WIDEN` field that is not an input of the target · a value that does not parse as the field's type · no end and no reason | `unknown_field` · `invalid_field` (`invalid_amount`, `invalid_bps`) · `invalid_field` |
//! | an empty claim | `invalid_field` |

// Consumers land with the `soe_challenge` tool and the cycle.
#![allow(dead_code)]

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::gates::{GateCode, GateFailure, GateVerdict};
use super::observe::EvidenceIndex;
use super::opportunity::{InputMut, Opportunity};
use super::proposal::{computed_keys, Provenance};
use super::record::{validate, Problems, SoeRecord, Verdict};
use super::value::{codes, Assumption, Better, Bps, Est, Minor, SchemaTag, ValueError};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ChallengeKind {
    DisconfirmingEvidence,
    HiddenLabor,
    DependencyFailure,
    BaseRate,
    LegalAccess,
    Transferability,
    DuplicateSource,
}

/// What a challenge does to its target (module table).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "SCREAMING_SNAKE_CASE", deny_unknown_fields)]
pub enum Effect {
    Widen {
        field: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        low: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        base: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        high: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        unknown_reason: Option<String>,
    },
    BlockGate {
        gate: GateCode,
    },
    None,
}

/// What a model writes (module table).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ChallengeDraft {
    pub target: String,
    pub kind: ChallengeKind,
    pub claim: String,
    pub evidence: Vec<String>,
    pub effect: Effect,
}

/// A draft from a model's JSON: computed keys refused first, then the shape.
pub fn challenge_from_json(text: &str) -> Result<ChallengeDraft, Vec<ValueError>> {
    let v: Value = serde_json::from_str(text)
        .map_err(|e| vec![ValueError::new(codes::INVALID_RECORD, e.to_string())])?;
    let computed = computed_keys(&v);
    if !computed.is_empty() {
        return Err(computed);
    }
    serde_json::from_value(v)
        .map_err(|e| vec![ValueError::new(codes::INVALID_RECORD, e.to_string())])
}

/// Module table: which way `field` helps the candidate (`None`: not an
/// input).
pub fn direction(field: &str) -> Option<Better> {
    let rest = field.strip_prefix("economics.")?;
    Some(match rest.strip_prefix("revenue.").unwrap_or(rest) {
        "leads_per_month"
        | "conversion"
        | "price_per_month"
        | "contract_value"
        | "win_probability"
        | "asset_monthly_revenue"
        | "partner_monthly_revenue"
        | "share" => Better::Higher,
        "churn_per_month"
        | "collection_loss"
        | "delivery_weeks"
        | "owner_hours_total"
        | "variable_cost"
        | "fixed_costs_per_month"
        | "owner_hours_per_month"
        | "ramp_months"
        | "initial.acquisition"
        | "initial.setup"
        | "initial.validation"
        | "initial.working_capital" => Better::Lower,
        _ => return None,
    })
}

/// A value of the field's type from decimal text.
trait Parse: Sized + Copy + Ord + fmt::Display {
    fn parse_text(s: &str) -> Result<Self, ValueError>;
}

impl Parse for u32 {
    fn parse_text(s: &str) -> Result<u32, ValueError> {
        s.trim()
            .parse()
            .map_err(|_| ValueError::new(codes::INVALID_FIELD, format!("`{s}`: a whole number")))
    }
}

impl Parse for Bps {
    fn parse_text(s: &str) -> Result<Bps, ValueError> {
        let v: i64 = s.trim().parse().map_err(|_| {
            ValueError::new(codes::INVALID_FIELD, format!("`{s}`: bps, an integer"))
        })?;
        Bps::new(v)
    }
}

impl Parse for Minor {
    fn parse_text(s: &str) -> Result<Minor, ValueError> {
        Minor::from_str(s.trim())
    }
}

/// The given ends, parsed.
struct Ends<T> {
    low: Option<T>,
    base: Option<T>,
    high: Option<T>,
}

fn ends<T: Parse>(
    low: &Option<String>,
    base: &Option<String>,
    high: &Option<String>,
) -> Result<Ends<T>, ValueError> {
    let p = |x: &Option<String>| x.as_deref().map(T::parse_text).transpose();
    Ok(Ends {
        low: p(low)?,
        base: p(base)?,
        high: p(high)?,
    })
}

/// What one merge did to one input.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Change {
    pub challenge: String,
    pub field: String,
    pub before: String,
    pub after: String,
}

/// A part of a challenge the merge did not apply, and why.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Ignored {
    pub challenge: String,
    pub why: String,
}

fn text<T: fmt::Display>(e: &Est<T>) -> String {
    match e {
        Est::Range { low, base, high } => format!("{low}..{base}..{high}"),
        Est::Unknown { reason: None } => "UNKNOWN".into(),
        Est::Unknown { reason: Some(r) } => format!("UNKNOWN: {r}"),
    }
}

/// The adverse merge of `e` with the given ends (module table); the new
/// estimate and what was ignored.
fn merge<T: Parse>(e: &Est<T>, x: Ends<T>, better: Better) -> (Est<T>, Vec<String>) {
    let Est::Range { low, base, high } = *e else {
        return (
            e.clone(),
            vec!["the input is unknown: a challenge cannot make it known".into()],
        );
    };
    let mut ignored = Vec::new();
    let adverse = |cur: T, v: Option<T>, end: &str, ignored: &mut Vec<String>| match v {
        None => cur,
        Some(v) => {
            let worse = match better {
                Better::Higher => v.min(cur),
                Better::Lower => v.max(cur),
            };
            if worse == cur && v != cur {
                ignored.push(format!(
                    "{end} {v} would flatter the candidate ({end} stays {cur})"
                ));
            }
            worse
        }
    };
    let (l, b, h) = (
        adverse(low, x.low, "low", &mut ignored),
        adverse(base, x.base, "base", &mut ignored),
        adverse(high, x.high, "high", &mut ignored),
    );
    let (l, b, h) = match better {
        // Ends only went down: pull base, then low, down to stay ordered.
        Better::Higher => {
            let b = b.min(h);
            (l.min(b), b, h)
        }
        // Ends only went up: push base, then high, up.
        Better::Lower => {
            let b = b.max(l);
            (l, b, h.max(b))
        }
    };
    (
        Est::Range {
            low: l,
            base: b,
            high: h,
        },
        ignored,
    )
}

/// Apply one `WIDEN` to one typed input; `(before, after, ignored)`.
fn widen_one<T: Parse>(
    a: &mut Assumption<T>,
    better: Better,
    low: &Option<String>,
    base: &Option<String>,
    high: &Option<String>,
    unknown_reason: &Option<String>,
) -> Result<(String, String, Vec<String>), ValueError> {
    let before = text(&a.value);
    if let Some(reason) = unknown_reason {
        if !a.value.is_known() {
            return Ok((before.clone(), before, vec!["already unknown".into()]));
        }
        a.value = Est::unknown(reason.as_str());
        return Ok((before, text(&a.value), Vec::new()));
    }
    let x = ends::<T>(low, base, high)?;
    let (next, ignored) = merge(&a.value, x, better);
    a.value = next;
    Ok((before, text(&a.value), ignored))
}

/// What the merge of every challenge on one candidate gives.
#[derive(Debug, Clone, PartialEq)]
pub struct Applied {
    /// The opportunity as challenged (the gates and the economics read this).
    pub opportunity: Opportunity,
    pub changes: Vec<Change>,
    pub ignored: Vec<Ignored>,
    /// `BLOCK_GATE` holds, for `GateVerdict::with_holds`.
    pub holds: Vec<GateFailure>,
}

impl Applied {
    /// `v` with this candidate's blocks (module table).
    pub fn held(&self, v: GateVerdict) -> GateVerdict {
        v.with_holds(self.holds.iter().cloned())
    }
}

/// Module table: every challenge in `challenges` on `opp` (others ignored),
/// by challenge id.
pub fn apply(opp: &Opportunity, challenges: &[&Challenge]) -> Result<Applied, ValueError> {
    let mut mine: Vec<&Challenge> = challenges
        .iter()
        .copied()
        .filter(|c| c.draft.target == opp.id)
        .collect();
    mine.sort_by(|a, b| a.id.cmp(&b.id));
    let mut out = Applied {
        opportunity: opp.clone(),
        changes: Vec::new(),
        ignored: Vec::new(),
        holds: Vec::new(),
    };
    for c in mine {
        match &c.draft.effect {
            Effect::None => {}
            Effect::BlockGate { gate } => out.holds.push(GateFailure {
                code: *gate,
                outcome: Verdict::Hold,
                field: Some(format!("challenge.{}", c.id)),
                detail: format!("{:?} challenge `{}`: {}", c.draft.kind, c.id, c.draft.claim),
            }),
            Effect::Widen {
                field,
                low,
                base,
                high,
                unknown_reason,
            } => {
                let (Some(better), Some(input)) =
                    (direction(field), out.opportunity.economics.input_mut(field))
                else {
                    return Err(ValueError::new(
                        codes::UNKNOWN_FIELD,
                        format!(
                            "challenge `{}`: `{field}` is not an input of `{}`",
                            c.id, opp.id
                        ),
                    ));
                };
                let r = match input {
                    InputMut::Count(a) => widen_one(a, better, low, base, high, unknown_reason),
                    InputMut::Share(a) => widen_one(a, better, low, base, high, unknown_reason),
                    InputMut::Amount(a) => widen_one(a, better, low, base, high, unknown_reason),
                };
                let (before, after, ignored) = r.map_err(|e| {
                    ValueError::new(
                        e.code,
                        format!("challenge `{}` {field}: {}", c.id, e.message),
                    )
                })?;
                for why in ignored {
                    out.ignored.push(Ignored {
                        challenge: c.id.clone(),
                        why: format!("{field}: {why}"),
                    });
                }
                if before != after {
                    out.changes.push(Change {
                        challenge: c.id.clone(),
                        field: field.clone(),
                        before,
                        after,
                    });
                }
            }
        }
    }
    Ok(out)
}

impl ChallengeDraft {
    /// Module table: every problem against the packet and the cycle's
    /// candidates.
    pub fn check(
        &self,
        index: &EvidenceIndex,
        targets: &[&Opportunity],
    ) -> Result<(), Vec<ValueError>> {
        let mut p = Problems::default();
        p.text("claim", &self.claim);
        let target = targets.iter().find(|o| o.id == self.target);
        p.check(
            target.is_some(),
            codes::UNKNOWN_TARGET,
            "target",
            format_args!("`{}` is not a candidate of this cycle", self.target),
        );
        for (i, id) in self.evidence.iter().enumerate() {
            p.check(
                index.contains(id),
                codes::UNSUPPORTED_EVIDENCE,
                &format!("evidence[{i}]"),
                format_args!("`{id}` is not in the cycle's evidence packet"),
            );
        }
        if let (
            Effect::Widen {
                field,
                low,
                base,
                high,
                unknown_reason,
            },
            Some(o),
        ) = (&self.effect, target)
        {
            let mut probe = (*o).clone();
            match (direction(field), probe.economics.input_mut(field)) {
                (Some(_), Some(input)) => {
                    let parsed = match input {
                        InputMut::Count(_) => ends::<u32>(low, base, high).map(|_| ()),
                        InputMut::Share(_) => ends::<Bps>(low, base, high).map(|_| ()),
                        InputMut::Amount(_) => ends::<Minor>(low, base, high).map(|_| ()),
                    };
                    if let Err(e) = parsed {
                        p.push(e.code, "effect", e.message);
                    }
                }
                _ => p.push(
                    codes::UNKNOWN_FIELD,
                    "effect.field",
                    format_args!("`{field}` is not an input of `{}`", o.id),
                ),
            }
            p.check(
                low.is_some() || base.is_some() || high.is_some() || unknown_reason.is_some(),
                codes::INVALID_FIELD,
                "effect",
                "WIDEN gives an end or an unknown_reason",
            );
        }
        p.into_result()
    }
}

/// `soe.challenge/1` (module table).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Challenge {
    pub schema: SchemaTag,
    pub id: String,
    pub version: u32,
    pub cycle_id: String,
    pub provenance: Provenance,
    pub draft: ChallengeDraft,
}

impl Challenge {
    /// `draft` stamped as challenge `id` of `cycle_id`; validated.
    pub fn stamp(
        id: &str,
        cycle_id: &str,
        draft: ChallengeDraft,
        provenance: Provenance,
    ) -> Result<Challenge, Vec<ValueError>> {
        let c = Challenge {
            schema: SchemaTag::v1("challenge").map_err(|e| vec![e])?,
            id: id.to_string(),
            version: 1,
            cycle_id: cycle_id.to_string(),
            provenance,
            draft,
        };
        validate(&c)?;
        Ok(c)
    }
}

impl SoeRecord for Challenge {
    const RECORD: &'static str = "challenge";

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
        p.id("draft.target", &self.draft.target);
        p.text("draft.claim", &self.draft.claim);
        self.provenance.problems(p);
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::domain::soe::economics::{scenarios, Metric};
    use crate::domain::soe::gates::gates;
    use crate::domain::soe::gates::tests::{at, cited, passing};
    use crate::domain::soe::observe::tests::{filing, packet, t_ms};
    use crate::domain::soe::profile::tests::synthetic;
    use crate::domain::soe::proposal::tests::provenance;

    pub(crate) fn challenge(
        id: &str,
        target: &str,
        kind: ChallengeKind,
        effect: Effect,
    ) -> Challenge {
        Challenge::stamp(
            id,
            "2026-W41",
            ChallengeDraft {
                target: target.into(),
                kind,
                claim: format!("{id}: the synthetic critic's claim"),
                evidence: vec![],
                effect,
            },
            provenance("critic"),
        )
        .unwrap()
    }

    pub(crate) fn widen(
        field: &str,
        low: Option<&str>,
        base: Option<&str>,
        high: Option<&str>,
    ) -> Effect {
        Effect::Widen {
            field: field.into(),
            low: low.map(String::from),
            base: base.map(String::from),
            high: high.map(String::from),
            unknown_reason: None,
        }
    }

    fn value(o: &Opportunity, field: &str) -> String {
        o.economics.input(field).unwrap().value_text()
    }

    #[test]
    fn challenge_only_widens_conservatively() {
        let o = passing(); // revenue share, exactly at the target
        let id = o.id.clone();
        let hours = "economics.owner_hours_per_month";
        let share = "economics.revenue.share";
        let cs = [
            // Hidden labour: more hours (adverse) — applied.
            challenge(
                "c1",
                &id,
                ChallengeKind::HiddenLabor,
                widen(hours, None, Some("14"), Some("20")),
            ),
            // A "challenge" that improves the share (favourable) — ignored.
            challenge(
                "c2",
                &id,
                ChallengeKind::BaseRate,
                widen(share, Some("4500"), Some("5000"), None),
            ),
            // Fewer hours than now (favourable) — ignored.
            challenge(
                "c3",
                &id,
                ChallengeKind::HiddenLabor,
                widen(hours, Some("2"), None, None),
            ),
            // A challenge on another candidate — not this one's.
            challenge(
                "c4",
                "someone-else",
                ChallengeKind::BaseRate,
                widen(share, None, Some("1"), None),
            ),
        ];
        let refs: Vec<&Challenge> = cs.iter().collect();
        let a = apply(&o, &refs).unwrap();
        assert_eq!(value(&a.opportunity, hours), "10..14..20");
        assert_eq!(value(&a.opportunity, share), value(&o, share));
        assert_eq!(
            a.changes,
            [Change {
                challenge: "c1".into(),
                field: hours.into(),
                before: "10..10..10".into(),
                after: "10..14..20".into()
            }]
        );
        assert_eq!(a.ignored.len(), 3, "{:?}", a.ignored); // c2 low + base, c3 low
                                                           // More hours cut the time-adjusted contribution: the merge is adverse.
        let before = scenarios(&o, &synthetic()).unwrap().base;
        let after = scenarios(&a.opportunity, &synthetic()).unwrap().base;
        let ta = |m: &Metric<Minor>| *m.known().unwrap();
        assert!(ta(&after.time_adjusted_contribution) < ta(&before.time_adjusted_contribution));

        // Every input, pushed both ways by one point: never better for the candidate.
        for (field, _) in o.economics.inputs() {
            let better = direction(&field).unwrap_or_else(|| panic!("{field}"));
            let now = value(&o, &field);
            for v in ["0", "1", "9999"] {
                let c = challenge(
                    "cx",
                    &id,
                    ChallengeKind::BaseRate,
                    widen(&field, Some(v), Some(v), Some(v)),
                );
                let a = apply(&o, &[&c]).unwrap();
                let base = |x: &Opportunity| {
                    scenarios(x, &synthetic())
                        .unwrap()
                        .base
                        .time_adjusted_contribution
                };
                if let (Some(b), Some(n)) = (
                    base(&o).known().copied(),
                    base(&a.opportunity).known().copied(),
                ) {
                    assert!(
                        n <= b,
                        "{field} = {v} ({better:?}, was {now}) improved {b} → {n}"
                    );
                }
            }
        }

        // UNKNOWN wins; an unknown stays unknown.
        let unk = Effect::Widen {
            field: share.into(),
            low: None,
            base: None,
            high: None,
            unknown_reason: Some("the partner's books are unaudited".into()),
        };
        let c5 = challenge("c5", &id, ChallengeKind::DisconfirmingEvidence, unk);
        let c6 = challenge(
            "c6",
            &id,
            ChallengeKind::BaseRate,
            widen(share, Some("100"), Some("200"), Some("300")),
        );
        let a = apply(&o, &[&c6, &c5]).unwrap();
        assert_eq!(
            value(&a.opportunity, share),
            "UNKNOWN: the partner's books are unaudited"
        );
        assert!(a
            .ignored
            .iter()
            .any(|i| i.challenge == "c6" && i.why.contains("cannot make it known")));
        let v = gates(&a.opportunity, &cited(), &synthetic(), at()).unwrap();
        assert_eq!(v.verdict, Verdict::Hold);

        // A field the model does not have is refused; so is a bad value.
        let bad = challenge(
            "c7",
            &id,
            ChallengeKind::BaseRate,
            widen("economics.revenue.price_per_month", None, Some("1"), None),
        );
        assert_eq!(apply(&o, &[&bad]).unwrap_err().code, codes::UNKNOWN_FIELD);
        let bad = challenge(
            "c8",
            &id,
            ChallengeKind::BaseRate,
            widen(share, None, Some("lots"), None),
        );
        assert_eq!(apply(&o, &[&bad]).unwrap_err().code, codes::INVALID_FIELD);
    }

    #[test]
    fn block_gate_holds() {
        let o = passing();
        let v = gates(&o, &cited(), &synthetic(), at()).unwrap();
        assert_eq!(v.verdict, Verdict::Pass);
        let c = challenge(
            "c1",
            &o.id,
            ChallengeKind::Transferability,
            Effect::BlockGate {
                gate: GateCode::LegalUnresolved,
            },
        );
        let a = apply(&o, &[&c]).unwrap();
        let held = a.held(v.clone());
        assert_eq!(held.verdict, Verdict::Hold);
        assert_eq!(held.labels(), ["LEGAL_UNRESOLVED"]);
        assert_eq!(held.failures[0].field.as_deref(), Some("challenge.c1"));
        assert!(held.failures[0]
            .detail
            .contains("Transferability challenge `c1`"));
        // The figures did not move: a block is a hold, not a number.
        assert_eq!(a.opportunity, o);
        // A rejected candidate stays rejected; a block never lifts a failure.
        let rejected = gates(
            &crate::domain::soe::gates::tests::revenue_share("\"16000.00\"", "7500", "160"),
            &cited(),
            &synthetic(),
            at(),
        )
        .unwrap();
        let both = a.held(rejected.clone());
        assert_eq!(both.verdict, Verdict::Reject);
        assert_eq!(
            both.labels(),
            ["CONTRIBUTION_BELOW_TARGET", "LEGAL_UNRESOLVED"]
        );
        // NONE changes nothing.
        let none = challenge("c2", &o.id, ChallengeKind::BaseRate, Effect::None);
        assert_eq!(apply(&o, &[&none]).unwrap().held(v.clone()), v);
    }

    #[test]
    fn drafts_are_checked_against_the_packet_and_targets() {
        let f = filing("0000000001-26-000020", 2);
        let idx = EvidenceIndex::of(&packet(std::slice::from_ref(&f), t_ms()));
        let o = passing();
        let ok = ChallengeDraft {
            target: o.id.clone(),
            kind: ChallengeKind::DisconfirmingEvidence,
            claim: "the filing says the partner is winding down".into(),
            evidence: vec![f.record_id.clone()],
            effect: widen(
                "economics.revenue.partner_monthly_revenue",
                None,
                None,
                Some("1.00"),
            ),
        };
        assert_eq!(ok.check(&idx, &[&o]), Ok(()));
        let mut bad = ok.clone();
        bad.target = "ghost".into();
        bad.evidence.push("sec_edgar:x:y".into());
        bad.claim = " ".into();
        let codes: Vec<&str> = bad
            .check(&idx, &[&o])
            .unwrap_err()
            .iter()
            .map(|e| e.code)
            .collect();
        assert_eq!(
            codes,
            [
                codes::INVALID_FIELD,
                codes::UNKNOWN_TARGET,
                codes::UNSUPPORTED_EVIDENCE
            ]
        );
        let mut empty = ok.clone();
        empty.effect = widen("economics.revenue.share", None, None, None);
        assert_eq!(
            empty.check(&idx, &[&o]).unwrap_err()[0].code,
            codes::INVALID_FIELD
        );
        // Computed keys are refused before the shape.
        let mut v = serde_json::to_value(&ok).unwrap();
        assert_eq!(challenge_from_json(&v.to_string()).unwrap(), ok);
        v["verdict"] = "REJECT".into();
        assert_eq!(
            challenge_from_json(&v.to_string()).unwrap_err()[0].code,
            codes::COMPUTED_FIELD
        );
    }
}
