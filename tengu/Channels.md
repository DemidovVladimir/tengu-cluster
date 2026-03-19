---
tags:
  - core
  - channels
---

# Channels

**Channels** are communication adapters — how users interact with [[Agents]]. They are isolated so adding a new channel **never touches business logic**.

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
- Direct multi-agent dispatch via event-bus
- `cargo run -- orchestrate`

## Adding a New Channel

A new channel adapter needs only:
1. **Approval Adapter** — implements `ToolApprovalPort`
2. **Activity Adapter** — implements `ToolActivityPort`

All shared logic lives in `src/adapters/channel_runtime.rs`:
- Tool/executor/prompt rebuilding
- Memory initialization
- Agent routing
- Message chunking
- State factories

The core logic is completely channel-agnostic.

## Channel-Ready Design

The system is built to support future channels (Slack, Discord, API, email) with minimal effort. Each channel is an adapter — no changes to:
- [[Agents]] configuration
- [[Tools]] or [[Skills]]
- [[Orchestrator]] logic
- [[Memory]] subsystem

## Related

- [[Agents]] — what channels talk to
- [[Orchestrator]] — multi-agent routing through channels
- [[Architecture]] — project structure
- [[Configuration]] — channel-specific config (`[telegram]`)
