# Claude Code Backend PRD

> **Status:** Implemented. See [[engine-backends#Claude Code]] for live documentation and [[claude-code-backend-plan]] for the technical plan.

## Summary
Tengu Cluster currently relies on token-backed API providers such as OpenRouter for model execution. The goal of this change is to let Tengu run selected or all agents against a Claude Code subscription instead of API tokens, while preserving the current OpenRouter path as a configurable alternative.

This PRD defines the product requirements for adding a `claude_code` backend alongside `openrouter`, with backend selection controlled by config. The target outcome is that an operator can choose, per agent and per planner, whether a flow runs on token-backed APIs or on a Claude Code subscription.

## Problem
- Tengu agents currently depend on API tokens for Anthropic-capable workflows.
- Users with an active Claude Code subscription cannot route Tengu through that subscription.
- The current engine layer ignores `agent.engine` in practice and always builds the OpenRouter path.
- Tengu tools are tightly coupled to the current outer tool-loop runtime, which makes backend replacement non-trivial.

## Goals
- Add a real `claude_code` backend choice next to `openrouter`.
- Allow backend selection per agent and for the planner.
- Support Claude Code native workspace abilities for file reads, edits, and optional shell access.
- Preserve access to Tengu-native capabilities such as HTTP tools, shared cache, crypto tools, and skills when using Claude Code.
- Keep the change config-driven rather than introducing a new runtime surface in v1.

## Non-Goals
- Replacing OpenRouter entirely.
- Making the Claude and OpenRouter backends identical at the transport level.
- Solving token-free embeddings in the same change.
- Rebuilding Telegram, TUI, or orchestrator flows around a separate Claude-specific runtime.

## Users and Use Cases
### Primary users
- Tengu operators who already pay for Claude Code and want to avoid Anthropic API token usage for some or all flows.
- Operators who want to mix subscription-backed and token-backed execution in the same cluster.

### Key use cases
- Run the planner on Claude Code while leaving execution agents on OpenRouter.
- Run coding or repo-centric agents on Claude Code with workspace access.
- Switch an existing agent between `openrouter` and `claude_code` by changing config only.
- Keep OpenRouter available for fallback or for cases where it remains preferable.

## Functional Requirements
### Backend selection
- Each agent must support `engine = "openrouter"` or `engine = "claude_code"`.
- The orchestrator planner must support `planner_engine = "openrouter"` or `planner_engine = "claude_code"`.
- Backend selection must be resolved from config and must not be silently ignored.

### Claude Code execution
- Claude-backed agents must run through the local Claude Code CLI.
- Claude-backed agents must receive the Tengu-composed system prompt explicitly.
- Claude-backed agents must map the configured workspace to Claude Code’s working directory.
- Claude-backed agents must support built-in native tool profiles:
  - `none`
  - `read_only`
  - `editor`
  - `editor_shell`

### Tengu tool access under Claude
- Claude-backed agents must retain access to Tengu-native tools through a bridge rather than losing those capabilities outright.
- Workspace-native operations should use Claude Code built-ins instead of duplicating them through Tengu tools.
- Non-native Tengu tools should remain available through a bridge layer that Claude can invoke.

### Mixed-backend operation
- OpenRouter and Claude Code must coexist in the same config.
- Existing OpenRouter-backed agents must keep their current behavior.
- Claude-backed and OpenRouter-backed agents must both work under TUI, Telegram, and orchestrator flows.

## Safety Requirements
- The implementation must not rely on permissive defaults from third-party Claude wrappers.
- Claude tool approval must be explicit and policy-driven.
- Claude edits must be limited to the configured workspace.
- Writes to `skills/` and `.tengu/skills/` must remain blocked.
- Destructive shell patterns must be denied when Claude is allowed to use Bash.
- Tengu tool outputs sent back through the bridge must remain secret-redacted.

## Configuration Requirements
- Add a global `[claude_code]` section with `cli_path`.
- Add per-agent Claude config under `[agents.<id>.claude_code]`.
- Per-agent Claude config must include `builtin_tools_profile`.
- Validation must reject unsupported engine values and unsupported Claude profiles.

## Constraints and Dependencies
- Claude Code must be installed locally and accessible via the configured CLI path.
- Tengu must remain operable without network calls to provider APIs when Claude Code is selected, except for subsystems that still depend on API-backed embeddings.
- Memory embeddings are explicitly out of scope for token-free operation in this change; they remain a separate dependency.

## Success Criteria
- A Tengu operator can configure at least one agent and the planner to use `claude_code`.
- The backend actually changes at runtime based on config.
- Claude-backed workspace agents can inspect and edit files inside the configured workspace.
- Claude-backed agents can still reach Tengu-native non-workspace tools through the bridge.
- OpenRouter-backed agents behave as they did before.

## Risks
- Claude Code tool naming or control-protocol behavior may differ from assumptions and require adapter tuning.
- The bridge layer adds failure modes around subprocess lifecycle and stdio protocol handling.
- Tool exposure must be carefully split so Claude does not see duplicate or misleading workspace capabilities.
- Memory-backed agents will still need token-backed embeddings unless that subsystem is redesigned later.

## Implementation Notes

All PRD requirements are met in the current implementation:
- Backend selection: `engine = "claude_code"` per agent and `planner_engine = "claude_code"` for planner
- Claude execution: via `claude-agents-sdk` one-shot `query_result()` in `src/adapters/claude_code_engine.rs`
- Tool bridge: external stdio MCP server in `src/adapters/mcp_bridge.rs`, invoked as `tengu mcp-bridge`
- Safety: `can_use_tool` callback enforces profile, workspace containment, destructive command denial
- Config: `[claude_code]` global section + `[agents.<id>.claude_code]` per-agent section
- Mixed operation: verified by having both `openrouter` and `claude_code` agents in the same config

### Known V1 Limitations
- Memory embeddings still need `OPENROUTER_API_KEY` (not token-free)
- Each turn spawns a fresh `claude` CLI process (~1-2s overhead)
- Secret redaction not yet applied in the MCP bridge path
- Sessions are stateless (history re-formatted into prompt each turn)

## Open Decisions Already Fixed For This Work
- Backend switching is config-only in v1.
- Claude support is intended for any agent definition, not only a narrow hybrid subset.
- Claude Code should use native workspace tools; Tengu-native tools should be bridged separately.
