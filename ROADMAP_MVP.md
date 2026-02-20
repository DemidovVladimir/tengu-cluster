# 🗺️ Tengu Cluster — Roadmap & MVP Sprint Plan

> What to build first to get a working demo

---

## 📍 Current State

### ✅ Already Working (Phase 1 — ~90%)

| Component | Status | File |
|-----------|--------|------|
| CLI interface | ✅ Working | `src/main.rs` |
| Ollama Engine | ✅ Working (streaming `TextDelta`) | `crates/tengu-backends/src/ollama/mod.rs` |
| Anthropic Engine | ✅ Working (typed REST, non-streaming) | `crates/tengu-backends/src/anthropic/mod.rs` |
| Full config schema | ✅ | `crates/tengu-core/src/config/schema.rs` (399 lines) |
| Env var substitution (`${VAR}`) | ✅ | `config/schema.rs` |
| Runtime Profile Detection | ✅ | `crates/tengu-core/src/config/profile.rs` |
| CLI Pipe | ✅ | `crates/tengu-channels/src/cli/mod.rs` |
| Noop + Rule Refiner | ✅ | `crates/tengu-optimizer/src/` |
| Traits: Engine, Pipe, Refiner, Tool | ✅ | `crates/tengu-core/src/lib.rs` |
| Adapter + event-driven runtime baseline | ⚠️ Partial | Trait adapters + stream events are live; `DomainEvent`/`EventBus` contracts, bounded in-process bus, and runtime emitters are implemented, subscriber migration is pending |
| Slash commands (`/eco`, `/cost`, `/reset`, etc.) | ✅ | `src/main.rs` |
| `tengu doctor` (Ollama + flow-store integrity checks) | ✅ | `src/main.rs` |
| `tengu status` | ✅ | `src/main.rs` |
| Knowledge Store (runtime-wired) | ⚠️ Basic | `crates/tengu-memory/src/lib.rs` + `src/main.rs` |
| Flow persistence (CLI flows) | ⚠️ Partial | `src/flow_store.rs` + `src/main.rs` (index/transcripts + compaction wired; retention/rotation remains) |
| Backend diagnostics metadata | ✅ | `crates/tengu-core/src/lib.rs` + `src/main.rs` (`status`/`doctor`/`/engine`) |
| User story coverage matrix | ✅ Artifact | `USER_STORIES.md` |

### 🔲 Stubs (defined but not implemented)

| Component | Status |
|-----------|--------|
| HuggingFace Engine | Feature flag exists, code not written |
| Telegram Pipe | Feature flag exists, code not written |
| Discord Pipe | Feature flag exists, code not written |
| WebChat Pipe | Feature flag exists, code not written |
| Tool System (Kit) | Trait defined, no tools implemented |
| Flow Persistence | Partial (CLI transcripts/index implemented, compaction/retention pending) |
| Multi-agent routing | Partial (router implemented, serve runtime pending) |
| Skills System | Config exists, loader not implemented |
| Internal domain event bus | ⚠️ In progress (`DomainEvent` + `EventBus` contracts, bounded bus, and runtime emitters done; subscriber migration pending) |
| Single-orchestrator topology profile | Planned (docs/config shape defined; runtime execution pending) |

---

## 🎯 MVP Definition: What's Needed for the "Wow Demo"

Demo scenario: **Sanya from Karaganda texts a Telegram bot and manages his fleet.**

Requirements:
1. ✅ ~~Config and CLI~~ (done)
2. Anthropic Engine (Claude API) — high-quality responses
3. Telegram Pipe — chat via Telegram
4. Basic tools (Kit) — read/write files
5. Tool-calling loop — agent uses tools autonomously
6. Flow Persistence — conversations survive restarts
7. Multi-agent routing — multiple users, isolated agents

---

## 🏃 Sprints

### Sprint 1 — "Brains" (3-5 days)
**Goal:** Tengu works with Claude, not just Ollama

