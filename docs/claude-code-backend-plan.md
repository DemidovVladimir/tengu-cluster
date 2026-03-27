# Claude Code Backend Technical Plan

> **Status:** Implemented. This is an archived planning document. For live documentation see [[engine-backends]] and [[mcp-bridge]].
> **Key files:** `claude_code_engine.rs`, `mcp_bridge.rs`, `engine_builder.rs`, `config.rs`

## Summary
Add `claude_code` as a real backend next to `openrouter`, using the local Claude Code CLI for execution while keeping Tengu's orchestration, channel adapters, prompts, and routing intact.

The implementation has two core pieces:
- Claude Code handles native workspace abilities such as file reads, edits, and optional shell access.
- Tengu exposes its non-native tools through an external stdio MCP bridge subprocess.

This is a config-only backend switch. The same agent definition can run on either backend.

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
  These are not exposed as Tengu tools when `claude_code` is active. Claude Code uses its own built-ins instead.
- Portable Tengu tools:
  - platform tools
  - shared cache
  - active shell skills
  - memory tools, subject to the v1 limitation below
  These are exposed through the MCP bridge.

### Bridge transport
- The bridge is an external stdio MCP server launched as a Tengu subcommand: `tengu mcp-bridge`.
- Parent and child communicate bridge configuration through environment variables (`TENGU_BRIDGE_WORKSPACE`, `TENGU_BRIDGE_TOOLS`).
- The child bridge process owns executor construction. The parent does not pass a live executor across the boundary.

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

## Safety Policy
- Explicit `can_use_tool` callback for every Claude-backed request
- Allow only tools included in the selected native-tool profile
- Deny edits outside the configured workspace
- Deny writes into `skills/` and `.tengu/skills/`
- Allow Bash only for `editor_shell`
- Deny destructive Bash patterns: `rm -rf`, `sudo`, `mkfs`, `dd if=`, `chmod 777`
- MCP tools allow-listed by the bridge spec

## Known V1 Limitations
- Memory embeddings still need `OPENROUTER_API_KEY` (not token-free)
- Each turn spawns a fresh `claude` CLI process (~1-2s overhead)
- Sessions are stateless (history re-formatted into prompt each turn)
- Bridge re-creates executors per invocation (no shared state with parent)
- Secret redaction not yet applied in the MCP bridge path

## Related
- [[engine-backends]] — live engine documentation
- [[mcp-bridge]] — live bridge documentation
- [[claude-code-backend-prd]] — product requirements
