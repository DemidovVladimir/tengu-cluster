//! Weekly memo (O3 stage "Report"; roadmap O3 "private memo with
//! facts / inference split, uncertainty and next information value"; PRD
//! § 10 internal evidence): one Markdown page per decided week. Pure: the
//! decided week, its proposals and challenges in, text out. Private — it
//! lives with the cycle under `<TENGU_HOME>/state/soe/`, never published.
//!
//! | Section | From | Rule |
//! |---|---|---|
//! | Verdict | the portfolio + `allocate::Note`s | `HOLD` week or not, counts, allocation; per candidate its list, action and why |
//! | Facts | `observe::EvidenceIndex` | every record a proposal cites (signals, `FACT` bases): id in full, event, class, source, published / observed, independent confirmations, citing candidates — no model text |
//! | Inference | `INFERENCE` bases | candidate, field, value, why (fenced), model and generation from the provenance |
//! | Computed | the portfolio only | rank, action and its fields, rank keys, gates, allocation, hashes — every number here is a portfolio field |
//! | Unsupported claims | `ProposalDraft::unsupported` | known inputs with no basis |
//! | Unknowns | the challenged opportunities | every `UNKNOWN` input with its reason |
//! | Challenges | `challenge::Applied` | kind, effect, what moved (before → after), what was ignored, the claim (fenced) |
//! | Next information worth buying | `next_information` | in the portfolio's order |
//!
//! Model and source text appears only inside a `source-text` fence
//! (`domain::source::fence_untrusted`) after one [`FENCE_NOTE`]; a `|` in a
//! table cell is escaped. Ids and hashes print in full.

// Consumers land with the cycle (`memo.md` per cycle) and `tengu soe show`.
#![allow(dead_code)]

use std::collections::BTreeMap;
use std::fmt::Write;

use super::allocate::Week;
use super::challenge::{Challenge, Effect};
use super::portfolio::{PortfolioAction, WeeklyPortfolio};
use super::proposal::{Basis, MechanismProposal};
use crate::domain::lineage::value::Time;
use crate::domain::source::{fence_untrusted, FENCE_NOTE};

/// What a memo is rendered from.
#[derive(Debug, Clone, Copy)]
pub struct MemoInput<'a> {
    pub week: &'a Week,
    pub proposals: &'a [MechanismProposal],
    pub challenges: &'a [Challenge],
}

/// A table cell: `|` escaped, line breaks flattened.
fn cell(s: &str) -> String {
    s.replace('|', "\\|").replace(['\n', '\r'], " ")
}

fn fenced(id: &str, field: &str, text: &str) -> String {
    cell(&fence_untrusted(id, field, text))
}

fn when(ms: i64) -> String {
    Time::At(ms).to_string()
}

/// The fields of an action, as `name=value` (portfolio values only).
fn action_fields(a: &PortfolioAction) -> String {
    match a {
        PortfolioAction::CheapTest {
            max_cash,
            max_hours,
        } => format!("max_cash={max_cash} max_hours={max_hours}"),
        PortfolioAction::Diligence {
            questions,
            max_next_tranche,
        } => format!(
            "max_next_tranche={max_next_tranche} questions={}",
            questions.len()
        ),
        PortfolioAction::Reprice { min_price } => format!("min_price={min_price}"),
        PortfolioAction::ContinueActive | PortfolioAction::Hold | PortfolioAction::Reject => {
            String::new()
        }
    }
}

fn verdict(out: &mut String, m: &MemoInput) {
    let p = &m.week.portfolio;
    let _ = writeln!(out, "## Verdict\n");
    match &p.hold_rationale {
        Some(r) if p.is_hold() => {
            let _ = writeln!(out, "- `HOLD` week: {}", cell(r));
        }
        Some(r) => {
            let _ = writeln!(out, "- {}", cell(r));
        }
        None => {}
    }
    let _ = writeln!(
        out,
        "- {} ranked · {} held · {} rejected; allocated {} h · {} {}\n",
        p.ranked.len(),
        p.held.len(),
        p.rejected.len(),
        p.allocation.owner_hours,
        p.allocation.cash,
        p.currency
    );
    let _ = writeln!(
        out,
        "| Candidate | List | Action | Why |\n|---|---|---|---|"
    );
    let notes: BTreeMap<&str, &str> = m
        .week
        .notes
        .iter()
        .map(|n| (n.id.as_str(), n.why.as_str()))
        .collect();
    let rows = p
        .ranked
        .iter()
        .map(|r| (r.id.as_str(), "ranked", &r.action))
        .chain(p.held.iter().map(|r| (r.id.as_str(), "held", &r.action)))
        .chain(
            p.rejected
                .iter()
                .map(|r| (r.id.as_str(), "rejected", &r.action)),
        );
    for (id, list, action) in rows {
        let _ = writeln!(
            out,
            "| `{id}` | {list} | `{}` | {} |",
            action.kind(),
            cell(notes.get(id).copied().unwrap_or(""))
        );
    }
    let _ = writeln!(out);
}

