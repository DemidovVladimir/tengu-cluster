# Claude Code Backend Technical Plan

> **Status:** Implemented. Key files: `claude_code_engine.rs`, `mcp_bridge.rs`, `engine_builder.rs`, `config.rs`. See [[engine-backends]] and [[mcp-bridge]] for live documentation.

## Summary
Add `claude_code` as a real backend next to `openrouter`, using the local Claude Code CLI for execution while keeping Tengu’s orchestration, channel adapters, prompts, and routing intact.

The implementation has two core pieces:
- Claude Code handles native workspace abilities such as file reads, edits, and optional shell access.
- Tengu exposes its non-native tools through an external stdio MCP bridge subprocess.

This is a config-only backend switch in v1. The same agent definition should be able to run on either backend.

## Architecture
### Backend model
- `openrouter` keeps the current outer tool-loop model.
- `claude_code` runs one fresh Claude CLI query per Tengu turn.
- Planner backend selection uses the same mechanism as agent backend selection.

### Tool model
- Native workspace tools:
  - `read_file`
  - `list_directory`
  - `write_file`
  - `run_command`
  These are not exposed as Tengu tools when `claude_code` is active. Claude Code should use its own built-ins instead.
- Portable Tengu tools:
  - platform tools
  - shared cache
  - active shell skills
  - memory tools, subject to the v1 limitation below
  These are exposed through the MCP bridge.

### Bridge transport
- The bridge is an external stdio MCP server launched as a Tengu subcommand such as `tengu mcp-bridge`.
- Parent and child communicate bridge configuration through a temporary JSON spec file, not large env-var payloads.
- The spec file should include:
  - workspace path
  - portable tool definitions
  - active shell skill definitions needed for execution
  - memory configuration required to decide whether memory tools are available
- The child bridge process owns executor construction. The parent does not pass a live executor across the boundary.

## Key Changes
### Config and validation
- Keep `agent.engine` and `orchestrator.planner_engine`, but make them real selectors for:
  - `openrouter`
  - `claude_code`
- Add global Claude config:
  - `[claude_code]`
  - `cli_path = "claude"`
- Add per-agent Claude config:
  - `[agents.<id>.claude_code]`
  - `builtin_tools_profile = "none" | "read_only" | "editor" | "editor_shell"`
- Validate engine values, planner engine values, and Claude tool-profile values.

### Runtime refactor
- Refactor tool assembly so workspace-native tools and portable Tengu tools are built separately.
- Replace the current all-or-nothing tool path, where `manages_own_workspace() = true` effectively suppresses all tools.
- Keep OpenRouter behavior unchanged.
- For Claude-backed agents:
  - outer-loop tools should remain empty
  - bridge tools should be assembled separately and passed to the Claude backend

### Engine context
- Extend `EngineContext` to carry bridge configuration needed by `claude_code`.
- The new context payload should support at least:
  - workspace
  - system prompt
  - bridge spec input, or a structured equivalent that the engine can serialize into a spec file
- Update all `EngineContext` construction sites, including chat, task, orchestrator, Telegram, and any other runtime path using engines.

### Claude engine
- Add `ClaudeCodeEngine` as a separate module under the adapters layer.
- Use one-shot Claude CLI execution per Tengu turn through `claude-agents-sdk`.
- Always pass explicit:
  - system prompt
  - working directory
  - permission mode
  - allowed native tools
  - `can_use_tool` callback
  - MCP server configuration for the bridge
- Use profile-to-tool mapping:
  - `none` → no native tools, `plan` mode
  - `read_only` → `Read`, `Glob`, `Grep`
  - `editor` → `Read`, `Glob`, `Grep`, `Edit`, `Write`, `MultiEdit`
  - `editor_shell` → `Read`, `Glob`, `Grep`, `Edit`, `Write`, `MultiEdit`, `Bash`
- The engine returns final text and usage only. It does not emit outer `ToolCall*` events.

