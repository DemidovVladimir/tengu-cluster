//! Solana LP policy + typed outputs — pure, no IO. One file per family:
//! DLMM (`dlmm`), Jupiter perps (`perps`), wallet/tx (`wallet`), price and
//! pool list (`market`), hedge controller (`hedge`), LP gates (`gates`), the
//! composed snapshot + decision envelopes (`snapshot`), and the write-path
//! instruction encoders (`dlmm_ix`, `perps_ix`).

pub(crate) mod dlmm;
pub(crate) mod dlmm_ix;
pub(crate) mod gates;
pub(crate) mod hedge;
pub(crate) mod market;
pub(crate) mod perps;
pub(crate) mod perps_ix;
pub(crate) mod snapshot;
pub(crate) mod wallet;
