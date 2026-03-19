# Remote Federation Architecture

Design document for extending Tengu Cluster with sandbox-scoped remote
federation over the internet. The goal is to let Tengu communicate with remote
Tengu sandboxes and external agentic systems without exposing the local fleet
directly. Local event-bus orchestration remains internal. All remote traffic
terminates at a gateway boundary that enforces routing, policy, protocol
translation, and observability.

---

## Goals

- Enable remote Tengu-to-Tengu communication across cloud providers, VPSes, and
  other internet-reachable deployments.
- Enable integration with external A2A agents and agentic systems.
- Enable integration with MCP ecosystems for tool and resource exchange.
- Preserve sandbox isolation as the primary remote security boundary.
- Preserve least privilege for remote access, delegation, and publication.
- Make every remote hop and policy decision observable in logs so operators can
  react immediately.

## Non-Goals

- No raw cross-cluster `EventBus` federation.
- No direct exposure of local role agents to the internet.
- No generic shell-driven tunnel creation by LLM agents.
- No unrestricted export of transcripts, memory, workspace contents, secrets, or
  environment variables.

## Core Architecture

The design introduces three new runtime surfaces and a protocol adapter layer.

### `remote_gateway`

`remote_gateway` is a non-LLM system worker. It is not a normal role agent and
must not be treated like one.

Responsibilities:

- Receive local delegated tasks from the orchestrator.
- Translate local task payloads into outbound protocol requests.
- Track remote progress, completion, failure, and cancellation.
- Map remote responses back into local `Progress`, `TaskCompletion`, and
  `TaskError` events.

`remote_gateway` owns the data plane for remote execution.

### `remote_access_agent`

`remote_access_agent` is an optional trusted local admin agent. It exists to
manage policy and publication, not to own raw internet exposure.

Responsibilities:

- Enable or disable protocol publication for a sandbox.
- Manage peer allowlists and peer metadata.
- Publish or refresh A2A Agent Cards.
- Publish or refresh MCP manifests.
- Query gateway status and routing state.

`remote_access_agent` owns the control plane for remote federation.

### Gateway Server

The gateway server is a stable HTTPS ingress surface owned by the runtime and
the surrounding infrastructure. It is the only internet-facing entrypoint for
remote federation.

Responsibilities:

- Accept authenticated inbound A2A, MCP, and native Tengu requests.
- Resolve the target sandbox.
- Enforce policy before any request reaches a local worker.
- Hand off allowed requests to the sandbox-local `remote_gateway`.
- Emit hop-by-hop structured logs for every remote action.

The gateway server owns the network plane.

### Protocol Adapters

All protocol specifics terminate behind the gateway and `remote_gateway`.

- `A2A`
  - Consume remote Agent Cards and delegate tasks to remote A2A peers.
  - Publish selected Tengu capabilities as an A2A-compatible surface.
- `MCP`
  - Consume remote MCP tools/resources/prompts as imported capabilities.
  - Publish selected Tengu tools/skills through an MCP-compatible surface.
- `Native Tengu`
  - Provide an optimized Tengu-to-Tengu task API aligned with the same remote
    task abstraction used by A2A and MCP publication.

## Routing Model

The recommended routing shape is one cluster gateway with sandbox-scoped routes
or equivalent host-based routing. Each sandbox is treated as a separate remote
principal.

Example routes:

- `/desci/a2a`
- `/desci/mcp`
- `/desci/native`
- `/webstudio/a2a`

### Inbound path

```text
remote peer
    |
    v
HTTPS gateway
    |
    v
sandbox route resolution
    |
    v
policy + auth + export checks
    |
    v
remote_gateway
    |
    v
local EventBus
```

Inbound flow:

`remote peer -> HTTPS gateway -> sandbox route -> remote_gateway -> local event bus`

### Outbound path

```text
local orchestrator
    |
    v
remote_gateway
    |
    v
protocol adapter
    |
    v
remote peer
```

