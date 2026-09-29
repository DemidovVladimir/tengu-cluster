//! Meteora DLMM (lb_clmm 0.11.0, program `ids::DLMM`) instructions the write
//! tools send — hand-encoded Anchor/borsh, account lists in IDL order. Pure.
//!
//! | Instruction | Used by |
//! |---|---|
//! | `initialize_position` (position = fresh keypair, signs) | `dlmm_open_position` |
//! | `initialize_bin_array` (only for arrays that do not exist) | `dlmm_open_position` |
//! | `add_liquidity_by_strategy2` | `dlmm_open_position` |
//! | `remove_liquidity_by_range2`, `claim_fee2`, `claim_reward2`, `close_position_if_empty` | `dlmm_close_position` |
//!
//! Rules taken from the SDK (`@meteora-ag/dlmm` 1.9.7): an optional account
//! left empty is the program id (read-only); bin arrays covering the range go
//! last as writable remaining accounts, ascending; `RemainingAccountsInfo` is
//! always `[{TransferHookX, 0}, {TransferHookY, 0}]` for liquidity /
//! fee instructions and `[{TransferHookReward, 0}]` for rewards (no
//! transfer-hook accounts: hook mints are refused before building); the SDK
//! maps spot / curve / bid-ask to the `*ImBalanced` strategy variants.
//! Checked against `tests/fixtures/solana/tx/golden.json`.

use crate::domain::solana::{bin_array_pda, find_program_address, ids, Pubkey};
use crate::domain::solana_tx::{AccountMeta, Instruction};

use super::gates::bin_array_indexes;

pub const INITIALIZE_POSITION: [u8; 8] = [219, 192, 234, 71, 190, 191, 102, 80];
pub const INITIALIZE_BIN_ARRAY: [u8; 8] = [35, 86, 19, 185, 78, 212, 75, 211];
pub const ADD_LIQUIDITY_BY_STRATEGY2: [u8; 8] = [3, 221, 149, 218, 111, 141, 118, 213];
pub const REMOVE_LIQUIDITY_BY_RANGE2: [u8; 8] = [204, 2, 195, 145, 53, 145, 145, 205];
pub const CLAIM_FEE2: [u8; 8] = [112, 191, 101, 171, 28, 144, 127, 187];
pub const CLAIM_REWARD2: [u8; 8] = [190, 3, 127, 119, 178, 87, 157, 183];
pub const CLOSE_POSITION_IF_EMPTY: [u8; 8] = [59, 124, 212, 118, 91, 152, 110, 157];

/// `bps_to_remove` for a full withdrawal.
pub const BPS_ALL: u16 = 10_000;
/// Default bitmap covers bin-array indexes [-512, 511]; outside it the pool
/// needs the `["bitmap", lb_pair]` extension account.
const BITMAP_MIN_INDEX: i64 = -512;
const BITMAP_MAX_INDEX: i64 = 511;

/// Liquidity shape. The SDK sends the `*ImBalanced` variants (6 / 7 / 8).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Strategy {
    Spot,
    Curve,
    BidAsk,
}

impl Strategy {
    pub fn parse(s: &str) -> Option<Strategy> {
        match s {
            "spot" => Some(Strategy::Spot),
            "curve" => Some(Strategy::Curve),
            "bidask" | "bid_ask" => Some(Strategy::BidAsk),
            _ => None,
        }
    }
    fn byte(self) -> u8 {
        match self {
            Strategy::Spot => 6,
            Strategy::Curve => 7,
            Strategy::BidAsk => 8,
        }
    }
}

fn program() -> Pubkey {
    ids::key(ids::DLMM)
}

fn event_accounts() -> [AccountMeta; 2] {
    [
        AccountMeta::readonly(ids::key(ids::DLMM_EVENT_AUTHORITY), false),
        AccountMeta::readonly(program(), false),
    ]
}

/// Optional account: the key, or the program id (read-only) when absent.
fn optional_writable(k: Option<Pubkey>) -> AccountMeta {
    match k {
        Some(k) => AccountMeta::writable(k, false),
        None => AccountMeta::readonly(program(), false),
    }
}

