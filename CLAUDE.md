# CLAUDE.md — Tengu-Cluster Project Guide for AI Assistants

> **Read this first, every session, before touching any code.**
> This project is "vibe-coded" — there's a real underlying doctrine but the
> implementation has accumulated layers and naming choices that aren't
> obvious from the file tree alone. The docs below are not optional.

---

## What this project is

Tengu-Cluster is a multi-agent harness in **Rust**. Single binary. The user runs
`tengu chat --sandbox <name>` (or `tengu telegram --sandbox <name>`) and types
messages into a TUI / Telegram chat. The harness:

1. Loads `sandboxes/<name>/config.toml` (which agent is the orchestrator, what
   model to use, MCP servers, scope rules).
2. Builds a `RagPlanner` — an LLM that picks which agent should handle the
   message based on a semantic search over `tengu_registry` (Qdrant).
3. Dispatches plan steps to `SubprocessRunner`, which spawns
   `tengu run-agent` as a child process for each step. The child loads the
   `agents/<step.agent>.toml` spec, runs an LLM-with-tools loop until it calls
   `compress_and_store`, then writes the result back to `tengu_outputs`.

The doctrine is **LLM = heart, RAG = brain, tools = hands**. The planner picks;
subagents execute. The boundary is enforced in code (`run_turn_with_system`
strips tools/memory/grounding on the planner-side LLM call).

---

## Output style — read this before writing ANY doc or response

The user does not have time to read walls of prose. Default to terse, scannable, table-driven output. Applies to docs, chat replies, and commit messages.

| Rule | Applied as |
|---|---|
| Tables over prose | If it can be a table, it must be. |
| One-line decisions, not justifications | Cite a §-ref instead of recapping reasoning. |
| Cap new docs at one screen | Unless the user explicitly asks for depth. |
| Cut "why this matters" / "honest read" / recap preambles | The table is the explanation. |
| Bullets, not paragraphs | Three sentences in a row → convert. |
| Use existing pointers | Don't re-explain `skill_distill` if a § already does. |

**Test**: would a busy operator skim this in 30 seconds and know what to do next? If not, cut more.

Why this rule exists: an earlier draft of `docs/skill-research-2026-04-28.md` ballooned past 700 lines of prose. The user pushed back twice. Don't make them push back again.

---

## REQUIRED reading before any code changes

Read in this order. Do not skip.

1. **`docs/architecture-2026-04-27.md`** — the line-by-line architecture
   walkthrough. Five-sentence TL;DR up top, then §1 walks the seven steps from
   prompt to reply, §2 lists every file in every subsystem, §5 has a
   "trace one turn end-to-end before changing anything" recipe.
2. **`docs/architecture-2026-04-27.svg`** — the picture. Companion to the
   walkthrough. Open both side by side.
3. **`docs/architecture-2026-04-27.html`** — interactive explorer. Tabs for
   walking a turn step-by-step, expanding subsystem cards, searching the file
   map. Use this when you don't remember which file owns what.
4. **`docs/SESSION_HANDOFF.md`** — running state log. What landed last session,
   what's open, known gotchas. Always read the top section.
5. **`REDESIGN.md`** — original v2 design brief. The "why" behind the doctrine.
6. **`docs/IMPLEMENTATION_PLAN.md`** — phase-by-phase roadmap. Many phases
   are now done; the doc tracks what shipped and what didn't.
7. **`docs/comparison-2026-04-26.md`** + **`.svg`** — Tengu vs Hermes Agent
   vs PI/Cowork. Useful when deciding "should we add feature X" — often
   already exists in one of the other two and informs design.
