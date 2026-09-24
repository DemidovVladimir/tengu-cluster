//! Solana LP policy — pure decision cores ported from `delta_neutral_bot`
//! (hedge controller, LP gates, DLMM bin/fee math). No IO, no clocks.

pub(crate) mod gates;
pub(crate) mod hedge;
