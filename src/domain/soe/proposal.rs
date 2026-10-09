//! Architect proposal (O3 stage "Architect"; PRD § 8 step 3): one mechanism
//! for one candidate as data — the `soe.opportunity/1` hypothesis, the basis
//! of every input, a novelty claim and what it predicts. Pure. The model
//! writes a [`ProposalDraft`]; the tool stamps it into a
//! [`MechanismProposal`] (id, cycle, provenance). Nothing computed is ever
//! written by a model: economics, gates, rank keys, confirmations and the
//! action are the engine's (`economics.rs`, `gates.rs`, `rank.rs`,
//! `allocate.rs`).
//!
//! | [`ProposalDraft`] (model-written) | Value |
//! |---|---|
//! | `opportunity` | a `soe.opportunity/1` record (its own header; an active candidate's next version keeps its id) |
//! | `[[bases]]` | `field` (an input of the opportunity's model, `EconomicInputs::inputs`), `basis` = [`Basis`]: `{ kind = "FACT", evidence = [record ids] }` · `{ kind = "INFERENCE", why }` |
//! | `novelty` | `Tier` the model claims — recorded for the O4 novelty-bias grade, never read by the allocation |
//! | `[[forecast]]` | `forecast::ForecastItem`s |
//!
//! | `soe.mechanism_proposal/1` | Value |
//! |---|---|
//! | header | `schema`, `id`, `version` ≥ 1 |
//! | `cycle_id` | the cycle it was made in |
//! | `[provenance]` | [`Provenance`]: `agent`, `model`, `engine`, `skill_sha256`, `generation`, `call_id`, `proposed_at` — stamped by the tool from the stage run |
//! | `[draft]` | the [`ProposalDraft`] |
//!
//! | Check | Code |
//! |---|---|
//! | a computed or stamped key anywhere in a draft ([`COMPUTED_KEYS`]) | `computed_field` (path in full) |
//! | any other unknown key; a bad value | `invalid_record` (serde, `deny_unknown_fields` at every level) |
//! | the opportunity's record rules | its codes (`fake_recurring`, `future_leakage`, …) |
//! | the opportunity dated after the decision | `future_leakage` |
//! | a `signals` id, or a `FACT` evidence id, not in the cycle's packet (`observe::EvidenceIndex`); a `FACT` id not among `signals` | `unsupported_evidence` |
//! | a `FACT` basis without evidence · an `INFERENCE` without why | `fact_without_evidence` · `invalid_field` |
//! | a basis `field` that is not an input · twice | `unknown_field` · `duplicate` |
//! | an input with evidence locators whose basis is not `FACT`, or a locator other than `url:<the url of one of its FACT records>` | `untraced_evidence` |
//! | a forecast item (`forecast::ForecastItem::problems`) | `invalid_time` · `unknown_field` · `invalid_range` |
//!
//! [`ProposalDraft::unsupported`] lists the known inputs with no basis at
//! all — kept, never refused, and shown as unsupported claims in the memo.

// Consumers land with the `soe_propose` tool and the cycle.
#![allow(dead_code)]

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::forecast::ForecastItem;
use super::observe::EvidenceIndex;
use super::opportunity::Opportunity;
use super::record::{validate, Problems, SoeRecord, Tier};
use super::value::{codes, SchemaTag, ValueError};
use crate::domain::lineage::value::{Locator, Time, TimeOrder};

/// Keys a model never writes: computed figures, verdicts, rank inputs,
/// actions and the tool's stamps.
pub const COMPUTED_KEYS: [&str; 33] = [
    "monthly_revenue_collected",
    "monthly_cash_contribution",
    "owner_time_cost",
    "time_adjusted_contribution",
    "initial_capital",
    "payback",
    "payback_time_adjusted",
    "expected_loss",
    "scenarios",
    "occupied_months",
    "run_rate_month",
    "economics_version",
    "inputs_sha256",
    "verdict",
    "gates",
    "failures",
    "next_information",
    "rank",
    "keys",
    "fit",
    "evidence_confidence",
    "confidence",
    "confirmations",
    "action",
    "allocation",
    "provenance",
    "proposed_at",
    "call_id",
    "model",
    "engine",
    "generation",
    "skill_sha256",
    "cycle_id",
];

