//! `fetch_accounts` — cache-through account reads: fresh `acct/1:<pubkey>`
//! rows from the observation store are reused, **one** `getMultipleAccounts`
//! (chunked by `SolanaRpc`) covers the rest, and the new reads are put back
//! (slot-monotonic upsert). This is the phase-5 seam: a stream that writes
//! `acct/1` rows makes typed builders RPC-free.
//!
//! | Rule | Value |
//! |---|---|
//! | Reuse a row | schema `acct/1`, `is_fresh(now, max_age_ms)`, and `slot >= min_slot` when `min_slot` is set |
//! | GMA pin | `minContextSlot = max(min_slot, highest reused slot)` — live reads are never older than reused ones |
//! | Row TTL | [`ACCOUNT_ROW_TTL_MS`]; the caller's `max_age_ms` is the effective bound (`max_age_ms = 0` never reuses) |
//! | Absent accounts | cached too (a legitimate answer, e.g. a flat perp side) |
//! | Store failure | warn, read everything live (doctrine #4) |
//! | RPC failure | `Err` ([`super::rpc::RpcError`] in the chain → `rpc::read_error`) |

use std::collections::BTreeSet;

use anyhow::Result;
use tracing::warn;

use super::rpc::SolanaRpc;
use crate::domain::observation::{ObsSource, Observation, Observed};
use crate::domain::solana::{AccountRead, AccountSet, Pubkey};
use crate::ports::observation::ObservationStore;

/// TTL stored on `acct/1` rows — the longest any caller may reuse one.
pub(crate) const ACCOUNT_ROW_TTL_MS: u64 = 60_000;
/// `Observation::tool` of rows written here.
pub(crate) const ACCOUNTS_TOOL: &str = "solana_accounts";

/// Store key of an account row: `acct/1:<pubkey>`.
pub(crate) fn account_key(key: &Pubkey) -> String {
    Observation::key_for(AccountRead::SCHEMA, &key.to_string())
}

/// Accounts `keys` (deduplicated) as one [`AccountSet`]; see the module table.
pub(crate) async fn fetch_accounts(
    rpc: &SolanaRpc,
    store: Option<&dyn ObservationStore>,
    keys: &[Pubkey],
    max_age_ms: u64,
    min_slot: Option<u64>,
    now_ms: i64,
) -> Result<AccountSet> {
    let mut seen = BTreeSet::new();
    let uniq: Vec<Pubkey> = keys.iter().copied().filter(|k| seen.insert(*k)).collect();
    let mut set = AccountSet::default();
    if uniq.is_empty() {
        return Ok(set);
    }

    let mut missing: Vec<Pubkey> = Vec::new();
    let mut reused_max: Option<u64> = None;
    let cached = match store {
        Some(s) if max_age_ms > 0 => {
            let row_keys: Vec<String> = uniq.iter().map(account_key).collect();
            match s.get_many(&row_keys).await {
                Ok(rows) if rows.len() == uniq.len() => Some(rows),
                Ok(rows) => {
                    warn!(
                        got = rows.len(),
                        want = uniq.len(),
                        "observation store returned a wrong row count; reading accounts live"
                    );
                    None
                }
                Err(e) => {
                    let error = format!("{e:#}");
                    warn!(%error, "observation store read failed; reading accounts live");
                    None
                }
            }
        }
        _ => None,
    };
    match cached {
        Some(rows) => {
            for (key, row) in uniq.iter().zip(rows) {
                match row.and_then(|r| reusable(&r, key, now_ms, max_age_ms, min_slot)) {
                    Some(read) => {
                        reused_max = reused_max.max(Some(read.slot));
                        set.insert(read);
                    }
                    None => missing.push(*key),
                }
            }
        }
        None => missing = uniq,
    }

    if !missing.is_empty() {
        let pin = match (min_slot, reused_max) {
            (Some(a), Some(b)) => Some(a.max(b)),
            (a, b) => a.or(b),
        };
        let (_, reads) = rpc.get_multiple_accounts(&missing, pin).await?;
        for read in reads {
            if let Some(s) = store {
                let obs = Observation::of(
                    ACCOUNTS_TOOL,
                    &read,
                    now_ms,
                    ACCOUNT_ROW_TTL_MS,
                    ObsSource::Live,
                );
                if let Err(e) = s.put(&obs).await {
                    let error = format!("{e:#}");
                    warn!(key = %obs.key, %error, "observation store write failed");
                }
            }
            set.insert(read);
        }
    }
    Ok(set)
}