fn facts(out: &mut String, m: &MemoInput) {
    let _ = writeln!(out, "## Facts\n");
    // Record id → the candidates citing it.
    let mut cited: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    for x in m.proposals {
        let o = x.opportunity();
        let ids = o
            .signals
            .iter()
            .chain(x.draft.bases.iter().flat_map(|b| match &b.basis {
                Basis::Fact { evidence } => evidence.as_slice(),
                Basis::Inference { .. } => &[],
            }));
        for id in ids {
            let by = cited.entry(id.as_str()).or_default();
            if !by.contains(&o.id.as_str()) {
                by.push(o.id.as_str());
            }
        }
    }
    if cited.is_empty() {
        let _ = writeln!(out, "(none cited)\n");
        return;
    }
    let _ = writeln!(
        out,
        "| Evidence | Event | Class | Source | Published | Observed | Confirmations | Cited by |\n|---|---|---|---|---|---|---|---|"
    );
    for (id, by) in cited {
        let Some(e) = m.week.evidence.get(id) else {
            let _ = writeln!(
                out,
                "| `{id}` | — | not in the packet | — | — | — | 0 | {} |",
                by.join(", ")
            );
            continue;
        };
        let class = e
            .source_class
            .map_or_else(|| e.state.as_str().to_string(), |c| c.as_str().to_string());
        let _ = writeln!(
            out,
            "| `{id}` | `{}` | {class} | `{}`{} | {} | {} | {} ({}) | {} |",
            e.event_key,
            e.source_id,
            e.origin
                .as_deref()
                .map(|o| format!(" (copy of `{o}`)"))
                .unwrap_or_default(),
            e.published_ms.map(when).unwrap_or_else(|| "—".into()),
            when(e.observed_ms),
            e.confirmations,
            e.confidence.as_str(),
            by.join(", ")
        );
    }
    let _ = writeln!(out);
}

fn inference(out: &mut String, m: &MemoInput) {
    let _ = writeln!(out, "## Inference\n");
    let _ = writeln!(
        out,
        "| Candidate | Field | Value | Why | Model | Generation |\n|---|---|---|---|---|---|"
    );
    let mut any = false;
    for x in m.proposals {
        let o = x.opportunity();
        for b in &x.draft.bases {
            let Basis::Inference { why } = &b.basis else {
                continue;
            };
            any = true;
            let value = o
                .economics
                .input(&b.field)
                .map(|i| i.value_text())
                .unwrap_or_default();
            let _ = writeln!(
                out,
                "| `{}` | `{}` | {} | {} | {} | {} |",
                o.id,
                b.field,
                cell(&value),
                fenced(&x.id, &b.field, why),
                cell(&x.provenance.model),
                cell(&x.provenance.generation)
            );
        }
    }
    if !any {
        let _ = writeln!(out, "| — | — | — | — | — | — |");
    }
    let _ = writeln!(out);
}

