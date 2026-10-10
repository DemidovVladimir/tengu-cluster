# Studio editor — design only (ST-40, 2026-10-09)

> **Built 2026-10-10 as the Studio builder** — `docs/studio-builder-2026-10-10.md` (deviations listed in `TENGU_STUDIO_PLAN.md` § 7). This page stays as the design record.

`TENGU_STUDIO_PLAN.md` § 7. **No code, route or save exists** (on main with #44, `f7ac9a5636e2493e3839e896cba9951a2d41aefb`, as this design only): production edit/save waits for Operator Review #3. No throwaway branch — the spike = the seams below, which already run in the read-only Studio. Gate 4 was waived, so the vocabulary is the read model's, not observed use.

## Palette — from Rust, never a JS list

| Group | Source | Served as |
|---|---|---|
| Node kinds + columns | `domain::workflow::NodeKind` + `layer` (the graph Studio draws) | same `kind` / `layer` |
| Tools of an agent | `bootstrap::tools::agent_base_tools` → `catalog()` `ToolDef` (name, description, `parameters`, schema-linted by `tools/schema_lint.rs`); opt-in only via `domain::tools::WORKSPACE_TOOLS`; `[[mcp_servers]]` tools as discovered (`{server}__{tool}`) | the tool's own JSON schema |
| Node fields | `DecisionLoopConfig` / `ActionConfig` / `SlotConfig` / `SeqStep`, `FeedConfig` / `FeedWindow`, `RuntimeConfig`, `AgentConfig.tools` / `scopes` | a Rust `palette()` (key, type, default, required); a test round-trips each entry through the `deny_unknown_fields` struct — no `schemars` dep |
| Never offered | `run_command`, shell skills, a code / script node, `[solana]` signer, wallets, `[risk]` / `[paper]` | — |

## Connections = existing edges (`domain/workflow.rs`) → the TOML they write

| Edge | Writes | Checked by |
|---|---|---|
| `fires` feed / webhook → loop | `[feeds.<f>] kind = "tick"`, `target`, `event` · `[webhooks.endpoints.<e>] loop` | `FeedConfig` / webhook validation |
| `reads` loop → world | `world = { <alias> = "<schema>:<subject>" }`, `world_max_age_secs` | `DecisionLoopConfig::validation_errors` |
| `asks` · `chooses` | `model`, `act_at`, `history`, `max_steps` · `[decision_loops.<l>.actions.<a>]` (no `tool` = terminal) | same |
| `calls` action / tool feed → tool | `tool`, `args`, `read_only` | tool in the agent's list |
| `binds` world / action / event → action | `slots.<s> = { observation, path }` · `{ from, path }` · `{ event = "/x" }` | unresolved ⇒ not legal |
| `next` · `guards` · `escalates` | `sequence = ["a", "b?"]` · `caps`, `requires`, `[agents.<a>.scopes.<tool>]` · `escalate` | same |
| `owns` agent → loop / tool feed | `agent = "<a>"` | agent exists |
| `owns` `[soe] architect` → job feed | `[feeds.<f>] kind = "job"`, `job = "soe_cycle"` (`application/studio/graph.rs`) | none here: `[soe]` is hardened ⇒ view-only |

## Validate → preview → Save as new

| # | Step |
|---|---|
| 1 | Page sends typed edit ops, never TOML text: `POST /api/v1/drafts {from, name, ops}` behind the control guard (token header + `Origin` + `Sec-Fetch-Site`) |
| 2 | Rust applies the ops to the source text and writes `<scratch>/sandboxes/<name>/config.toml` (the path matters: hardening and `[generation]` checks read it) |
| 3 | `Config::load` on it — the same parse, `validation_errors`, hardening, lineage binding; errors verbatim; page checks are hints only |
| 4 | Preview: the generated TOML, a unified diff vs the source (`similar`, already a dep) and `build_graph` of the draft |
| 5 | Save as new: `sandboxes/<name>/config.toml`, `create_new` (an existing dir is refused), loaded again after the write. Never in place, never a running sandbox (lease held) |

| Rule | Behaviour |
|---|---|
| `[generation]`-bound (W1) · hardened | view-only; Save-as-new drops `[generation]` (new design = new sandbox / generation, `tengu lineage`); hardened knobs stay hand-edited TOML |
| Execution Map | the "narrow this loop" panel emits map JSON only, checked by `ExecutionMap::apply` (`GET /api/v1/graph?map=`, `tengu studio graph --map`); never written into TOML |
| Safety | scopes, wallets, modes and caps are explicit fields in the diff; nothing widens silently |

## Open for Review #3

| Question | Default pick |
|---|---|
| Keep comments on save | `toml_edit` as a direct dep (0.22 is already in `Cargo.lock` via `toml` 0.8) |
| Where drafts live | `sandboxes/<name>/` (reviewable in git) |
| Edit switch | `tengu studio --allow-edit`, off by default, like control |