Outbound flow:

`local orchestrator -> remote_gateway -> protocol adapter -> remote peer`

### Sandbox scoping

The gateway must route by sandbox before doing protocol-specific work. This
ensures that:

- peer policy is sandbox-local,
- exported capabilities are sandbox-local,
- audit and logs are sandbox-local,
- access to one sandbox does not imply access to another.

## Trust Model

Zero-trust is the default.

Remote peers are untrusted even when they are allowlisted. Allowlisting means
"eligible for policy evaluation," not "trusted with broad authority."

Rules:

- Only explicitly exported capabilities are reachable remotely.
- Sandbox access is independent and must be granted per sandbox.
- A peer allowed for `desci` is not automatically allowed for `webstudio`.
- Remote protocol metadata is untrusted input and must be validated before use.
- Remote outputs are untrusted and must be bounded, sanitized, and traced.

## Security Boundaries

The gateway boundary is the security boundary. It must enforce these hard
rules:

- Remote peers never receive the full user dialog.
- Remote peers never receive recalled memory blocks.
- Remote peers never receive system prompts.
- Remote peers never receive environment variables.
- Remote peers never receive secrets.
- Remote peers never receive direct workspace access.

`remote_gateway` gets only a narrow capability:

- `remote.delegate`

`remote_gateway` must not have:

- `workspace.*`
- `memory.*`
- `crypto.*`
- `http_request`
- `run_command`

`remote_access_agent` may manage gateway policy only through deterministic admin
tools. It must not manage remote exposure through arbitrary shell commands.

This design is intentionally stricter than local fleet trust. It assumes prompt
injection, protocol misuse, malformed remote manifests, and operator mistakes
will happen.

## Agent Responsibilities

Three planes exist and must remain separate.

### Data plane: `remote_gateway`

Handles:

- task delegation,
- remote progress tracking,
- remote cancellation,
- result return,
- task status mapping back into local bus events.

### Control plane: `remote_access_agent`

Handles:

- enable/disable protocol publication,
- manage peers,
- manage allowlists,
- publish metadata,
- inspect remote gateway status,
- request safe config changes through narrow admin tools.

### Network plane: gateway server

Handles:

- bind/listen,
- authenticate,
- authorize,
- route,
- log,
- emit fast failure diagnostics.

The planes must not collapse into one role. In particular, a trusted local
admin agent must not gain raw infra execution primitives just because it is
"your agent."

## Protocol Semantics

Protocols serve different purposes and must stay separated conceptually.

### A2A

A2A is the agent/task delegation protocol.

Use A2A for:

- discovering remote agent cards,
- choosing a supported interface,
- delegating bounded tasks,
- receiving task status and results,
- publishing Tengu as an A2A-compatible remote agent surface.

First implementation preference:

- HTTPS + JSON-RPC
- SSE where streaming or subscription is enabled

### MCP

MCP is the tool/resource exchange protocol.

Use MCP for:

- importing remote tools/resources/prompts as namespaced capabilities,
- exporting selected Tengu tools/skills as MCP-safe primitives.

MCP is not the transport for raw orchestrator federation.

### Native Tengu

Native Tengu is the optimized Tengu-to-Tengu task API.

Use Native Tengu for:

- same-stack remote delegation,
- simpler peer-to-peer task exchange,
- protocol behavior aligned with the same remote task abstraction used by A2A.

The first implementation should align all three protocols to one normalized
remote task model:

- request envelope,
- status envelope,
- result envelope,
- cancellation semantics,
- correlation and observability metadata.

## Configuration Model

The final schema can evolve, but the runtime should support configuration at
these levels:

- cluster gateway settings,
- sandbox route settings,
- `remote_gateway`,
- `remote_access_agent`,
- peer allowlists,
- export/import allowlists,
- payload limits,
- transcript/memory/workspace export flags.

Illustrative TOML:

