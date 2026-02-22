# Webstudio Execution Flow

Date: 2026-02-22  
Scope: current runtime behavior in this repository

## Purpose

This document explains what actually happens when you run the Webstudio scenario and ask:

`"Create me another website for company X."`

It also clarifies:
- how the event system is used
- whether dependent agents can execute in parallel today

## Scenario Baseline

Reference config:
- `config.webstudio.example.toml`

Default agent in this template:
- `agents.orchestrator` (`default = true`)

Important implication:
- your normal chat prompt is processed by the orchestrator agent unless you explicitly delegate through control-plane commands.

## Flow A: Direct Chat Request (No Explicit Delegation)

When you type:
- `Create me another website for company X`

Current runtime path:
1. CLI adapter receives inbound text and sends it to chat runtime.
2. Runtime emits `DomainEvent::InboundTurnReceived`.
3. Runtime resolves flow key and emits `DomainEvent::FlowResolved`.
4. Prompt is assembled (system + history + retrieval budget) and emits `DomainEvent::PromptAssembled`.
5. Orchestrator engine runs once and emits `DomainEvent::EngineTurnStarted`.
6. Engine output is streamed/collected, persisted to flow, and completion/failure events are emitted.
7. Response is returned to user via CLI pipe.

What does not happen automatically yet:
- no automatic multi-agent fan-out from this plain user prompt
- no automatic orchestrator-planned parallel delegation to specialists

## Flow B: Delegated Specialist Work (Explicit `/assign`)

Delegation is currently explicit via commands:
- `/assign <dependent-agent-id> <cap1,cap2,...> [objective...]`
- `/stopall` (emergency stop for delegated workers/assignments)

Example:
- `/assign frontend tool:read_file,tool:write_file build landing page structure`

Execution path:
1. Command is parsed and validated against governance + topology policy.
2. Runtime emits `DomainEvent::HandoffTaskDispatched`.
3. Acceptance subscriber emits non-terminal `HandoffResultReceived(status=accepted)`.
4. Execution subscriber runs one dependent-agent turn (`execute_delegated_handoff_task_once`).
5. Execution subscriber emits terminal `HandoffResultReceived(status=completed|failed)`.
6. Control-plane audit subscriber persists lifecycle records to:
   - `~/.tengu/state/audit/capability_assignments.jsonl`

Current limitation:
- terminal handoff results are emitted and audited, but not yet auto-injected back into a synthesized orchestrator answer pipeline.

Safety control:
- if delegated retries/rework loops are active and you want to stop token spend immediately, use `/stopall`
- this aborts delegated handoff worker subscribers and revokes active delegated assignments in runtime state

## Event System Role

The runtime is event-driven for side-effects:
- audit persistence (tool + control-plane)
- metrics counters
- policy-reaction logging
- delegated handoff queue + execution

Core chat still has a main orchestration loop, but critical secondary behaviors are handled by event subscribers.

## Parallel vs Sequence (Current State)

Short answer:
- direct chat turn processing: **sequential per inbound turn**
- delegated handoff execution: **sequential today**
- event side-effect processing: **concurrent subscribers**

Detailed behavior:
1. Multiple subscriber tasks run concurrently (`tool_audit`, `control_plane_audit`, metrics, policy reaction, handoff acceptance, handoff execution).
2. The delegated execution subscriber currently processes `HandoffTaskDispatched` events in a single async loop and awaits each dependent run before taking the next one.
3. This means delegated specialist jobs are effectively one-by-one right now, not a parallel worker pool.

## What To Expect for Webstudio Today

For a single natural-language request:
- you get one orchestrator response.

For specialist delegation:
- use explicit `/assign` commands.
- delegated tasks execute with event/audit traceability.
- specialist runs are currently serialized, not parallelized.

## Worked Example: "Implement a simple web replica of a Solana trading bot UI"

User request example:
- `Implement a website: simple replica of an online web-based trading bot on Solana chain.`

### Goal Decomposition (Practical)

Suggested specialist split in this template:
1. `design` agent: visual system (layout, typography, color tokens, component shape).
2. `frontend` agent: UI implementation plan (page sections, component tree, interactions).
3. `content` agent: copy and legal-safe placeholder wording.
4. `accountant` agent: cost/ops framing (hosting tiers, rough maintenance budget).

Why split:
- keeps scope bounded per specialist
- gives auditable delegated decisions in control-plane log
- reduces one giant prompt risk for orchestrator

### Current Runtime-Accurate Flow

#### Step 1: Orchestrator baseline turn

When:
- immediately after user prompt

Why:
- establish architecture and delegation plan

Input:
- normal chat message from user

Output:
- one orchestrator answer (plan/assumptions/questions)

#### Step 2: Explicit delegated assignments

When:
- after you accept orchestrator plan

Why:
- trigger dependent specialist execution via event-driven handoff path

Example commands:
1. `/assign design tool:read_file define visual direction for a Solana trading dashboard replica`
2. `/assign frontend tool:read_file draft component structure and interaction model for the dashboard`
3. `/assign content tool:read_file produce concise website copy and disclaimers (no financial promises)`
4. `/assign accountant tool:read_file estimate infra and maintenance cost ranges for MVP`

Immediate runtime response (from command handler):
- `Delegated assignment approved: orchestrator -> <agent> [<capabilities>]`

Events emitted:
1. `HandoffTaskDispatched`
2. `HandoffResultReceived(status=accepted)` (queue acknowledgement)
3. `HandoffResultReceived(status=completed|failed)` (terminal execution result)

#### Step 3: How dependent output is produced today

Current implementation detail:
- each delegated task runs one dependent-agent model turn with a generated prompt
- result is a typed handoff result summary (`Completed`/`Failed`)
- lifecycle is persisted in `~/.tengu/state/audit/capability_assignments.jsonl`

Important limitation (today):
- delegated result summaries are not auto-merged into orchestrator chat response
- delegated tasks are not auto-chained into other dependent tasks
- delegated runs are processed one-by-one in current execution subscriber
- there is no enforced quality gate yet to prevent invalid upstream outputs from cascading to downstream dependents

### How Other Agents Use Results (Today vs Target)

Today:
1. Dependent result is emitted as event + audit record.
2. Orchestrator does not automatically consume that result in the next turn.
3. User drives the next step manually (ask orchestrator to continue, or issue more `/assign` commands).

Target direction (`E6-T9`):
1. Orchestrator dispatches multi-dependent tasks as part of one runtime plan.
2. Runtime collects terminal dependent results.
3. Orchestrator/validator enforces per-result quality decisions (`accept`/`retry`/`rework`/`fail`) before downstream dependency release.
4. Orchestrator receives aggregated validated specialist output and produces one synthesized final response.
5. Optional parallel worker fan-out for independent tasks.

### Sequence vs Parallel for This Example

For this Solana dashboard replica request:
- current behavior: `design -> frontend -> content -> accountant` effectively serial unless you tolerate queued sequential processing
- not true parallel execution yet

So the realistic expectation today:
- delegated specialist tasks are useful for auditable bounded delegation
- end-to-end "multi-agent parallel build" is still a planned upgrade, not current behavior

## Planned Direction

Tracked under in-progress orchestration work:
- `E6-T9` (topology-aware multi-agent execution loop continuation)

Expected future upgrade:
- orchestrator-driven multi-dependent execution with optional parallel worker fan-out and deterministic merge/reporting.
