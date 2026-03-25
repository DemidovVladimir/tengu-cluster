# Architecture

Tengu is a single-binary AI agent runtime. All code lives in `src/adapters/` + `src/main.rs` — flat structure, no sub-crates.

## Core Loop

```
Channel (TUI/Telegram) → ChatRuntimeService → Engine → StreamEvent → Response
                              ↓                  ↑
                         ToolExecutor ←── collect_engine_response (tool loop)
```

1. A **channel adapter** (TUI, Telegram) receives user input
2. **ChatRuntimeService** handles memory recall, prompt budgeting, history management
3. The **engine** processes the prompt and returns `StreamEvent`s
4. **collect_engine_response** runs the outer tool loop for engines that don't manage their own tools (OpenRouter)
5. For engines that manage their own tools (Claude Code), the tool loop runs inside the engine subprocess

## Engine Backends

Tengu supports two engine backends, selectable per agent via `engine = "..."` in config:

| Backend | Transport | Tool Loop | Workspace | Feature Flag |
|---------|-----------|-----------|-----------|-------------|
| [[engine-openrouter\|OpenRouter]] | HTTP JSON | Tengu outer loop | Tengu tools | `openrouter` (default) |
| [[engine-claude-code\|Claude Code]] | CLI subprocess | Claude internal | Claude native + MCP bridge | `claude_code` |

See [[engine-backends]] for detailed comparison.

## Key Abstractions

### Engine trait (`src/adapters/types.rs`)
```rust
trait Engine: Send + Sync {
    fn id(&self) -> &str;
    fn context_window(&self) -> usize;
    fn supports_tool_use(&self) -> bool;
    fn manages_own_workspace(&self) -> bool;
    async fn run(&self, messages, tools, context) -> Stream<StreamEvent>;
}
```

The `manages_own_workspace()` flag is the key discriminator:
- `false` (OpenRouter): Tengu builds tools, runs the outer tool loop, manages workspace operations
- `true` (Claude Code): Engine handles workspace ops natively; Tengu tools are bridged via [[mcp-bridge]]

### EngineContext (`src/adapters/types.rs`)
Passed to every `engine.run()` call:
- `workspace` — filesystem path for the agent
- `system_prompt` — Tengu-composed prompt with identity, skills, memory
- `bridge_tools` — tool definitions for the [[mcp-bridge]] (Claude Code only)

### Tool Assembly (`src/adapters/channel_runtime.rs`)
- `compute_base_tools()` — workspace + platform + memory + cache tools for the outer loop
- `compute_bridge_tools()` — same set but for the MCP bridge when `manages_own_workspace = true`
- `build_tool_executor()` — composite executor wiring workspace, HTTP, crypto, cache, skill executors

## Module Map

| Module | Purpose |
|--------|---------|
| `config.rs` | TOML config schema, validation |
| `types.rs` | Engine trait, Message, ToolCall, StreamEvent, EngineContext |
| `engine_builder.rs` | Engine factory + OpenRouter implementation |
| `claude_code_engine.rs` | Claude Code engine (feature-gated) |
| `mcp_bridge.rs` | Stdio MCP server for tool bridging |
| `chat_builder.rs` | ChatRuntimeService — per-turn orchestration |
| `channel_runtime.rs` | Shared logic for all channel adapters |
| `tool_builder.rs` | Tool definitions + workspace executor |
| `skill_builder.rs` | Skill parsing, registry, system prompt building |
| `memory_builder.rs` | Memory types, service, disk store |
| `telegram_builder.rs` | Telegram bot adapter |
| `tui/mod.rs` | Terminal UI adapter |
| `orchestrator.rs` | Multi-agent fleet orchestrator |

## Related
- [[engine-backends]] — detailed engine comparison
- [[configuration]] — config reference
- [[mcp-bridge]] — MCP tool bridge details
- [[skills]] — skill system architecture