| # | Task | Crate | Complexity |
|---|------|-------|------------|
| 1.1 | Anthropic Engine (Claude API) (Done) | `tengu-backends` | Medium |
| 1.2 | Streaming for Ollama (Done) | `tengu-backends` | Easy |
| 1.3 | `/engine` — switch model at runtime | `src/main.rs` | Easy |
| 1.4 | Anthropic feature flag + conditional compilation | `Cargo.toml` | Easy |

**Outcome:** `tengu chat` works with both Claude and Ollama. Ollama is streamed; Anthropic is typed REST (non-streaming for now).

```
$ tengu chat --engine anthropic --model claude-sonnet-4-5-20250929
> Hello, tell me about yourself
[streaming response from Claude]
```

---

### Sprint 2 — "Hands" (5-7 days)
**Goal:** Agent can read and write files (Kit)

| # | Task | Crate | Complexity |
|---|------|-------|------------|
| 2.1 | Tool definitions (JSON Schema) | `tengu-core` | Medium |
| 2.2 | `read_file` tool | new `tengu-kit` | Easy |
| 2.3 | `write_file` tool | same | Easy |
| 2.4 | `edit_file` tool (patch-based) | same | Medium |
| 2.5 | `find_files` tool (glob) | same | Easy |
| 2.6 | `search_content` tool (grep) | same | Easy |
| 2.7 | `shell` tool (with confirmation) | same | Medium |
| 2.8 | Tool-calling loop in main.rs | `src/main.rs` | Hard |
| 2.9 | Allow/Deny tool filtering from config | `tengu-core` | Medium |

**Outcome:** Agent can read files, write data, search content.

```
> Create a file drivers/serik.md with the driver's profile
Tengu: ✅ File created: drivers/serik.md
[shows contents]
```

---

### Sprint 3 — "Telegram" (5-7 days)
**Goal:** Tengu is accessible via Telegram

| # | Task | Crate | Complexity |
|---|------|-------|------------|
| 3.1 | Telegram Pipe (teloxide) | `tengu-channels` | Medium |
| 3.2 | Access Policy (approval / allowlist) | `tengu-channels` | Medium |
| 3.3 | Routing: sender → agent | `tengu-core/routing` | Medium |
| 3.4 | Multi-pipe event loop in main.rs | `src/main.rs` | Hard |
| 3.5 | Graceful shutdown (Ctrl+C) | `src/main.rs` | Easy |
| 3.6 | `tengu serve` — daemon mode | `src/main.rs` | Medium |

**Outcome:** Write in Telegram — get a response from Tengu. Different users → different agents.

---

### Sprint 4 — "Memory" (3-5 days)
**Goal:** Conversations persist, knowledge is indexed

| # | Task | Crate | Complexity |
|---|------|-------|------------|
| 4.1 | Flow index + transcript writer (`flows/index.json` + JSONL) | `tengu-core` | Medium |
| 4.2 | Atomic writes + lock discipline for flow index updates | `tengu-core` | Medium |
| 4.3 | Prompt budget assembler (system/recent/retrieval/summary buckets) | `src/main.rs` | Hard |
| 4.4 | Wire `KnowledgeStore::query_with_budget` into chat loop | `src/main.rs` | Medium |
| 4.5 | Compaction pipeline (keep recent + summarize old ranges) | `tengu-memory` | Medium |
| 4.6 | History limit + oversized tool-result guards | `tengu-core` | Medium |
| 4.7 | Transcript retention/rotation + corruption recovery path | `tengu-core` | Medium |
| 4.8 | `/store` + storage diagnostics commands | `src/main.rs` | Easy |
| 4.9 | Internal event bus v1 (`DomainEvent` + bounded in-process bus + runtime emitters) | `tengu-core` + `src/main.rs` | Hard |

**Outcome:** Tengu remembers past conversations and knows the contents of workspace files.

Definition of done for Sprint 4:
1. Restart-safe flows: after process restart, same `flow_key` restores recent context.
2. Token-safe assembly: runtime never exceeds configured input budget; low-priority context is dropped first.
3. Retrieval wired: knowledge results are budget-capped and lens-aware at runtime, not only in library tests.
4. Storage hygiene: retention/rotation and repair routines are test-covered.

