//! `ledger:<account>` — a `ResultSource` over a paper `ledger.db` (the vault
//! copy of a forward run): the account graded by `domain::xm::grade`
//! (`tengu evidence grade`), so `tengu lineage verify --evidence` recomputes
//! a forward experiment's figures from the ledger itself.
//!
//! | Field | From |
//! |---|---|
//! | `n` | trades (open → close per instrument opening) |
//! | `mean_net_bps` | mean of the trades' net bps on filled entry notional |
//! | `net_usd` | Σ trade net (gross − fees − funding paid) |
//! | refusal | an account that does not reconcile (any check FAIL) is an error naming the checks — its figures are not evidence |

use std::path::{Path, PathBuf};

use crate::adapters::outbound::evidence::ledger_reader::SqliteLedgerReader;
use crate::domain::xm::grade::{grade_account, CheckStatus};
use crate::ports::evidence::LedgerSource;
use crate::ports::lineage::{Extracted, ResultSource};

/// The `ledger:` kind of `extract`.
pub(crate) struct LedgerGrade;

impl ResultSource for LedgerGrade {
    fn extract(&self, path: &Path, extract: &str) -> Option<Result<Extracted, String>> {
        let account = extract.strip_prefix("ledger:")?;
        Some(grade(path.to_path_buf(), account))
    }
}

fn grade(path: PathBuf, account: &str) -> Result<Extracted, String> {
    let rows = SqliteLedgerReader::new(path.clone())
        .read()
        .map_err(|e| format!("{}: {e:#}", path.display()))?;
    let g = grade_account(&rows, account)?;
    let failed: Vec<&str> = g
        .checks
        .iter()
        .filter(|c| c.status == CheckStatus::Fail)
        .map(|c| c.check.as_str())
        .collect();
    if !failed.is_empty() {
        return Err(format!(
            "account `{account}` does not reconcile ({})",
            failed.join(", ")
        ));
    }
    Ok(Extracted {
        n: Some(g.totals.trades as u64),
        mean_net_bps: g.totals.mean_net_bps,
        ci95_bps: None,
        net_usd: Some(g.totals.net_usd),
        t_stat: None,
    })
}