### MCP bridge
- Add a stdio MCP server module and corresponding CLI subcommand.
- Support at minimum:
  - `initialize`
  - `tools/list`
  - `tools/call`
- Rebuild Tengu tool executors inside the bridge process from the serialized spec.
- Redact secrets on tool results before returning them to Claude.
- Expose only portable tools through MCP. Do not expose duplicated native workspace primitives when Claude Code already owns them.

### Prompt building
- Update prompt generation so Claude-backed agents are informed about:
  - Claude native workspace abilities from the selected builtin profile
  - portable Tengu tools reachable through the MCP bridge
- Do not rely on the current workspace-tool advertisement gate, because that logic is OpenRouter-centric.

## Safety Policy
- Never rely on SDK default behavior for system prompt or tool approval.
- Use an explicit `can_use_tool` callback for every Claude-backed request.
- Allow only tools included in the selected native-tool profile.
- Deny edits outside the configured workspace.
- Deny writes into `skills/` and `.tengu/skills/`.
- Allow Bash only for `editor_shell`.
- Deny clearly destructive Bash patterns such as:
  - `rm -rf`
  - `sudo`
  - `mkfs`
  - `dd if=`
  - writes to obvious system paths
- Treat MCP tools as allow-listed by the bridge spec. Claude should only see the portable tools assembled for that specific agent and turn.

## Known Limitations For V1
- Memory embeddings still rely on `OPENROUTER_API_KEY`, so Claude-backed agents are not fully token-free if memory is enabled.
- One Claude CLI subprocess is spawned per Tengu turn, so there is startup overhead.
- Claude session continuity is stateless across turns; Tengu history is formatted back into the prompt each turn.
- Bridge state is reconstructed per run from serialized config rather than kept as a long-lived in-memory service.

## Test Plan
- Config validation tests for:
  - `agent.engine`
  - `orchestrator.planner_engine`
  - Claude builtin tool profiles
- Engine factory tests proving correct dispatch for `openrouter` and `claude_code`.
- Unit tests for splitting native workspace tools from portable Tengu tools.
- Unit tests for Claude permission policy:
  - deny out-of-workspace edits
  - deny writes into skill directories
  - deny destructive Bash
  - allow per profile
- Unit tests for bridge spec serialization and MCP `tools/list` generation.
- Manual or integration tests for `tengu mcp-bridge` handling:
  - `initialize`
  - `tools/list`
  - `tools/call`
- End-to-end tests for:
  - Claude-backed planner
  - Claude-backed workspace agent
  - Claude-backed custom-tool agent using portable tools through MCP
  - mixed OpenRouter and Claude agents in the same config
- Regression tests proving OpenRouter behavior is unchanged.

## Implementation Mapping

| Plan Item | Implemented In |
|-----------|---------------|
| Config + validation | `src/adapters/config.rs` — `ClaudeCodeConfig`, `AgentClaudeCodeConfig`, engine validation |
| Runtime refactor | `src/adapters/channel_runtime.rs` — `compute_bridge_tools()`, TUI/Telegram wiring |
| EngineContext extension | `src/adapters/types.rs` — `bridge_tools` field |
| Claude engine | `src/adapters/claude_code_engine.rs` — `ClaudeCodeEngine` |
| MCP bridge | `src/adapters/mcp_bridge.rs` + `tengu mcp-bridge` subcommand |
| Engine dispatch | `src/adapters/engine_builder.rs` — `build_engine()`, `build_planner_engine()` |
| Safety policy | `src/adapters/claude_code_engine.rs` — `build_safety_policy()` |
| Sandbox | `sandboxes/aura-claude/config.toml` |

## Assumptions
- Claude Code is installed locally and authenticated outside Tengu.
- The implementation uses an external stdio MCP bridge, not the SDK’s incomplete in-process MCP support.
- Backend switching remains config-only in v1.
- This work extends Tengu’s existing engine/runtime architecture rather than replacing it.