/// `RemainingAccountsInfo` with the two zero-length transfer-hook slices.
const LIQUIDITY_SLICES: [u8; 8] = [2, 0, 0, 0, 0, 0, 1, 0];
/// `RemainingAccountsInfo` with one zero-length reward-hook slice.
const REWARD_SLICES: [u8; 6] = [1, 0, 0, 0, 2, 0];

/// `["bitmap", lb_pair]`.
pub fn bitmap_extension_pda(lb_pair: &Pubkey) -> Pubkey {
    find_program_address(&[b"bitmap", &lb_pair.0], &program()).0
}

/// Whether `[min_bin, max_bin]` touches a bin array outside the default
/// bitmap (then add-liquidity must pass the extension).
pub fn range_needs_bitmap_extension(min_bin: i32, max_bin: i32) -> bool {
    bin_array_indexes(min_bin, max_bin)
        .iter()
        .any(|i| !(BITMAP_MIN_INDEX..=BITMAP_MAX_INDEX).contains(i))
}

/// Bin array PDAs covering `[min_bin, max_bin]`, ascending.
pub fn bin_arrays_for_range(lb_pair: &Pubkey, min_bin: i32, max_bin: i32) -> Vec<Pubkey> {
    bin_array_indexes(min_bin, max_bin)
        .into_iter()
        .map(|i| bin_array_pda(lb_pair, i))
        .collect()
}

fn with_bin_arrays(mut accounts: Vec<AccountMeta>, bin_arrays: &[Pubkey]) -> Vec<AccountMeta> {
    accounts.extend(bin_arrays.iter().map(|k| AccountMeta::writable(*k, false)));
    accounts
}

/// `initialize_position(lower_bin_id, width)`; `owner` pays and signs.
pub fn initialize_position(
    owner: &Pubkey,
    position: &Pubkey,
    lb_pair: &Pubkey,
    lower_bin_id: i32,
    width: i32,
) -> Instruction {
    let mut data = INITIALIZE_POSITION.to_vec();
    data.extend_from_slice(&lower_bin_id.to_le_bytes());
    data.extend_from_slice(&width.to_le_bytes());
    let mut accounts = vec![
        AccountMeta::writable(*owner, true),
        AccountMeta::writable(*position, true),
        AccountMeta::readonly(*lb_pair, false),
        AccountMeta::readonly(*owner, true),
        AccountMeta::readonly(ids::key(ids::SYSTEM), false),
        AccountMeta::readonly(ids::key(ids::SYSVAR_RENT), false),
    ];
    accounts.extend(event_accounts());
    Instruction {
        program_id: program(),
        accounts,
        data,
    }
}

/// `initialize_bin_array(index)`; `funder` pays the (non-refundable) rent.
pub fn initialize_bin_array(lb_pair: &Pubkey, index: i64, funder: &Pubkey) -> Instruction {
    let mut data = INITIALIZE_BIN_ARRAY.to_vec();
    data.extend_from_slice(&index.to_le_bytes());
    Instruction {
        program_id: program(),
        accounts: vec![
            AccountMeta::readonly(*lb_pair, false),
            AccountMeta::writable(bin_array_pda(lb_pair, index), false),
            AccountMeta::writable(*funder, true),
            AccountMeta::readonly(ids::key(ids::SYSTEM), false),
        ],
        data,
    }
}

/// Accounts shared by add / remove liquidity (IDL order).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LiquidityAccounts {
    pub position: Pubkey,
    pub lb_pair: Pubkey,
    pub bitmap_extension: Option<Pubkey>,
    pub user_token_x: Pubkey,
    pub user_token_y: Pubkey,
    pub reserve_x: Pubkey,
    pub reserve_y: Pubkey,
    pub token_x_mint: Pubkey,
    pub token_y_mint: Pubkey,
    pub sender: Pubkey,
    pub token_x_program: Pubkey,
    pub token_y_program: Pubkey,
}

