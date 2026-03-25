# Engine Backends

Tengu supports multiple engine backends. Each agent selects its backend via `engine = "..."` in [[configuration]]. Backends are plug-and-play — switching an agent between backends requires only a config change.

## OpenRouter (`engine = "openrouter"`)

**Transport:** HTTP JSON to OpenRouter API
**Feature flag:** `openrouter` (default)
**File:** `src/adapters/engine_builder.rs`

The default backend. Sends chat completions to OpenRouter, which proxies to any supported model (Anthropic, OpenAI, Google, Meta, etc.).

### How it works
1. `Engine::run()` sends an HTTP request to OpenRouter `/v1/chat/completions`
2. Response is parsed into `StreamEvent::TextDelta` and `StreamEvent::ToolCallStart/Delta/End`
3. Tengu's outer tool loop (`collect_engine_response`) executes tool calls and feeds results back
4. Loop continues until no more tool calls or max rounds exceeded

### Key properties
- `manages_own_workspace()` = `false` — Tengu handles all workspace operations
- `supports_tool_use()` = `true` — tool definitions passed to model
- One API key (`OPENROUTER_API_KEY`) for all models
- Pay-per-token pricing

## Claude Code (`engine = "claude_code"`)

**Transport:** CLI subprocess via `claude-agents-sdk`
**Feature flag:** `claude_code` (opt-in: `cargo build --features claude_code`)
**File:** `src/adapters/claude_code_engine.rs`

Runs agents through the local Claude Code CLI. Uses the operator's Claude subscription instead of API tokens.

### How it works
1. `Engine::run()` calls `query_result()` from the Claude Agent SDK
2. SDK spawns the `claude` CLI as a subprocess
3. Claude CLI loads its own CLAUDE.md, MCP servers, and native tools
4. Tengu tools are exposed via the [[mcp-bridge]] subprocess
5. Claude executes the full prompt internally (may use many tools across multiple turns)
6. Final text response returned as `StreamEvent::TextDelta` + `StreamEvent::Done`
7. Tengu's outer tool loop sees no tool calls — passes through immediately

### Key properties
- `manages_own_workspace()` = `true` — Claude handles Read/Write/Edit/Bash natively
- `supports_tool_use()` = `true` — tools handled internally
- Each turn spawns a fresh CLI process (~1-2s overhead)
- Conversation history formatted into the prompt (stateless sessions)
- Uses Claude subscription, no per-token cost

### Builtin Tools Profiles

Configured via `[agents.<id>.claude_code].builtin_tools_profile`:

| Profile | Claude Native Tools | Permission Mode |
|---------|-------------------|-----------------|
| `none` | (none) | Plan (read-only) |
| `read_only` | Read, Glob, Grep | Default |
| `editor` | Read, Glob, Grep, Edit, Write, MultiEdit | Default |
| `editor_shell` | Read, Glob, Grep, Edit, Write, MultiEdit, Bash | Default |

### Safety Policy

Applied via `can_use_tool` callback on every tool request:
- Claude native tools gated by profile
- MCP tools (Tengu tools) always allowed (gated by MCP tool list)
- Bash: deny `rm -rf`, `sudo`, `mkfs`, `dd if=`, `chmod 777`
- File writes: deny `skills/` and `.tengu/skills/` paths
- Workspace containment: deny absolute paths outside workspace

### MCP Bridge

See [[mcp-bridge]] for how Tengu-native tools (http_request, crypto, cache, skills) are exposed to Claude.

## Adding a New Backend

To add a new engine backend:

1. Create `src/adapters/my_engine.rs` implementing the `Engine` trait
2. Add a feature flag in `Cargo.toml`
3. Register in `src/adapters/mod.rs` (feature-gated)
4. Add dispatch in `engine_builder.rs` `build_engine()` and `build_planner_engine()`
5. Add engine name to config validation in `config.rs` `validate_agent()`
6. If the engine manages its own workspace, set `manages_own_workspace() = true` and use `bridge_tools` from `EngineContext`

## Related
- [[architecture]] — system overview
- [[configuration]] — config reference
- [[mcp-bridge]] — tool bridging for self-managing engines