/// What an input rests on (module table).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "SCREAMING_SNAKE_CASE", deny_unknown_fields)]
pub enum Basis {
    /// Source record ids of the cycle's packet, in full.
    Fact { evidence: Vec<String> },
    /// The model's reasoning: never a fact.
    Inference { why: String },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InputBasis {
    pub field: String,
    pub basis: Basis,
}

/// What a model writes (module table).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProposalDraft {
    pub opportunity: Opportunity,
    pub bases: Vec<InputBasis>,
    pub novelty: Tier,
    pub forecast: Vec<ForecastItem>,
}

/// A known input with no basis.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Unsupported {
    pub field: String,
    /// The estimate as text (`low..base..high`).
    pub value: String,
}

/// Every `computed_field` in `v`, each with its dotted path.
pub fn computed_keys(v: &Value) -> Vec<ValueError> {
    fn walk(v: &Value, path: &str, out: &mut Vec<ValueError>) {
        match v {
            Value::Object(m) => {
                for (k, x) in m {
                    let at = if path.is_empty() {
                        k.clone()
                    } else {
                        format!("{path}.{k}")
                    };
                    if COMPUTED_KEYS.contains(&k.as_str()) {
                        out.push(ValueError::new(
                            codes::COMPUTED_FIELD,
                            format!(
                                "{at}: computed or stamped by the engine, never written by a model"
                            ),
                        ));
                    } else {
                        walk(x, &at, out);
                    }
                }
            }
            Value::Array(a) => {
                for (i, x) in a.iter().enumerate() {
                    walk(x, &format!("{path}[{i}]"), out);
                }
            }
            _ => {}
        }
    }
    let mut out = Vec::new();
    walk(v, "", &mut out);
    out
}

/// A draft from a model's JSON: computed keys refused first (every one
/// listed), then the shape, then the opportunity's own rules.
pub fn draft_from_json(text: &str) -> Result<ProposalDraft, Vec<ValueError>> {
    let v: Value = serde_json::from_str(text)
        .map_err(|e| vec![ValueError::new(codes::INVALID_RECORD, e.to_string())])?;
    let computed = computed_keys(&v);
    if !computed.is_empty() {
        return Err(computed);
    }
    let d: ProposalDraft = serde_json::from_value(v)
        .map_err(|e| vec![ValueError::new(codes::INVALID_RECORD, e.to_string())])?;
    validate(&d.opportunity)?;
    Ok(d)
}

impl ProposalDraft {
    /// The opportunity's input fields.
    pub fn fields(&self) -> Vec<String> {
        self.opportunity
            .economics
            .inputs()
            .into_iter()
            .map(|(f, _)| f)
            .collect()
    }

    /// The basis of `field`, if any.
    pub fn basis(&self, field: &str) -> Option<&Basis> {
        self.bases
            .iter()
            .find(|b| b.field == field)
            .map(|b| &b.basis)
    }

