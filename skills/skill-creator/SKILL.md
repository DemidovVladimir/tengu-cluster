---
name: skill-creator
description: Use when creating a new skill or modifying an existing skill for a tengu agent. Covers skill anatomy, frontmatter, naming, the three-tier hierarchy, and the create/modify workflow using workspace primitives.
editable_by_learner: true
metrics:
  - name: distill_quality
    kind: llm_judge
    rubric_file: metrics/distill_quality.md
    min_pass_rate: 0.7
  - name: triggers_correctly
    kind: description_trigger
    queries_file: metrics/trigger_queries.yaml
    min_pass_rate: 0.85
---

# Skill Creator

Author and modify skills for tengu agents. Skills are the logic layer -- any strategy, workflow, playbook, or policy is a skill, not Rust.

## When to Use

- Creating a new skill from scratch
- Modifying or improving an existing skill
- Distilling a successful in-conversation workflow into a reusable skill
- Need to understand how skills work in tengu

## Skill Anatomy

Every skill is a directory containing at minimum one `SKILL.md` file:

```
skills/
  my-skill/
    SKILL.md              # Main file (required)
    references/           # Optional: heavy reference material
      api-docs.md
```

### Frontmatter (YAML)

Required fields:

| Field | Description |
|-------|-------------|
| `name` | Skill name. Letters, numbers, hyphens only. |
| `description` | Starts with "Use when...". Triggering conditions only -- never summarize what the skill does. Third person. |

Optional gating fields (a skill whose prerequisites are missing is not loaded; a log line names why):

| Field | Description |
|-------|-------------|
| `requires_bins` | List of binaries that must be on PATH (e.g. `["git", "cargo"]`) |
| `requires_env` | List of env vars that must be set (e.g. `["API_KEY"]`) |
| `os` | OS filter (e.g. `["macos", "linux"]`) |

Optional learner-platform fields:

| Field | Description |
|-------|-------------|
| `editable_by_learner` | `bool`. Absent = editable; only an explicit `false` makes `manage_skill` (`edit_body`, `patch`, `add_resource`, `remove_resource`), `apply_improver_proposal` and `tengu skill evolve` refuse to write it. |
| `learner_facing` | `bool` marker written by `tengu skill seed` / `manage_skill create`. Per-learner state (`skills/<name>/state/<learner_id>.json`) is not read or written by the runtime yet. |

### Two Skill Types

**Documentation skills** (most common): Frontmatter + SKILL.md body. The body goes into the agent's system prompt (capped by `prompt_budget.max_skill_context_tokens`) for every agent that lists the skill in `skill_packages`.

**Shell skills**: Create named tools with execution templates. The skill defines a tool name, description, and a command template that the runtime injects as a callable tool. Not loaded for an agent that runs no shell (`[risk]` / Solana-signer sandbox).

## Three-Tier Hierarchy

| Tier | Path | Use case |
|------|------|----------|
| Managed | `~/.tengu/skills/` | User's personal skills, shared across workspaces |
| Workspace | `<workspace>/.tengu/skills/` | One agent workspace only |
| Project | `<workspace>/skills/` | The agent workspace's `skills/` |
| Cwd | `<cwd>/skills/` | The repo's `skills/` when tengu runs from the repo root (searched when it differs from Project) |

First match wins, in that order: a managed skill shadows a workspace one of the same name, and so on.

## Writing Good Descriptions

The description is how agents decide whether to load your skill. It must answer: "Should I read this right now?"

**Rules:**
- Start with "Use when..."
- Describe triggering conditions, not the skill's workflow
- Include concrete symptoms, situations, and contexts
- Write in third person
- Keep under 500 characters

```yaml
# BAD: summarizes workflow
description: Creates skills by analyzing requirements, writing frontmatter, then testing

# BAD: too vague
description: For skill management

# GOOD: triggering conditions only
description: Use when creating a new skill or modifying an existing skill for a tengu agent
```

## Create Flow

