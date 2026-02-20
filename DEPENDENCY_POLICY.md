# Dependency Policy

This document defines dependency rules for Tengu Cluster.

## Goals

- Keep security and supply-chain risk low.
- Minimize maintenance burden and custom protocol drift.
- Preserve deploy flexibility: Raspberry Pi minimal mode and full desktop mode.

## Rules

1. Prefer official Rust SDKs from the provider/vendor.
2. If no official Rust SDK exists, use typed direct REST clients against official API docs.
3. Avoid third-party multi-provider abstraction crates in the core runtime.
4. New dependencies must be optional behind feature flags unless required for baseline (`cli` + `ollama`).
5. Keep default features lightweight and aligned with current implemented modules.
6. Provider/channel/refiner/tool integrations must enter runtime only through `tengu-core` adapter traits (`Engine`, `Pipe`, `Refiner`, `Tool`).
7. Runtime side-effects (audit/metrics/policy reactions) must migrate to domain-event subscribers (`DomainEvent` + `EventBus`) instead of adding new tight inline coupling in `main.rs`.

## Architecture Conformance Requirements

For every new runtime-facing feature:
1. Define or reuse adapter-trait contracts in `tengu-core` instead of introducing provider-specific logic branches in orchestration code.
2. Emit/consume typed events for lifecycle transitions; avoid stringly-typed event payloads.
3. Document current-vs-target state explicitly when event-bus migration is partial.

## Provider Decisions (as of 2026-02-17)

- Ollama: direct REST (`reqwest`) to local API.
- Hugging Face: use `hf-hub` for model/artifact access where possible.
- OpenAI: no official Rust SDK adopted here yet; use typed REST integration.
- Anthropic: no official Rust SDK adopted here yet; use typed REST integration.
- Google Gemini: no official Rust SDK adopted here yet; use typed REST integration.

## Local Acceleration (Candle)

- Candle is the default path for in-process local ML acceleration in optimizer/refiner flows.
- Resource policy: use GPU backends when available (CUDA on NVIDIA, Metal on Apple Silicon), with CPU fallback.
- Goal: maximize local machine utilization for speed/cost while keeping minimal mode available on low-resource devices.

## Channel Decisions

- CLI: `tokio` stdin/stdout (baseline).
- Telegram: target `teloxide`.
- Discord: target `serenity` or `twilight` after benchmark and API-surface review.
- WebChat: target `axum` + WebSocket stack.

## Build Profiles

- Minimal (Pi): `--no-default-features --features "ollama"`.
- Full workstation: default features plus selected optional channels/providers.

## Acceptance Checklist for New Crates

- Maintainer trust: clear ownership and active releases.
- Security posture: public issue response, dependency hygiene, no abandoned transitive graph.
- Operational fit: async runtime compatibility, predictable memory/CPU behavior.
- License compatibility with workspace policy.
