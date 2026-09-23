# Skill Subsystem — Redesign Reference (2026-04-29)

Concise schema. Tables. No prose.

---

## The patchwork problem

Today's lifecycle has 4 specialized agents (skill-author / skill-evaluator / resource-finder / skill-improver-inline) and 3 bespoke tools (`skill_distill`, `apply_improver_proposal`, `skill_resource`). Each turn's `adjust yourself` is a 3-step plan with JSON proposals shuttled between processes. The plumbing breaks in 4 distinct places and patching one surfaces the next:

| Symptom | Root cause |
|---|---|
| Skill name routed as agent | Roster mixed kinds; planner picked top-scoring entry |
| resources/ unreachable | Agent workspace ≠ project root |
| Improver returned prose, not JSON | LLM saw "no write_file" and gave up |
| Tool not in agent's tool list | `AgentSpec` had no `workspace_tools` field; silently ignored (historical — `AgentSpec` deleted 2026-09-18; `[agents.<name>].tools` opts allow-list names in) |
| Lifecycle plan target ambiguous | Plan goal lacks the skill name |

Each is a real bug. But the meta-bug is the architecture: too many agents + too many tools + too much plumbing.

---

## Hermes's pattern (canonical reference)

**Two tools, one entrypoint each.**

| Tool | Actions | Purpose |
|---|---|---|
| `skill_view` (read-only) | `list`, `view <name> [file_path]` | Discovery + reading |
| `skill_manage` (writes, scoped) | `create`, `edit`, `patch`, `delete`, `write_file`, `remove_file` | All authoring + edits |

Killer features:
- **`patch` action with `fuzzy_find_and_replace`** — 8-strategy fuzzy matching (exact, line-trimmed, ws-normalized, indent-flex, escape-normalized, trimmed-boundary, unicode-normalized, block-anchor, context-aware). The agent gets one shot; harness handles whitespace drift.
- **Atomic writes via tempfile + os.replace**; rolled back on post-write security scan failure.
- **One agent IS the manager** — no orchestration, no JSON proposals, no approval gates.

`skill_manage(action, name, content?, file_path?, file_content?, old_string?, new_string?, replace_all?)` returns `{success, message?, error?, path?, ...}`.

---

## Tengu redesign — match the pattern

### New tools

| Tool | Actions | Replaces |
|---|---|---|
| `view_skill` | `list`, `read`, `read_resource` | `skill_resource` (kept as alias) |
| `manage_skill` | `create`, `edit_body`, `patch`, `add_resource`, `remove_resource`, `delete` | `skill_distill` (kept as alias), `apply_improver_proposal` (delete) |

`view_skill` actions:
- `list()` → `[{name, description, tier, fixtures, resources_count, metrics_count}]`
- `read(name)` → `{name, frontmatter, body, resources: [{path, size_bytes}], metrics_summary}`
- `read_resource(name, path)` → `{content, bytes}` (max 1 MB)

`manage_skill` actions:
- `create(name, description, body, tier?, learner_facing?, editable_by_learner?, resources?)` — atomic write of SKILL.md + optional initial resources. Refuse on collision.
- `edit_body(name, new_body)` — full SKILL.md body replacement. Frontmatter preserved.
- `patch(name, old_string, new_string, file_path?, replace_all?)` — fuzzy find-and-replace inside SKILL.md or `resources/<file_path>`. The killer in-chat edit verb.
- `add_resource(name, path, content, overwrite?)` — atomic write under `resources/<path>`. Path validated.
- `remove_resource(name, path)` — atomic delete.
- `delete(name)` — refuse if active scratch worktree. Audit-logged.

All `manage_skill` writes:
- Validate skill name regex
- Validate `editable_by_learner` flag (refuse on `false`)
- Atomic temp+rename
- Append `skills/.audit.jsonl` line
- Return `{success, action, path, ...}` JSON

### Single agent

Replace `skill-author / skill-evaluator / resource-finder / skill-improver-inline` with ONE `learning-agent`:

```toml
# sandboxes/aura/config.toml
[agents.learning-agent]
engine = "openrouter"
model = "anthropic/claude-opus-4-7"
tools = ["view_skill", "manage_skill", "http_request", "read_file", "list_directory"]  # manage_skill opts in via the allow-list
skill_packages = ["skill-creator"]   # `skills = [...]` accepted
description = "..."                 # presence = planner-routable
```

Description tells the agent: "you handle the full skill lifecycle — view existing state, fetch web resources if needed, apply changes via `manage_skill`. Don't emit JSON to the user; call the tool."

