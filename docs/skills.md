# Skills

Skills are portable workflow documents that compose Tengu's platform primitives (tools) into higher-level capabilities. The platform never modifies a skill on its own — they are copied as-is from external sources; only the opt-in lifecycle tools and `tengu skill evolve` (with the operator's approval) write one.

**Code:** `src/application/skills/registry.rs` (parse, discover, registry), `src/application/skills/lifecycle/` (metrics, eval, evolve, scanner).

## Inventory (`skills/`, 2026-10-09)

| Skill | Kind | Purpose | Loaded by |
|---|---|---|---|
| `privy-agentic-wallets` | API reference (`base_url: https://api.privy.io`) — a git submodule pinned at `7f104aa118a891aca85cfebbd68bf9f4a2cd85e7` with no `.gitmodules` entry: empty in a fresh clone or worktree | Privy agentic wallets: create / manage wallets, policies, transactions | no sandbox lists it |
| `execution-map` | documentation | Architect → Jev: the execution-map JSON for `tengu decide --map` (`config/execution_map.rs`) — fields, narrowing rules, outcomes | no sandbox lists it |
| `skill-creator` | documentation (+ `metrics`, `evals/`) | Create or modify a skill: anatomy, frontmatter, tiers, workflow | no sandbox lists it |
| `telegram-rag-ingest` | documentation | Resources shared in Telegram (attachments, URLs, text) → searchable vector memory; answer from it later (`http_request` → `write_file` → `persistent_store`) | `storage-test` → `storage` |
| `xlab-research` | documentation | xlab Architect protocol: hypothesis → strategy spec → backtest after costs → tune on the in-sample half → ONE holdout read → critique; copy-paste call shapes for `market_history` / `backtest` | `xlab`, `xlab-w2` → `xl_architect` |
| `soe-architect` | documentation (+ `resources/proposal.example.json`) | SOE Architect stage: read the run's packet, at least two mechanisms + `HOLD`, ranges with evidence ids or `UNKNOWN`, never a computed field; `soe_propose` call shapes and the draft format | `soe` → `soe_architect` |
| `soe-critic` | documentation | SOE Critic stage: the seven challenge kinds and their conservative-only effects; `soe_challenge` flat call shape | `soe` → `soe_critic` |
| `orchestrator` | documentation (+ `plan_schema.json`) | The planner's system prompt: a direct answer or plan JSON | every `[orchestrator]` sandbox (`lping` only) — read by the planner from `<cwd>/skills/orchestrator/SKILL.md` (`planner.rs`), not through `skill_packages`; its lifecycle-verb rows need a `learning-agent` block no sandbox ships |
| `skill-eval` | documentation | Points to `tengu eval` / `tengu skill metrics` / `tengu skill evolve` | none |
| `german-teacher` · `spanish-teacher` | documentation, learner-facing (`resources/`, `evals/prompts.yaml`) | Teacher-seeded skills (`tengu skill seed`), `editable_by_learner: true`; read their materials with `skill_resource` | none |
| `orchestration-e2e` | no `SKILL.md` — `evals/` only | 9 eval rows + an orchestrated eval config (`tengu eval orchestration-e2e`) | `tengu eval` |

## Kinds

| Kind | Frontmatter | Creates tools | How it works |
|---|---|---|---|
| Documentation | `name` + `description` | no | Body is a context fragment in the system prompt |
| API reference | + `base_url` (alias `homepage`) | no | Documentation for `http_request` — the agent calls the API itself |
| Shell (classic) | none: `# <tool_name>`, `## Parameters`, `## Execution` with a fenced command | yes | A named tool; the template runs with parameter substitution |

An agent that runs no shell (`[risk]`, `[soe]` or a `[solana]` signer sandbox: `AgentConfig::hardened`) loads no shell skill (`SkillRegistry::with_shell_skills`, warn naming them).

## Loading

| Order (first match wins) | Directory |
|---|---|
| 1 Managed | `~/.tengu/skills/` — the home dir's, even when `TENGU_HOME` points elsewhere (`tengu skill install --tier managed`) |
| 2 Workspace dotdir | `<workspace>/.tengu/skills/` |
| 3 Project | `<workspace>/skills/` |
| 4 Cwd | `<cwd>/skills/` when it differs from 3 — the repo's `skills/` when tengu runs from the repo root |

