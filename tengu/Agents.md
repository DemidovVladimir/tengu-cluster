---
tags:
  - core
  - agents
---

# Agents

An **agent** is an LLM-backed actor with a role, identity, and bounded permissions. Agents are the primary execution units in the [[Overview|Tengu Cluster]] multi-agent system.

## Definition

Agents are defined in TOML config — no code changes required:

```toml
[agents.qa]
engine = "openrouter"
model = "google/gemini-2.5-flash"
role = "qa"
capabilities = ["workspace.read", "workspace.list", "workspace.shell"]
skill_packages = ["test_runner"]

[agents.qa.identity]
name = "QA Agent"
instructions = "You review code, run tests, and verify correctness."

[agents.qa.limits]
max_tokens_per_flow = 100_000
```

## Key Properties

- **Role** — dynamic string wrapper (`AgentRole`), any non-empty name works. Not an enum.
- **[[Capabilities]]** — permission list gating which [[Tools]] the agent can use
- **`skill_packages`** — filters which [[Skills]] are loaded for this agent
- **`allowed_tools`** — further restricts workspace tool access per agent
- **Identity** — name + instructions injected into system prompt
- **Token budget** — per-flow limit with 80% warning and 100% hard cutoff

## Multi-Agent Coordination

Agents coordinate through the [[Orchestrator]]:

1. User sends a goal
2. [[Orchestrator]] decomposes it into tasks via LLM-based planner
3. Tasks are assigned to agents based on roles
4. Independent tasks run in **parallel batches** (`JoinSet`)
5. Dependent tasks receive prior output **inline in their prompt**
6. Results are auto-summarized into [[Memory]] as topic overviews

## Routing

- **CLI orchestrator**: interactive role-based dispatch
- **Telegram**: `@role: message` for explicit routing, plain messages auto-orchestrated
- **`/agents`**: lists available agents and their roles

## Sandbox Isolation

[[Configuration|Sandbox configs]] (`sandboxes/<name>/config.toml`) define domain-specific teams with per-agent tool and skill restrictions. See [[Configuration]] for details.

## Related

- [[Tools]] — what agents can do
- [[Skills]] — domain knowledge agents can use
- [[Capabilities]] — what agents are allowed to do
- [[Orchestrator]] — how agents coordinate
- [[Channels]] — how users interact with agents
