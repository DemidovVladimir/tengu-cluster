---
tags:
  - core
  - principles
---

# Principles

Design and development principles governing the [[Overview|Tengu Cluster]] codebase.

## Architecture

**[[Architecture|Flat module structure]]** — all code in `src/adapters/` alongside `src/main.rs`.
- Easy to navigate and modify
- No unnecessary layering

## Code Practices

**DRY (Don't Repeat Yourself)**
- Shared channel logic in `channel_runtime.rs` — not duplicated per adapter
- Shared tool logic in `tool_builder.rs` — not per-channel
- Shared memory init via `build_memory_handle()` — not per-adapter

**KISS (Keep It Simple)**
- Simplest correct solution wins
- No premature abstractions
- No speculative features or dead code
- 3 similar lines of code > premature abstraction

**Idiomatic Rust**
- `Send + Sync` bounds for async concurrency
- `Arc<T>` for shared ownership across tokio tasks
- Trait objects for polymorphism (ports)
- Feature gates for optional deps (`--features qdrant`, `--features telegram`)
- Follow `cargo clippy` and `cargo fmt`
- Meaningful error types, no unwrap in production paths

## Composability

**[[Skills]] = knowledge** — portable, cross-platform, never modified by the platform
**[[Tools]] = primitives** — general purpose, reusable across all skills
**[[Channels]] = communication** — isolated via ports, no leakage
**[[Capabilities]] = permissions** — gate platform tools, config-driven

See [[Architecture#Agents, Tools, Skills, Capabilities]] for the full model.

## Process

- **Small changes, verify** — one change at a time, compile, prove it works
- **Every task includes tests** — architecture enforcement, integration, unit
- **Docs stay current** — every feature change updates corresponding documentation
- **Research before building** — check existing frameworks and patterns first
- **No redundant abstractions** — skills ARE capabilities, don't wrap in new types

## What We Avoid

- Hardcoded tool name matching in generic infrastructure
- Dead code (heartbeat, desci_tools.rs, separate crates removed March 2026)
- Domain-specific tool adapters (all execution via platform primitives + skills)
- Per-channel duplicated logic
- Over-engineering and speculative features

## Related

- [[Architecture]] — project structure
- [[Testing]] — test strategy
- [[Overview]] — system goals
