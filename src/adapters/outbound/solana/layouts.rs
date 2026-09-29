//! Account layouts — intentionally empty. Decoding bytes is pure, so every
//! decoder lives in the domain as a function over `&[u8]` / `AccountSet`:
//!
//! | Accounts | Decoders |
//! |---|---|
//! | SPL Mint, SPL Token account | `domain/lp/wallet.rs` |
//! | DLMM LbPair, PositionV2, BinArray | `domain/lp/dlmm.rs` |
//! | Jupiter perps Position, Custody, JLP Pool | `domain/lp/perps.rs` |
//!
//! Raw reads come from `accounts::fetch_accounts` (`domain::solana::AccountSet`;
//! `AccountSet::data_owned_by` = owner check + minimum length).
