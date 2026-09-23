# Skills

Skills are portable, cross-platform workflow documents that compose Tengu's [[architecture#Key Abstractions|platform primitives]] into higher-level capabilities. Skills are never modified by the platform — they are copied as-is from external sources.

**File:** `src/application/skills/registry.rs`

## Skill Types

| Type | Creates Tools | How It Works |
|------|--------------|-------------|
| **Documentation** (frontmatter) | No | Compact XML catalog in system prompt; agent reads SKILL.md on demand |
| **Shell** (classic) | Yes | Named tools with execution templates, injected inline |

API skills are documentation-only — they describe API endpoints for the agent to use with `http_request`. They do not create tools.

## Loading

Skills are loaded from three tiers — first match wins (`application/skills/registry.rs::skill_directories`):
1. **Managed:** `~/.tengu/skills/` (`tengu skill install --tier managed`)
2. **Workspace (dotdir):** `<workspace>/.tengu/skills/`
3. **Project:** `<workspace>/skills/`

Agents select skills via `skill_packages` on any `[agents.<name>]` block of the sandbox config ([[configuration]]); `skills = [...]` is an accepted alias:
```toml
[agents.main]
skill_packages = ["aura-orchestrator", "beach-science"]
```

## Cross-Engine Compatibility

Skills work identically with both [[engine-backends]]:

### OpenRouter
- Skill tools are registered in Tengu's tool loop
- Skill context injected into system prompt
- Agent calls skill tools directly

### Claude Code
- Skill context still injected into system prompt (via `build_system_prompt()`)
- Skill tools exposed via [[mcp-bridge]] as MCP tools
- Agent calls MCP tools by name (same names as the Tengu tools the skill references)
- Claude's native tools (Read, Write, Bash) also available alongside MCP tools

The skill document references Tengu tool names like `http_request`, `sign_and_send_transaction`, `shared_cache`. These exact names are registered as MCP tools in the bridge, so skill instructions work unchanged across engines.

## Frontmatter

```yaml
---
name: my-skill
description: What this skill does
homepage: https://example.com
requires_bins: ["curl"]     # optional: required CLI tools
requires_env: ["API_KEY"]   # optional: required env vars
os: ["linux", "macos"]      # optional: OS filter
editable_by_learner: true   # optional (default false): in-chat `adjust yourself` / `fix it` may mutate this skill
learner_facing: true        # optional (default false): per-learner state at skills/<name>/state/<learner_id>.json
metrics:                    # optional: accuracy metrics (see below)
  - name: output_quality
    kind: llm_judge
    rubric_file: metrics/rubric.md
    min_pass_rate: 0.7
---
```

Gating metadata (`requires_bins`, `requires_env`, `os`) is evaluated at load time. Skills that fail gating checks are silently skipped.

## Metrics & Evolution

A skill's `metrics:` block declares accuracy characteristics the harness can measure against. Six built-in kinds (`src/application/skills/lifecycle/metric_kinds/`):

| Kind | What it does |
|------|--------------|
| `shell_check` | Runs a command, matches exit code + stdout regex. For deterministic outcomes (tx confirmed, file exists). |
| `llm_judge` | Loads a `rubric_file`, lets a judge LLM score the fixture transcript against narrative criteria. For qualitative quality. |
| `tool_assertion` | Dispatches a workspace tool (e.g. `persistent_store`) and asserts on its output via `value_matches` / `value_equals` / `value_in`. |
| `script` | Invokes `sh <path>` with fixture data in env vars; parses stdout JSON `{pass, score, notes?}`. Escape hatch for custom checks. |
| `dialog_replay` | Reflective eval: scores the current dialog slice from `from_message_index` by delegating to a sibling `llm_judge` / `tool_assertion` metric (`delegate_metric`). |
| `description_trigger` | Judge LLM decides whether the planner would route to this skill for each query in `queries_file` (`{queries: [{query, should_trigger}]}`); `runs_per_query` (3), `holdout` (0.4). |

Each metric may set `min_pass_rate` (0.0..=1.0). A metric whose rolling pass rate falls below its threshold is **gated** — `tengu skill evolve` targets the lowest-scoring gated metric.

