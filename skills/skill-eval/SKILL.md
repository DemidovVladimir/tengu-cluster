---
name: skill-eval
description: Use when measuring a skill's declared accuracy metrics or inspecting rolling pass rates. Points to the `tengu eval` / `tengu skill metrics` / `tengu skill evolve` CLI surface.
---

# Skill Eval

The Phase-0 drift-audit workflow has been subsumed by the harness-owned skill lifecycle subsystem. Use the CLI:

| Command | When to use |
|---|---|
| `tengu eval <skill>` | Replay the skill's fixtures, score its metrics, write a rolling report. (Top-level — separate runner.) |
| `tengu skill metrics <skill>` | Show the current rolling `metrics.json` + recent history entries. |
| `tengu skill evolve <skill>` | Launch a bounded rewrite->rescore loop with a user approval gate. Use when a gated metric has been failing and you want the harness to propose a revision. |
| `tengu skill list [--tier T]` | Walk the three tiers; print a table of installed skills with fixture counts, gated metrics, and shell-kind warnings. |
| `tengu skill remove <name> [--tier T] [--yes]` | Delete a skill directory. Refuses while an evolve worktree is active. Audit-logged. |
| `tengu skill doctor [--no-fail]` | Cross-check `agents/*.toml::skills` vs the filesystem; report orphans, phantoms, missing rubric files, and scanner findings. Exits non-zero on phantoms unless `--no-fail`. |
| `tengu skill export <name> [--out <path>]` | Bundle SKILL.md + evals/prompts.yaml + metrics/*.md (and *.sh when Script metrics declared) as a tar.gz. |
| `tengu skill install <source> [--tier T] [--strict] [--yes]` | Quarantine -> symlink-aware extract -> validate frontmatter -> scan -> atomic move. `--strict` refuses on caution/dangerous verdict. |
| `tengu skill seed <name> <resources_dir> [--tier T] [--description S] [--learner-facing] [--yes]` | Teacher onboarding: drop a SKILL.md template + copy a folder of materials into `skills/<name>/resources/`. Atomic. Defaults to `learner_facing: true` so `adjust yourself` can mutate it. |

Every skill declares its own metrics in SKILL.md frontmatter. See `skills/skill-creator/SKILL.md` for authoring guidance and `docs/superpowers/specs/2026-04-20-skill-metrics-evolution-design.md` section 6 for the frontmatter contract.

**This skill is documentation-only.** It does not call any tool; it points to the CLI.