impl LiquidityAccounts {
    fn metas(&self, memo: bool) -> Vec<AccountMeta> {
        let mut v = vec![
            AccountMeta::writable(self.position, false),
            AccountMeta::writable(self.lb_pair, false),
            optional_writable(self.bitmap_extension),
            AccountMeta::writable(self.user_token_x, false),
            AccountMeta::writable(self.user_token_y, false),
            AccountMeta::writable(self.reserve_x, false),
            AccountMeta::writable(self.reserve_y, false),
            AccountMeta::readonly(self.token_x_mint, false),
            AccountMeta::readonly(self.token_y_mint, false),
            AccountMeta::readonly(self.sender, true),
            AccountMeta::readonly(self.token_x_program, false),
            AccountMeta::readonly(self.token_y_program, false),
        ];
        if memo {
            v.push(AccountMeta::readonly(ids::key(ids::MEMO), false));
        }
        v.extend(event_accounts());
        v
    }
}

/// `LiquidityParameterByStrategy` (97 bytes).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StrategyLiquidity {
    pub amount_x: u64,
    pub amount_y: u64,
    pub active_id: i32,
    pub max_active_bin_slippage: i32,
    pub min_bin_id: i32,
    pub max_bin_id: i32,
    pub strategy: Strategy,
}

/// `add_liquidity_by_strategy2`; `bin_arrays` = [`bin_arrays_for_range`].
pub fn add_liquidity_by_strategy2(
    accounts: &LiquidityAccounts,
    p: &StrategyLiquidity,
    bin_arrays: &[Pubkey],
) -> Instruction {
    let mut data = ADD_LIQUIDITY_BY_STRATEGY2.to_vec();
    data.extend_from_slice(&p.amount_x.to_le_bytes());
    data.extend_from_slice(&p.amount_y.to_le_bytes());
    data.extend_from_slice(&p.active_id.to_le_bytes());
    data.extend_from_slice(&p.max_active_bin_slippage.to_le_bytes());
    data.extend_from_slice(&p.min_bin_id.to_le_bytes());
    data.extend_from_slice(&p.max_bin_id.to_le_bytes());
    data.push(p.strategy.byte());
    // `parameteres: [u8; 64]` — all zero (byte 0 = single-sided-X flag, unused).
    data.extend_from_slice(&[0u8; 64]);
    data.extend_from_slice(&LIQUIDITY_SLICES);
    Instruction {
        program_id: program(),
        accounts: with_bin_arrays(accounts.metas(false), bin_arrays),
        data,
    }
}

/// `remove_liquidity_by_range2(from, to, bps)`.
pub fn remove_liquidity_by_range2(
    accounts: &LiquidityAccounts,
    from_bin_id: i32,
    to_bin_id: i32,
    bps_to_remove: u16,
    bin_arrays: &[Pubkey],
) -> Instruction {
    let mut data = REMOVE_LIQUIDITY_BY_RANGE2.to_vec();
    data.extend_from_slice(&from_bin_id.to_le_bytes());
    data.extend_from_slice(&to_bin_id.to_le_bytes());
    data.extend_from_slice(&bps_to_remove.to_le_bytes());
    data.extend_from_slice(&LIQUIDITY_SLICES);
    Instruction {
        program_id: program(),
        accounts: with_bin_arrays(accounts.metas(true), bin_arrays),
        data,
    }
}

/// `claim_fee2(min, max)` — note its account order differs from remove.
pub fn claim_fee2(
    a: &LiquidityAccounts,
    min_bin_id: i32,
    max_bin_id: i32,
    bin_arrays: &[Pubkey],
) -> Instruction {
    let mut data = CLAIM_FEE2.to_vec();
    data.extend_from_slice(&min_bin_id.to_le_bytes());
    data.extend_from_slice(&max_bin_id.to_le_bytes());
    data.extend_from_slice(&LIQUIDITY_SLICES);
    let mut accounts = vec![
        AccountMeta::writable(a.lb_pair, false),
        AccountMeta::writable(a.position, false),
        AccountMeta::readonly(a.sender, true),
        AccountMeta::writable(a.reserve_x, false),
        AccountMeta::writable(a.reserve_y, false),
        AccountMeta::writable(a.user_token_x, false),
        AccountMeta::writable(a.user_token_y, false),
        AccountMeta::readonly(a.token_x_mint, false),
        AccountMeta::readonly(a.token_y_mint, false),
        AccountMeta::readonly(a.token_x_program, false),
        AccountMeta::readonly(a.token_y_program, false),
        AccountMeta::readonly(ids::key(ids::MEMO), false),
    ];
    accounts.extend(event_accounts());
    Instruction {
        program_id: program(),
        accounts: with_bin_arrays(accounts, bin_arrays),
        data,
    }
}

