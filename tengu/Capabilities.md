---
tags:
  - core
  - capabilities
---

# Capabilities

**Capabilities** are permissions that control which [[Tools]] an [[Agents|agent]] can use. They provide hard runtime enforcement — actual tool filtering at execution time.

## How It Works

```toml
[agents.designer]
capabilities = ["workspace.read", "workspace.list", "workspace.write"]
# No workspace.shell — this agent cannot run commands

[agents.backend]
capabilities = ["workspace.read", "workspace.list", "workspace.write", "workspace.shell", "http.request"]
# Full workspace + HTTP access
```

At runtime, `filter_tools_by_capability()` removes tools the agent is not permitted to use. The agent never sees them in its system prompt.

**Default behavior:** When `capabilities` is not set in agent config, all platform tools are available. You only need to specify capabilities when restricting access.

## Capability IDs

### Workspace

| ID | Effect Class | Gates |
|----|-------------|-------|
| `workspace.read` | Read | `read_file` |
| `workspace.list` | Read | `list_directory` |
| `workspace.write` | Write | `write_file` |
| `workspace.shell` | ShellExec | `run_command` |

### Platform

| ID | Effect Class | Gates |
|----|-------------|-------|
| `http.request` | ExternalApi | `http_request` |
| `crypto.sign_tx` | ChainTx | `sign_and_send_transaction` |
| `crypto.sign_message` | ChainTx | `sign_message` |
| `crypto.wallet_address` | Read | `get_wallet_address` |
| `crypto.abi_encode` | Read | `abi_encode` |

### Memory

| ID | Effect Class | Gates |
|----|-------------|-------|
| `memory.remember` | Write | `remember` |

## Effect Classes

Defined in `src/domain/capability.rs`:

- **Read** — no side effects, safe by default
- **Write** — modifies state, requires approval
- **ExternalApi** — calls external services
- **ChainTx** — on-chain transactions
- **ShellExec** — arbitrary shell execution, highest risk

## Skills Don't Need Capabilities

[[Skills]] are documentation — they inject context into the system prompt but don't create tools. Capabilities only gate **platform tools**. Skills are controlled separately via `skill_packages`.

```
Agent config
  +-- capabilities -> filter available platform Tools
  +-- skill_packages -> filter available Skills (documentation)
        |
  Agent sees: permitted Tools + loaded Skill documentation
```

Adding a new skill is plug-and-play: drop the SKILL.md, add to `skill_packages`, done. No capability changes needed — the agent uses `http_request` (or other platform tools) guided by the skill documentation.

## Related

- [[Agents]] — who has capabilities
- [[Tools]] — what capabilities gate
- [[Skills]] — knowledge that guides tool usage (no capabilities needed)
- [[Architecture]] — how capability filtering fits in the hex layers
