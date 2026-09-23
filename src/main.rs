//! Tengu binary entry point. Layers: `domain` ← `ports` ← `application` ←
//! `adapters`, wired by `bootstrap` (see `docs/hexagonal-plan-2026-09-23.md`).

mod adapters;
mod application;
mod bootstrap;
mod config;
mod domain;
mod ports;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    adapters::inbound::cli::run().await
}