fn computed(out: &mut String, p: &WeeklyPortfolio) {
    let _ = writeln!(out, "## Computed\n");
    let _ = writeln!(
        out,
        "| Candidate | Version | List | Rank | Action | Fields | Rank keys · gates |\n|---|---|---|---|---|---|---|"
    );
    for r in &p.ranked {
        let keys: Vec<String> = r
            .keys
            .iter()
            .map(|k| {
                let name = serde_json::to_value(k.key)
                    .ok()
                    .and_then(|v| v.as_str().map(String::from))
                    .unwrap_or_default();
                format!("{name}={}", k.value)
            })
            .collect();
        let _ = writeln!(
            out,
            "| `{}` | {} | ranked | {} | `{}` | {} | {} |",
            r.id,
            r.opportunity_version,
            r.rank,
            r.action.kind(),
            action_fields(&r.action),
            cell(&keys.join("; "))
        );
    }
    for (list, rows) in [("held", &p.held), ("rejected", &p.rejected)] {
        for r in rows {
            let _ = writeln!(
                out,
                "| `{}` | {} | {list} | — | `{}` | {} | {} |",
                r.id,
                r.opportunity_version,
                r.action.kind(),
                action_fields(&r.action),
                r.gates.join(", ")
            );
            if let PortfolioAction::Diligence { questions, .. } = &r.action {
                for q in questions {
                    let _ = writeln!(out, "| | | | | | question | {} |", cell(q));
                }
            }
        }
    }
    let _ = writeln!(
        out,
        "\n- allocation: owner_hours={} cash={} {}\n- profile_sha256 `{}` · inputs_sha256 `{}` · economics_version {}\n",
        p.allocation.owner_hours,
        p.allocation.cash,
        p.currency,
        p.profile_sha256,
        p.inputs_sha256,
        p.economics_version
    );
}

fn unsupported(out: &mut String, m: &MemoInput) {
    let _ = writeln!(out, "## Unsupported claims\n");
    let rows: Vec<String> = m
        .proposals
        .iter()
        .flat_map(|x| {
            x.draft.unsupported().into_iter().map(move |u| {
                format!(
                    "| `{}` | `{}` | {} | no basis |",
                    x.opportunity().id,
                    u.field,
                    cell(&u.value)
                )
            })
        })
        .collect();
    if rows.is_empty() {
        let _ = writeln!(out, "(none: every known input names a basis)\n");
        return;
    }
    let _ = writeln!(
        out,
        "| Candidate | Field | Value | Basis |\n|---|---|---|---|"
    );
    for r in rows {
        let _ = writeln!(out, "{r}");
    }
    let _ = writeln!(out);
}

fn unknowns(out: &mut String, m: &MemoInput) {
    let _ = writeln!(out, "## Unknowns\n");
    let mut rows = Vec::new();
    for d in &m.week.decided {
        let o = &d.applied.opportunity;
        for (field, i) in o.economics.inputs() {
            if !i.is_known() {
                rows.push(format!(
                    "| `{}` | `{field}` | {} |",
                    o.id,
                    cell(i.unknown_reason().unwrap_or("no reason given"))
                ));
            }
        }
    }
    if rows.is_empty() {
        let _ = writeln!(out, "(none)\n");
        return;
    }
    let _ = writeln!(out, "| Candidate | Field | Reason |\n|---|---|---|");
    for r in rows {
        let _ = writeln!(out, "{r}");
    }
    let _ = writeln!(out);
}

fn challenges(out: &mut String, m: &MemoInput) {
    let _ = writeln!(out, "## Challenges\n");
    if m.challenges.is_empty() {
        let _ = writeln!(out, "(none)\n");
        return;
    }
    let _ = writeln!(
        out,
        "| Challenge | Target | Kind | Effect | Result | Claim |\n|---|---|---|---|---|---|"
    );
    let mut sorted: Vec<&Challenge> = m.challenges.iter().collect();
    sorted.sort_by(|a, b| a.id.cmp(&b.id));
    for c in sorted {
        let d = &c.draft;
        let effect = match &d.effect {
            Effect::Widen { field, .. } => format!("WIDEN `{field}`"),
            Effect::BlockGate { gate } => format!("BLOCK_GATE `{}`", gate.as_str()),
            Effect::None => "NONE".into(),
        };
        let applied = m
            .week
            .decided
            .iter()
            .find(|x| x.applied.opportunity.id == d.target)
            .map(|x| &x.applied);
        let mut result: Vec<String> = Vec::new();
        if let Some(a) = applied {
            result.extend(
                a.changes
                    .iter()
                    .filter(|x| x.challenge == c.id)
                    .map(|x| format!("{}: {} → {}", x.field, x.before, x.after)),
            );
            result.extend(
                a.ignored
                    .iter()
                    .filter(|x| x.challenge == c.id)
                    .map(|x| format!("ignored: {}", x.why)),
            );
            result.extend(
                a.holds
                    .iter()
                    .filter(|h| h.field.as_deref() == Some(&format!("challenge.{}", c.id)))
                    .map(|h| format!("held: {}", h.code.as_str())),
            );
        }
        if result.is_empty() {
            result.push("no change".into());
        }
        let kind = serde_json::to_value(d.kind)
            .ok()
            .and_then(|v| v.as_str().map(String::from))
            .unwrap_or_default();
        let _ = writeln!(
            out,
            "| `{}` | `{}` | {kind} | {} | {} | {} |",
            c.id,
            d.target,
            effect,
            cell(&result.join("; ")),
            fenced(&c.id, "claim", &d.claim)
        );
    }
    let _ = writeln!(out);
}

