//! Search accounting input (`docs/lineage-2026-10-06.md` § 5, handoff § 21):
//! every backtest run dir and holdout read of the scanned state dirs
//! (`<TENGU_HOME>/state/<state>/backtests/`), read through
//! `ports::lineage::AttemptSource` — never written.
//!
//! | Rule | Value |
//! |---|---|
//! | Run dirs | `<run id>/` and `keep-<run id>/` with a `report.json` (`run_id`, `strategy`, `kind`, `spec_sha256`, `split`) |
//! | Holdout reads | the lines of `holdout-reads.jsonl` (`run_id`, `spec_sha256`, `strategy`, `time`) |
//! | Problems | an unreadable dir or line is listed, never dropped silently |
//! | Order | by state, then run id |

use crate::domain::lineage::query::Attempts;
use crate::ports::lineage::AttemptSource;

/// Module table: the attempts of `states`.
pub(crate) fn scan(source: &dyn AttemptSource, states: &[String]) -> Attempts {
    let mut out = Attempts {
        states: states.to_vec(),
        ..Default::default()
    };
    for state in states {
        let (runs, problems) = source.runs(state);
        out.runs.extend(runs);
        out.problems.extend(problems);
        let (reads, problems) = source.holdout_reads(state);
        out.holdout_reads.extend(reads);
        out.problems.extend(problems);
    }
    out.runs
        .sort_by(|a, b| (&a.state, &a.run_id).cmp(&(&b.state, &b.run_id)));
    out
}
