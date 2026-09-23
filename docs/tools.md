# Tools — where they live, how to add one, how agents get them

## Pick the lightest option

| Need | Use | Rust? | Lives in |
|---|---|---|---|
| Call an HTTP API | Skill that teaches `http_request` | no | `skills/<name>/SKILL.md` |
| Reuse an existing tool server | `[[mcp_servers]]` in the sandbox config | no | `sandboxes/<name>/config.toml` |
| New native capability | Rust tool in the catalog | yes | `src/adapters/outbound/tools/<name>/` |

## Built-in tools (`src/adapters/outbound/tools/`)

| Dir | Tools | Gate |
|---|---|---|
| `workspace/` | `read_file`, `list_directory`, `write_file`, `run_command` | always |
| `memory/` | `memory_ingest`, `memory_search` | `[memory] enabled` |
| `memory/` | `persistent_store` | opt-in |
| `http/` | `http_request` | always |
| `crypto/` | `sign_and_send_transaction`, `sign_message`, `get_wallet_address`, `abi_encode`, `hex_to_uint256` | always |
| `skill_resource/`, `view_skill/` | `skill_resource`, `view_skill` | always |
| `cache/` | `shared_cache` | opt-in |
| `agentic_memory/` | `agentic_memory` | opt-in, feature `postgres_memory` |
| `skill_lifecycle/` | `skill_distill`, `apply_improver_proposal` (+ implicit `compress_and_store`) | opt-in |
| `manage_skill/` | `manage_skill` | opt-in |

The list is `catalog()` in `tools/mod.rs` — one `ToolEntry` row per group. That row drives in-process registration, the MCP bridge (Claude Code subagents), and the tool list the model sees.

## Add a Rust tool

1. `src/adapters/outbound/tools/<name>/mod.rs`: `impl Tool` (from `ports::tool`), a `ToolPlugin`, `tool_defs()`. First line of `execute` = `ctx.scope.check_*(..)` or `// scope: pure-compute` (`tests/scope_lint.rs`).
2. `pub(crate) mod <name>;` + one `ToolEntry` in `catalog()`.
3. Opt-in only: add the name to `src/domain/tools.rs::WORKSPACE_TOOLS` (`catalog_tests` fail otherwise).
4. `cargo test --bin tengu catalog && cargo test --test scope_lint`.

## Give it to an agent

```toml
[agents.researcher]
engine = "openrouter"
model = "anthropic/claude-sonnet-4-6"
description = "..."                       # makes it routable by the planner
tools = ["http_request", "read_file", "shared_cache"]   # allow-list; "shared_cache" also opts in
skill_packages = ["my-skill"]

[agents.researcher.scopes.http_request]  # per-tool permissions (deny-by-default per field)
net_hosts = ["api.example.com"]
env_reads = ["EXAMPLE_API_KEY"]

[default_scopes.http_request]            # fallback for agents with no own entry
net_hosts = ["*"]
```

| Field | Effect |
|---|---|
| `tools` | Allow-list for plan-step runs (`tengu run-agent`). Empty = every always-on tool plus configured opt-ins. Opt-in names listed here are switched on. |
| `workspace_tools` | Older way to switch on opt-in tools; merged with `tools`. |
| `scopes.<tool>` | `fs_roots`, `net_hosts`, `env_reads`, `shell_bins`, `wallets`. Per-agent entry replaces `default_scopes` wholesale. |
| `compress_and_store` | Added to every subagent automatically — never list it. |

## MCP servers

```toml
[[mcp_servers]]
name = "github"                  # tools appear as "github__<tool>"
transport = "stdio"              # or "http" + url = "..." + auth = { type = "bearer", token = "$TOKEN" }
command = ["npx", "-y", "@modelcontextprotocol/server-github"]
env = { GITHUB_TOKEN = "$GITHUB_TOKEN" }
```

List them in an agent's `tools` like any other tool (`tools = ["github__create_issue"]`); an empty `tools` gets every one.

| Agent kind | Sees `[[mcp_servers]]` tools? | How |
|---|---|---|
| Plan-step subagent, `engine = "openrouter"` | yes | `run-agent` executor connects, advertises them |
| Plan-step subagent, `engine = "claude_code"` | yes | engine passes the servers to the tengu bridge (`TENGU_BRIDGE_MCP_SERVERS`), which proxies them under the egress policy — as `mcp__tengu-tools__<server>__<tool>` |
| In-process OpenRouter agent (TUI, Telegram) | yes | same executor |
| In-process Claude Code agent (TUI, Telegram) | no | open gap — its bridge list comes from the catalog only (`SESSION_HANDOFF.md`) |

Names use `__` because model APIs reject `.` in tool names.