### Orchestrator lifecycle verbs (1-step plans)

| User says | Plan |
|---|---|
| `create skill from our dialog [as <name>]` | `{steps:[{agent:"learning-agent", goal:"create skill <name> via manage_skill(action='create',...) using messages [N..current]"}]}` |
| `evaluate` / `evaluate this skill` | `{steps:[{agent:"learning-agent", goal:"evaluate <skill> against this dialog. Use view_skill(read) and view_skill(read_resource) for ground truth."}]}` |
| `fix it` / `adjust yourself` / `improve` | `{steps:[{agent:"learning-agent", goal:"adjust <skill> based on the dialog. Use view_skill to inspect, http_request for web research, manage_skill(patch / add_resource) to apply."}]}` |
| `rollback` | Direct, surface `git checkout skills/<name>/SKILL.md` |

ONE step. ONE agent. The agent decides what tool calls to make.

### Migration

| Old | New | Migration |
|---|---|---|
| `skill_distill` (LLM tool) | `manage_skill(action="create",...)` | Keep `skill_distill` as a thin wrapper that calls `manage_skill(create)`. CLI `tengu skill seed` continues working. |
| `apply_improver_proposal` (LLM tool) | `manage_skill(action="patch",...)` and/or `add_resource` | Delete (was 1 day old, no docs depended on it). |
| `skill_resource` (LLM tool) | `view_skill` | Keep as alias. Existing seeded SKILL.md bodies still reference it; their next edit can switch. |
| `skill-author` agent | `learning-agent` | Delete. |
| `skill-evaluator` agent | `learning-agent` | Delete. |
| `skill-improver-inline` agent | `learning-agent` | Delete. |
| `resource-finder` agent | `learning-agent` (calls http_request directly) | Delete. |
| `tengu skill evolve` CLI | unchanged | Existing offline-eval driver still uses `skill-improver` agent + `ImproverProposal` JSON. Independent path. |

### Cache discipline (unchanged)

All `manage_skill` writes return `loaded_in_current_conversation: false`. Agent tells user to start a new session.

### `editable_by_learner` flag (unchanged)

`manage_skill` refuses writes to skills with `editable_by_learner: false` in frontmatter. Default-allow when flag absent.

### Scope (unchanged)

3-tier shadowing: managed → workspace → project. `_find_skill(name)` walks all three, first match wins. `view_skill` reads from any tier; `manage_skill` writes to the tier where the skill currently lives, except `create` defaults to project-tier.

---

## Why this works where the patchwork didn't

| Pain | Patchwork | Redesign |
|---|---|---|
| Tool list confusion (3 lifecycle tools, each with subtly different params) | LLM picks wrong one or invents schema | One tool, action enum, schema is the LLM's only option |
| Multi-step plan coordination | JSON proposals shuttled, plan steps can fail independently | One step, one agent, atomic |
| `workspace_tools` silently ignored on agent specs (historical) | `apply_improver_proposal` never made it to the agent | New tools live in regular `tools = [...]` (allow-list names listed there opt in like `workspace_tools`) |
| Improver "doesn't have write_file" excuse | LLM gave up, returned prose | Agent has `manage_skill` directly; tool description says "this is how you write skill files" |
| `resources/` unreadable | Workspace mismatch | `view_skill(read_resource)` walks tiers, returns content |
| Whitespace drift on body edits | `apply_proposal_to_skill_md` whole-body replace lost precision | `patch` with fuzzy matching handles drift |

---

## Execution order

1. (parallel) Implement `view_skill` plugin (Subagent X) + `manage_skill` plugin (Subagent Y) + `[agents.learning-agent]` block + orchestrator SKILL.md update (Subagent Z).
2. Integrate: register plugins, update `compute_base_tools` / bridge / allowlist, write SESSION_HANDOFF entry.
3. Smoke test: `mkdir -p`-free seed, `adjust yourself`, paste output.

Code pointers (existing, reusable):
- Atomic write pattern: `src/adapters/outbound/tools/skill_lifecycle/distill.rs:148–204`
- Path validation: `src/application/skills/lifecycle/evolve.rs::validate_resource_path`
- Three-tier walk: `src/adapters/outbound/tools/view_skill/mod.rs` (managed → workspace → project, first wins)
- `editable_by_learner` check: `src/application/skills/lifecycle/evolve.rs::is_editable_by_learner`
- Audit log: `src/application/skills/lifecycle/audit.rs`

Fuzzy matching is new (no existing helper). Implement only **two strategies** for v1: exact match, then whitespace-normalized. The other 6 hermes strategies can land later if drift remains a problem.