/// The cached read when the row may stand in for a live one.
fn reusable(
    row: &Observation,
    key: &Pubkey,
    now_ms: i64,
    max_age_ms: u64,
    min_slot: Option<u64>,
) -> Option<AccountRead> {
    if row.schema != AccountRead::SCHEMA || !row.is_fresh(now_ms, max_age_ms) {
        return None;
    }
    let read: AccountRead = row.typed().ok()?;
    if read.pubkey != *key || min_slot.is_some_and(|m| read.slot < m) {
        return None;
    }
    Some(read)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    use async_trait::async_trait;
    use serde_json::Value;

    use crate::adapters::outbound::observations::SqliteObservationStore;
    use crate::adapters::outbound::solana::rpc::tests::{fake_rpc, FakeTransport};
    use crate::adapters::outbound::solana::rpc::{read_error, RpcError};
    use crate::application::observe::tests::MemStore;
    use crate::domain::observation::{ErrorClass, ObsStatus};
    use crate::domain::solana::{ids, AccountState};

    fn k(i: u8) -> Pubkey {
        Pubkey([i; 32])
    }

    fn acct(key: Pubkey, lamports: u64) -> AccountRead {
        AccountRead::from_bytes(key, 0, ids::key(ids::TOKEN), lamports, &[lamports as u8; 3])
    }

    /// Keys requested by each GMA, as strings.
    fn gma_keys(t: &FakeTransport) -> Vec<Vec<String>> {
        t.gma_params()
            .iter()
            .map(|p| {
                p[0].as_array()
                    .unwrap()
                    .iter()
                    .map(|v| v.as_str().unwrap().to_string())
                    .collect()
            })
            .collect()
    }

    fn setup(slot: u64) -> Arc<FakeTransport> {
        let t = Arc::new(FakeTransport::at_slot(slot));
        t.put_account(acct(k(1), 10));
        t.put_account(acct(k(2), 20));
        t
    }

    #[tokio::test]
    async fn without_a_store_reads_everything_in_one_gma() {
        let t = setup(100);
        let rpc = fake_rpc(&t);
        let set = fetch_accounts(&rpc, None, &[k(1), k(2), k(1), k(3)], 5_000, None, 0)
            .await
            .unwrap();
        assert_eq!(
            gma_keys(&t),
            vec![vec![k(1).to_string(), k(2).to_string(), k(3).to_string()]]
        );
        assert_eq!(set.accounts.len(), 3);
        assert_eq!(set.get(&k(2)).unwrap().lamports(), Some(20));
        assert!(!set.get(&k(3)).unwrap().exists(), "unknown key is Absent");
        assert_eq!((set.slot_min, set.slot_max), (100, 100));
        assert!(fetch_accounts(&rpc, None, &[], 5_000, None, 0)
            .await
            .unwrap()
            .accounts
            .is_empty());
        assert_eq!(t.gma_params().len(), 1, "no request for no keys");
    }

    #[tokio::test]
    async fn fresh_rows_are_reused_and_absent_is_cached() {
        let t = setup(100);
        let rpc = fake_rpc(&t);
        let store = MemStore::default();
        let keys = [k(1), k(2), k(3)];
        let a = fetch_accounts(&rpc, Some(&store), &keys, 5_000, None, 1_000)
            .await
            .unwrap();
        assert_eq!(t.gma_params().len(), 1);
        let row = store.get(&account_key(&k(3))).await.unwrap().unwrap();
        assert_eq!(
            (row.status, row.slot, row.ttl_ms),
            (ObsStatus::Ok, Some(100), ACCOUNT_ROW_TTL_MS)
        );
        assert_eq!(row.tool, ACCOUNTS_TOOL);
        // Within max_age: served entirely from the store.
        let b = fetch_accounts(&rpc, Some(&store), &keys, 5_000, None, 5_999)
            .await
            .unwrap();
        assert_eq!(t.gma_params().len(), 1, "no second GMA");
        assert_eq!(a, b);
        // max_age 0 forces a live read.
        fetch_accounts(&rpc, Some(&store), &keys, 0, None, 6_000)
            .await
            .unwrap();
        assert_eq!(t.gma_params().len(), 2);
    }

    #[tokio::test]
    async fn stale_rows_are_refetched_with_the_reused_slot_as_pin() {
        let t = setup(100);
        let rpc = fake_rpc(&t);
        let store = MemStore::default();
        fetch_accounts(&rpc, Some(&store), &[k(1)], 5_000, None, 0)
            .await
            .unwrap();
        t.slot.store(130, std::sync::atomic::Ordering::SeqCst);
        fetch_accounts(&rpc, Some(&store), &[k(2)], 5_000, None, 4_000)
            .await
            .unwrap();
        // k(1) fresh (slot 100), k(2) fresh (slot 130); k(1) row is 6 s old
        // at t=6_000 with max_age 5 s → stale; k(2) reused at slot 130.
        t.put_account(acct(k(1), 11));
        let set = fetch_accounts(&rpc, Some(&store), &[k(1), k(2)], 5_000, None, 6_000)
            .await
            .unwrap();
        let calls = t.gma_params();
        assert_eq!(calls.len(), 3);
        assert_eq!(gma_keys(&t)[2], vec![k(1).to_string()]);
        assert_eq!(
            calls[2][1]["minContextSlot"], 130,
            "pinned to the reused slot"
        );
        assert_eq!(set.get(&k(1)).unwrap().lamports(), Some(11));
        assert_eq!((set.slot_min, set.slot_max), (130, 130));
    }

    #[tokio::test]
    async fn min_slot_skips_older_rows_and_pins_the_gma() {
        let t = setup(100);
        let rpc = fake_rpc(&t);
        let store = MemStore::default();
        fetch_accounts(&rpc, Some(&store), &[k(1), k(2)], 5_000, None, 0)
            .await
            .unwrap();
        t.slot.store(120, std::sync::atomic::Ordering::SeqCst);
        let set = fetch_accounts(&rpc, Some(&store), &[k(1), k(2)], 5_000, Some(110), 1)
            .await
            .unwrap();
        let calls = t.gma_params();
        assert_eq!(
            calls.len(),
            2,
            "rows at slot 100 < min_slot 110 are not reused"
        );
        assert_eq!(calls[1][1]["minContextSlot"], 110);
        assert_eq!(set.slot_min, 120);
        // A min_slot the node has not reached is a Transient error (after one retry).
        let e = fetch_accounts(&rpc, Some(&store), &[k(1)], 5_000, Some(999), 2)
            .await
            .unwrap_err();
        assert_eq!(read_error("accounts", &e).class, ErrorClass::Transient);
    }

    struct BrokenStore;

    #[async_trait]
    impl ObservationStore for BrokenStore {
        async fn get(&self, _key: &str) -> Result<Option<Observation>> {
            anyhow::bail!("disk on fire")
        }
        async fn get_many(&self, _keys: &[String]) -> Result<Vec<Option<Observation>>> {
            anyhow::bail!("disk on fire")
        }
        async fn put(&self, _obs: &Observation) -> Result<bool> {
            anyhow::bail!("disk on fire")
        }
    }

    #[tokio::test]
    async fn store_failures_fall_back_to_live_reads() {
        let t = setup(100);
        let rpc = fake_rpc(&t);
        let set = fetch_accounts(&rpc, Some(&BrokenStore), &[k(1), k(2)], 5_000, None, 0)
            .await
            .unwrap();
        assert_eq!(set.accounts.len(), 2);
        assert_eq!(t.gma_params().len(), 1);
    }

    #[tokio::test]
    async fn rpc_errors_propagate_classified() {
        let t = setup(100);
        let rpc = fake_rpc(&t);
        t.push(Ok(
            crate::adapters::outbound::solana::rpc::tests::err_envelope(
                -32429,
                "max usage reached",
            ),
        ));
        let e = fetch_accounts(&rpc, None, &[k(1)], 5_000, None, 0)
            .await
            .unwrap_err();
        assert_eq!(read_error("accounts", &e).class, ErrorClass::QuotaExhausted);
        t.push(Err(RpcError::new(ErrorClass::AuthRequired, "HTTP 401")));
        let e = fetch_accounts(&rpc, None, &[k(1)], 5_000, None, 0)
            .await
            .unwrap_err();
        assert_eq!(read_error("accounts", &e).class, ErrorClass::AuthRequired);
    }

    #[tokio::test]
    async fn sqlite_store_round_trip_with_the_real_fixture() {
        use crate::adapters::outbound::solana::rpc::tests::{core_keys, CORE_GMA};
        let dir = tempfile::tempdir().unwrap();
        let store = SqliteObservationStore::open(dir.path()).unwrap();
        let t = Arc::new(FakeTransport::at_slot(0));
        t.push(Ok(serde_json::from_str::<Value>(CORE_GMA).unwrap()));
        let rpc = fake_rpc(&t);
        let keys = core_keys();
        let live = fetch_accounts(&rpc, Some(&store), &keys, 10_000, None, 1_000)
            .await
            .unwrap();
        assert_eq!(live.accounts.len(), 13);
        assert_eq!((live.slot_min, live.slot_max), (450_104_084, 450_104_084));
        // Second read: all 13 rows (incl. the Absent long PDA) from SQLite.
        let cached = fetch_accounts(&rpc, Some(&store), &keys, 10_000, None, 3_000)
            .await
            .unwrap();
        assert_eq!(t.gma_params().len(), 1);
        assert_eq!(cached, live);
        let long: Pubkey = "FqymRcB92t63jpwh7om4RLbxMNUGoHnZPQMkkAA8ksVY"
            .parse()
            .unwrap();
        assert_eq!(cached.get(&long).unwrap().state, AccountState::Absent);
        let perps = ids::key(ids::JUP_PERPS);
        assert!(cached
            .data_owned_by(&ids::key(ids::JUP_CUSTODY_SOL), &perps, 1060)
            .is_some());
        // An older slot never overwrites a newer row.
        let mut older = cached.get(&long).unwrap().clone();
        older.slot = 450_104_000;
        let obs = Observation::of(
            ACCOUNTS_TOOL,
            &older,
            4_000,
            ACCOUNT_ROW_TTL_MS,
            ObsSource::Live,
        );
        assert!(!store.put(&obs).await.unwrap());
    }
}