```toml
[gateway]
enabled = true
bind_addr = "127.0.0.1:8080"
public_base_url = "https://tengu.example.com"
protocols = ["a2a", "mcp", "native"]
structured_logging = true

[gateway.sandbox_routes.desci]
a2a_path = "/desci/a2a"
mcp_path = "/desci/mcp"
native_path = "/desci/native"
auth_profile = "trusted-peer"
peer_allowlist = ["peer-user-b", "lab-cluster-1"]
export_allowlist = ["remote.research_delegate", "mcp.read_project_status"]
import_allowlist = ["a2a.research", "mcp.remote_docs"]
max_remote_context_bytes = 32768
allow_transcript_export = false
allow_memory_export = false
allow_workspace_export = false

[agents.remote_gateway]
kind = "remote_gateway"
role = "remote_gateway"
capabilities = ["remote.delegate"]
sandbox = "desci"

[agents.remote_access_agent]
kind = "remote_access_agent"
role = "remote_access_agent"
capabilities = ["gateway.admin"]
sandbox = "desci"
```

The important constraint is not the exact syntax. The important constraint is
that protocol publication, peer access, and export scope are all configured
explicitly and per sandbox.

## Remote Task Envelope

The outbound remote payload must be minimal and task-scoped.

Required fields:

- goal
- bounded task description
- selected upstream artifacts
- bounded summary
- allowlisted metadata

Excluded fields:

- full transcripts
- recalled memory blocks
- hidden internal state
- system prompts
- secrets
- environment variables
- direct file handles or workspace path access

Conceptual envelope:

```json
{
  "correlationId": "run-42.rev-7",
  "taskId": "remote-task-3",
  "goal": "Prepare research summary for external collaborator",
  "description": "Summarize the selected upstream findings and return a concise result.",
  "summary": "Previous tasks identified three candidate findings and two URLs.",
  "artifacts": {
    "research.notes": {"key_points": ["finding-a", "finding-b"]},
    "research.url.paper": "https://example.org/paper"
  },
  "metadata": {
    "sandbox": "desci",
    "originRole": "remote_gateway"
  }
}
```

This envelope exists to prevent accidental over-disclosure. It is the only data
the remote peer should need to execute the delegated task.

## Stable Endpoint Model

The answer to "can it create ngrok or expose itself?" is: not through generic
LLM-controlled shell access.

The correct model is a stable infrastructure-owned endpoint.

Rules:

- Do not allow an LLM agent to open tunnels through shell commands.
- Do not allow an LLM agent to expose itself directly.
- Use a stable gateway endpoint owned by runtime plus external infrastructure.
- Let `remote_access_agent` request policy changes or protocol publication only
  through safe admin tools.
- If ingress automation is added later, it must be implemented as a
  deterministic runtime adapter, not generic shell execution.

Example safe admin tools:

- `gateway.status`
- `gateway.enable_protocol`
- `gateway.disable_protocol`
- `peer.allowlist.add`
- `peer.allowlist.remove`
- `agent_card.publish`
- `mcp_manifest.publish`

This preserves operational control and prevents the trusted local admin agent
from becoming a prompt-injection path to arbitrary internet exposure.

## Observability and Debugging

Every remote step must emit structured logs. This is not optional. Operators
must be able to trace every single hop or action, correlate the path quickly,
and react fast when policy denies, auth fails, routing breaks, or remote peers
misbehave.

Structured logs must include:

- timestamp
- sandbox id
- protocol
- peer id
- correlation id
- task id
- hop name
- action
- decision result
- latency
- error code if any

Minimum required hops to log:

- inbound request accepted/rejected
- auth success/failure
- sandbox route resolution
- policy evaluation
- capability/export check
- payload redaction/minimization
- handoff into `remote_gateway`
- protocol request serialization
- outbound request start
- outbound response received
- progress update mapped
- completion mapped
- cancellation requested
- cancellation acknowledged/failed
- imported manifest/card accepted/rejected
- local event-bus dispatch start/end
- retry and timeout decisions

