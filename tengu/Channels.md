---
tags:
  - core
  - channels
---

# Channels

**Channels** are communication adapters — how users interact with [[Agents]]. They are isolated behind port traits, so adding a new channel **never touches business logic**.

## Current Channels

### TUI (Terminal)
- Full-screen interface via `cursive`
- Interactive tool approval dialogs
- `cargo run -- chat`

### Telegram
- Headless bot via `teloxide` (feature-gated `--features telegram`)
- Multi-agent routing: `@role: message` or auto-orchestrated
- Inline keyboard approval (Approve/Deny buttons, 60s timeout)
- `/agents`, `/team <goal>`, `/project <name>`, `/wallet` commands
- File attachments (docs, photos) via `download_telegram_file()`
- Typing indicator survives sync tool blocking
- `/stop` cancellation via `AtomicBool`
- `cargo run -- telegram`

### CLI Orchestrator
- Direct multi-agent dispatch
- `cargo run -- orchestrate`

## Adding a New Channel

A new channel adapter needs only:
1. **I/O Pipe** — implements `Pipe` trait from `tengu-core`
2. **Approval Adapter** — implements `ToolApprovalPort`
3. **Activity Adapter** — implements `ToolActivityPort`

All shared logic lives in `src/adapters/channel_runtime.rs`:
- Tool/executor/prompt rebuilding
- Memory initialization
- Agent routing
- Message chunking
- State factories

The [[Architecture|application and domain layers]] are completely channel-agnostic.

## Channel-Ready Design

The system is built to support future channels (Slack, Discord, API, email) with minimal effort. Each channel is an adapter behind a port trait — no changes to:
- [[Agents]] configuration
- [[Tools]] or [[Skills]]
- [[Orchestrator]] logic
- [[Memory]] subsystem

## Related

- [[Agents]] — what channels talk to
- [[Orchestrator]] — multi-agent routing through channels
- [[Architecture]] — ports and adapters pattern
- [[Configuration]] — channel-specific config (`[telegram]`)