/// Module table: the memo of `m`.
pub fn render_memo(m: &MemoInput) -> String {
    let p = &m.week.portfolio;
    let mut out = String::new();
    let _ = writeln!(
        out,
        "# SOE memo — `{}` (week {})\n\nDecided at {} · private (PRD § 10): never published.\n\n{FENCE_NOTE}\n",
        p.id, p.week, p.as_of
    );
    verdict(&mut out, m);
    facts(&mut out, m);
    inference(&mut out, m);
    computed(&mut out, p);
    unsupported(&mut out, m);
    unknowns(&mut out, m);
    challenges(&mut out, m);
    let _ = writeln!(out, "## Next information worth buying\n");
    if p.next_information.is_empty() {
        let _ = writeln!(out, "(none)");
    }
    for (i, f) in p.next_information.iter().enumerate() {
        let _ = writeln!(out, "{}. `{f}`", i + 1);
    }
    out
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;
    use crate::domain::canonical::canonical_json;
    use crate::domain::soe::allocate::tests::{automation, decide, post, proposals};
    use crate::domain::soe::challenge::tests::{challenge, widen};
    use crate::domain::soe::challenge::ChallengeKind;
    use crate::domain::soe::observe::tests::filing;
    use crate::domain::soe::record::Tier;
    use crate::domain::source::SourceRecord;

    /// The section `name` of `memo` (to the next `## `).
    fn section<'a>(memo: &'a str, name: &str) -> &'a str {
        let start = memo
            .find(&format!("## {name}\n"))
            .unwrap_or_else(|| panic!("no section {name}"));
        let rest = &memo[start + 3..];
        let end = rest.find("\n## ").map_or(rest.len(), |e| e + 1);
        &rest[..end]
    }

    struct Fixture {
        records: Vec<SourceRecord>,
        props: Vec<MechanismProposal>,
        challenges: Vec<Challenge>,
    }

    /// `a` on a primary filing with one INFERENCE basis carrying a marker;
    /// `b` on two demand posts, one known input left without a basis; a
    /// Critic widening `b`'s hours.
    fn fixture() -> Fixture {
        let f = filing("0000000001-26-000501", 3);
        let (p1, p2) = (post("p-51", 3), post("p-52", 2));
        let mut props = proposals(vec![
            (automation("a", "\"600.00\""), vec![&f], Tier::Medium),
            (automation("b", "\"600.00\""), vec![&p1, &p2], Tier::High),
        ]);
        let ramp = props[0]
            .draft
            .bases
            .iter_mut()
            .find(|b| b.field == "economics.ramp_months")
            .unwrap();
        ramp.basis = Basis::Inference {
            why: "MARKER-INFERENCE | two months, as the last rollout".into(),
        };
        props[1]
            .draft
            .bases
            .retain(|b| b.field != "economics.fixed_costs_per_month");
        let challenges = vec![challenge(
            "k1",
            "b",
            ChallengeKind::HiddenLabor,
            widen(
                "economics.owner_hours_per_month",
                None,
                Some("12"),
                Some("20"),
            ),
        )];
        Fixture {
            records: vec![f, p1, p2],
            props,
            challenges,
        }
    }

    fn memo_of(x: &Fixture) -> (Week, String) {
        let w = decide(&x.records, &x.props, &x.challenges, &BTreeMap::new());
        let memo = render_memo(&MemoInput {
            week: &w,
            proposals: &x.props,
            challenges: &x.challenges,
        });
        (w, memo)
    }

    #[test]
    fn facts_and_inference_split() {
        let x = fixture();
        let (_, memo) = memo_of(&x);
        let facts = section(&memo, "Facts");
        let inference = section(&memo, "Inference");
        // Facts: every cited record in full, its confirmations, no model text.
        for r in &x.records {
            assert!(facts.contains(&format!("`{}`", r.record_id)), "{facts}");
        }
        assert!(facts.contains("| 1 (confirmed) |"), "{facts}");
        assert!(facts.contains("customer_demand"), "{facts}");
        assert!(!facts.contains("MARKER-INFERENCE"));
        assert!(!memo
            .split("## Inference")
            .next()
            .unwrap()
            .contains("MARKER"));
        // Inference: fenced, with the model and the generation; a `|` escaped.
        assert!(inference.contains("`economics.ramp_months`"), "{inference}");
        assert!(
            inference.contains("<source-text record=\"p-a\" field=\"economics.ramp_months\">MARKER-INFERENCE \\| two months"),
            "{inference}"
        );
        assert!(
            inference.contains("| synthetic-model | SOE-G0 |"),
            "{inference}"
        );
        assert_eq!(memo.matches(FENCE_NOTE).count(), 1);
        // The Critic's claim is fenced too; its effect is reported.
        let ch = section(&memo, "Challenges");
        assert!(
            ch.contains("economics.owner_hours_per_month: 9..9..9 → 9..12..20"),
            "{ch}"
        );
        assert!(
            ch.contains("<source-text record=\"k1\" field=\"claim\">"),
            "{ch}"
        );
    }

    #[test]
    fn memo_numbers_trace_to_portfolio() {
        let x = fixture();
        let (w, memo) = memo_of(&x);
        let json = canonical_json(&serde_json::to_value(&w.portfolio).unwrap());
        let computed = section(&memo, "Computed");
        assert!(computed.contains(&w.portfolio.inputs_sha256));
        let numbers: Vec<&str> = computed
            .split(|c: char| !(c.is_ascii_digit() || c == '.'))
            .map(|t| t.trim_matches('.'))
            .filter(|t| t.chars().any(|c| c.is_ascii_digit()))
            .collect();
        assert!(numbers.len() > 10, "{computed}");
        for n in numbers {
            assert!(
                json.contains(n),
                "`{n}` in Computed is not a portfolio field\n{computed}"
            );
        }
        // The verdict counts are the portfolio's too.
        let v = section(&memo, "Verdict");
        assert!(v.contains(&format!(
            "{} ranked · {} held · {} rejected; allocated {} h · {} EUR",
            w.portfolio.ranked.len(),
            w.portfolio.held.len(),
            w.portfolio.rejected.len(),
            w.portfolio.allocation.owner_hours,
            w.portfolio.allocation.cash
        )));
        // A rerun is byte-identical.
        assert_eq!(memo_of(&x).1, memo);
    }

    #[test]
    fn unsupported_claims_listed() {
        let x = fixture();
        let (_, memo) = memo_of(&x);
        let u = section(&memo, "Unsupported claims");
        assert!(
            u.contains(
                "| `b` | `economics.fixed_costs_per_month` | 40.00..40.00..40.00 | no basis |"
            ),
            "{u}"
        );
        assert!(!u.contains("| `a` |"), "{u}");
        // Unknowns list an UNKNOWN input with its reason.
        let mut y = fixture();
        let field = "economics.revenue.churn_per_month";
        if let Some(crate::domain::soe::opportunity::InputMut::Share(a)) =
            y.props[0].draft.opportunity.economics.input_mut(field)
        {
            a.value = crate::domain::soe::value::Est::unknown("no cohort data yet");
            a.evidence.clear();
        }
        y.props[0].draft.bases.retain(|b| b.field != field);
        let (_, memo) = memo_of(&y);
        let k = section(&memo, "Unknowns");
        assert!(
            k.contains(&format!("| `a` | `{field}` | no cohort data yet |")),
            "{k}"
        );
        let n = section(&memo, "Next information worth buying");
        assert!(n.contains(&format!("1. `{field}`")), "{n}");
    }
}