    /// Module table: every problem of this draft against the cycle's
    /// packet, decided at `decided_at`.
    pub fn check(&self, index: &EvidenceIndex, decided_at: &Time) -> Result<(), Vec<ValueError>> {
        let mut p = Problems::default();
        if let Err(e) = validate(&self.opportunity) {
            for x in e {
                p.push(x.code, "opportunity", x.message);
            }
        }
        let o = &self.opportunity;
        p.check(
            o.as_of.order(decided_at) != TimeOrder::After,
            codes::FUTURE_LEAKAGE,
            "opportunity.as_of",
            format_args!("{} is after the decision {decided_at}", o.as_of),
        );
        for (i, id) in o.signals.iter().enumerate() {
            p.check(
                index.contains(id),
                codes::UNSUPPORTED_EVIDENCE,
                &format!("opportunity.signals[{i}]"),
                format_args!("`{id}` is not in the cycle's evidence packet"),
            );
        }
        let fields = self.fields();
        p.unique("bases.field", self.bases.iter().map(|b| b.field.as_str()));
        for (i, b) in self.bases.iter().enumerate() {
            let at = |x: &str| format!("bases[{i}].{x}");
            p.check(
                fields.contains(&b.field),
                codes::UNKNOWN_FIELD,
                &at("field"),
                format_args!(
                    "`{}` is not an input of a {} model",
                    b.field,
                    o.economics.revenue.kind()
                ),
            );
            match &b.basis {
                Basis::Fact { evidence } => {
                    p.check(
                        !evidence.is_empty(),
                        codes::FACT_WITHOUT_EVIDENCE,
                        &at("basis"),
                        "a FACT names the records it rests on",
                    );
                    for id in evidence {
                        if !index.contains(id) {
                            p.push(
                                codes::UNSUPPORTED_EVIDENCE,
                                &at("basis.evidence"),
                                format_args!("`{id}` is not in the cycle's evidence packet"),
                            );
                        } else if !o.signals.contains(id) {
                            p.push(
                                codes::UNSUPPORTED_EVIDENCE,
                                &at("basis.evidence"),
                                format_args!("`{id}` is not among the opportunity's signals"),
                            );
                        }
                    }
                }
                Basis::Inference { why } => p.text(&at("basis.why"), why),
            }
        }
        // Evidence locators trace to the basis's records.
        for (field, input) in o.economics.inputs() {
            let locators: Vec<&Locator> = input
                .evidence()
                .iter()
                .filter(|l| **l != Locator::Unknown)
                .collect();
            if locators.is_empty() {
                continue;
            }
            let urls: BTreeSet<&str> = match self.basis(&field) {
                Some(Basis::Fact { evidence }) => evidence
                    .iter()
                    .filter_map(|id| index.get(id))
                    .map(|r| r.url.as_str())
                    .filter(|u| !u.is_empty())
                    .collect(),
                _ => {
                    p.push(
                        codes::UNTRACED_EVIDENCE,
                        &format!("{field}.evidence"),
                        "evidence locators need a FACT basis naming the records they come from",
                    );
                    continue;
                }
            };
            for l in locators {
                let traced = matches!(l, Locator::Url(u) if urls.contains(u.as_str()));
                p.check(
                    traced,
                    codes::UNTRACED_EVIDENCE,
                    &format!("{field}.evidence"),
                    format_args!("`{l}` is not the url of a record its FACT basis names"),
                );
            }
        }
        for (i, x) in self.forecast.iter().enumerate() {
            x.problems(&format!("forecast[{i}]"), decided_at, &fields, &mut p);
        }
        p.into_result()
    }

    /// Module table: known inputs with no basis, in field order.
    pub fn unsupported(&self) -> Vec<Unsupported> {
        self.opportunity
            .economics
            .inputs()
            .into_iter()
            .filter(|(f, i)| i.is_known() && self.basis(f).is_none())
            .map(|(field, i)| Unsupported {
                value: i.value_text(),
                field,
            })
            .collect()
    }
}

/// Who made a proposal or a challenge — stamped by the tool from the stage
/// run, never by the model.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Provenance {
    pub agent: String,
    pub model: String,
    pub engine: String,
    /// The stage skill's SKILL.md, in full.
    pub skill_sha256: String,
    pub generation: String,
    pub call_id: String,
    pub proposed_at: Time,
}

impl Provenance {
    pub fn problems(&self, p: &mut Problems) {
        for (f, v) in [
            ("provenance.agent", &self.agent),
            ("provenance.model", &self.model),
            ("provenance.engine", &self.engine),
            ("provenance.generation", &self.generation),
            ("provenance.call_id", &self.call_id),
        ] {
            p.text(f, v);
        }
        p.sha256("provenance.skill_sha256", &self.skill_sha256);
        p.known("provenance.proposed_at", &self.proposed_at);
    }
}