8. **`docs/context-management-2026-04-27.{md,svg,html}`** — canonical
   reference for everything that shapes what an LLM sees: 7 layers,
   ~25 mechanisms (token primitives, per-flow lifecycle, per-turn
   assembly, inner tool loop, MCP bridge cap, subagent IPC, RAG hygiene,
   file chunking). Read this BEFORE touching `prompt_budget.rs`,
   `flow_builder.rs`, `engine_builder.rs::collect_engine_response`,
   `chat_builder.rs::process_user_text`, or any of the `LimitsConfig`
   knobs. The .html is interactive (search mechanisms, walk a turn,
   symptom → cause lookup); the .md is the canonical narrative.
   Two focused companions slice this further:
   - `docs/context-cutting-flow-2026-04-27.{svg,html}` — Layer 3 deep-dive
     (the inner tool loop).
   - `docs/compression-flow-2026-04-27.{md,svg}` — Layer 5 deep-dive
     (`compress_and_store` step protocol).

---

## REQUIRED updates after non-trivial changes

If you touched code that affects architecture, file structure, or the per-turn
flow, audit these for staleness **before declaring done**:

| File | When to update |
|---|---|
| `docs/SESSION_HANDOFF.md` | Almost always — mark the items you closed, add new opens, update the "TL;DR" if behaviour changed |
| `docs/architecture-2026-04-27.md` | If you changed the per-turn flow, added/removed a subsystem, or moved a file's responsibilities |
| `docs/architecture-2026-04-27.svg` | If you changed the per-turn flow OR added/removed a file in a subsystem panel |
| `docs/architecture-2026-04-27.html` | Same triggers as the svg + md. The SUBSYSTEMS / STEPS / FILE_MAP arrays in the inline `<script>` need to stay in sync |
| `docs/comparison-2026-04-26.md` + `.svg` | If your change affects how Tengu compares to Hermes or PI on memory/skills/tools/routing |
| `docs/skill-research-2026-04-28.md` | If the skill-lifecycle plan, gap inventory, or learning-platform A1/A2/A3 decisions change. |
| `docs/context-management-2026-04-27.{md,svg,html}` | If you changed any of the ~25 context-shaping mechanisms (anything in `prompt_budget.rs`, `flow_builder.rs`, `engine_builder.rs::collect_engine_response`, `chat_builder.rs::process_user_text`, the `LimitsConfig` / `MemoryConfig` defaults, the `compress_and_store` protocol, or `rag/cleanup.rs`). The .html keeps inline JS arrays — keep them in sync with the .md. |
| `CLAUDE.md` (this file) | If you added/removed a top-level subsystem, changed the doctrine, or added a new "required reading" doc |
| Inline `mod.rs` doc comments in `src/adapters/memory/` and `src/adapters/rag/` | If you changed the layering between memory/ (low-level) and rag/ (v2 facade) |
| `agents/*.toml` | If you changed the AgentSpec schema, document the new field in `src/adapters/agents/mod.rs` and update each agent file |
| `docs/webhooks-2026-05-11.md` | If you changed `src/adapters/webhook_builder.rs`, `WebhookConfig`, or the request/response shape. Canonical operator doc for the webhook listener. |
| `skills/orchestrator/SKILL.md` | If you changed what the planner can output OR added a new prompt block (e.g. cross-session recall) |
| `skills/orchestrator/plan_schema.json` | If you changed the plan JSON shape (e.g. added `Step.compose` for C→B fallback) |
| `src/adapters/metrics.rs` doc-comments | If you changed `MetricsRecord` shape, added a new `MetricsKind`, or moved the global sink semantics. The header doctrine block sells the design — keep it accurate. |

**Rule of thumb:** if a future Claude opening this project would learn the
"wrong" thing from a doc, the doc is stale. Fix it in the same commit that
made the doc stale, not later.

---

## How to read context/token metrics

Every LLM and embedding call emits a `MetricsRecord` (see
`src/adapters/metrics.rs`). Three surfaces:

1. **Always-on tracing** — `RUST_LOG=tengu=info` prints one structured line
   per call: `kind=`, `agent=`, `model=`, `prompt_tokens=`,
   `completion_tokens=`, `latency_ms=`, `prompt_chars=`. Per-layer breakdown
   (system / roster / cross_session / history / user_message) goes to `debug`
   so the info line stays narrow.
