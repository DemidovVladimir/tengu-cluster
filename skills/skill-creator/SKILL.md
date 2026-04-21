---
name: skill-creator
description: Use when creating a new skill or modifying an existing skill for a tengu agent. Covers skill anatomy, frontmatter, naming, the three-tier hierarchy, and the create/modify workflow using workspace primitives.
metrics:
  - name: distill_quality
    kind: llm_judge
    rubric_file: metrics/distill_quality.md
    min_pass_rate: 0.7
---

# Skill Creator

Author and modify skills for tengu agents. Skills are the logic layer -- any strategy, workflow, playbook, or policy is a skill, not Rust.

## When to Use

- Creating a new skill from scratch
- Modifying or improving an existing skill
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

Optional gating fields (prevent loading when prerequisites are missing):

| Field | Description |
|-------|-------------|
| `requires_bins` | List of binaries that must be on PATH (e.g. `["git", "cargo"]`) |
| `requires_env` | List of env vars that must be set (e.g. `["API_KEY"]`) |
| `os` | OS filter (e.g. `["macos", "linux"]`) |

### Two Skill Types

**Documentation skills** (most common): Frontmatter + SKILL.md body. The agent reads the skill content and follows the instructions. Progressive disclosure -- compact catalog entry in the system prompt, agent reads the full SKILL.md on demand.

**Shell skills**: Create named tools with execution templates. The skill defines a tool name, description, and a command template that the runtime injects as a callable tool.

## Three-Tier Hierarchy

Skills load from three locations, higher tiers shadow lower:

| Tier | Path | Use case |
|------|------|----------|
| Managed | `~/.tengu/skills/` | User's personal skills, shared across workspaces |
| Workspace | `.tengu/skills/` | Workspace-specific skills, checked into the repo |
| Project | `skills/` | Project-level skills at the repo root |

If two tiers have a skill with the same name, the higher tier wins.

## Writing Good Descriptions

The description is how agents decide whether to load your skill. It must answer: "Should I read this right now?"

**Rules:**
- Start with "Use when..."
- Describe triggering conditions, not the skill's workflow
- Include concrete symptoms, situations, and contexts
- Write in third person
- Keep under 500 characters

```yaml
# BAD: summarizes workflow -- agent may follow description instead of reading skill
description: Creates skills by analyzing requirements, writing frontmatter, then testing

# BAD: too vague
description: For skill management

# GOOD: triggering conditions only
description: Use when creating a new skill or modifying an existing skill for a tengu agent
```

## Create Flow

1. **Pick a name.** Verb-first, hyphenated: `rate-limit-handler`, `api-auditor`. Match what the skill does.
2. **Write frontmatter.** `name` + `description` (required). Add gating fields if the skill depends on external tools.
3. **Write the body.** Structure: Overview (1-2 sentences) -> When to Use -> Core Content -> Common Mistakes.
4. **Place in the correct tier.** Personal? `~/.tengu/skills/`. Workspace? `.tengu/skills/`. Project? `skills/`.
5. **Test.** Load the agent and ask it a question the skill should handle. Verify it uses the skill content.

## Modify Flow

1. **Read the existing skill.** Use `read_file` to load `SKILL.md`.
2. **Identify what to change.** Is the description not triggering correctly? Is the body missing a case?
3. **Edit.** Use `write_file` to update the skill.
4. **Test.** Same as create -- verify the agent uses the updated content correctly.

## Distillation (from a live conversation)

When the user says "let's save this as a skill" or equivalent after completing a successful workflow, call the `skill_distill` tool. Prefer it over `write_file` for skill authoring -- it handles fixture seeding and metric scaffolding in one step.

**Inputs you supply (you are the author):**

- `name` -- kebab-case, verb-first (e.g. `mint-ipnft`).
- `description` -- starts with "Use when...", third person, triggering conditions only.
- `body_markdown` -- the skill body you compose from your in-context understanding. Structure: Overview -> When to Use -> Procedure -> Common Mistakes. Refer to what *worked*; omit exploration that failed.
- `metrics` -- at least one metric. Prefer `shell_check` for deterministic outcomes (tx confirmed, file exists). Use `llm_judge` with a narrative rubric for qualitative criteria. See `docs/superpowers/specs/2026-04-20-skill-metrics-evolution-design.md` s.6 for the full schema.
- `from_message_index` -- the 0-based message index where the distilled behaviour started. When in doubt, pick the message where the user stated the goal.
- `fixture_hints.drop_tool_names` -- exclude noise like `memory_search` that doesn't belong in the replay fixtures.

**Invariant:** the new skill does NOT activate in the current conversation. It becomes available on next session start. This is intentional -- prompt-caching requires a stable tool/skill inventory per conversation.

## The No-Compromise Test

Before putting logic in Rust, ask: "Does a non-engineer user need to change this behaviour by editing a markdown file, or by filing a PR?" If the answer is "markdown file," it is a skill, not Rust.

## Anti-Patterns

- **Rust-level policy in a skill.** Skills instruct the LLM; they don't compile. If the behaviour needs enforcement at the binary level, it belongs in Rust.
- **Workflow summary in description.** Agents may follow the description instead of reading the full skill. Keep descriptions to triggering conditions only.
- **External runtime dependencies.** Skills should use only tengu workspace primitives (`read_file`, `write_file`, `list_directory`, `run_command`). No Python scripts, no TypeScript, no external runtimes.
- **Overly long skills.** If a skill exceeds 500 words, split heavy reference into `references/` files.
