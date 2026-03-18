---
tags:
  - core
  - skills
  - tools
  - extensibility
---

# Skills

Skills are **portable knowledge packages** defined as markdown files. They provide domain-specific documentation that guides agents to use [[Tools|platform primitives]] for specific workflows. Skills are the primary extension mechanism — **fully plug-and-play, cross-platform compatible, no code changes required**.

## How It Works

1. Drop a `SKILL.md` file into `skills/<name>/` directory
2. Tengu loads it at startup (hot-reloadable)
3. The skill documentation is injected into the agent's system prompt
4. The agent reads the docs and uses platform primitives (`http_request`, `sign_and_send_transaction`, etc.)

**Skills are never modified by the platform.** They are copy-pasted as-is from external sources (beach.science, OpenClaw skill registry, etc.) and work on any platform that provides basic primitives.

## Two Skill Formats

### Documentation Skills (Frontmatter)

External API documentation. The markdown body is injected into the system prompt. No tools are created — the agent uses `http_request` guided by the docs.

```markdown
---
name: beach-science
description: Scientific social platform for AI agents.
homepage: https://beach.science
---

# Beach.Science API

POST /api/v1/posts — create a post
GET /api/v1/posts — list posts
Authorization: Bearer $BEACH_API_KEY
...
```

Only `name` and `homepage` (or `base_url`) are required in frontmatter. All other fields are optional and platform-specific — ignored by platforms that don't support them.

### Shell Skills (Classic)

Tengu-native tools with shell execution templates. These DO create named tools.

```markdown
# grep_code

Search source files for a pattern.

## Parameters
- `pattern` (string, required): regex pattern

## Execution
```bash
rg --no-heading "{{pattern}}" src/
```
```

## Skill Discovery

At startup (and on hot-reload), Tengu scans three locations in priority order:

| Priority | Path | Use Case |
|----------|------|----------|
| 1 | `{workspace}/.tengu/skills/` | Agent-specific overrides |
| 2 | `{workspace}/skills/` | Workspace-local skills |
| 3 | `{cwd}/skills/` | Global/repo-wide skills |

Higher-priority paths win on name collisions.

## Per-Agent Filtering

```toml
# QA agent: only search tools
[agents.qa]
skill_packages = ["search", "test_runner"]

# DeSci agent: minting workflow
[agents.onchain_minter]
skill_packages = ["aura-orchestrator"]

# Main agent: no skill_packages = all skills available
[agents.main]
```

## Cross-Platform Compatibility

Skills are designed to work on any platform (Tengu, OpenClaw, ZeroClaw, Claude Code, etc.) that provides:
- `http_request` — for REST API calls
- `sign_and_send_transaction` / `sign_message` — for blockchain operations
- `read_file` / `write_file` / `run_command` — for workspace operations

The skill provides the **knowledge** (endpoints, auth, workflow order). The platform provides the **muscles** (HTTP client, crypto signing, file I/O).

## No Capabilities Required

Skills don't need [[Capabilities]]. They are filtered by `skill_packages` only. The agent's platform tool access is controlled separately via `capabilities` in agent config.

Adding a skill = add SKILL.md + add to `skill_packages`. Done.

## Context Injection

For documentation skills (frontmatter format), the entire markdown body is injected into the agent's system prompt. This gives the agent full API reference to construct correct `http_request` calls. Context is capped at `prompt_budget.max_skill_context_tokens` (default: 4000 tokens per skill).

## Hot-Reload

The skill registry supports live updates:
- **Content-hash tracking**: Changed files are re-parsed and swapped in automatically
- **Per-skill enable/disable**: Via CLI or runtime commands

## Skill Catalog

### DeSci Skills

| Skill | Description | Agent |
|-------|-------------|-------|
| [[Skill - POI Register]] | Register Proof of Innovation for research PDF | onchain_minter |
| [[Skill - IP-NFT Mint]] | 10-step IP-NFT minting pipeline on Sepolia | onchain_minter |
| [[Skill - Molecule Auth]] | 4-step Molecule service token authentication | mol_labs |
| [[Skill - Molecule Project]] | Create Molecule project data room | mol_labs |
| [[Skill - Molecule Upload]] | 3-step S3 file upload to Molecule | mol_labs |
| [[Skill - Molecule Announcement]] | Create project announcement | mol_labs |
| [[Skill - Beach Science]] | Scientific social platform publishing | beach_scientist |
| [[Skill - Aura Orchestrator]] | Complete DeSci workflow reference | all DeSci agents |

### Infrastructure Skills

| Skill | Description | Agent |
|-------|-------------|-------|
| [[Skill - Privy Wallets]] | Agentic wallet management (15+ chains) | wallet_manager |

## Related

- [[Tools]] — primitives that skills compose
- [[Agents]] — who loads and uses skills
- [[Capabilities]] — permissions for platform tools (separate from skills)
- [[Configuration]] — `skill_packages` and `prompt_budget` settings
- [[Agent Configuration]] — concise config guide
