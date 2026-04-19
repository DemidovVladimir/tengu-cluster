---
name: skill-eval
description: Use when evaluating whether existing skills still work, checking for drift or stale references, or auditing the skill inventory across all tiers.
---

# Skill Eval

Evaluate the health of installed skills. This is the feedback half of the evolution loop -- skill-creator creates, skill-eval measures.

## When to Use

- Periodic audit of all installed skills
- After changing the workspace or tool configuration
- When a skill seems to not be triggering correctly
- Before a major release to catch stale skills

## Evaluation Procedure

### Step 1: Enumerate Skills

Walk the three-tier hierarchy and list every installed skill:

```
~/.tengu/skills/*/SKILL.md      # Managed tier
.tengu/skills/*/SKILL.md        # Workspace tier
skills/*/SKILL.md               # Project tier
```

Use `list_directory` on each tier path. For each skill found, use `read_file` to load the frontmatter.

### Step 2: Gating Checks (automated)

For each skill, check the gating metadata in its frontmatter:

| Field | Check | Command |
|-------|-------|---------|
| `requires_bins` | Is each binary on PATH? | `run_command: which <bin>` |
| `requires_env` | Is each env var set? | `run_command: printenv <var>` (or check if non-empty) |
| `os` | Does the current OS match? | Compare against runtime OS |

**Verdict:** `gate-pass` (all prerequisites met) or `gate-fail` (list specific missing dependencies).

### Step 3: Drift Checks (semi-automated)

Read the full SKILL.md body and check for stale references:

- **Tool references:** Does the skill mention tool names (e.g. `http_request`, `sign_and_send_transaction`) that no longer exist in the agent's tool registry?
- **File references:** Does the skill reference file paths or directories that no longer exist in the workspace? Use `list_directory` or `read_file` to verify.
- **Capability references:** Does the skill mention capabilities or features the agent no longer has?

**Verdict:** `pass` (no stale references) or `drift` (list specific stale references).

### Step 4: Dead Skill Detection

A skill is "dead" if it is installed but never triggered:

- **Heuristic 1:** No agent in the config has the skill's package in `skill_packages`. Check the workspace `config.toml`.
- **Heuristic 2:** The skill's description doesn't match any plausible user workflow for the configured agents.

**Verdict:** `active` or `dead` with reasoning.

## Report Format

Produce a table with one row per skill:

| Skill | Tier | Verdict | Reasoning | Action |
|-------|------|---------|-----------|--------|
| `aura-orchestrator` | project | pass | All gates pass, no drift | keep |
| `molecule-x402` | project | gate-fail | Missing env: X402_GATEWAY_URL | keep (conditional) |
| `old-workflow` | workspace | drift | References removed tool `plan_create` | revise |
| `unused-skill` | managed | dead | No agent has this in skill_packages | archive |

## Recommended Actions

| Action | When | What to do |
|--------|------|------------|
| **keep** | All checks pass | No action needed |
| **keep (conditional)** | Gate-fail but skill is otherwise valid | Document the missing prerequisites |
| **revise** | Drift detected | Use skill-creator in modify mode to update stale references |
| **archive** | Dead skill | Move out of active tier (e.g. to a `skills/_archive/` directory) |

## Limitations (Phase 0)

This version of skill-eval does NOT support:
- **LLM-judge mode** -- no automatic prompt replay or output comparison. Specified in `docs/superpowers/specs/2026-04-19-eval-runner-design.md` as the `tengu eval <skill>` CLI runner, which replays `skills/<skill>/evals/prompts.{md,yaml}` against a live agent and scores each row pass/fail via an LLM judge. Skill-eval stays focused on health/drift checks; the eval runner is the dynamic counterpart.
- **Scheduled runs** -- eval runs only on explicit trigger.
- **Automatic remediation** -- reports only, does not auto-fix. Phase E (`2026-04-16-phase-e-self-alignment-design.md`) adds structural fixture replay + proposed-edit generation.

These capabilities are being added incrementally as usage data accumulates.
