# Claude Code Backend PRD

> **Status:** Implemented. This is an archived planning document. For live documentation see [[engine-backends#Claude Code]] and [[claude-code-backend-plan]].

## Summary
Add a `claude_code` backend alongside `openrouter`, with backend selection controlled by config. An operator can choose, per agent and per planner, whether a flow runs on token-backed APIs or on a Claude Code subscription.

## Problem
- Tengu agents depend on API tokens for Anthropic-capable workflows
- Users with a Claude Code subscription cannot route Tengu through that subscription
- Tengu tools are tightly coupled to the outer tool-loop runtime

## Goals
- Add a real `claude_code` backend choice next to `openrouter`
- Allow backend selection per agent and for the planner
- Support Claude Code native workspace abilities (file reads, edits, shell access)
- Preserve access to Tengu-native tools (HTTP, crypto, cache, skills) via MCP bridge
- Keep the change config-driven

## Non-Goals
- Replacing OpenRouter entirely
- Token-free embeddings (memory still needs `OPENROUTER_API_KEY`)
- Rebuilding channel adapters around Claude-specific runtime

## Users and Use Cases
- Operators who pay for Claude Code and want to avoid API token usage
- Mixed subscription + token-backed execution in the same cluster
- Run planner on Claude Code, execution agents on OpenRouter (or vice versa)
- Switch an agent between backends by changing config only

## Functional Requirements
- `engine = "openrouter"` or `engine = "claude_code"` per agent
- Planner backend = the `engine` of the `[orchestrator] agent` block (the `planner_engine` key was removed with the orchestration collapse)
- Claude-backed agents run through local `claude` CLI
- Builtin tool profiles: `none`, `read_only`, `editor`, `editor_shell`
- Tengu-native tools bridged via MCP (not lost when using Claude)
- OpenRouter and Claude Code coexist in the same config

## Safety Requirements
- Explicit `can_use_tool` callback (no permissive defaults)
- Workspace containment for all file operations
- `skills/` and `.tengu/skills/` paths blocked for writes
- Destructive shell patterns denied
- Secret redaction on tool results

## Implementation
All requirements met in the current codebase:
- Backend selection: `engine = "claude_code"` per `[agents.<name>]` block; the planner uses the `[orchestrator] agent` block's engine (per `CLAUDE.md`: keep the planner on OpenRouter, subagents on Claude Code)
- Claude execution: one `claude -p --output-format stream-json` subprocess per turn in `src/adapters/claude_code_engine.rs` (no SDK dependency)
- Tool bridge: external stdio MCP server in `src/adapters/mcp_bridge.rs`
- Safety: `--tools <profile>` + `--allowedTools mcp__tengu-tools__*`, per-tool scopes via `TENGU_BRIDGE_SCOPES`, `[egress]` drops builtin Bash under a proxy — the `can_use_tool` callback, workspace containment and destructive-command denial were not implemented (see [[engine-backends#Claude Code]])
- Config: `[claude_code]` global + `[agents.<id>.claude_code]` per-agent
- Mixed operation: verified with both engines in the same config

## Related
- [[engine-backends]] — live engine documentation
- [[mcp-bridge]] — live bridge documentation
- [[claude-code-backend-plan]] — technical plan