Required log levels:

- `info` for normal lifecycle hops
- `warn` for policy denials, unusual fallbacks, malformed remote inputs
- `error` for failed dispatch, auth failures, protocol mapping failures,
  timeouts

Operational requirements:

- all denials and failures must log immediately
- logs must be correlation-friendly across gateway, adapter, and event-bus
  layers
- remote tasks must be traceable end-to-end by `correlation_id`
- policy decisions must log both reason and subject
- structured logs are preferred over prose-only logs

Example structured log lines:

```text
INFO  ts=2026-03-18T12:00:01Z sandbox=desci protocol=a2a peer_id=lab-cluster-1 correlation_id=run-42.rev-7 task_id=remote-task-3 hop=gateway.ingress action=request.accept result=allowed latency_ms=2
INFO  ts=2026-03-18T12:00:01Z sandbox=desci protocol=a2a peer_id=lab-cluster-1 correlation_id=run-42.rev-7 task_id=remote-task-3 hop=policy.export_check action=capability.validate result=allowed latency_ms=1
INFO  ts=2026-03-18T12:00:01Z sandbox=desci protocol=a2a peer_id=lab-cluster-1 correlation_id=run-42.rev-7 task_id=remote-task-3 hop=payload.minimize action=redact_context result=success latency_ms=3
INFO  ts=2026-03-18T12:00:01Z sandbox=desci protocol=a2a peer_id=lab-cluster-1 correlation_id=run-42.rev-7 task_id=remote-task-3 hop=event_bus.dispatch action=task_assignment result=sent latency_ms=1
WARN  ts=2026-03-18T12:00:04Z sandbox=desci protocol=mcp peer_id=unknown correlation_id=run-44.rev-1 task_id=- hop=gateway.auth action=peer_auth result=denied latency_ms=0 error_code=auth_failed
ERROR ts=2026-03-18T12:00:07Z sandbox=desci protocol=a2a peer_id=lab-cluster-1 correlation_id=run-42.rev-7 task_id=remote-task-3 hop=adapter.outbound action=send_message result=timeout latency_ms=30000 error_code=remote_timeout
```

The implementation should treat observability as a first-class design
constraint, not as an afterthought added after the protocol layer works.

## Failure Modes

The gateway and adapters must explicitly handle at least these cases:

- unauthorized peer
- sandbox mismatch
- unsupported protocol
- malformed Agent Card
- malformed MCP manifest
- unsupported exported capability
- oversized payload
- remote timeout
- remote cancellation race
- partial remote result
- attempted transcript access
- attempted secret access
- attempted environment variable access

Each of these failure modes must:

- fail closed,
- produce structured logs immediately,
- include `sandbox`, `peer_id`, `protocol`, and `correlation_id` where
  available,
- avoid leaking internal details to the remote caller.

## Recommended First Implementation

Rollout order:

1. Stable HTTPS gateway
2. Sandbox-scoped routing
3. `remote_gateway` system worker
4. Zero-trust remote task envelope
5. Structured logging and correlation ids
6. A2A consume/publish support
7. MCP consume/publish support
8. Optional native Tengu peer adapter

This order keeps the first implementation operationally safe:

- ingress exists before protocol exposure,
- sandbox routing exists before peer federation,
- payload minimization exists before remote traffic,
- logging exists before operators need to debug failures.

---

## Summary

Remote federation should be implemented as a sandbox-scoped gateway
architecture, not as direct exposure of local agents or raw event-bus traffic.
`remote_gateway` handles the data plane, `remote_access_agent` handles the
control plane, and the gateway server handles the network plane. A2A, MCP, and
Native Tengu all map onto one normalized remote task abstraction. The design is
zero-trust by default and requires structured hop-by-hop logging so every
remote action, decision, and failure is traceable in real time.
