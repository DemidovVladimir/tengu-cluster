//! The `tengu` library — every module; the binary (`src/main.rs`) only calls
//! [`run`]. Layers: `domain` ← `ports` ← `application` ← `adapters`, wired by
//! `bootstrap` (see `docs/archive/2026-09/hexagonal-plan-2026-09-23.md`). Modules stay
//! private, so dead-code analysis sees what `run` reaches; unit tests run with
//! `cargo test --lib`.

mod adapters;
mod application;
mod bootstrap;
mod config;
mod domain;
mod ports;

/// The `tengu` CLI: parse the arguments, run the command
/// (`adapters/inbound/cli`).
pub async fn run() -> anyhow::Result<()> {
    adapters::inbound::cli::run().await
}
