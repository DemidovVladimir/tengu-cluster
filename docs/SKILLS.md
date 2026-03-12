# Skills Guide

Skills are custom tools defined as markdown files. They let your agent execute shell commands, scripts, and workflows during conversation.

## How It Works

1. You create a `skills/*.md` file describing a tool.
2. Tengu parses it into a tool definition at startup.
3. The agent sees the tool in its available tools and can call it.
4. When called, Tengu substitutes parameters into the execution template and runs it.
5. The output is returned to the agent as a tool result.

## Creating a Skill

Create a markdown file in the `skills/` directory at your project root. The filename doesn't matter — the skill name comes from the `# heading`.

### Minimal Example

```markdown
# grep_code

Search source files for a pattern.

## Parameters
- `pattern` (string, required): regex pattern to search for

## Execution
```bash
rg --no-heading "{{pattern}}" src/
```
```

### Full Example with All Sections

```markdown
# deploy_service

Deploy a service to the staging environment.

## Parameters
- `service` (string, required): service name to deploy
- `tag` (string, required): Docker image tag
- `dry_run` (boolean, optional): preview without applying

## Execution
```bash
./scripts/deploy.sh --service "{{service}}" --tag "{{tag}}" {{#dry_run}}--dry-run{{/dry_run}}
```

## Policy
- risk_level: high
- requires_approval: true
```

## Skill File Format

### Heading (Required)

The `# heading` is the skill name. It must be alphanumeric with underscores only (no spaces or hyphens).

```markdown
# my_tool_name
```

### Description (Required)

The text between the heading and the first `##` section is the tool description shown to the model.

```markdown
# search

Search the codebase for files matching a pattern. Returns file paths, one per line.
```

### Parameters Section (Required)

Define parameters as a bullet list under `## Parameters`:

```markdown
## Parameters
- `name` (type, required): description text
- `name` (type, optional): description text
```

**Supported types:**

| Type | JSON Schema | What the Model Sends |
|------|------------|---------------------|
| `string` | `"string"` | Text value |
| `number` | `"number"` | Integer or float |
| `boolean` | `"boolean"` | `true` or `false` |

**Format rules:**
- Parameter name must be in backticks
- Type must be in parentheses
- `required` or `optional` must follow the type
- Description follows the colon

### Execution Section (Required)

A fenced code block under `## Execution` with the shell command template:

```markdown
## Execution
```bash
command "{{param1}}" --flag "{{param2}}"
```
```

**Template placeholders:**
- `{{param_name}}` is replaced with the parameter value (shell-escaped)
- Parameters are substituted before execution
- The command runs in the agent's workspace directory (if configured)

### Policy Section (Optional)

```markdown
## Policy
- risk_level: low
- requires_approval: false
```

| Field | Values | Default |
|-------|--------|---------|
| `risk_level` | `low`, `medium`, `high` | `medium` |
| `requires_approval` | `true`, `false` | `true` |

**Risk levels:**

| Level | Meaning | Example |
|-------|---------|---------|
| `low` | Read-only, no side effects | Search, list files, read configs |
| `medium` | Potentially mutating, bounded impact | Write files, modify configs |
| `high` | System-level, hard to reverse | Delete operations, deploy, restart services |

When `requires_approval: true`, the user is prompted to confirm before execution.

## API Skills (Frontmatter Format)

API skills use YAML frontmatter instead of the classic `# name` + `## Execution` format. They're designed for wrapping external REST APIs — Tengu auto-generates a curl-based tool and injects the markdown body into the agent's system prompt as context.

### Format

```markdown
---
name: beach-science
description: Scientific social platform for AI agents.
homepage: https://beach.science
---

# Beach.Science API

Full API documentation here. This entire body is injected into the
agent's system prompt so it knows how to use the API.

## Endpoints

POST /api/v1/post — create a post
GET /api/v1/post — list posts
```

### Frontmatter Fields

| Field | Required | Description |
|-------|----------|-------------|
| `name` | Yes | Skill name. Hyphens are normalized to underscores (e.g., `beach-science` becomes `beach_science`). |
| `description` | No | Short description shown to the model in the tool definition. |
| `homepage` / `base_url` | Yes | Base URL for API calls. Used in the generated curl template. |
| `auth_env` | No | Environment variable holding the API key. Defaults to `<NAME>_API_KEY` (e.g., `BEACH_SCIENCE_API_KEY`). |
| `headers` | No | Custom HTTP headers (indented key-value pairs). When set, replaces the default `Authorization: Bearer` header. Values may reference env vars with `$VAR`. |

### Generated Tool

Each API skill produces a single tool with three parameters:

| Parameter | Type | Required | Description |
|-----------|------|----------|-------------|
| `method` | string | Yes | HTTP method (GET, POST, PUT, DELETE) |
| `path` | string | Yes | API path (e.g., `/api/v1/post`) |
| `body` | string | No | JSON request body |

The execution template is:

```bash
curl -s -X {{method}} 'BASE_URL{{path}}' -H 'Content-Type: application/json' -H 'Authorization: Bearer $AUTH_ENV' -d '{{body}}'
```

### Custom Headers

By default, API skills generate an `Authorization: Bearer $AUTH_ENV` header. To use different headers (e.g., API keys, service tokens), add a `headers` block:

```markdown
---
name: molecule-api
description: DeSci GraphQL API
homepage: https://staging.graphql.api.molecule.xyz/graphql
headers:
  x-api-key: $MOLECULE_API_KEY
  x-service-token: $MOLECULE_SERVICE_TOKEN
---
```

When `headers` is present, the default `Authorization: Bearer` header is replaced entirely. `Content-Type: application/json` is always included.

### GraphQL APIs

For GraphQL APIs where all requests go to the same endpoint, set `homepage` to the full GraphQL URL. The agent should use `path: ""` (empty string) since the base URL already includes the endpoint. Document this in the skill body to prevent the model from guessing paths like `/graphql` or `/api/v1/graphql`.

### Context Injection

The markdown body (everything after the closing `---`) is injected into the agent's system prompt. This gives the model full API documentation so it can construct correct requests. Each context fragment is capped at `prompt_budget.max_skill_context_tokens` (default: 8000 tokens, configurable per-agent).

### Example

Given `skills/beach-science.md` with frontmatter, the agent gets:
- A tool called `beach_science(method, path, body)` it can call
- The full API docs in its system prompt for reference

## Per-Agent Skill Filtering

Restrict which skills an agent can use via the `skills` config field:

```toml
# QA agent: only search and test tools
[agents.qa]
engine = "openrouter"
model = "anthropic/claude-sonnet-4"
role = "qa"
skills = ["search", "test_runner", "lint"]

# Backend agent: file manipulation and build tools
[agents.backend]
engine = "openrouter"
model = "anthropic/claude-sonnet-4"
role = "backend_engineer"
skills = ["read_file", "write_file", "search", "build"]

# Main agent: no skills field = all skills available
[agents.main]
default = true
engine = "openrouter"
model = "anthropic/claude-sonnet-4"
```

**Rules:**
- `skills = ["name1", "name2"]` — agent can only use listed skills
- No `skills` field — agent can use all discovered skills
- Skill names must match the `# heading` in the markdown file

## Built-In Workspace Primitives

When an agent has `workspace` configured, four built-in primitives are available automatically (no skill file needed):

| Primitive | Parameters | Risk | Approval | Description |
|-----------|-----------|------|----------|-------------|
| `read_file` | `path` (string) | Low | No | Read file contents (text and PDF with auto-extraction) |
| `list_directory` | `path` (string) | Low | No | List files and directories (`"."` for root) |
| `write_file` | `path` (string), `content` (string) | Medium | Yes | Write content to file, creates parent dirs |
| `run_command` | `command` (string) | High | Yes | Execute a shell command in the workspace directory (via `sh -c`) |

These are the stable foundation through which all skills and external tools interact with the workspace. Additional subsystems (e.g., memory) register their own tools dynamically — when memory is enabled, the `remember` tool is automatically available.

Tools with "Yes" approval require user confirmation — via dialog in TUI mode, or inline keyboard buttons in Telegram mode. Approval dialogs are generated from tool metadata, not hardcoded per tool name.

These tools operate strictly within the workspace boundary.

## Skill Discovery

At startup (and on hot-reload), Tengu scans three locations in priority order:

| Priority | Path | Use Case |
|----------|------|----------|
| 1 | `{workspace}/.tengu/skills/` | Agent-specific overrides |
| 2 | `{workspace}/skills/` | Workspace-local skills |
| 3 | `{cwd}/skills/` | Global/repo-wide skills (shared across sandboxes) |

Higher-priority paths win on name collisions (dedup by skill name). The CWD path allows sandboxes with external workspaces (e.g., `~/desci-workspace`) to use skills from the tengu-cluster repo (`skills/aura-orchestrator/`, `skills/beach-science/`).

For each discovered skill file, Tengu:

1. Tries frontmatter parsing first; falls back to classic format
2. Validates: name format, required sections, parameter types
3. Filters by agent's `skills` allowlist (if set)
4. Classic skills produce a `ToolDef` + `SkillDefinition` for execution
5. API skills produce a `ToolDef` + `SkillDefinition` + context fragment for the system prompt
6. Names that conflict with built-in workspace primitives (`read_file`, `list_directory`, `write_file`, `run_command`) are rejected

## Examples

### File Search

```markdown
# search

Find files matching a glob pattern in the workspace.

## Parameters
- `pattern` (string, required): glob pattern (e.g., "*.rs", "src/**/*.ts")

## Execution
```bash
find . -name "{{pattern}}" -type f | head -50
```

## Policy
- risk_level: low
- requires_approval: false
```

### Test Runner

```markdown
# test_runner

Run the project test suite and report results.

## Parameters
- `filter` (string, optional): test name filter pattern

## Execution
```bash
cargo test {{filter}} 2>&1
```

## Policy
- risk_level: low
- requires_approval: false
```

### Git Status

```markdown
# git_status

Show current git status including branch, staged, and unstaged changes.

## Parameters

## Execution
```bash
git status
```

## Policy
- risk_level: low
- requires_approval: false
```

### Database Migration

```markdown
# db_migrate

Run database migrations on the configured database.

## Parameters
- `direction` (string, required): "up" or "down"
- `steps` (number, optional): number of migrations to apply

## Execution
```bash
sqlx migrate {{direction}} {{steps}}
```

## Policy
- risk_level: high
- requires_approval: true
```

## Hot-Reload and Enable/Disable

The skill registry supports live updates without restarting the agent:

- **Content-hash hot-reload**: Each skill file is tracked by a SHA-256 content hash. On periodic rescan, changed files are re-parsed and swapped in automatically. Unchanged files are skipped.
- **Per-skill enable/disable**: Individual skills can be disabled via `cargo run -- skill disable <name>` and re-enabled with `cargo run -- skill enable <name>`. Disabled skills are excluded from the active tool set but remain in the registry for re-enabling.