/// One initialized farming reward of the pool.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RewardAccounts {
    pub reward_index: u64,
    pub reward_vault: Pubkey,
    pub reward_mint: Pubkey,
    pub user_token_account: Pubkey,
    pub token_program: Pubkey,
}

/// `claim_reward2(reward_index, min, max)`.
pub fn claim_reward2(
    lb_pair: &Pubkey,
    position: &Pubkey,
    sender: &Pubkey,
    r: &RewardAccounts,
    min_bin_id: i32,
    max_bin_id: i32,
    bin_arrays: &[Pubkey],
) -> Instruction {
    let mut data = CLAIM_REWARD2.to_vec();
    data.extend_from_slice(&r.reward_index.to_le_bytes());
    data.extend_from_slice(&min_bin_id.to_le_bytes());
    data.extend_from_slice(&max_bin_id.to_le_bytes());
    data.extend_from_slice(&REWARD_SLICES);
    let mut accounts = vec![
        AccountMeta::writable(*lb_pair, false),
        AccountMeta::writable(*position, false),
        AccountMeta::readonly(*sender, true),
        AccountMeta::writable(r.reward_vault, false),
        AccountMeta::readonly(r.reward_mint, false),
        AccountMeta::writable(r.user_token_account, false),
        AccountMeta::readonly(r.token_program, false),
        AccountMeta::readonly(ids::key(ids::MEMO), false),
    ];
    accounts.extend(event_accounts());
    Instruction {
        program_id: program(),
        accounts: with_bin_arrays(accounts, bin_arrays),
        data,
    }
}

