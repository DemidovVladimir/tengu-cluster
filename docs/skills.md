# Skills

Skills are portable, cross-platform workflow documents that compose Tengu's [[architecture#Key Abstractions|platform primitives]] into higher-level capabilities. Skills are never modified by the platform — they are copied as-is from external sources.

**File:** `src/adapters/skill_builder.rs`

## Skill Types

| Type | Creates Tools | How It Works |
|------|--------------|-------------|
| **Documentation** (frontmatter) | No | Compact XML catalog in system prompt; agent reads SKILL.md on demand |
| **Shell** (classic) | Yes | Named tools with execution templates, injected inline |

API skills are documentation-only — they describe API endpoints for the agent to use with `http_request`. They do not create tools.

## Loading

Skills are loaded from three tiers (later tiers shadow earlier):
1. **Managed:** `~/.tengu/skills/`
2. **Workspace (dotdir):** `<workspace>/.tengu/skills/`
3. **Workspace (root):** `<workspace>/skills/`

Agents select skills via `skill_packages` in [[configuration]]:
```toml
[agents.main]
skill_packages = ["aura-orchestrator", "beach-science"]
```

## Cross-Engine Compatibility

Skills work identically with both [[engine-backends]]:

### OpenRouter
- Skill tools are registered in Tengu's tool loop
- Skill context injected into system prompt
- Agent calls skill tools directly

### Claude Code
- Skill context still injected into system prompt (via `build_system_prompt()`)
- Skill tools exposed via [[mcp-bridge]] as MCP tools
- Agent calls MCP tools by name (same names as the Tengu tools the skill references)
- Claude's native tools (Read, Write, Bash) also available alongside MCP tools

The skill document references Tengu tool names like `http_request`, `sign_and_send_transaction`, `shared_cache`. These exact names are registered as MCP tools in the bridge, so skill instructions work unchanged across engines.

## Frontmatter

```yaml
---
name: my-skill
description: What this skill does
homepage: https://example.com
requires_bins: ["curl"]     # optional: required CLI tools
requires_env: ["API_KEY"]   # optional: required env vars
os: ["linux", "macos"]      # optional: OS filter
---
```

Gating metadata (`requires_bins`, `requires_env`, `os`) is evaluated at load time. Skills that fail gating checks are silently skipped.

## Example: Documentation Skill

`skills/aura-orchestrator/SKILL.md` — a documentation skill that orchestrates a 7-phase DeSci pipeline:
- References `http_request`, `sign_and_send_transaction`, `abi_encode`, `sign_message`, `get_wallet_address`, `shared_cache`, `read_file`, `run_command`
- Works on OpenRouter (Tengu tool loop) and Claude Code (MCP bridge)

## Example: Shell Skill

```markdown
# test_runner
Run the project test suite.
## Parameters
- `filter` (string, optional): test name filter
## Execution
\```bash
cargo test {{filter}} 2>&1
\```
```

Shell skills create named tools that execute templates with parameter substitution. They are injected inline into the tool list.

## Related
- [[architecture]] — where skills fit in the system
- [[configuration]] — skill_packages config
- [[mcp-bridge]] — how skill tools reach Claude Code
- [[engine-backends]] — how skills work with different backends
