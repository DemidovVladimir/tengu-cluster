# Sandboxes — Multi-Agent Domain Configurations

Sandboxes let you define isolated multi-agent teams for different domains (web studio, logistics, customer support, etc.) without modifying your default `~/.tengu/config.toml`.

## Quick Start

```bash
# Create a sandbox
mkdir -p sandboxes/webstudio
cp config.example.toml sandboxes/webstudio/config.toml
# Edit to define your agents...

# Run orchestrator with sandbox
tengu orchestrate --sandbox webstudio

# Run Telegram bot with sandbox
tengu telegram --sandbox webstudio
```

## How It Works

The `--sandbox <name>` flag loads config from `sandboxes/<name>/config.toml` relative to the current working directory, instead of `~/.tengu/config.toml`.

Each sandbox is a self-contained config file. There is no inheritance from the default config — define everything the team needs.

**Memory isolation**: When agents have a `workspace` configured, persistent memory is automatically stored in `<workspace>/memory/` instead of the global `~/.tengu/memory/`. This means DeSci and WebStudio sandboxes get separate memory stores by default — no config changes needed. For Qdrant, a workspace-scoped collection name is used (e.g., `tengu-memory-desci-sandbox`).

## Defining Agents

Agents are fully dynamic. Any role string works — there are no hardcoded role names. Define agents in `[agents.<id>]` sections:

```toml
[agents.warehouse_manager]
engine = "openrouter"
model = "nvidia/nemotron-3-super-120b-a12b:free"
role = "warehouse_manager"
workspace = "~/logistics-project"
capabilities = ["workspace.read", "workspace.list", "workspace.shell"]

[agents.warehouse_manager.identity]
name = "Warehouse Manager"
instructions = """You manage inventory and warehouse operations.
Track stock levels, coordinate shipments, and optimize storage."""

[agents.warehouse_manager.flow]
scope = "per-sender"
reset_mode = "idle"

[agents.warehouse_manager.limits]
max_tokens_per_flow = 80_000
```

### Key Fields

| Field | Description |
|-------|-------------|
| `role` | Any non-empty string. Used for task routing in orchestrator (`role: task description`). |
| `requires` | List of role keys this agent depends on. The planner ensures tasks for this agent always follow tasks from required roles. Example: `requires = ["hypothesis_researcher"]`. |
| `capabilities` | Hard runtime permissions. Controls which workspace primitives and subsystem tools the agent can use. See examples below. |
| `skill_packages` | Skill/workflow packages to load into the agent prompt and tool registry. |
| `workspace` | Shared or per-agent workspace directory. Tilde expansion supported. |
| `identity.instructions` | Role-specific system prompt. This is where you define what the agent does. |

### Capability Restrictions

The `capabilities` field controls which workspace primitives and subsystem features an agent can use:

```toml
# Read-only advisor — cannot write files or run commands
capabilities = ["workspace.read", "workspace.list"]

# Full workspace access
capabilities = ["workspace.read", "workspace.list", "workspace.write", "workspace.shell"]

# Workspace + memory
capabilities = ["workspace.read", "workspace.list", "workspace.write", "memory.remember"]
```

Available workspace capabilities: `workspace.read`, `workspace.list`, `workspace.write`, `workspace.shell`. Subsystem tools (e.g., `memory.remember`) require their corresponding capability. See [CONFIGURATION.md](CONFIGURATION.md) for the full capabilities reference.

## Using the Orchestrator

Submit tasks with `<role>: <description>`:

```
> frontend_engineer: Build a responsive navbar component
> designer: Review the color palette for accessibility
> backend_engineer: Add rate limiting to the API
```

Commands:
- `/fleet` — Show agent status (idle/busy/failed)
- `/tasks` — Show task history
- `/quit` — Exit

## Using with Telegram

```bash
tengu telegram --sandbox webstudio
```

All agents from the sandbox config are loaded. Route messages to specific agents with `@role:` prefix:

```
@backend_engineer: add rate limiting to the auth endpoint
@designer: review the navbar spacing
@frontend_engineer: build a responsive hero section
```

Messages without a prefix are orchestrated across the team automatically. Use `@role: message` when you want to force a specific agent.

Commands:
- `/agents` — List all available agents and their roles
- `/team <goal>` — Explicitly plan and execute a goal across multiple agents (parallel batches with dependencies)
- `/project <name>` — Create a new project subfolder in the workspace (resets conversations)
- `/help` — Show help
- `/stop` — Cancel the current operation
- `/reset` — Clear conversation for the active agent

Set `TELEGRAM_BOT_TOKEN` in your secrets vault or environment.

## Example Sandboxes

### Web Studio (`sandboxes/webstudio/`)

A 4-agent web development team:
- **system_designer** — System architecture, API contracts (full tools)
- **designer** — Design tokens, specs, visual review (no shell)
- **backend_engineer** — APIs, database, auth (full tools)
- **tech_writer** — Documentation, API references (no shell)

### DeSci (`sandboxes/desci/`)

A 4-agent Decentralized Science pipeline with declared dependencies:
- **hypothesis_researcher** — PDF analysis, hypothesis extraction
- **onchain_minter** — IPNFT minting via Molecule/Sepolia (`requires = ["hypothesis_researcher"]`)
- **mol_labs** — Molecule project creation, file uploads (`requires = ["onchain_minter"]`)
- **beach_scientist** — Science publishing on Beach.science (`requires = ["hypothesis_researcher", "onchain_minter", "mol_labs"]`)

## Workspace Scaffold

Sandboxes can auto-create workspace directories and seed files on startup. This runs before agents start, so the workspace is always ready.

```toml
[scaffold]
root = "~/webstudio-project"
directories = ["src", "src/components", "public", "docs", "config"]

[[scaffold.files]]
path = "README.md"
content = """# My Project
Managed by Tengu.
"""

[[scaffold.files]]
path = "src/index.html"
content = """<!DOCTYPE html>
<html><head><title>Project</title></head><body></body></html>
"""
```

### Scaffold Rules

- `root` — workspace root directory (tilde expansion supported)
- `directories` — created recursively (like `mkdir -p`)
- `files` — seed files created only if they don't already exist (never overwrites)
- Runs once at startup, before any agent handles messages

### Creating Your Own

1. `mkdir -p sandboxes/<name>`
2. Create `sandboxes/<name>/config.toml`
3. Define `[scaffold]` (optional), `[orchestrator]`, `[agents.*]` sections
4. Run with `--sandbox <name>`

Any domain works: logistics, healthcare, education, finance, etc. The system imposes no restrictions on role names or agent counts.

## Multi-Project Workflows

Use `/project <name>` in Telegram to create isolated project subfolders within the workspace:

```
/project peptide-study
```

This creates `{workspace}/peptide-study/` with all scaffold subdirectories, switches all agents to work inside it, and resets conversations. Start another project anytime without restarting:

```
/project coral-hypothesis
```

Use `/project` (no argument) to see the current workspace path.