2. **TUI bottom panel** — opt-in via `TENGU_TUI_METRICS=1`. Renders a
   compact aggregated status line as a System bubble after every call. The
   in-process aggregator (`AggregatorState`) absorbs records even when the
   panel is off.
3. **OrchestratorEvent::MetricsRecorded** — the same record bridged onto the
   orchestrator bus (subscribers like `eval_builder` consume it directly).

Subagent records cross the IPC boundary in `AgentIpcOutput.metrics` (new
field; `default + skip_serializing_if = Vec::is_empty` keeps the payload
byte-compatible). The parent's `SubprocessRunner` re-emits each one on the
global metrics sink so the TUI sees a unified stream.

## How to add a new tool (single source of truth — Phase 7.7)

1. Create the plugin module at `src/adapters/plugins/<name>/mod.rs` (and `tool_defs()` returning a `Vec<ToolDef>`).
2. If it should be available everywhere (in-process planner-side AND Claude Code subagents via the MCP bridge), add ONE registration line to `channel_runtime::register_core_plugins`. That's it. The bridge picks it up automatically.
3. If the tool is workspace-tools opt-in (like `shared_cache`/`persistent_store`/`skill_distill`), add it to the `WORKSPACE_TOOLS` list (Phase 7.7 refactor #5 — TBD).

**Do NOT** add registration lines to both `build_tool_executor` and `build_bridge_executor`. That duplication is the whole reason for `register_core_plugins`. The exceptions (registered outside the helper) are `SkillPlugin` and `McpPlugin` — both have inputs the bridge can't sensibly provide.

## How to add a new agent

1. Drop `agents/<name>.toml` in the workspace root.
2. Restart `tengu chat` — auto-reindex picks up the new agent on first turn.

Schema: `name`, `description`, optional `example_queries`, `engine` (default `"openrouter"`), `model`, `tools`, `skills`, `max_turns`, `timeout_secs`, `sandbox`. See `src/adapters/agents/mod.rs::AgentSpec` for fields.

## How to add a new skill

1. Drop a directory under `skills/<name>/` (or `~/.tengu/skills/<name>/`, or `<workspace>/.tengu/skills/<name>/` — three-tier scanner with shadowing).
2. Add `SKILL.md` with YAML frontmatter (`name`, `description`).
3. Restart `tengu chat` — auto-reindex picks it up.

## Doctrine — keep this when changing code

These are not preferences. They're load-bearing.

1. **LLM = heart, RAG = brain, tools = hands.** The planner LLM has exactly
   one job: emit plan JSON. Tools, memory, grounding all stripped on its turn
   (see `channel_runtime::run_turn_with_system`). Subagents do the actual
   work in their own subprocess with their own tools.

2. **Behaviour changes via TOML and SKILL.md, not Rust.** If a feature
   *can* be expressed by editing `agents/*.toml`, `skills/*/SKILL.md`, or a
   sandbox config, do that. Adding new Rust types or trait methods is a last
   resort. The auto-reindex on first chat turn means TOML edits take effect
   without rebuilds.

3. **Composition over wholesale.** Per-agent scopes override
   `default_scopes` wholesale (NOT field-merged). Per-step `Step.compose`
   overrides the base spec's skills/tools wholesale. This keeps the contract
   simple even if it costs some convenience.

4. **Fail-soft on memory operations, hard on plan-shape errors.** Qdrant
   down → log warn, return empty roster, planner LLM still runs. JSON parse
   error on plan output → hard error, retried up to 3x. The split is
   deliberate: memory degradation should not break a session, but a plan
   you can't parse means the model is broken.

---

## Key gotchas (compiled from SESSION_HANDOFF + scars)

- **`workspace_tools` is a narrow allow-list of FIVE values** —
  `shared_cache`, `persistent_store`, `skill_distill`,
  `apply_improver_proposal`, `manage_skill`. Anything else fails config
  validation. The first four are legacy / single-purpose; `manage_skill`
  is the canonical unified write API (see `plugins/manage_skill/`).
- **`agents/*.toml` has NO `workspace_tools` field** — `AgentSpec`
  (`src/adapters/agents/mod.rs`) defines only `tools`. Workspace-tool
  opt-ins go in `tools = [...]` for subagent specs; `agent_config_from_spec`
  derives the synthesized `workspace_tools` by filtering `tools` against
  `WORKSPACE_TOOLS_ALLOWLIST`. A `workspace_tools = […]` line in an
  agents/*.toml file is silently ignored by serde — confusing because
  the parent's `[agents.*]` block in `sandboxes/<name>/config.toml`
  DOES have a real `workspace_tools` field on `AgentConfig`. If you're
  wiring a subagent (`agents/foo.toml`) to use `skill_distill` /
  `apply_improver_proposal` / `persistent_store` / `shared_cache`,
  put the name in the `tools` array, not `workspace_tools`.
- **`compress_and_store` is appended IMPLICITLY** — never list it in
  `agents/*.toml::tools`. The runner appends it itself for every subagent.
- **Planner LLM call strips tools/memory/grounding** —
  `run_turn_with_system` in `channel_runtime.rs` sets `tools = []`,
  `tool_executor = None`, `memory_manager = None`,
  `suppress_grounding_nudge = true` when `system_override.is_some()`.
- **`engine = "rag"` is the only valid orchestrator engine post-7.1.**
  Anything else logs a warn and disables orchestration for that channel.
  The qdrant feature is effectively required for orchestration.
- **Registry score floor is `~0.15`, not `0.6`** —
  `text-embedding-3-small` against short agent descriptions tops out around
  `0.30–0.40` even for clearly-relevant matches. The SKILL.md asks the
  planner LLM to use its own judgement reading the description; the score
  is a noise floor, not a real gate.
- **Auto-reindex runs on first chat turn** — `RagPlanner::rag()`'s lazy
  `OnceCell` init calls `reindex_all_workspace` from `current_dir()` exactly
  once per chat process. Editing `agents/*.toml` and restarting `tengu chat`
  is sufficient to pick up the change.
- **Workspace fingerprint dedup (Phase 6.2) skips the reindex when nothing
  changed** — `<root>/.tengu/registry-fingerprint`. Set
  `TENGU_REGISTRY_FORCE_REINDEX=1` to bypass for recovery.
- **Sandbox config crosses the IPC boundary (Phase 7.2)** —
  `Config.sandbox_name` (`#[serde(skip)]`) is set by `load_sandbox_or` and
  threaded through `SubprocessRunner` → `AgentIpcInput.sandbox_config` so the
  child re-resolves the same `sandboxes/<name>/config.toml` and inherits the
  parent's scopes/secrets/MCP servers. Without this, `http_request`
  scope-denies in the child even when the parent's sandbox allows it.
- **TUI debug panel is opt-in via `TENGU_TUI_RAG_DEBUG=1`** — when on,
  `OrchestratorEvent::RagQueried` renders as a compact System bubble showing
  top-3 hits per planner call.
- **TUI metrics panel is opt-in via `TENGU_TUI_METRICS=1`** — when on,
  every `OrchestratorEvent::MetricsRecorded` updates the in-process
  `AggregatorState` and a compact one-liner is pushed as a System bubble
  (`metrics: tok in/out 4.5k/812 · last planner (1.2k) · session 5.6k · researcher 4.0k`).
  Aggregator absorbs events even when the panel is OFF — flipping it on
  mid-session shows real numbers, not zero. Tracing baseline
  (`RUST_LOG=tengu=info` → one `metrics` info line per call) is always-on.
- **Per-context-layer metric attribution is approximate** — `MetricsLayer`
  only sees the strings the planner builds (system / roster / cross_session
  / history / recall / failure / user_message). Engine-side framing
  (Anthropic `<thinking>` blocks, OpenAI tool-call schema, function-calling
  message wrappers) isn't counted. Treat layers as relative attribution
  for "which planner block bloated this turn", not absolute byte-perfect
  accounting against `prompt_tokens`.
- **Metrics records cross the IPC boundary via `AgentIpcOutput.metrics`** —
  added 2026-04-28. Old child binaries produce JSON without the field;
  serde's `#[serde(default)]` gives the parent an empty vec. Re-emission
  happens in `SubprocessRunner::run_step` for both the Ok and Failed paths,
  so a partial subagent run still surfaces its consumed tokens.
- **`session_id` is unified between planner and runner (Fix B 2026-05-09)** —
  `channel_runtime::build_orchestrator` resolves the id ONCE
  (env override `TENGU_SESSION_ID` > fresh UUID), passes the same string to
  `RagPlanner::new(...)` AND `SubprocessRunner::new(sandbox_name, session_id)`.
  The IPC payload's `session_id` matches the planner's, so
  `compress_and_store` writes `extra.rag_session_id` = planner's id, and
  `RagPlanner::session_output_recall_block` can filter to it on the next
  turn. Pre-Fix-B these were minted independently (the long-standing open
  issue from the original handoff). `SubprocessRunner::default()` still
  mints a fresh UUID for standalone test / CLI use.
- **Within-session output recall is opt-in via a config knob (Fix A
  2026-05-09)** — `[memory] within_session_output_top_k = N` in
  `sandboxes/<name>/config.toml`. Default 0 = off (back-compat — the
  planner prompt is unchanged). When > 0, `RagPlanner::plan` injects a
  `## Recent step outputs (this session)` block from `tengu_outputs`
  filtered by the planner's `session_id`. Pairs with Fix B — without the
  unified id the filter would never match. Without this knob,
  `tengu_outputs` is only read on `replan()` (the `cross_plan_top_k`
  path), which is why follow-up questions like "was the molecule project
  created?" used to come back with "I have no record of that step".
  Recommended `3–5`.
- **`spec.engine` selects the subagent engine (Phase 7.3)** —
  `agents/<name>.toml::engine` defaults to `"openrouter"` for back-compat;
  set to `"claude_code"` to run that agent through the Claude Code CLI.
  `model` slug format depends on engine: OpenRouter wants
  `anthropic/claude-sonnet-4-6`; Claude Code wants the bare `claude-sonnet-4-6`.
  Building with `--features claude_code` is required.
- **Don't put a Claude Code agent in the planner role.** The Claude Code CLI
  has tool access via MCP at engine-construction time, so the planner-side
  tool stripping (intended for OpenRouter's per-turn `tools = []`) doesn't
  prevent the planner LLM from calling tools instead of emitting plan JSON.
  Result: planner bypasses orchestration. **Use OpenRouter for the planner
  agent and Claude Code for subagents** (mixed-mode is the verified pattern).
- **`parse_verdict` has a Phase 7.4 prose-fallback** — when the planner LLM
  emits non-JSON (Claude Code being conversational), the harness wraps the
  whole text as a `Direct { response }` verdict with a warn log. No more
  `System error: orchestrator initial call failed`. Watch for the warn line
  to know when this is firing.
- **`sandbox_config` crosses the IPC boundary (Phase 7.2)** — already in
  this list above. Worth re-emphasising because it was the root of two
  bugs in this session.
- **Adding a new tool: ONE place (Phase 7.7)** —
  `channel_runtime::register_core_plugins`. Both the in-process executor and
  the MCP bridge call this single helper. Don't add registration lines to
  `build_tool_executor` or `build_bridge_executor` directly. The exceptions
  (registered outside) are `SkillPlugin` (needs a SkillRegistry the bridge
  can't construct) and `McpPlugin` (the bridge would create double-hop
  routing).
- **Adding a workspace-tool opt-in: ONE constant (Phase 7.7)** —
  `channel_runtime::WORKSPACE_TOOLS_ALLOWLIST`. Both `agent_config_from_spec`
  and the bridge filter against it.
- **`compress_and_store` reliability with Claude Code subagents (Fix C
  2026-05-09)** — Claude Code subagents don't reliably call
  `compress_and_store` as their final action; they just stop. The Phase 5c
  middle-ground protocol forgives this — the final assistant text becomes
  the IPC summary. **Fix C** now ALSO writes that final text to
  `tengu_outputs` via a backstop call to `write_summary` on the same path,
  so the durable row exists for within-session recall (Fix A) on the next
  user turn. Watch for `compress_and_store: backstop wrote final_text
  summary to tengu_outputs (Fix C — model skipped the protocol call)` in
  logs. The original `model finished without calling compress_and_store`
  warn is still emitted before the backstop fires. Workaround for the
  underlying behaviour (still useful for clarity): stronger nudge in the
  agent's `identity.instructions`.
- **Long step summaries are char-capped before embedding (Fix D
  2026-05-09)** — `text-embedding-3-small` rejects inputs over 8192
  tokens (~32K chars). `compress_and_store::write_summary` now truncates
  at `MAX_SUMMARY_CHARS = 24_000` (UTF-8-safe) and appends a marker
  before handing to the embedder. Pre-Fix-D, long pipeline summaries
  silently failed to embed and the fail-soft swallowed the error → no
  row in `tengu_outputs`. Watch for the info log: `compress_and_store:
  summary truncated to fit embedder input cap (Fix D)` with `original_chars`
  and `capped_chars`. Tail content beyond the cap is lost; chunked
  multi-row writes are an open follow-up.
- **User message is embedded ONCE per `plan()` turn (Fix E 2026-05-09)** —
  pre-Fix-E the same user message was embedded up to 4 times per turn:
  `search_registry`, `cross_session_recall_block`, `persist_user_message`,
  `session_output_recall_block`. Fix E adds `*_with_vec` variants
  (`RagStore::search_registry_with_vec`, `search_outputs_for_session_with_vec`,
  `search_messages_with_vec`, `store_memory_with_vec`) and threads the
  cached `Option<&[f32]>` through `RagPlanner::plan` → all helpers. Helpers
  fall back to embedding internally when `embed_vec = None` (replan path,
  standalone tests). Direct relief for "embedding API quota exceeded"
  errors. The string-input methods on `RagStore` are unchanged.
- **Qdrant search honours `extra.<key>` payload filters (Fix F
  2026-05-09)** — pre-Fix-F `VectorStore::search`'s `_filter` arg was
  ignored; callers had to over-fetch and post-filter (e.g.
  `search_outputs_for_session` did 5× over-fetch). Fix F adds
  `build_search_filter(&ChunkMetadata) -> Option<Filter>` in
  `vector/qdrant.rs`, which translates structured fields + string-valued
  `extra` entries into `FieldCondition` + `MatchValue::Keyword` conditions
  on the underlying Qdrant call. Most-used target: `extra.rag_session_id`
  for Fix A. Disk-backed `VectorStore` impls still ignore the filter;
  callers keep an in-process post-filter as defence-in-depth.
  `search_outputs_for_session_with_vec` over-fetch dropped from 5× to 2×.
- **Webhook turns persist their final text via Fix H (2026-05-09)** — `webhook_builder::run_one_shot` calls `compress_and_store::write_summary` AFTER `orchestrator.handle()` returns, with synthetic `step_id = "webhook-handler"`. Without this, webhook turns where the planner emits a `Direct { response }` verdict (no subagent dispatched, so no `compress_and_store` and no Fix-C backstop) leave NOTHING recallable — `tengu memory inspect` returns 0 rows even though the agent answered. Watch for `webhook output persisted to tengu_outputs (Fix H)` per turn. Coexists with subagent rows on the same session_id (different step_ids, both findable).
- **`tengu webhooks` is the inbound HTTP listener (2026-05-09)** — feature-gated by `webhooks` cargo feature (`cargo build --features webhooks`); off by default. Each `[webhooks.endpoints.<name>]` block in `sandboxes/<name>/config.toml` binds `/webhooks/<name>` to one agent. Auth is HMAC-SHA256 on `X-Tengu-Signature: sha256=<hex>`; secret resolved from `secret_env` (env var name, preferred) or inline `secret` (TOML literal, dev-only). Response is **always async** — 202 Accepted with `{"session_id": "webhook-<name>-<uuid>"}`. Per-request session_id; recall the run later with `tengu memory inspect --session <id>`. Fresh `build_orchestrator` per request — RagStore lazy-inits each time (~100–500ms gRPC handshake), fine for low rates. The `build_orchestrator` signature was extended in this commit to take an explicit `session_id: String` (caller-resolves). Existing surfaces use the new `channel_runtime::resolve_session_id()` helper to preserve env-or-fresh semantics; webhooks build the per-request value `format!("webhook-{name}-{uuid}")`.
- **`tengu memory inspect` is the diagnostic for "did the write land"
  (2026-05-09)** — `tengu memory inspect --session <id> [--collection
  outputs|messages] [--limit N]`. Server-side scroll filtered by
  `rag_session_id`, prints index, step_id, content snippet, age per row.
  Empty result prints a checklist of likely causes (no compress_and_store
  call AND no Fix-C build, oversize summary AND no Fix-D build, collection
  reset, session_id mismatch). First thing to run when the planner says
  "I have no record of that step" — eliminates the "is the data even
  there?" question without re-running the pipeline.
- **MCP bridge tool names are prefixed `mcp__tengu-tools__<name>`** — when
  Claude Code calls a tengu tool through the bridge, the model sees
  `mcp__tengu-tools__persistent_store`, not bare `persistent_store`. Skills
  that say "check that tool X is in your tool list" should look for both
  forms or just attempt the call and read the error.

---

## Open items still on the list

See `docs/SESSION_HANDOFF.md` for the running list. As of 2026-05-09 (end
of day), the recall pipeline is end-to-end working. Fixes A+B (recall block
+ unified session_id) and C+D+E+F (backstop write, char cap, embed cache,
server-side filter) all landed. Remaining open items are smaller and listed
in the handoff: chunked multi-row writes for oversize summaries (today's
Fix D loses tail content past 24K chars), `cross_plan_top_k` (replan-side)
"this session only" mode reusing Fix F's filter, a real `scroll_filtered`
primitive on `VectorStore` (today `tengu memory inspect` uses a
zero-vector-with-filter shape), and `shared_cache` plugin gaining session
scoping or TTL (currently leaks stale entries across sessions).

---

## What to do when you're stuck

1. **Trace one turn end-to-end before changing anything.** Run
   `cargo run --release --features qdrant -- chat --sandbox aura`, type
   "what is the BTC price?", and follow the logs. The flow is in §1 of
   `docs/architecture-2026-04-27.md`.
2. **Open `docs/architecture-2026-04-27.html` in a browser** — the file map
   tab is searchable. Type the symbol you're looking for; it'll surface
   which subsystem owns it.
3. **`SESSION_HANDOFF.md` "Active gotchas" section** — most weird symptoms
   are documented there with the fix.
4. **If you're about to add a new abstraction layer** — stop and check
   whether the change can be expressed in TOML / SKILL.md / config instead.
   The doctrine prefers data changes over code changes.

---

*Last updated 2026-04-27. If you're reading this in the future and the
companion doc filenames have rolled forward (e.g. `architecture-2026-05-12.md`),
update the references in this file too. Stale references in `CLAUDE.md` are
the worst kind of stale.*