1. **Pick a name.** Verb-first, hyphenated: `rate-limit-handler`, `api-auditor`.
2. **Write frontmatter.** `name` + `description` (required). Add gating fields if applicable.
3. **Write the body.** Structure: Overview -> When to Use -> Procedure -> Common Mistakes.
4. **Place in the correct tier.**
5. **Test.** Load the agent and ask it a question the skill should handle.

## Modify Flow

1. **Read the existing skill** with `view_skill` (`action: "read"` / `"read_resource"`).
2. **Identify what to change.** Description not triggering? Body missing a case?
3. **Edit** with `manage_skill` (`edit_body`, `patch`, `add_resource`, `remove_resource`; opt-in — the agent must list it in `tools`). `write_file` refuses any path under a `skills/` directory.
4. **Test.** Changes load on the next session.

## Distillation (from a live conversation)

When the user says "let's save this as a skill", "distill this", "turn this into a skill", or equivalent after a successful workflow, ALWAYS call the `skill_distill` tool (opt-in: the agent must list it in `tools`). `write_file` cannot write skills -- `skill_distill` handles fixture seeding and metric scaffolding in one step. Optional `tier`: `project` (default, `<workspace>/skills/`) or `workspace` (`<workspace>/.tengu/skills/`); `managed` is refused.

### Required inputs (you are the author)

- **`name`** -- kebab-case, verb-first (e.g. `summarize-paper`, `deploy-contract`).
- **`description`** -- starts with "Use when...", third person, triggering conditions only. Never summarize the workflow.
- **`body_markdown`** -- the skill body you compose from your in-context understanding. Required structure:
  - **Overview** (1-2 sentences: what this skill does)
  - **When to Use** (triggering situations)
  - **Procedure** (numbered steps that worked; omit failed exploration)
  - **Common Mistakes** (pitfalls observed in the conversation)
- **`metrics`** -- at least one metric. Every distilled skill MUST ship with metrics:
  - Prefer `shell_check` for deterministic outcomes (tx confirmed, file exists, exit code 0).
  - Use `llm_judge` with a narrative rubric for qualitative criteria.
  - See `docs/superpowers/specs/2026-04-20-skill-metrics-evolution-design.md` s.6 for the full schema.
- **`from_message_index`** -- the 0-based message index where the distilled behaviour started. When in doubt, pick the message where the user stated the goal.
- **`fixture_hints.drop_tool_names`** -- exclude noise like `memory_search` or unrelated tool calls that shouldn't appear in replay fixtures.

### Quality bar for distillation

- **Focus on what worked.** Strip dead ends, retries, and debugging detours from the body.
- **Be concrete.** Reference exact tool names, file paths, and commands used in the successful run.
- **Generalize carefully.** The skill should apply to future instances of the same task, not just replay this one.
- **Always include metrics.** A skill without metrics cannot be evaluated or improved -- never skip this field.
- **Pick a tight `from_message_index`.** Too early bloats the fixture with irrelevant context; too late loses the goal statement.

### Invariant

The newly distilled skill does NOT activate in the current conversation -- it becomes available on next session start. This is intentional: prompt-caching requires a stable tool/skill inventory per conversation. Tell the user this explicitly so they know to start a new session to use the skill.

## The No-Compromise Test

Before putting logic in Rust, ask: "Does a non-engineer user need to change this behaviour by editing a markdown file, or by filing a PR?" If the answer is "markdown file," it is a skill, not Rust.

## Anti-Patterns

- **Rust-level policy in a skill.** Skills instruct the LLM; they don't compile.
- **Workflow summary in description.** Keep descriptions to triggering conditions only.
- **External runtime dependencies.** Use only tengu workspace primitives (`read_file`, `write_file`, `list_directory`, `run_command`). No Python, no TypeScript, no external runtimes.
- **Overly long skills.** If a skill exceeds 500 words, split heavy reference into `references/` files.
- **Distilling without metrics.** A metric-less skill cannot be graded or evolved -- always supply at least one.
- **Using `write_file` for new skills during distillation.** Use `skill_distill` so fixtures and metrics scaffold correctly.
- **Vague `body_markdown`.** If the body omits the concrete steps/tools that succeeded, the distilled skill won't replay.