See `docs/superpowers/specs/2026-04-20-skill-metrics-evolution-design.md` §6.1 for the complete frontmatter schema.

## Distillation

An agent with `workspace_tools = ["skill_distill"]` (or `skill_distill` in a subagent block's `tools`) can author a new skill mid-conversation via the `skill_distill` tool. Given in-context understanding plus a starting message index, the tool writes `skills/<name>/{SKILL.md, evals/prompts.yaml, evals/config.toml, metrics/<scaffolds>}` atomically (`evals/config.toml` mirrors the calling agent's engine + model). The new skill does **not** load into the current conversation — cache discipline requires a stable tool/skill inventory per conversation. It becomes available on next session start.

`manage_skill` (`create | edit_body | patch | add_resource | remove_resource | delete`; opt-in like `skill_distill`) + `view_skill` (`list | read | read_resource`; always on) are the unified in-chat read/write API — `docs/skill-redesign-2026-04-29.md`. `skill_resource` and `apply_improver_proposal` are kept as back-compat aliases. Audit log: `skills/.audit.jsonl`.

## CLI commands

| Command | Purpose |
|---------|---------|
| `tengu eval <skill>` | Replay `evals/prompts.yaml` fixtures, score each via the skill's metrics, write `metrics.json` + append to `metrics/history.jsonl`, emit per-row transcripts under `evals/runs/<ts>/`. |
| `tengu skill metrics <skill>` | Show rolling `metrics.json` + recent history entries (read-only, no API calls). |
| `tengu skill evolve <skill>` | Bounded rewrite→rescore loop. Baseline-evals, picks the lowest-gated metric as target, spawns `skill-improver` in a scratch git worktree for N cycles, picks the best cycle (no regression > 0.05 on other gated metrics), shows a diff + metric delta, prompts y/n/d/o. |
| `tengu skill accept-proposal <path>` | Reserved for auto-trigger follow-up (no-op in v1). |
| `tengu skill list [--tier T]` | Walk the three tiers; print name, tier, metrics health. |
| `tengu skill remove <name> [--tier T] [--yes]` | Delete a skill dir (default tier `project`). |
| `tengu skill doctor [--no-fail]` | Cross-check `[agents.*].skill_packages` of the active config vs disk: phantoms (exit non-zero unless `--no-fail`), orphans, missing rubric files, scanner findings. |
| `tengu skill export <name> [--out <path>]` | Tar.gz bundle of the skill. |
| `tengu skill install <source> [--tier T] [--strict] [--yes]` | Quarantine → scan → validate → install from URL / git / local path (default tier `managed`; `--strict` refuses caution/dangerous verdicts). |
| `tengu skill seed <name> [<resources_dir>] [--tier T]` | Teacher onboarding: SKILL.md template + `resources/` folder. |

Activating evolve requires `[skill_lifecycle]` + `[agents.skill-improver]` in the active config (`sandboxes/<name>/config.toml` via `--sandbox`, else `~/.tengu/config.toml`); `fixture_runner_agent` is accepted but unused. `sandboxes/aura/config.toml` ships a working block — see [[configuration]].

## Example: Documentation Skill

`skills/aura-orchestrator/SKILL.md` — a documentation skill that orchestrates a 7-phase DeSci pipeline:
- References `http_request`, `sign_and_send_transaction`, `abi_encode`, `sign_message`, `get_wallet_address`, `shared_cache`, `read_file`, `run_command`
- Works on OpenRouter (Tengu tool loop) and Claude Code (MCP bridge)

## Example: Shell Skill

```markdown
# test_runner
Run the project test suite.
## Parameters
- `filter` (string, optional): test name filter
## Execution
\```bash
cargo test {{filter}} 2>&1
\```
```

Shell skills create named tools that execute templates with parameter substitution. They are injected inline into the tool list.

## Related
- [[architecture]] — where skills fit in the system
- [[configuration]] — skill_packages config + `[skill_lifecycle]` block
- [[mcp-bridge]] — how skill tools reach Claude Code
- [[engine-backends]] — how skills work with different backends
- `docs/superpowers/specs/2026-04-20-skill-metrics-evolution-design.md` — metrics + evolve design spec
