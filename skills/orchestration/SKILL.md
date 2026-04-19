---
name: orchestration
description: Use whenever a user request has multiple steps, spans specialist agents, can be parallelized, or requires progress tracking. Teaches decomposition, delegation via sessions_spawn and sessions_fan_out, failure handling, and progress logging. Use this skill even when the user doesn't explicitly ask to "orchestrate" — any request that touches two or more subagents, or has sequential dependencies, triggers this skill.
---

# Orchestration Playbook

This skill teaches you (the main agent) how to decompose user requests, delegate work to subagents, and drive multi-step plans without a central coordinator. You are the orchestrator.

## When to orchestrate

Trigger this playbook when any of the following apply:

- The request has two or more distinct sub-tasks (e.g. "research X, then mint Y").
- A sub-task needs specialist knowledge that matches a team member in your roster.
- Sub-tasks are independent and can run in parallel.
- The work is long enough that the user benefits from progress updates.
- A previous attempt failed and you need a different decomposition.

If the request is a single-step, single-specialty question, **don't orchestrate — answer directly.**

## Read your roster first

Your system prompt lists the team members you can delegate to. Each has a role key, a description, and capabilities. Read the roster carefully before deciding how to decompose. If no specialist matches a sub-task, handle it yourself.

## Decomposition

1. Read the full request before planning anything.
2. Sketch the decomposition: what needs to happen, in what order, which tasks are independent.
3. For each sub-task, pick the team member whose description matches best, or keep it for yourself.
4. Identify which sub-tasks can run in parallel (no output dependency) and which must be sequential.

## Sequential delegation

When step N+1 needs step N's output:

- Call `sessions_spawn(agent="<role>", prompt="<focused prompt>")` and await the result.
- The subagent's output lands in your context. Read it.
- When composing the next spawn's prompt, embed only the **relevant** parts of the previous output. Don't forward everything — long outputs waste context.
- If the previous output is large and only a fraction matters downstream, summarize it yourself before embedding.

## Parallel delegation

When sub-tasks are independent:

- Call `sessions_fan_out(requests=[{agent, prompt}, ...])` with all independent sub-tasks at once.
- Fan-out returns combined results. Read them and decide next.
- Prefer fan-out whenever it cuts wall-clock time and subagents don't need each other's outputs.

## Failure handling

- When a spawned subagent returns an error, read the message and decide:
  - **Retry** if the failure looks transient (rate limit, timeout, HTTP 5xx).
  - **Switch strategy** if the failure is structural (wrong specialist, missing data).
  - **Abandon** if further work is impossible or unsafe.
- When a tool inside your own turn fails, apply the same reasoning.
- If one sub-task fails and downstream work depends on it, reason explicitly about whether the downstream step still makes sense. If not, explain the cascade to the user.

## Progress tracking

- For multi-step work taking more than ~30s of wall-clock time, write a short status note via `memory_write` under `memory/YYYY-MM-DD.md`.
- Record identifiers (tx hashes, addresses, UUIDs, doc ids) in the daily log so they survive across turns.
- Do not hide progress behind silence.

## When NOT to orchestrate

- Single-step requests — answer directly.
- Conversation, clarification, small talk — respond directly.
- Requests explicitly about your own opinion or synthesis — don't delegate.

## Primitive cheat sheet

| Tool | Use for |
|---|---|
| `sessions_spawn` | Sequential delegation; await result |
| `sessions_fan_out` | Parallel delegation; independent tasks in one call |
| `subagents` (action=list \| kill) | Inspect or cancel running subagents |
| `memory_write` | Record progress and identifiers in the daily log |
