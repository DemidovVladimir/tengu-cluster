---
name: skill-eval
description: Use when measuring a skill's declared accuracy metrics or inspecting rolling pass rates. Points to the `tengu eval` / `tengu skill metrics` / `tengu skill evolve` CLI surface.
---

# Skill Eval

The Phase-0 drift-audit workflow has been subsumed by the harness-owned skill lifecycle subsystem. Use the CLI:

| Command | When to use |
|---|---|
| `tengu eval <skill>` | Replay the skill's fixtures, score its metrics, write a rolling report. |
| `tengu skill metrics <skill>` | Show the current rolling `metrics.json` + recent history entries. |
| `tengu skill evolve <skill>` | Launch a bounded rewrite->rescore loop with a user approval gate. Use when a gated metric has been failing and you want the harness to propose a revision. |

Every skill declares its own metrics in SKILL.md frontmatter. See `skills/skill-creator/SKILL.md` for authoring guidance and `docs/superpowers/specs/2026-04-20-skill-metrics-evolution-design.md` section 6 for the frontmatter contract.

**This skill is documentation-only.** It does not call any tool; it points to the CLI.