/// `soe.mechanism_proposal/1` (module table).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MechanismProposal {
    pub schema: SchemaTag,
    pub id: String,
    pub version: u32,
    pub cycle_id: String,
    pub provenance: Provenance,
    pub draft: ProposalDraft,
}

impl MechanismProposal {
    /// `draft` stamped as proposal `id` of `cycle_id`; validated.
    pub fn stamp(
        id: &str,
        cycle_id: &str,
        draft: ProposalDraft,
        provenance: Provenance,
    ) -> Result<MechanismProposal, Vec<ValueError>> {
        let p = MechanismProposal {
            schema: SchemaTag::v1("mechanism_proposal").map_err(|e| vec![e])?,
            id: id.to_string(),
            version: 1,
            cycle_id: cycle_id.to_string(),
            provenance,
            draft,
        };
        validate(&p)?;
        Ok(p)
    }

    pub fn opportunity(&self) -> &Opportunity {
        &self.draft.opportunity
    }
}

impl SoeRecord for MechanismProposal {
    const RECORD: &'static str = "mechanism_proposal";

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
        self.provenance.problems(p);
        if let Err(e) = validate(&self.draft.opportunity) {
            for x in e {
                p.push(x.code, "draft.opportunity", x.message);
            }
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::domain::soe::economics::scenarios;
    use crate::domain::soe::economics::Metric;
    use crate::domain::soe::forecast::Observable;
    use crate::domain::soe::gates::tests::passing;
    use crate::domain::soe::gates::{gates, CitedKind};
    use crate::domain::soe::observe::tests::{filing, packet, t_ms};
    use crate::domain::soe::observe::{cited_views, EvidenceIndex};
    use crate::domain::soe::profile::tests::synthetic;
    use crate::domain::soe::record::Verdict;
    use crate::domain::soe::value::{Bps, Est};
    use crate::domain::source::SourceRecord;

    const SKILL: &str = "0000000000000000000000000000000000000000000000000000000000000005";

    pub(crate) fn decided() -> Time {
        Time::At(t_ms())
    }

    pub(crate) fn provenance(agent: &str) -> Provenance {
        Provenance {
            agent: agent.into(),
            model: "synthetic-model".into(),
            engine: "openrouter".into(),
            skill_sha256: SKILL.into(),
            generation: "SOE-G0".into(),
            call_id: format!("soe:2026-W41:{agent}:1"),
            proposed_at: "2026-10-05T11:00:00Z".parse().unwrap(),
        }
    }

    /// `o` resting on `facts`: its signals are the records, every revenue
    /// input a FACT basis on all of them (locators = their urls), the
    /// rest INFERENCE.
    pub(crate) fn draft_on(mut o: Opportunity, facts: &[&SourceRecord]) -> ProposalDraft {
        o.signals = facts.iter().map(|r| r.record_id.clone()).collect();
        let ids: Vec<String> = o.signals.clone();
        let urls: Vec<Locator> = facts
            .iter()
            .map(|r| format!("url:{}", r.url).parse().unwrap())
            .collect();
        let fields: Vec<String> = o.economics.inputs().into_iter().map(|(f, _)| f).collect();
        let mut bases = Vec::new();
        for f in fields {
            let revenue = f.starts_with("economics.revenue.");
            if let Some(input) = o.economics.input_mut(&f) {
                use crate::domain::soe::opportunity::InputMut;
                let ev = if revenue { urls.clone() } else { Vec::new() };
                match input {
                    InputMut::Count(a) => a.evidence = ev,
                    InputMut::Share(a) => a.evidence = ev,
                    InputMut::Amount(a) => a.evidence = ev,
                }
            }
            bases.push(InputBasis {
                basis: if revenue && !ids.is_empty() {
                    Basis::Fact {
                        evidence: ids.clone(),
                    }
                } else {
                    Basis::Inference {
                        why: format!("{f}: the operator's last comparable delivery"),
                    }
                },
                field: f,
            });
        }
        ProposalDraft {
            opportunity: o,
            bases,
            novelty: Tier::Medium,
            forecast: vec![ForecastItem {
                observable: Observable::OperatorResolves {
                    question: "does a pilot buyer sign".into(),
                },
                probability: Bps::new(4000).unwrap(),
                resolve_by: "2026-11-15".parse().unwrap(),
            }],
        }
    }

    fn codes_of(r: Result<(), Vec<ValueError>>) -> Vec<&'static str> {
        r.err()
            .unwrap_or_default()
            .into_iter()
            .map(|e| e.code)
            .collect()
    }