An agent loads only the skills named in its `skill_packages` (`skills = [...]` alias; each entry is lowercased with `-` → `_` and compared with the skill name — a frontmatter `name` is normalised the same way, a classic skill's `# heading` is taken as written) — an empty list loads none:

```toml
[agents.xl_architect]
skill_packages = ["xlab-research"]
tools = ["market_history", "backtest", "read_file", "list_directory"]   # a skill gets only these tools
```

List every tool the `SKILL.md` calls in the agent's `tools` (an empty `tools` = every base tool + configured opt-ins).

## Engines

Every skill works under `openrouter`, `local` and `claude_code` (operator rule 2026-09-30).

| Engine | Documentation / API skills | Shell skills |
|---|---|---|
| `openrouter`, `local` | in the system prompt | tools in Tengu's tool loop |
| `claude_code` | in the system prompt (`build_system_prompt()`) | served by `tengu mcp-bridge` (`skill_packages` + the requested names), seen as `mcp__tengu-tools__<name>` |

Claude's own built-ins (Read, Write, Bash) follow `[agents.<a>.claude_code] builtin_tools_profile`; `none` (required in a hardened sandbox) also stops the CLI loading its own skills, settings, hooks and plugins. Skill text names Tengu tools (`http_request`, `shared_cache`, …); the bridge serves them under those names behind the `mcp__tengu-tools__` prefix.

## Frontmatter

```yaml
---
name: my-skill
description: What this skill does — the planner and the agent read it
env_vars:                   # optional; `NAME?` = optional
  - MY_API_URL
editable_by_learner: false # optional; absent = editable: `manage_skill` writes refuse only an explicit false
learner_facing: true        # optional marker (`tengu skill seed`, `manage_skill create`)
metrics:                    # optional: accuracy metrics (below)
  - name: output_quality
    kind: llm_judge
    rubric_file: metrics/rubric.md
    min_pass_rate: 0.7
---
```

| Key | Read by | Effect |
|---|---|---|
| `name` · `description` | registry | identity (`-` → `_`, lowercase) · catalog / planner text |
| `base_url` / `homepage` | registry | makes it an API-reference skill; must be a valid URL |
| `env_vars` | registry | `$VAR` / `${VAR}` of listed, non-secret names (no `KEY`, `SECRET`, `TOKEN`, `PASSWORD`, `PASS`) expanded in the body; `/skills` marks a skill whose required vars are unset (`missing: …`) — it still loads |
| `commands` | registry | slash commands the skill declares |
| `metrics` | lifecycle | `tengu eval` / `skill evolve` / `skill doctor` |
| `editable_by_learner` | `manage_skill`, `apply_improver_proposal`, `skill evolve` | `false` refuses their writes (`manage_skill`: `edit_body`, `patch`, `add_resource`, `remove_resource` — not `create` or `delete`); absent or unparseable = editable |
| `learner_facing` | written by `skill seed` / `manage_skill create` | marker; the per-learner state module (`skills/<name>/state/<learner_id>.json`, `lifecycle/learner_state.rs`) is landed but not wired |

`requires_bins` (each on `PATH`), `requires_env` (each set) and `os` (`macos` · `linux` · `windows`; `darwin` = macos) gate loading of frontmatter skills (2026-10-08, `registry.rs::SkillGate`, checked on every `reload`): a skill missing one is not loaded, an info log line names why. Inline `[a, b]` or a block list.

## Metrics & Evolution

A skill's `metrics:` block declares accuracy characteristics the harness measures. Six built-in kinds (`src/application/skills/lifecycle/metric_kinds/`):

| Kind | What it does |
|------|--------------|
| `shell_check` | Runs a command, matches exit code + stdout regex. For deterministic outcomes (tx confirmed, file exists). |
| `llm_judge` | Loads a `rubric_file`, lets a judge LLM score the fixture transcript against narrative criteria. For qualitative quality. |
| `tool_assertion` | Dispatches a workspace tool (e.g. `persistent_store`) and asserts on its output via `value_matches` / `value_equals` / `value_in`. |
| `script` | Invokes `sh <path>` with fixture data in env vars; parses stdout JSON `{pass, score, notes?}`. Escape hatch for custom checks. |
| `dialog_replay` | Reflective eval: scores the current dialog slice from `from_message_index` by delegating to a sibling `llm_judge` / `tool_assertion` metric (`delegate_metric`). |
| `description_trigger` | Judge LLM decides whether the planner would route to this skill for each query in `queries_file` (`{queries: [{query, should_trigger}]}`); `runs_per_query` (3), `holdout` (0.4). |

Each metric may set `min_pass_rate` (0.0..=1.0). A metric whose rolling pass rate falls below its threshold is **gated** — `tengu skill evolve` targets the lowest-scoring gated metric. Full frontmatter schema: `docs/superpowers/specs/2026-04-20-skill-metrics-evolution-design.md` §6.1.

## Distillation and in-chat edits

| Tool (opt-in unless noted) | Does |
|---|---|
| `skill_distill` | Authors a new skill mid-conversation: `skills/<name>/{SKILL.md, evals/prompts.yaml, evals/config.toml, metrics/<scaffolds>}` atomically (`evals/config.toml` mirrors the calling agent's engine + model). Loads on the next session (stable tool / skill inventory per conversation). Refuses a call with no conversation (a bridged call gets the run's transcript) |
| `manage_skill` | `create \| edit_body \| patch \| add_resource \| remove_resource \| delete` — the unified write API (`docs/skill-redesign-2026-04-29.md`) |
| `view_skill` (always) | `list \| read \| read_resource` |
| `skill_resource` (always), `apply_improver_proposal` | back-compat aliases |

Writers refuse agent / CLI state paths (`.tengu/`, `.claude/`, `skills/` via `write_file`, `CLAUDE.md`, `AGENTS.md`, …; `domain::scope::protected_write`). Audit log: `skills/.audit.jsonl`.

## CLI commands

| Command | Purpose |
|---------|---------|
| `tengu eval [<skill>...] [--sandbox] [--judge-model] [--concurrency] [--format table\|json] [--out] [--filter] [--keep-workspace] [--keep-runs] [--no-persist] [--max-runs]` | Replay `evals/prompts.yaml` (or `prompts.md`) fixtures, score each via the skill's metrics, write `metrics.json` + append `metrics/history.jsonl`, per-row transcripts under `evals/runs/<ts>/`. No skill = every skill with evals; judge default `anthropic/claude-opus-4-7`; `--concurrency` > 1 is refused (not implemented) |
| `tengu skill metrics <skill> [--last N]` | Rolling `metrics.json` + recent history entries (read-only, no API calls) |
| `tengu skill evolve <skill> [--max-cycles] [--target-metric] [--base-branch] [--sandbox]` | Bounded rewrite → rescore loop: baseline eval, target = the lowest gated metric, `skill-improver` in a scratch git worktree for N cycles, best cycle (target beats its baseline, no other metric drops > 0.05 — passing ones included), diff + metric delta, y/n/d/o prompt |
| `tengu skill accept-proposal <path>` | Reserved for auto-trigger follow-up (no-op in v1) |
| `tengu skill list [--tier T]` | Walk the tiers; name, tier, metrics health |
| `tengu skill remove <name> [--tier T] [--yes]` | Delete a skill dir (default tier `project`) |
| `tengu skill doctor [--sandbox] [--no-fail]` | Cross-check `[agents.*].skill_packages` of the active config vs disk: phantoms (exit non-zero unless `--no-fail`), orphans, missing rubric files, scanner findings |
| `tengu skill export <name> [--out <path>]` | Tar.gz bundle of the skill |
| `tengu skill install <source> [--tier T] [--strict] [--yes]` | Quarantine → scan → validate → install from URL / git / local path (default tier `managed`; `--strict` refuses caution / dangerous verdicts) |
| `tengu skill seed <name> [<resources_dir>] [--tier T] [--description] [--learner-facing] [--yes]` | Teacher onboarding: SKILL.md template + `resources/` folder |

Evolve needs `[skill_lifecycle]` (`improver_agent`) + that agent's block in the active config (`--sandbox <name>`, else `~/.tengu/config.toml`); no sandbox ships one — commented `[skill_lifecycle]` sample in `config.example.toml`, `docs/configuration.md` § Skill lifecycle.

## Example: shell skill

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

Its command is gated by `shell_bins` in its scope (first command word only — a guard rail, not a sandbox); it never loads in a hardened sandbox.

## Related
- `docs/configuration.md` — `skill_packages`, `[skill_lifecycle]`
- `docs/mcp-bridge.md` — how skill tools reach Claude Code
- `docs/engine-backends.md` — engines
- `docs/skill-redesign-2026-04-29.md` · `docs/superpowers/specs/2026-04-20-skill-metrics-evolution-design.md`
