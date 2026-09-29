//! Jupiter perpetuals keeper-request instructions (program `ids::JUP_PERPS`,
//! IDL `perpetuals` 0.1.0 as used by delta_neutral_bot) — hand-encoded
//! Anchor/borsh. Pure.
//!
//! A market order is TX1 of a two-step flow: the request escrows collateral
//! into the request's ATA; a Jupiter keeper fills it (TX2) at oracle price
//! bounded by `price_slippage`. The request PDA is
//! `["position_request", position, counter u64 LE, [1 increase | 2 decrease]]`
//! (`jupiterPerps.ts:185-202`). An absent `referral` is the program id.
//! Checked against `tests/fixtures/solana/tx/golden.json`.

use crate::domain::solana::{ata, find_program_address, ids, Pubkey};
use crate::domain::solana_tx::{AccountMeta, Instruction};

use super::perps::Side;

/// `sha256("global:create_increase_position_market_request")[..8]`.
pub const INCREASE_MARKET: [u8; 8] = [184, 85, 199, 24, 105, 171, 156, 56];
/// `sha256("global:create_decrease_position_market_request")[..8]`.
pub const DECREASE_MARKET: [u8; 8] = [74, 198, 195, 86, 193, 99, 1, 79];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RequestChange {
    Increase,
    Decrease,
}

impl RequestChange {
    fn byte(self) -> u8 {
        match self {
            RequestChange::Increase => 1,
            RequestChange::Decrease => 2,
        }
    }
}

fn program() -> Pubkey {
    ids::key(ids::JUP_PERPS)
}

/// Keeper request PDA of `position` for `counter`.
pub fn position_request_pda(position: &Pubkey, counter: u64, change: RequestChange) -> Pubkey {
    find_program_address(
        &[
            b"position_request",
            &position.0,
            &counter.to_le_bytes(),
            &[change.byte()],
        ],
        &program(),
    )
    .0
}

/// Collateral mint of a SOL position: wSOL for longs, USDC for shorts.
pub fn collateral_mint(side: Side) -> Pubkey {
    match side {
        Side::Long => ids::key(ids::WSOL),
        Side::Short => ids::key(ids::USDC),
    }
}

/// `CreateIncreasePositionMarketRequestParams`; USD values 6-dp.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IncreaseParams {
    pub size_usd_delta: u64,
    pub collateral_token_delta: u64,
    pub side: Side,
    pub price_slippage: u64,
    pub jupiter_minimum_out: Option<u64>,
    pub counter: u64,
}

/// `CreateDecreasePositionMarketRequestParams`; USD values 6-dp.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DecreaseParams {
    pub collateral_usd_delta: u64,
    pub size_usd_delta: u64,
    pub price_slippage: u64,
    pub jupiter_minimum_out: Option<u64>,
    pub entire_position: Option<bool>,
    pub counter: u64,
}

fn opt_u64(v: Option<u64>, out: &mut Vec<u8>) {
    match v {
        None => out.push(0),
        Some(v) => {
            out.push(1);
            out.extend_from_slice(&v.to_le_bytes());
        }
    }
}

/// The 16 request accounts; slot 1 = funding (increase) / receiving
/// (decrease) ATA of `owner`, slot 9 = input / desired mint.
fn request_accounts(
    owner: &Pubkey,
    position: &Pubkey,
    side: Side,
    change: RequestChange,
    counter: u64,
) -> Vec<AccountMeta> {
    let mint = collateral_mint(side);
    let token = ids::key(ids::TOKEN);
    let request = position_request_pda(position, counter, change);
    vec![
        AccountMeta::writable(*owner, true),
        AccountMeta::writable(ata(owner, &mint, &token), false),
        AccountMeta::readonly(ids::key(ids::JUP_PERPETUALS), false),
        AccountMeta::readonly(ids::key(ids::JLP_POOL), false),
        AccountMeta {
            pubkey: *position,
            is_signer: false,
            is_writable: change == RequestChange::Increase,
        },
        AccountMeta::writable(request, false),
        AccountMeta::writable(ata(&request, &mint, &token), false),
        AccountMeta::readonly(ids::key(ids::JUP_CUSTODY_SOL), false),
        AccountMeta::readonly(ids::key(side.collateral_custody()), false),
        AccountMeta::readonly(mint, false),
        // referral: none
        AccountMeta::readonly(program(), false),
        AccountMeta::readonly(token, false),
        AccountMeta::readonly(ids::key(ids::ATA), false),
        AccountMeta::readonly(ids::key(ids::SYSTEM), false),
        AccountMeta::readonly(ids::key(ids::JUP_PERPS_EVENT_AUTHORITY), false),
        AccountMeta::readonly(program(), false),
    ]
}