    fn world() -> (SourceRecord, EvidenceIndex) {
        let f = filing("0000000001-26-000010", 3);
        let idx = EvidenceIndex::of(&packet(std::slice::from_ref(&f), t_ms()));
        (f, idx)
    }

    #[test]
    fn computed_fields_refused() {
        let (f, _) = world();
        let d = draft_on(passing(), &[&f]);
        let mut v = serde_json::to_value(&d).unwrap();
        assert!(computed_keys(&v).is_empty(), "a clean draft has none");
        assert_eq!(draft_from_json(&v.to_string()).unwrap(), d);
        // A model writing figures, a verdict or the tool's stamps is refused,
        // every one named by path.
        v["opportunity"]["economics"]["time_adjusted_contribution"] = "9000.00".into();
        v["verdict"] = "PASS".into();
        v["provenance"] = serde_json::json!({"model": "x"});
        v["bases"][0]["confirmations"] = 2.into();
        let e = draft_from_json(&v.to_string()).unwrap_err();
        let msgs: Vec<String> = e.iter().map(|x| x.to_string()).collect();
        assert!(
            e.iter().all(|x| x.code == codes::COMPUTED_FIELD),
            "{msgs:?}"
        );
        for path in [
            "opportunity.economics.time_adjusted_contribution",
            "verdict",
            "provenance",
            "bases[0].confirmations",
        ] {
            assert!(
                msgs.iter().any(|m| m.contains(&format!("{path}:"))),
                "{path}: {msgs:?}"
            );
        }
        // Any other unknown key is a shape error.
        let mut w = serde_json::to_value(&d).unwrap();
        w["mood"] = "bullish".into();
        assert_eq!(
            draft_from_json(&w.to_string()).unwrap_err()[0].code,
            codes::INVALID_RECORD
        );
        // A stamped proposal round-trips and keeps the provenance apart.
        let p = MechanismProposal::stamp("p-1", "2026-W41", d, provenance("architect")).unwrap();
        let back: MechanismProposal =
            crate::domain::soe::record::from_json(&serde_json::to_string(&p).unwrap()).unwrap();
        assert_eq!(back, p);
        let mut bad = provenance("architect");
        bad.skill_sha256 = "abc".into();
        assert!(MechanismProposal::stamp("p-2", "2026-W41", p.draft.clone(), bad).is_err());
    }

