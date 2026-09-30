//! Hyperliquid outbound — the budgeted, egress-gated `POST /info` client
//! (`info.rs`). Replies are decoded purely (`domain/market.rs` rows and
//! decimal helpers; `domain/hl/` decoders next); the `hl_*` tools build on
//! [`info::HlInfo`]. Replay fixtures: `tests/fixtures/hyperliquid/`.

pub(crate) mod info;