/// `create_increase_position_market_request` for `owner`'s SOL `position`.
pub fn increase_market_request(
    owner: &Pubkey,
    position: &Pubkey,
    p: &IncreaseParams,
) -> Instruction {
    let mut data = INCREASE_MARKET.to_vec();
    data.extend_from_slice(&p.size_usd_delta.to_le_bytes());
    data.extend_from_slice(&p.collateral_token_delta.to_le_bytes());
    data.push(p.side.byte());
    data.extend_from_slice(&p.price_slippage.to_le_bytes());
    opt_u64(p.jupiter_minimum_out, &mut data);
    data.extend_from_slice(&p.counter.to_le_bytes());
    Instruction {
        program_id: program(),
        accounts: request_accounts(owner, position, p.side, RequestChange::Increase, p.counter),
        data,
    }
}

/// `create_decrease_position_market_request`; `side` picks the receiving
/// mint (wSOL for longs — that ATA must survive until the keeper fills).
pub fn decrease_market_request(
    owner: &Pubkey,
    position: &Pubkey,
    side: Side,
    p: &DecreaseParams,
) -> Instruction {
    let mut data = DECREASE_MARKET.to_vec();
    data.extend_from_slice(&p.collateral_usd_delta.to_le_bytes());
    data.extend_from_slice(&p.size_usd_delta.to_le_bytes());
    data.extend_from_slice(&p.price_slippage.to_le_bytes());
    opt_u64(p.jupiter_minimum_out, &mut data);
    match p.entire_position {
        None => data.push(0),
        Some(b) => data.extend_from_slice(&[1, u8::from(b)]),
    }
    data.extend_from_slice(&p.counter.to_le_bytes());
    Instruction {
        program_id: program(),
        accounts: request_accounts(owner, position, side, RequestChange::Decrease, p.counter),
        data,
    }
}

/// Anchor instruction discriminator `sha256("global:<name>")[..8]`.
#[cfg(test)]
pub fn anchor_discriminator(name: &str) -> [u8; 8] {
    use sha2::{Digest, Sha256};
    let h = Sha256::digest(format!("global:{name}").as_bytes());
    h[..8].try_into().expect("8 bytes")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::solana_tx::golden::{assert_ix, k, pda, signer};

    const COUNTER: u64 = 987_654_321;

    #[test]
    fn discriminators_are_anchor_global_hashes() {
        assert_eq!(
            anchor_discriminator("create_increase_position_market_request"),
            INCREASE_MARKET
        );
        assert_eq!(
            anchor_discriminator("create_decrease_position_market_request"),
            DECREASE_MARKET
        );
    }

    #[test]
    fn pdas_match_web3() {
        let pos = k(30);
        assert_eq!(
            position_request_pda(&pos, COUNTER, RequestChange::Increase),
            pda("request_increase")
        );
        assert_eq!(
            position_request_pda(&pos, COUNTER, RequestChange::Decrease),
            pda("request_decrease")
        );
        assert_eq!(pda("perpetuals"), ids::key(ids::JUP_PERPETUALS));
        assert_eq!(
            find_program_address(&[b"perpetuals"], &program()).0,
            ids::key(ids::JUP_PERPETUALS)
        );
        assert_eq!(
            find_program_address(&[b"__event_authority"], &program()).0,
            ids::key(ids::JUP_PERPS_EVENT_AUTHORITY)
        );
        assert_eq!(
            pda("perps_event_authority"),
            ids::key(ids::JUP_PERPS_EVENT_AUTHORITY)
        );
    }

    #[test]
    fn increase_requests_match_anchor() {
        let w = signer("wallet");
        assert_ix(
            "perps_increase_short",
            &increase_market_request(
                &w,
                &k(30),
                &IncreaseParams {
                    size_usd_delta: 1_000_000_000,
                    collateral_token_delta: 250_000_000,
                    side: Side::Short,
                    price_slippage: 150_750_000,
                    jupiter_minimum_out: None,
                    counter: COUNTER,
                },
            ),
        );
        assert_ix(
            "perps_increase_long_minout",
            &increase_market_request(
                &w,
                &k(30),
                &IncreaseParams {
                    size_usd_delta: 1_000_000_000,
                    collateral_token_delta: 1_500_000_000,
                    side: Side::Long,
                    price_slippage: 152_250_000,
                    jupiter_minimum_out: Some(42),
                    counter: COUNTER,
                },
            ),
        );
    }

    #[test]
    fn decrease_requests_match_anchor() {
        let w = signer("wallet");
        assert_ix(
            "perps_decrease_short_entire",
            &decrease_market_request(
                &w,
                &k(30),
                Side::Short,
                &DecreaseParams {
                    collateral_usd_delta: 0,
                    size_usd_delta: 0,
                    price_slippage: 151_500_000,
                    jupiter_minimum_out: None,
                    entire_position: Some(true),
                    counter: COUNTER,
                },
            ),
        );
        assert_ix(
            "perps_decrease_long_partial",
            &decrease_market_request(
                &w,
                &k(30),
                Side::Long,
                &DecreaseParams {
                    collateral_usd_delta: 5_000_000,
                    size_usd_delta: 20_000_000,
                    price_slippage: 149_250_000,
                    jupiter_minimum_out: None,
                    entire_position: None,
                    counter: COUNTER,
                },
            ),
        );
    }
}