    #[test]
    fn fact_without_evidence_refused() {
        let (f, idx) = world();
        let at = decided();
        let ok = draft_on(passing(), &[&f]);
        assert_eq!(ok.check(&idx, &at), Ok(()));

        let mut d = ok.clone();
        d.bases[0].basis = Basis::Fact { evidence: vec![] };
        // … and its evidence locators then trace to nothing.
        assert_eq!(
            codes_of(d.check(&idx, &at)),
            [codes::FACT_WITHOUT_EVIDENCE, codes::UNTRACED_EVIDENCE]
        );

        // An id the packet does not hold — in signals or a basis.
        let ghost = format!("sec_edgar:0000000009-26-000099:{}", "0".repeat(64));
        let mut d = ok.clone();
        d.opportunity.signals.push(ghost.clone());
        d.bases[0].basis = Basis::Fact {
            evidence: vec![f.record_id.clone(), ghost.clone()],
        };
        let e = d.check(&idx, &at).unwrap_err();
        assert_eq!(e.len(), 2);
        assert!(e.iter().all(|x| x.code == codes::UNSUPPORTED_EVIDENCE));
        assert!(e[0].message.contains(&ghost), "ids in full: {e:?}");

        // An inference cannot carry evidence locators; a locator must be the
        // url of a record its FACT names.
        let mut d = ok.clone();
        d.bases[0].basis = Basis::Inference {
            why: "seems likely".into(),
        };
        assert_eq!(codes_of(d.check(&idx, &at)), [codes::UNTRACED_EVIDENCE]);
        let mut d = ok.clone();
        if let Some(crate::domain::soe::opportunity::InputMut::Amount(a)) = d
            .opportunity
            .economics
            .input_mut("economics.revenue.partner_monthly_revenue")
        {
            a.evidence = vec!["url:https://example.org/made-up".parse().unwrap()];
        }
        assert_eq!(codes_of(d.check(&idx, &at)), [codes::UNTRACED_EVIDENCE]);

        // A basis on a field the model does not have, twice, an empty why.
        let mut d = ok.clone();
        d.bases[0].field = "economics.revenue.price_per_month".into();
        d.bases[1].field = d.bases[2].field.clone();
        d.bases[3].basis = Basis::Inference { why: " ".into() };
        let got = codes_of(d.check(&idx, &at));
        assert!(got.contains(&codes::UNKNOWN_FIELD), "{got:?}");
        assert!(got.contains(&codes::DUPLICATE), "{got:?}");
        assert!(got.contains(&codes::INVALID_FIELD), "{got:?}");

        // An opportunity written after the decision.
        let mut d = ok.clone();
        d.opportunity.as_of = "2026-10-06".parse().unwrap();
        assert!(codes_of(d.check(&idx, &at)).contains(&codes::FUTURE_LEAKAGE));
    }

    #[test]
    fn missing_value_stays_unknown() {
        let (f, idx) = world();
        let mut d = draft_on(passing(), &[&f]);
        // The model does not know the partner's revenue: it says so.
        let field = "economics.revenue.partner_monthly_revenue";
        if let Some(crate::domain::soe::opportunity::InputMut::Amount(a)) =
            d.opportunity.economics.input_mut(field)
        {
            a.value = Est::unknown("the partner publishes no revenue");
            a.evidence.clear();
        }
        d.bases.retain(|b| b.field != field);
        assert_eq!(d.check(&idx, &decided()), Ok(()));
        // Unknown is not unsupported (nothing is claimed) …
        assert!(d.unsupported().iter().all(|u| u.field != field));
        // … and never becomes 0: the figure names what it lacks, the gates hold.
        let sc = scenarios(&d.opportunity, &synthetic()).unwrap();
        assert_eq!(
            sc.base.time_adjusted_contribution,
            Metric::Unknown {
                fields: vec![field.to_string()]
            }
        );
        let cited = cited_views(&packet(std::slice::from_ref(&f), t_ms()));
        assert_eq!(cited[0].kind, CitedKind::Fact);
        let v = gates(&d.opportunity, &cited, &synthetic(), decided()).unwrap();
        assert_eq!(v.verdict, Verdict::Hold);
        assert!(v.labels().contains(&format!("UNKNOWN_INPUT:{field}")));
        // A known input with no basis is listed as unsupported, never refused.
        let mut silent = draft_on(passing(), &[&f]);
        silent
            .bases
            .retain(|b| b.field != "economics.fixed_costs_per_month");
        assert_eq!(silent.check(&idx, &decided()), Ok(()));
        assert_eq!(
            silent.unsupported(),
            [Unsupported {
                field: "economics.fixed_costs_per_month".into(),
                value: "40.00..40.00..40.00".into()
            }]
        );
    }
}
