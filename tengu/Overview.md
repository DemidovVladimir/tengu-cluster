---
tags:
  - core
  - index
---

# Tengu Cluster

A **composable multi-agent system** built to run 24/7 — model-agnostic, pluggable, and designed for autonomous orchestration with permanent memory.

Single Rust binary. Zero external dependencies.

## Core Design Goals

1. **24/7 autonomous operation** — the [[Orchestrator]] coordinates [[Agents]], decomposes goals, routes tasks, and maintains continuity through [[Memory]]
2. **Plug-and-play extensibility** — new [[Skills]] are added as markdown files with zero code changes; new [[Agents]] require only config
3. **General-purpose [[Tools]]** — 4 stable primitives reusable across all skills, gated by [[Capabilities]] per agent
4. **Channel-agnostic** — [[Channels]] (TUI, Telegram, future Slack/Discord) are isolated
5. **Composable and understandable** — clear separation makes the system easy to extend and reason about

## Concept Map

```
User ──→ [[Channels|Channel]] ──→ [[Orchestrator]]
                                      │
                          ┌───────────┼───────────┐
                          ▼           ▼           ▼
                     [[Agents|Agent A]]  [[Agents|Agent B]]  [[Agents|Agent C]]
                          │           │           │
                     [[Capabilities]]  [[Capabilities]]  [[Capabilities]]
                          │           │           │
                     [[Tools]] + [[Skills]]  [[Tools]] + [[Skills]]  [[Tools]] + [[Skills]]
                          │           │           │
                          └─────┬─────┘           │
                                ▼                 ▼
                           [[Memory]]        [[Memory]]
```

## Key Principles

See [[Principles]] for the full list:
- [[Architecture|Flat module structure]]
- DRY and KISS
- Idiomatic Rust best practices
- Every task includes tests and doc updates

## Guides

**Getting Started:**
- [[Quickstart]] — 5-minute setup walkthrough
- [[Configuration]] — TOML reference overview
- [[Configuration Reference]] — detailed per-field tables
- [[Deployment]] — Docker, cloud, GPU

**Core Concepts:**
- [[Architecture]] — project structure
- [[Agents]] — roles, identity, permissions
- [[Tools]] — workspace primitives
- [[Skills]] — plug-and-play capabilities
- [[Capabilities]] — permission model
- [[Orchestrator]] — multi-agent coordination
- [[Sandboxes]] — domain-specific team configs
- [[Memory]] — persistent vector recall
- [[Channels]] — communication adapters

**Subsystems:**
- [[DeSci]] — IP-NFT minting pipeline
- [[Wallet]] — Privy agentic wallets and on-chain signing

**Development:**
- [[Principles]] — DRY, KISS, Rust best practices
- [[Testing]] — test strategy and enforcement
- [[Changelog]] — last 5 development iterations