---

### Sprint 5 — "Polish" (3-5 days)
**Goal:** Demo-ready product

| # | Task | Crate | Complexity |
|---|------|-------|------------|
| 5.1 | Error handling & recovery | All | Medium |
| 5.2 | `/cost` with real $ data | `src/main.rs` | Easy |
| 5.3 | Multi-agent in a single process | `src/main.rs` | Medium |
| 5.4 | README.md (English) | Project | Easy |
| 5.5 | Docker image (optional) | Project | Medium |
| 5.6 | E2E test: "Sanya" scenario | Tests | Medium |
| 5.7 | Migrate audit/metrics side-effects to event subscribers + backpressure validation | `src/main.rs` + `src/tool_audit.rs` | Hard |

**Outcome:** Ready to record a demo video showing the full journey from install to running a business via Telegram.

---

## 📅 Timeline

```
Week 1-2:  Sprint 1 (Anthropic Engine) ████████
Week 2-3:  Sprint 2 (Kit / Tools)      ██████████████
Week 3-4:  Sprint 3 (Telegram)         ██████████████
Week 5:    Sprint 4 (Memory)           ██████████
Week 5-6:  Sprint 5 (Polish)           ██████████
                                        ─────────────
                                        ~6 weeks to MVP
```

## Architecture Acceptance Gate (All Sprints)

Every sprint item that touches runtime orchestration must confirm:
1. New integrations stay behind adapter traits in `tengu-core`.
2. Lifecycle changes use typed events/contracts.
3. Any temporary inline side-effect path is explicitly tagged for `E11` subscriber migration.

---

## 📐 MVP Architecture

```
┌─────────────────────────────────────────────┐
│                 main.rs                      │
│  ┌─────────────────────────────────────┐     │
│  │           Event Loop                │     │
│  │  Pipes → Routing → Agent → Engine   │     │
│  └───┬─────────┬──────────┬──────┬────┘     │
│      │         │          │      │           │
│  ┌───┴───┐ ┌───┴───┐ ┌───┴──┐ ┌─┴──────┐   │
│  │CLI    │ │Telegram│ │Ollama│ │Anthropic│   │
│  │Pipe   │ │Pipe   │ │Engine│ │Engine   │   │
│  └───────┘ └───────┘ └──────┘ └─────────┘   │
│                    │                         │
│              ┌─────┴─────┐                   │
│              │    Kit    │                   │
│              │ read_file │                   │
│              │write_file │                   │
│              │   shell   │                   │
│              └───────────┘                   │
│                    │                         │
│              ┌─────┴─────┐                   │
│              │   Store   │                   │
│              │ Knowledge │                   │
│              │  + Flows  │                   │
│              └───────────┘                   │
└─────────────────────────────────────────────┘
```

---

## 🚀 Post-MVP (Phases 7-13)

| Phase | What | Priority |
|-------|------|----------|
| Claude Code Engine | Full AI coder via subprocess | High |
| WebChat Pipe | Browser interface | Medium |
| Skills System | Capabilities from markdown files | High |
| Candle ML Refiner + GPU Acceleration | Local token optimization with CUDA/Metal fallback strategy | Medium |
| Discord Pipe | Discord support | Low |
| Sandboxing | Docker, workspace isolation | Medium |

---

## ⚡ Quick Start: What to Do Right Now

**Recommendation:** Start with Sprint 2.1 — **Tool definitions (JSON Schema)**.

Why:
1. Multi-provider baseline now exists (`ollama` + `anthropic` + `openai`), so next bottleneck is tool execution.
2. Tool schema is the contract required for the full tool loop (`E8` epic).
3. It unlocks concrete business workflows (file reads/writes/search/shell) instead of chat-only demos.
4. It de-risks provider expansion by standardizing tool payload shape first.
5. It reduces later rework in runtime orchestration and policy/audit layers.

Ready to start?