/// `close_position_if_empty` — fails on-chain while liquidity or unclaimed
/// fees remain (the SDK's close path uses it after remove + claim).
pub fn close_position_if_empty(
    position: &Pubkey,
    sender: &Pubkey,
    rent_receiver: &Pubkey,
) -> Instruction {
    let mut accounts = vec![
        AccountMeta::writable(*position, false),
        AccountMeta::readonly(*sender, true),
        AccountMeta::writable(*rent_receiver, false),
    ];
    accounts.extend(event_accounts());
    Instruction {
        program_id: program(),
        accounts,
        data: CLOSE_POSITION_IF_EMPTY.to_vec(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::solana_tx::golden::{assert_ix, k, pda, signer};

    fn liquidity_accounts(ext: Option<Pubkey>) -> LiquidityAccounts {
        LiquidityAccounts {
            position: signer("position"),
            lb_pair: k(20),
            bitmap_extension: ext,
            user_token_x: k(21),
            user_token_y: k(22),
            reserve_x: k(23),
            reserve_y: k(24),
            token_x_mint: ids::key(ids::WSOL),
            token_y_mint: ids::key(ids::USDC),
            sender: signer("wallet"),
            token_x_program: ids::key(ids::TOKEN),
            token_y_program: ids::key(ids::TOKEN),
        }
    }

    fn params(strategy: Strategy) -> StrategyLiquidity {
        StrategyLiquidity {
            amount_x: 1_500_000_000,
            amount_y: 250_000_000,
            active_id: -5,
            max_active_bin_slippage: 1,
            min_bin_id: -39,
            max_bin_id: 30,
            strategy,
        }
    }

    #[test]
    fn pdas_match_the_sdk() {
        let lb = k(20);
        assert_eq!(
            bin_arrays_for_range(&lb, -39, 30),
            vec![pda("bin_array_lb20_m1"), pda("bin_array_lb20_0")]
        );
        assert_eq!(bitmap_extension_pda(&lb), pda("bitmap_ext_lb20"));
        assert_eq!(
            find_program_address(&[b"__event_authority"], &program()).0,
            ids::key(ids::DLMM_EVENT_AUTHORITY)
        );
        assert_eq!(
            pda("dlmm_event_authority"),
            ids::key(ids::DLMM_EVENT_AUTHORITY)
        );
    }

    #[test]
    fn position_and_bin_array_init_match_the_sdk() {
        let (w, p) = (signer("wallet"), signer("position"));
        assert_ix(
            "dlmm_initialize_position",
            &initialize_position(&w, &p, &k(20), -39, 70),
        );
        assert_ix(
            "dlmm_initialize_bin_array",
            &initialize_bin_array(&k(20), -1, &w),
        );
    }

    #[test]
    fn add_liquidity_matches_the_sdk_with_and_without_extension() {
        let arrays = bin_arrays_for_range(&k(20), -39, 30);
        assert_ix(
            "dlmm_add_liquidity_by_strategy2",
            &add_liquidity_by_strategy2(
                &liquidity_accounts(None),
                &params(Strategy::Spot),
                &arrays,
            ),
        );
        assert_ix(
            "dlmm_add_liquidity_by_strategy2_ext",
            &add_liquidity_by_strategy2(
                &liquidity_accounts(Some(k(25))),
                &params(Strategy::Spot),
                &arrays,
            ),
        );
        assert_ix(
            "dlmm_add_liquidity_bidask",
            &add_liquidity_by_strategy2(
                &liquidity_accounts(None),
                &params(Strategy::BidAsk),
                &arrays,
            ),
        );
    }

    #[test]
    fn close_path_matches_the_sdk() {
        let arrays = bin_arrays_for_range(&k(20), -39, 30);
        let a = liquidity_accounts(None);
        assert_ix(
            "dlmm_remove_liquidity_by_range2",
            &remove_liquidity_by_range2(&a, -39, 30, BPS_ALL, &arrays),
        );
        assert_ix("dlmm_claim_fee2", &claim_fee2(&a, -39, 30, &arrays));
        let r = RewardAccounts {
            reward_index: 1,
            reward_vault: k(26),
            reward_mint: k(27),
            user_token_account: k(28),
            token_program: ids::key(ids::TOKEN),
        };
        assert_ix(
            "dlmm_claim_reward2",
            &claim_reward2(&k(20), &a.position, &a.sender, &r, -39, 30, &arrays),
        );
        assert_ix(
            "dlmm_close_position_if_empty",
            &close_position_if_empty(&a.position, &a.sender, &a.sender),
        );
    }

    #[test]
    fn bitmap_extension_needed_only_outside_default_bitmap() {
        assert!(!range_needs_bitmap_extension(-39, 30));
        // index 511 = bins [35770, 35839]; 512 starts at 35840.
        assert!(!range_needs_bitmap_extension(35_800, 35_839));
        assert!(range_needs_bitmap_extension(35_800, 35_840));
        // index -512 = bins [-35840, -35771]; -513 ends at -35841.
        assert!(!range_needs_bitmap_extension(-35_840, -35_800));
        assert!(range_needs_bitmap_extension(-35_841, -35_800));
    }

    #[test]
    fn lb_pair_reward_infos_match_the_sdk_encoding() {
        use base64::Engine as _;
        let g = crate::domain::solana_tx::golden::golden();
        let case = &g["lb_pair_with_rewards"];
        let data = base64::engine::general_purpose::STANDARD
            .decode(case["data_b64"].as_str().unwrap())
            .unwrap();
        let pair = super::super::dlmm::decode_lb_pair(&data).unwrap();
        let want: Vec<(u64, Pubkey, Pubkey)> = case["rewards"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| {
                (
                    r[0].as_u64().unwrap(),
                    r[1].as_str().unwrap().parse().unwrap(),
                    r[2].as_str().unwrap().parse().unwrap(),
                )
            })
            .collect();
        assert_eq!(pair.rewards(), want);
    }

    #[test]
    fn strategy_names() {
        assert_eq!(Strategy::parse("spot"), Some(Strategy::Spot));
        assert_eq!(Strategy::parse("curve"), Some(Strategy::Curve));
        assert_eq!(Strategy::parse("bidask"), Some(Strategy::BidAsk));
        assert_eq!(Strategy::parse("Spot"), None);
    }
}
