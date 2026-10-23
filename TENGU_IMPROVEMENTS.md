# Tengu-Cluster — improvement suggestions

> Audit of 2026-10-22 at HEAD `9805470d751bc7fb18b4fa96153c1b095cdb73e6` (branch `feature/xpower-sandbox`). Read-only: no code was changed and cargo was not run.
> Method: 7 area reviewers read the code, 2 adversarial verifiers re-checked every high and medium finding, and the author spot-checked the rest.
> Result: 99 findings, 0 refuted, merged here into 86 rows. Line numbers are as of `9805470d751bc7fb18b4fa96153c1b095cdb73e6`.

## Progress — resume here

Updated 2026-10-23 (resumed from 2026-10-22). Work order (operator, 2026-10-22): bugs → leaks → everything else; § 11 rows wait for the operator.

| Where | Value |
|---|---|
| Worktree | `/Users/vladimirdemidov/development/tengu-wt-audit` (this file's tracked copy lives here) |
| Branch | `fix/audit-bugs`, from `origin/main` `fdfdc2eb1961964b33f154a829550ded52acb279` |
| Last code commit | `b4c5ef85ff8cbaff93692d4c81236593ddd2bf1c` (D6) · **not pushed, no PR** |
| Never | touch the main checkout: it runs `tengu run --sandbox xpower` + 2 `a2a serve` on `feature/xpower-sandbox` |
| Build env | `CARGO_TARGET_DIR=~/.cache/tengu-xm.noindex/audit-target`, `CARGO_BUILD_JOBS=6`, `nice`; scoped tests only |

| Row | State | Commit |
|---|---|---|
| A1 | done | `c4164852c3bce8128eecb1c4d5a1cf6828482c4e` |
| A2 | done | `6ac73cf50865e208deba78019570e6e145884986` |
| A3, A4 | done | `5da9fd291dded2c09b624ce47364b1b052641f51` |
| A5 | partial: shared stderr drain + Claude Code failed exit = `Error`; `cli_run::drive` (one run loop, keepalive for both) left | `327fcd89292f93933f2d802af7ade4c9495ec1fc` |
| W2 | done | `d237e4afc970c14535bbd5062e0829148bdc9da6` |
| W3 | done but `--older-than <days>` (deferred) | `3b246d771b0ce02803e613184691fded4a5a7cc3` |
| R1, B7 | done (also A10's "default build" half: `webhooks` is a default feature) | `d206882fdc1e1b4c6f7bb1415158693c63780f8c` |
| S1 | done | `3f4b80bd9bd250be4fa275db0eed6ce4da920417` |
| S2 | done | `f987c1a27505735be9968c7c776ec64655fe74f6` |
| S3 | done | `69e767e11ba7fcc0da2461fbf8a19548fe9dbe9a` |
| S9 | done | `1786b9147c702424bfe61b21ee039efc87862771` |
| S5 | done | `a240e48d8efb49ad61a63aa1c08c096a0f645df0` |
| S12 | done | `493aff525c031fec0f7290b7203da32ec0035b0c` |
| S13 | done | `b4a2441aab8dddee8155ba3bde1c74e11ac145bc` |
| R3 | done | `b5322abf867a24f5de610753182f7c7844c4d2eb` |
| T1 | done: registry 23.4 KB → ~2 KB per planner call | `8716a1b2d77f1a1b31b5b425ddec6db60343a8b2` |
| S13 follow-up | `core::net` in domain (the layering lint) | `731f8578083efef87515c951b4bf016f6aec95e2` |
| Y3 | done: `outbound/sqlite.rs` (`open_wal`, `owner_only`, `SharedConn`) in 8 stores | `d1523bcdda8a3754fe59e2cba47ed318c9a4bfa4` |
| Y4 | done: `outbound/fs_atomic.rs` (vault, builder, heartbeat, vector store) | `ff91596bee7ec0a2a180aabc52a6de05ee5d6066` |
| Y10 | done: `domain::runtime::valid_name`, `domain::secrets::map_strings` | `fe1e080dc875c94021c512fb6a17f7d939ec8367` |
| O6 | partial: ignore half; the `docs/plans/` move waits for wave 2 | `3e0ec9cff0654e2a06b60d9cf023e87343642fd0` |
| S8 | done | `a4d7ed4ca7b97bb9d406741026de56723e4ba8e2` |
| test | lease-loss test retries the steal (flaky under load) | `e117c22fce30884122b480b9a440caddf49cf32c` |
| S4 | done: doctrine-compatible variant (an added tool needs a configured scope) | `85a01bab7305794ca0873b7646c416d5ab2783d4` |
| S6 | done: `domain/untrusted.rs` — one fence at the LLM sinks (tool results, recall, webhook bodies, step inputs, errors, loop escalations) | `78808a059f3cacd34c90013f2ee8395bff88b58e` |
| S7 | done: CLI engines get `<turn role=…>` tags, stripped from every body | `5a5a378dcce47db9467fc18ab21d45cdb01ccd88` |
| S10 | done: `config/readers.rs` load warning (reader + side effect) + the `[orchestrator]` claude_code profile `none` error; pinned: lping/lping, lping/crypto_researcher, storage-test/storage (unlimited quiet since S1) | `938f7361594d87f0453e55f7db7a91b1734f55ad` |
| S11 | done: `tool_approvals` / `approve_only` removed, `[telegram]` `deny_unknown_fields` | `84ea7c110d8e42db21c30a361edddd74f83c88ce` |
| S14 | done: explicit `tools` in example `main`, tor-check `probe`, a2a-lab `planner` | `34fe62b320fcf0d1a51f76806057c0a69adb067e` |
| S15 | done: `manage_skill` scans before writing (`scanner::scan_file`), `serde_yaml` frontmatter | `bd9e95cbf0114db928fd776c046378f6e31f25c7` |
| S16 | done: `docs/tools.md` § Untrusted input map; venture-scan's 2 identity sentences removed (xpower's 2 are on its branch) | `3954018056c492563230aaa1aa0753afc7441727` |
| T2 | done: planner prompt 11.2 KB → 2.8 KB | `7d4c3e85e981cc63618e4070c97d6d948f30edba` |
| T4 | done: each step input cut at the receiver's tool-result cap | `d1cabf6c7f645ddebde723a23983a9b8bce303f2` |
| T5 | done: the plan reaches a step once, as bullets; a replan gets compact JSON | `47771639353feecf6140a0f5448f4ce835ab1a0c` |
| T7 | done: whole-prompt input tokens, `cached_input_tokens`, subagent per-role layers | `ee2d13e20fa569c6ac51c3d3aed914f9e825d4db` |
| T8 | waits: depends on Y7 (§ 11) | — |
| T9 | done: no `# Tools` list in the chat system prompt | `1c31aad557b174aaa58ce0769d87c341ca39a3d1` |
| D3 | done: the tutorial-sync hook names the pages, once per file per session | `87e010a8c6b8eddbdec4f2790f357c09db564dd3` |
| D5 | done: ~90 history comments in present tense (29 files) | `dc7920609d817458d7769efd35165d0ee0cd7767` |
| D6 | done: 21 test modules ≥ 500 lines → sibling files; the layering lint + code map treat test-only files as test code | `b4c5ef85ff8cbaff93692d4c81236593ddd2bf1c` |

| Next | Rows, in order |
|---|---|
| 1 · leaks | done |
| 2 · everything else (current) | A5 (rest), A6, A7, A8, A9, A10 (rest), A11, A12, R2, R4, R5, R6, R7, R8, Y1, Y2, Y5, Y6, Y8, Y9, B1, B2, B3, B4, B5, B6, B8, B9, B10, W1, W4, O1, O2, O3, O4, O5, O7, O8, O9, O10, O11 |
| Waits for the operator (§ 11) | D1, D2, T3, Y7, T6, D4, S4 narrow-only everywhere, S14 "empty = read-only set" |
| Finish | `docs/SESSION_HANDOFF.md`; push + one PR; merge on green CI; `handoff_for_check.md` — note `sandboxes/xpower` (not on this branch) changes on merge: S3 (`research` loses page fetches) and S13 (`*` no longer reaches local hosts), S10 (its readers checked against the side-effect rule), S16 (its 2 "web pages are untrusted" sentences to delete) |

Every commit follows the repo rules: tutorial pages + `sources.toml` (`cargo test --test tutorial_map`), code map regen (`TENGU_REGEN_CODE_MAP=1 cargo test --test code_map`), `CLAUDE.md` = `AGENTS.md`, `cargo fmt --all --check`.

---

| Legend | Meaning |
|---|---|
| Effort | **S** ≤ ½ day · **M** 1–3 days · **L** a week or more |
| ✅ | confirmed by an independent verifier |
| ✏️ | verifier corrected a fact or the fix; the row already carries the correction |
| 🔍 | spot-checked by the author (docs rows the verifiers did not reach) |
| ○ | low impact, not independently verified |
| ⚖️ | bends a written rule: the operator decides (list in § 11) |

| What the cleanup buys (reviewer estimates) | Size |
|---|---|
| Fixed input per planner call | ~8.5k tokens → ~1.5k tokens |
| Always-loaded developer context (CLAUDE.md + AGENTS.md twin) | 97 KB → ≤ 25 KB per file, one file |
| Production Rust removed by the merges and deletions below | ~3,500 lines, before the dead-code sweep (§ 6 R4) |
| Docs moved to an archive | 42 files, 2.07 MB; top-level `docs/` 86 → ~30 tracked files |
| `tengu.log` size | −89% (it is mostly a copy of `egress.jsonl`) |

---

## 0. Do these first

| # | Change | Why | Effort | Row |
|---|---|---|---|---|
| 1 | Permissive fallback scope: `env_reads = []` instead of `"*"` | Any unscoped tool can put any env secret (vault, `.env`, seal session) into a URL for any host | S | S1 |
| 2 | `http_request` expands `$VAR` only toward a named (non-`*`) host | lping sends `$SOLANA_RPC_URL` to `*` from webhook-driven agents | S | S2 |
| 3 | Claude Code `research` = WebSearch only, plus the isolation flags on every profile | Web-reading scouts can `Read` the disk and load the operator's hooks and plugins | S | S3 |
| 4 | Memory recall: fenced, capped at `max_recall_tokens`, never `Role::System` | Stored web text comes back with system authority; the fence can be escaped | S | S5 |
| 5 | Count cached input tokens in metrics | Without it no token saving below can be measured | M | T7 |
| 6 | Planner registry: agents only (23.4 KB → ~2 KB) | 81% of every planner prompt is a tool list the planner cannot use | S | T1 |
| 7 | Trim `skills/orchestrator/SKILL.md` (10.6 KB → ~4 KB) | 2.9 KB route to an agent no sandbox defines | S | T2 |
| 8 | `/stop` aborts the turn future | Today running plan steps keep going for up to 30 min | S | A1 |
| 9 | Run the existing `Plan::validate` before any step | A bad agent name costs 3 retries and sleeps; a cycle runs every other step first | S | A2 |
| 10 | Planner calls its engine directly; delete the dead port chain | ~600 lines; also fixes webhook and eval planner turns that silently drop the planner prompt | M | A3 |
| 11 | Mirror only denied / error egress lines into `tengu.log` | 66 MB of the 74 MB log is a copy of `egress.jsonl` | S | B1 |
| 12 | Trace Claude Code / Codex step tool calls | The step you debug is the one with no tool events | S | B3 |
| 13 | One canonical `AGENTS.md` ≤ 25 KB; `CLAUDE.md` = `@AGENTS.md` | ~18–21k tokens saved per session, one file to keep current | M ⚖️ | D1 |
| 14 | Gitignore the six private docs and `handoff_review.md` | One `git add docs` publishes private figures | S | O6 |
| 15 | Delete the feature flags that gate no crate | A default build cannot run the engine 13 of 15 sandboxes use | S | R1 |

**Two written-rule violations found in shipped code and docs (Model spend, 2026-10-14):**

| Where | What | Fix |
|---|---|---|
| `src/adapters/outbound/tools/agentic_memory/mod.rs:1242` | The wiki compiler defaults to `anthropic/claude-sonnet-4-6` over `OPENROUTER_API_KEY` | Fail unless `TENGU_WIKI_COMPILER_MODEL` is set (S8) |
| `docs/validation-checklist.md:5`, `docs/orchestration-test-scenarios.md:14` | Runbooks tell an agent to export `OPENROUTER_API_KEY` and quote OpenRouter dollar costs | Archive them (O2) |

---

## 1. Prompt injection and exfiltration

**Enforced in code today (keep):** planner turn has no tools · plan steps run only routable agents · Jev picks from typed menus · Solana writes default to `simulate` · SOE closed tool world with the fixpoint `<source-text>` fence · A2A host pinning · per-hop egress checks · Telegram allow-list · hardened-sandbox rules. **Prompt-only today:** "web pages are untrusted" (4 identity copies), compose confirmation, every other fence.

| ID | Problem | Evidence | Fix | Effort | ✓ |
|---|---|---|---|---|---|
| S1 | A tool with no configured scope gets `net_hosts`, `env_reads` and `shell_bins` = `"*"` plus the default wallet. `http_request` expands `$VAR` in URL and headers. A fetched page can therefore ship any env secret to any host. The outgoing request is not redacted. | `src/bootstrap/tools.rs:332` `permissive_scope`; `src/adapters/outbound/tools/http/request.rs:569` `expand_env_refs`; `src/domain/scope.rs:291`; `sandboxes/unlimited/config.toml:38` has no scopes | Fallback `env_reads = []`; under `no_shell` (hardened) the fallback is deny-all. First make `tengu doctor` list every (agent, tool) on the fallback. | S | ✅🔍 |
| S2 | A configured secret also expands toward `*`. | `sandboxes/lping/config.toml:64-67` (`net_hosts = ["*"]`, `env_reads = ["SOLANA_RPC_URL"]`); expansion at `request.rs:145` runs before the host gate | Expand a `$VAR` only when the host matches a non-`*` pattern. No new config field. | S | ✅ |
| S3 | Profile `research` = Read, Glob, Grep, WebSearch with `--dangerously-skip-permissions`, next to `http_request "*"`. Read is not workspace-bound. The isolation flags (`--setting-sources ""`, no hooks, no CLAUDE.md, no auto-memory) apply only to profile `none`. | `src/adapters/outbound/engines/claude_code.rs:76`, `:275`, `:289-300`; venture-scan and xpower scouts; `sandboxes/xpower/config.toml:631` records a Read reaching the state dir | `research` = `WebSearch` only. Apply the isolation flags to every profile and delete the branch. | S | ✅ |
| S4 | Outside hardened sandboxes `compose` replaces tools wholesale; `compose.tools = []` = every base tool on the permissive scope. The planner reads raw webhook payloads and failed partial output, so injected text can mint a shell. | `src/bootstrap/tools.rs:465-467`; `src/adapters/inbound/webhooks.rs:677-680`; `src/adapters/outbound/subprocess_runner.rs:481` | Doctrine-compatible: a widening compose may add only tools that have a configured scope for the base agent, or refuse widening on webhook and replan turns. Narrow-only everywhere is ⚖️ (doctrine 3). | S | ✏️⚖️ |
| S5 | In-process chat injects `[Relevant memories]` raw, uncapped, as `Role::System`. `max_recall_tokens` is never read. The `<memory-context>` sanitizer is single-pass, so a nested closing tag escapes the fence. | `src/application/chat/service.rs:362-368`, `:409-415`; `src/application/memory/fencing.rs:28-44`; second caller `src/adapters/outbound/memory/builtin.rs:130` | Use `build_memory_context_block` + `truncate_to_token_budget(max_recall_tokens)` in a user-role message. Loop the cleaner to a fixpoint, keeping `\n` and `\t`. | S | ✅ |
| S6 | Three fence formats, ~11 unfenced entry points (http bodies, A2A renders, MCP results, webhook payloads, 3 planner recall blocks, replan failure block, loop escalation) and 3 tool-result sinks. | `src/domain/source/packet.rs:873-904`; `src/application/orchestrator/executor.rs:266-279` (`<step-input>` unescaped); sinks `src/application/chat/tool_loop.rs:211`, `src/adapters/inbound/cli/run_agent.rs:581`, `src/adapters/inbound/mcp_bridge.rs:592` | One `domain/untrusted.rs::fence(origin, text)` with a newline-preserving cleaner (`clean_untrusted` flattens newlines, so it cannot be reused as is). The bridge calls `tool_result_content`. This is a label, not enforcement. | M | ✏️ |
| S7 | CLI engines get history as one string with `User:` / `Assistant:` labels; any body line `User: …` forges a turn. 32 of 34 agents run on CLI engines. | `src/adapters/outbound/engines/cli_run.rs:38-57` `format_prompt` | Tagged turns (`<turn role="user">`), tags stripped inside bodies. Matters most for in-process CLI chat and recalled memory. | S | ✏️ |
| S8 | Open Brain rows have no provenance. The model sets `kind`, `role`, `session_id`, `agent`. Cross-plan recall is on by default with no sandbox filter. "Reviewed" wiki reads `pending` rows. | `src/adapters/outbound/tools/agentic_memory/mod.rs:292-305`, `:545-556` (no sandbox column), `:959-975`, `:1242-1251`; `src/config/mod.rs:1189` | Closed kind list for agents; session and agent from env only; a sandbox column on every recall lane; wiki compiles `approved` rows only; no default compiler model. | M | ✅ |
| S9 | A claude_code agent without a `[claude_code]` block gets `editor_shell` (Bash outside tengu scopes). storage-test's Telegram ingester is one. | `src/config/mod.rs:1106`; `src/adapters/outbound/engines/mod.rs:131`; `sandboxes/storage-test/config.toml:42-53` | Default `none`. Delete the hardened "missing block" special case. | S | ✅ |
| S10 | No load rule separates readers of untrusted text from side-effect tools. | `src/config/soe.rs` + `src/domain/tools.rs:198` `SIDE_EFFECT_TOOLS` (SOE only) | One `Config::load` check (warn, then error): a reader agent (research, web search, `http_request "*"`, `a2a`, MCP) holds no side-effect tool or env read. Plus: an `[orchestrator]` claude_code agent must use profile `none`. Flags lping/lping, lping/crypto_researcher, storage-test/storage, unlimited. | M | ✅ |
| S11 | No tool call waits for approval anywhere. `[telegram] tool_approvals` / `approve_only` only warn. | `src/config/mod.rs:762-790`; set in `sandboxes/unlimited/config.toml:16` and `sandboxes/storage-test/config.toml:25` | Now: delete both keys from the code and both sandboxes. Later: one approval decorator in the single executor constructor (A6) for `SIDE_EFFECT_TOOLS` and non-GET `http_request`. | M | ✏️ |
| S12 | `tengu a2a serve` skips `registry_after_keys`, so the seal session token is not in its redaction set. | `src/adapters/inbound/cli/mod.rs:781-783` vs the 6 calls at `:738-808` | One `sandbox_ctx()` that always extends the registry, used by every command. | S | ✅ |
| S13 | `net_hosts "*"` matches loopback, LAN and link-local (Studio, A2A servers, Ollama, the operator's PC). | `src/domain/scope.rs:46-48` | In `check_net_host`, `*` excludes private ranges; name them explicitly. | S | ○ |
| S14 | No `tools` line = every default tool incl. `run_command` and `sign_*`. | `src/domain/tools.rs:109-125`; `config.example.toml` `main`, tor-check `probe`, a2a-lab `planner` | Now: explicit `tools = [...]` in those 3 blocks. Later: empty = read-only set ⚖️. | S | ○ |
| S15 | `manage_skill` writes live skills unscanned; `description` is `format!`-ed into YAML. No sandbox grants it today. | `src/adapters/outbound/tools/manage_skill/mod.rs:189-224`, `:268-291`; scanner only in `src/adapters/inbound/cli/skill.rs` | `serde_yaml` front matter + `scan_skill` before the rename. | S | ○ |
| S16 | No single map of which inputs are fenced, enforced or prompt-only. | `docs/soe-2026-10-08.md:95` only | One table in `docs/tools.md` § Untrusted input; delete the 4 identity sentences after S6. | S | ○ |

---

## 2. Tokens: runtime LLM calls

Measured fixed input per planner call: ~34 KB (≈ 8.5k tokens) = 10.6 KB `SKILL.md` + 23.4 KB registry. No engine handles provider cache tokens yet. The planner prompt order (stable system, registry, then variable recall) is already cache-friendly: keep it.

| ID | Problem | Evidence | Fix | Saves | Effort | ✓ |
|---|---|---|---|---|---|---|
| T1 | Registry sends all 51 catalog tools and all 11 skills (with absolute home paths) whatever the agents hold: Tools 18.9 KB, Skills 3.6 KB, Agents 0.6 KB. | `TENGU_PLANNER_REGISTRY.md`; `src/adapters/outbound/tools/mod.rs:433-444`; `src/application/orchestrator/shared_files.rs:180-241` | Render `## Agents` only (each with its own tools and skills by name). Keep one names-only line where compose widening must stay possible (`SKILL.md:131`). Drop `_(source: …)_` paths. | ~5.3k tokens per plan, ×3 with replans | S | ✅🔍 |
| T2 | Planner system prompt: 2.9 KB "Lifecycle verbs" for `learning-agent` (0 sandboxes), 2.2 KB repeated "JSON only", a pointer to `plan_schema.json` the planner never sees, human notes. | `skills/orchestrator/SKILL.md` (10,839 B) | Edit `SKILL.md` only: two JSON shapes, exact-name rule, `depends_on`, a ~600 B compose rule. Move lifecycle verbs to `skills/skill-creator/SKILL.md`. | ~1.65k tokens per call | S | ✅ |
| T3 | Subagents write the answer twice (text + `compress_and_store` summary). The suffix threatens a failure the harness never applies. run-agent builds its own prompt with uncapped skill bodies. | `src/adapters/inbound/cli/run_agent.rs:756-767`, `:683`, `:244-262` | Now: one honest suffix ("put the full result in `summary`, no other final text"). Then use the budgeted `rebuild_system_prompt` (pass `advertise = false`). Later: drop `compress_and_store` entirely ⚖️. | up to ~50% of final-answer output | S/M | ✅✏️ |
| T4 | Each dependency's full output is pasted into the next goal; the final reply joins all leaves. No cap. | `src/application/orchestrator/executor.rs:266-280`; `src/adapters/outbound/subprocess_runner.rs:407-410` | Cap each `<step-input>` at the receiver's tool-result cap, with a footer naming the step id. | bounds fan-in; protects `local` agents | S | ✅ |
| T5 | Plan rendered twice (bullets + pretty JSON) in every subagent system prompt, resent every round; replan sends pretty JSON. | `src/application/orchestrator/shared_files.rs:135-166`; `src/application/orchestrator/planner.rs:675` | Bullets only; compact JSON in replan. | ~50% of the plan block | S | ✏️ |
| T6 | Tool-result cap is 300,000 chars in-process but 50,000 over the bridge (3 hard-coded copies). Breaks "same under every engine". | `src/config/mod.rs:684-698`; `claude_code.rs:841`, `codex.rs:753`, `mcp_bridge.rs:637` | One `max_tool_result_chars`, default 50,000; delete `max_mcp_result_chars`. W1 agents keep the old value unless a new generation is cut. | worst case ~75k → ~12.5k tokens | S | ✏️ |
| T7 | `claude_code` metrics drop `cache_read` / `cache_creation` tokens; codex only logs cached input; subagent records have no layers. | `src/adapters/outbound/engines/claude_code.rs:445-456`, `:637-650`; `src/adapters/outbound/engines/codex.rs:515-530`; `src/adapters/inbound/cli/run_agent.rs:503` | Add `cached_input_tokens` (serde default) to `StreamEvent::Usage` and `MetricsRecord`; `prompt_tokens` = full input on both engines; fill subagent layers. | makes T1–T6 measurable | M | ✅ |
| T8 | EVM crypto tools are default (5 schemas, ~450 tokens) on every agent without a `tools` line; no sandbox grants a wallet. | `src/adapters/outbound/tools/mod.rs:144-149`; `src/domain/tools.rs:109-125` | See Y7. | ~450 tokens per such agent per turn | S | ✏️ |
| T9 | In-process chat repeats a `# Tools` list the API already receives as tool defs. | `src/application/skills/registry.rs:1336-1356` | Delete step 6 and the `tools` parameter. | ~30 B per tool per round | S | ○ |

## 3. Tokens: developer-agent context

| ID | Problem | Evidence | Fix | Saves | Effort | ✓ |
|---|---|---|---|---|---|---|
| D1 | `CLAUDE.md` and `AGENTS.md` are 97 KB twins (5 diff hunks) loaded every session: gotchas 51 KB, a 7.7 KB changelog footer, an "open items" table repeated in 4–5 files, and a REQUIRED-reading list of 36 files / 2.0 MB. | `wc -c CLAUDE.md AGENTS.md`; 32 of 35 commits touch both | `AGENTS.md` canonical, ≤ 25 KB (what it is, layers, doctrine, hard rules, output style, recipes, a 10-line "never do"); `CLAUDE.md` = `@AGENTS.md`. Gotchas → `docs/gotchas.md` plus `.claude/rules/<topic>.md` with `paths:` globs. Delete the footer. Five-step reading order (O7). | ~18–21k tokens per session | M | ✅⚖️ |
| D2 | The 30-row REQUIRED-updates table is prose; ~5 rows are test-backed. | `docs/tutorial/sources.toml` already maps 35 pages to 377 paths | Move it to `docs/sources.toml`, add living md docs, make the existing hook print the exact docs for the edited file. Update `tests/tutorial_map.rs:37` and the hook path. | 8.3 KB per twin; exact list per edit | M | 🔍 |
| D3 | The PostToolUse hook prints a 330-char reminder on every `src/*.rs` edit. | `.claude/settings.json` | Once per file per session, with the exact page list (D2). | repeated reminders | S | ○ |
| D4 | The operator's auto-memory `MEMORY.md` (20 KB) carries ~8.5 KB about code that is gone or moved. | `/Users/vladimirdemidov/.claude/projects/-Users-vladimirdemidov-development-tengu-cluster/memory/MEMORY.md` | Delete the dead sections; replace the live-but-stale ones (Shared Cache, Tool Gating) with a pointer to `docs/code-map.md`. Operator's file, operator's edit. | ~2.1k tokens per session | S | ✏️ |
| D5 | 503 history-comment lines ("Phase 7.4", "Fix B (2026-05-09)", "Pre-7.6") in 206 files; comments are 19% of `src` bytes; 252 comment lines exceed 300 chars. | `rg` counts; e.g. `src/application/orchestrator/planner.rs` 25 lines | One sweep per subsystem: keep the rule in present tense, delete the history (git holds it). | ~100 KB less to read in the heaviest modules | M | ○ |
| D6 | ~108.5k of 277k `src` lines are inline tests; 16 files are mostly tests. | e.g. `src/application/decision_loop/mod.rs` 1,085 prod of 2,936 | Move `mod tests` to sibling `tests.rs` files (pattern exists: `src/application/soe/tests.rs`). Pinned W1 tool defs stay byte-identical. | reading those files −60% | M | ✏️ |

---

## 4. Agents and loops

Loop map (all checked): chat tool loop (rounds, forced final turn) · run-agent loop (own copy, no final turn) · CLI engines (tool-call cap, idle timeout) · plan retry `[1, 3, 9] s` + replan · decision loop · runtime feeds (`domain::backoff`) · A2A poll · SOE cycle. **No new bounded-loop abstraction is needed:** the duplication is in two loops and one retry policy.

| ID | Problem | Evidence | Fix | Effort | ✓ |
|---|---|---|---|---|---|
| A1 | `/stop` never stops a running plan: `Orchestrator::cancel` has no caller and the executor never races the flag against a running step. | `src/application/orchestrator/mod.rs:106`; `src/application/orchestrator/executor.rs:116`, `:186`; `src/adapters/inbound/telegram.rs:985-987` | Spawn `handle()`, keep the `JoinHandle`, `.abort()` on `/stop` (drop already aborts steps). Delete `cancel_flag` and its plumbing. | S | ✅🔍 |
| A2 | `Plan::validate` exists, is dead, and is never called. Unknown agents cost 3 retries; cycles surface after every other step ran. | `src/domain/plan.rs:66-69`; `src/adapters/outbound/subprocess_runner.rs:370-405`; `src/application/orchestrator/retry.rs:20-29` | Call it after each plan verdict, resolving `compose.base_agent` first; on error go straight to replan. Drop `MultipleLeaves`. Keep the runner's agent lookup (it supplies limits). | S | ✏️ |
| A3 | The planner's one tool-less call passes through `OrchestratorChatPort`, 4 `ChatServiceFactory` impls and a snapshot map. Webhook and eval factories only implement `run_turn`, so their planner turns **drop `SKILL.md` and report zero telemetry**. Telegram builds a tool executor per agent before every message. `MemoryProvider` has one production impl. | `src/application/orchestrator/planner.rs:584`, `:705`; `src/ports/orchestration.rs:146-170`; `src/application/orchestrator/wiring.rs`; `src/application/memory/{injector,writer}.rs`; `src/adapters/inbound/telegram.rs:1555-1620` | Planner holds `Arc<dyn Engine>` + limits and calls `collect_engine_response` with no tools. Delete `wiring.rs`, the port, `injector.rs`, `writer.rs`, `OrchestratorSnapshots`, `Orchestrator.memory`; `MemoryManager` holds `Option<BuiltinMemory>`. ~600 lines; "no tools for the planner" becomes structural. | M | ✅ |
| A4 | run-agent keeps its own tool loop that drifted: no final turn at the cap (step fails, retries 3×70 rounds), provider call ids reach `ToolCtx.call_id` (the exec tools' idempotency key), different error prefix. | `src/adapters/inbound/cli/run_agent.rs:459-640`, `:555`, `:567`; `src/application/chat/tool_loop.rs:194`, `:229-233` | Add an intercept hook and a round hook to `collect_engine_response`; a `LoopOpts::from_limits` struct replaces 12 positional args; `step:<session>:<step>:<round>:<i>` call ids. Delete the dead `token_budget` parameter (None at all 7 sites). | M | ✏️ |
| A5 | `claude_code` and `codex` each carry a ~280-line copy of one CLI run loop. Only Codex drains stderr (Claude Code can stall until the idle kill), sends keepalives and maps a failed exit to `Error`. | `src/adapters/outbound/engines/claude_code.rs:779-1050`, `:809`, `:1021-1027`; `src/adapters/outbound/engines/codex.rs:676-964` | `cli_run::drive(child, parser, limits)`; each engine keeps its args, env policy and line parser. ~250 lines. | M | ✅ |
| A6 | The one-shot agent turn is hand-assembled in 4–5 places; `SanitizedToolExecutor` is wrapped by hand at 16 sites; `build_tool_executor` takes 10 args and returns the raw executor. | `src/adapters/inbound/webhooks.rs:1098-1240`; `src/bootstrap/a2a.rs:263-385`; `src/adapters/inbound/eval.rs:1867-2042`; `src/bootstrap/tools.rs:96` | Move `bootstrap::a2a::agent_turn` to `bootstrap/turn.rs::run_once` (options: system override, history, executor wrap) and use it everywhere. `build_tool_executor` returns a sanitized `Arc<dyn ToolExecutor>`: redaction by construction, and the home of S11's gate. ~450 lines. | M | ✅ |
| A7 | `AgentIpcInput` has 4 always-empty fields and back-compat for "old parents" that cannot exist (the child is `current_exe()`); the plan travels through a global `ACTIVE_PLANS` map and a `TENGU_PLAN.md` fallback. | `src/adapters/outbound/subprocess_runner.rs:246-249`, `:419-441`; `src/application/orchestrator/shared_files.rs:28-62` | `run_step(step, inputs, plan)`; delete the map, the guard, the file and the 4 fields. ~150 lines. | S | ✅ |
| A8 | Plan-step retry hand-rolls `[1, 3, 9] s` for every error; two drain loops are copies. | `src/application/orchestrator/retry.rs:13-70`; `src/application/runtime/loops.rs:190-226`, `:462-491` | Classify step errors (fatal / timeout / transient) and use `domain::backoff::next_delay`; one `drain_until` helper. ~60 lines. | S | ✅ |
| A9 | Knobs nothing reads: `lens` + `/eco` `/standard` `/precise`, `max_cost_per_flow`, `warn_at_cost` (docs promise cost caps that do not exist), `max_recall_tokens`; IPC `default_max_turns` 20 vs config 70. | `src/config/mod.rs:681-683`; `src/application/chat/service.rs:63-102`, `:236-237`; `src/adapters/inbound/tui/mod.rs:224-225` | Delete them (also the TUI header and the `Lens` enum); edit `sandboxes/unlimited/config.toml` and `config.example.toml` in the same commit. | S | ✏️ |
| A10 | Decision-loop escalation lives in the webhooks adapter behind a non-default feature; a default build stops with "escalation unavailable"; the answer only reaches a log line. | `src/adapters/inbound/run.rs:61-70`; `src/adapters/inbound/webhooks.rs:373-389`, `:780-797` | Move the escalator to `bootstrap/orchestrator.rs`; show the answer in Studio from `plan.completed`, or default `escalate = false`. | S | ○ |
| A11 | CLI-engine steps build a full tool executor they never call (the bridge builds another). 30 of 34 agents are `claude_code`, so an in-process runner would help only 2 agents. | `src/adapters/inbound/cli/run_agent.rs:345-365`; `src/adapters/inbound/mcp_bridge.rs:840` | Skip the executor when `bridges_tools(engine)`. Drop the in-process-runner idea. | S | ○ |
| A12 | `tengu webhooks` re-implements the runtime lifecycle `tengu run` already owns (leases, trace, drain); `tengu run` already mounts the router. | `src/adapters/inbound/webhooks.rs:132-336`; `src/adapters/inbound/run.rs:73-74` | Make it an alias of `tengu run` with no feeds or loops; delete ~200 lines; update the 3 deploy files. | M | ✅ |

---

## 5. Architecture

The core is clean: `application/chat`, `application/orchestrator` and `engines` import no vertical, the layering lint has zero exceptions, and production `domain` uses no tokio / reqwest / rusqlite. The weight sits in verticals (~88% of production domain code) and in dead layers.

| ID | Problem | Evidence | Fix | Effort | ✓ |
|---|---|---|---|---|---|
| R1 | Features that gate no crate: `openrouter` (0 sites), `claude_code` (off by default, used by 13/15 sandboxes), `codex`, `webhooks` (hmac already comes via `tengu-seal`). 46 cfg gates, 28 feature-driven dead-code allows. | `Cargo.toml` `[features]`; `src/adapters/outbound/engines/mod.rs:61`, `:177`; `.github/workflows/quality.yml:49` | Delete all four; keep `telegram` and `postgres_memory` (real crates); optionally merge `studio` + `a2a` into `server`. One CI leg goes; the docs lose ~33 "build with --features claude_code" lines. | S | ✏️ |
| R2 | Dead config: `[hub]` (nothing listens; the example suggests an authenticated hub), `runtime_profile` (a full `sysinfo` scan on every CLI start, incl. each `tengu tool call`), `[orchestrator] engine` (only `"rag"` is legal), `build_planner_engine` (0 callers), `tengu status` (a subset of doctor). | `src/config/mod.rs:44-133`, `:381-440`, `:1533`; `src/adapters/inbound/cli/mod.rs:733` | Delete; remove the keys from every sandbox and example in the same commit (`deny_unknown_fields`). `runtime_profile` is not W1-pinned; still run `tengu lineage verify --pins`. ~200 lines. | S | ✅ |
| R3 | RAG vestiges: `RagPlanner` (33 refs / 16 files), `RagQueried` with constant score 1.0, the TUI RAG panel, `emit_rag_query` logging the **full user message at INFO** every planner turn, the never-sent `StepProgress`. | `src/application/orchestrator/planner.rs:270-274`, `:807-850`; `src/adapters/inbound/eval.rs:2197` (a consumer) | Rename to `Planner`; delete the hits, event, panel, env var and `StepProgress`; update the eval trace consumer. ~150 lines. | S | ✏️ |
| R4 | 44 module-wide `#![allow(dead_code)]` (2 combined with a clippy lint) hide ~19.5k production lines; most cite waves that shipped. | `rg '^#!\[allow\(dead_code'`; `src/domain/lp/hedge.rs:22`, `src/domain/lp/gates.rs:27`; `src/ports/memory.rs:11-17` | Remove one module per commit and delete what rustc flags (not `domain/soe/portfolio.rs`: O7 has not shipped). A shrink-only allowlist in `tests/scope_lint.rs`. Size unknown until cargo runs. | M | ✏️ |
| R5 | The layering lint checks IO only in `domain`. ~20 application files do fs IO; a skill-lifecycle metric spawns `sh` with the inherited env, bypassing egress; the planner reads a cwd-relative `SKILL.md`; 223 direct `now_ms()` calls bypass `Clock`. | `tests/layering_lint.rs:88-98`; `src/application/skills/lifecycle/metric_kinds/script.rs:31-37`; `src/application/orchestrator/planner.rs:475`; `src/domain/observation.rs:432` | An `APPLICATION_IO` list with shrink-only exceptions; `script.rs` through `ShellExecutionPort`; pass the planner prompt in from bootstrap. | M | ✏️ |
| R6 | Only 6 files exceed 1,500 production lines; 4 mix concerns. | `src/adapters/inbound/eval.rs` 2,446 · `src/adapters/inbound/telegram.rs` 2,274 · `src/config/mod.rs` 1,932 · `src/domain/lp/snapshot.rs` 1,980 | Split those 4 (eval logic → `application/skills/lifecycle`; telegram session / commands / state; config agent / channels / memory / load; snapshot by its 5 schemas). Leave `risk.rs` and `dlmm.rs`. | M | ✏️ |
| R7 | One 277k-line bin crate, 3 `Cargo.lock` files, a 30 GB `target/`. | `Cargo.toml` (no `[workspace]`); 8 measured core→vertical seams (e.g. `src/domain/secrets.rs:113` → `domain::solana::bs58_decode`, 37 files importing time helpers from `domain::marketdata`) | Stage 1 (S): root `[workspace]` with `crates/tengu-seal`, exclude the wasm worker. Stage 2: cut the seams. Stage 3: `lib.rs`. Stage 4: `tengu-trading`. Keep `src/domain/backtest/spec.rs` at its path (6 W1 records cite it) and the 8 pinned tool schemas byte-identical. | L | ✅ |
| R8 | 10–11 SKILL.md frontmatter splitters with 3 rules: a CRLF or indented skill is visible to some consumers only. | e.g. `src/application/orchestrator/planner.rs:477`, `src/application/orchestrator/shared_files.rs:343`, `src/application/skills/registry.rs:479` | One pure `domain::skill::split_frontmatter`. ~150 lines. | S | ○ |

---

## 6. Code clutter, DRY and KISS

| ID | Duplication | Sites | Single home | Removes | Effort | ✓ |
|---|---|---|---|---|---|---|
| Y1 | The "egress gate → budget → send → classify → audit" HTTP pipeline, 8 copies; 8 `fn audit`; Solana RPC has no rate budget; the seal auth header in only 3 clients. | `src/adapters/outbound/hyperliquid/info.rs:268`, `src/adapters/outbound/backfill/{sec,gecko}.rs`, `src/adapters/outbound/sources/ted.rs:158`, `src/adapters/outbound/solana/{http_json,rpc,privy}.rs`, `src/adapters/outbound/a2a/mod.rs:443` | `GatedHttp` in `src/adapters/outbound/http_class.rs`; move `backfill::Retry` there (decisions.rs keeps its breaker) | ~350 lines | M | ✅ |
| Y2 | Two tool-executor builders that already drifted (MCP allow-list, http-client failure `None` vs `Err`): the root of engine parity. | `src/bootstrap/tools.rs:96-224` vs `src/adapters/inbound/mcp_bridge.rs:840-950` ("mirrors") | one `assemble_executor(ExecutorInputs)` used by chat, run-agent and the bridge (merge with A6) | ~100 lines | M | ✅ |
| Y3 | 9 SQLite openers, 7 `with_conn` wrappers; only `sources.db` is 0600 (`ledger.db`, `runtime.db`, `solana-writes.db` take the umask). | `src/adapters/outbound/{runtime_store,observations,paper_store,history_sqlite,decision_cache,market_data}.rs`, `src/adapters/outbound/solana/writes_store.rs`, `src/adapters/outbound/sources/store.rs:246` | `src/adapters/outbound/sqlite.rs::open_wal` + `SharedConn` | ~140 lines | S | ✅ |
| Y4 | 4 atomic writers; the vault writer uses a fixed `.tmp`, no fsync, chmod after rename. | `src/adapters/outbound/secrets.rs:466-473` vs `src/adapters/outbound/builder_store.rs:108-117` (correct) | `src/adapters/outbound/fs_atomic.rs::write_atomic(path, bytes, Mode)` | ~90 lines | S | ✅ |
| Y5 | OpenAI wire layer copied between openrouter and local. | `src/adapters/outbound/engines/local.rs:38-160` vs `src/adapters/outbound/engines/openrouter.rs:59-329` | `src/adapters/outbound/engines/openai_compat.rs` | ~130 lines | S | ✅ |
| Y6 | Argument parsers per tool family; `opt_str` trims in xlab, not in xm. | `src/adapters/outbound/tools/xm/paper.rs:76-108`, `src/adapters/outbound/tools/xlab/mod.rs:89-127`, `hyperliquid/mod.rs:75`, `solana/dlmm.rs:84` | `src/adapters/outbound/tools/args.rs` (keep `validate_path` there); note the xm trim change in the commit | ~100 lines | S | ✏️ |
| Y7 | EVM crypto family always on (`opt_in: None`), no wallet anywhere, 6 sandboxes carry deny-all stanzas. | `src/adapters/outbound/tools/mod.rs:144-149`; `src/domain/tools.rs:109-125` | Move the 5 names from `DEFAULT_TOOLS` to `WORKSPACE_TOOLS`; keep the `[risk]` deny rule until S1 lands; or delete `tools/crypto/` if retired (operator's call). Leave W1 / SOE-G0 stanzas. | up to 1,070 lines | S | ✏️ |
| Y8 | 13 `load_sandbox_or` + 6 `registry_after_keys` pairs by hand. | `src/adapters/inbound/cli/mod.rs:737-828` | `sandbox_ctx()` (also fixes S12) | ~25 lines | S | ✅ |
| Y9 | 7 sha256-hex, 6 hex encoders, 11 truncation helpers; chat and bridge build the same truncation footer separately. | `src/domain/canonical.rs:55` + 6 copies; `src/application/chat/tool_loop.rs:481` vs `src/adapters/inbound/mcp_bridge.rs:639` | `domain/canonical.rs` and `domain/token.rs` | ~80 lines | S | ○ |
| Y10 | Two sandbox-name rules; two JSON redaction walkers (`redact_value` skips object keys). | `src/config/builder/compile.rs:139` vs `src/adapters/outbound/trace_store.rs:44`; `src/domain/secrets.rs:158` vs `src/domain/trace.rs:490` | one `domain` name rule; one walker | ~30 lines | S | ○ |

---

## 7. Debugging and dashboard

Measured on the operator's home: `~/.tengu/logs` 128 MB, one xpower trace run 62,483 events / 49 MB, 0 tracing spans in `src`.

| ID | Problem | Evidence | Fix | Effort | ✓ |
|---|---|---|---|---|---|
| B1 | Every egress record is written to `egress.jsonl` **and** as an INFO line: 89% of `tengu.log` is that copy. | `src/adapters/outbound/egress.rs:593` | Mirror only `denied` / `error`, at WARN with host, tool and call id. One `if`. | S | ✅🔍 |
| B2 | No id finds one turn across sinks: no spans; egress `call_id` is null for plan steps, chat, bridge, A2A and webhook agents; 3 timestamp formats. | `rg 'info_span!'` = 0; `AttributedExecutor` only at `src/bootstrap/decision.rs:173`, `src/bootstrap/runtime.rs:595` | `info_span!("turn", run_id, session_id)` at the 5 entry points; the traced + attributed wrapper pair inside the one executor builder; `run_id` + `ts_ms` in egress. | M | ✅ |
| B3 | Claude Code / Codex step tool calls are never traced; an engine error or timeout kill loses the step's tool events and metrics. | `src/adapters/inbound/mcp_bridge.rs` (no trace); `src/adapters/inbound/cli/run_agent.rs:478` | Quick: turn each `ToolRan` into a trace draft. Full: a per-step 0600 trace file passed to the bridge, like `TENGU_BRIDGE_SUMMARY_FILE`, read by the parent on any exit. | S/M | ✅ |
| B4 | Failed calls and in-process turns record no `MetricsRecord`; 5 hand-built 14-field literals. | `src/application/orchestrator/planner.rs:750`; `src/application/chat/service.rs:436` | `MetricsRecord::call(...)` + serde-default `ok`; record in the error branch too. | S | ✅ |
| B5 | Step-summary memory writes are discarded (`let _ =`); no memory inspect CLI. | `src/adapters/inbound/cli/run_agent.rs:547`, `:632` | Emit each write as a `tool.completed` / `tool.failed` trace draft. Covers the common "did it land?" case. | S | ✅ |
| B6 | Reproducing a failed tool call means rebuilding its inputs by hand. | `tool.started` holds tool, args, call id; `src/adapters/inbound/cli/tool.rs:9` | `tengu tool call --from-trace <run_id>:<seq>`. | S | ✅ |
| B7 | 13 copies of the logging init in `main`; TUI / Telegram panic on an unwritable log dir; children log ANSI, then the parent strips it. | `src/adapters/inbound/cli/mod.rs:455-690`; `src/adapters/outbound/subprocess_runner.rs:337` | One `LogSink` + `init_logging`; ANSI off on stderr; delete `strip_ansi`. ~130 lines. | S | ○ |
| B8 | **Studio** re-parses the whole run file on every page: a full replay of the xpower run is ~7.8M event parses; listing runs parses every file. | `src/adapters/inbound/studio/api.rs:221`, `:276-279`; `src/adapters/outbound/trace_store.rs:235` | Cache the last 1–2 parsed runs by (run id, file length); build `RunSummary` from the first and last lines. | S | ✅ |
| B9 | **Studio** board shows no counts, error rates, latency, tokens or Jev cost; the data is already in the trace. | `src/application/studio/board.rs:52-60`; `src/domain/metrics.rs:189` `AggregatorState` (TUI only) | A pure `fold_stats(events)` served in the `/board` header and by `tengu trace stats --run <id>`. This is the run-inspector view. | S | ✅ |
| B10 | Every trace line repeats per-run constants (31% of each line); feed `tool.started` is 37% of lines. | `src/domain/trace.rs:9`, `:21-22` | Schema v2: constants in `run.opened` only; the reader re-stamps; optionally drop feed `tool.started`. −40–55% size. | M | ○ |

---

## 8. Workspaces and on-disk layout

| ID | Problem | Evidence | Fix | Effort | ✓ |
|---|---|---|---|---|---|
| W1 | No command prints where a sandbox reads and writes; paths are rebuilt by hand (14 `.join("state")`, 7 `.join("logs")`); the `.env` that sets `TENGU_HOME` loads silently (the worktree trap). | `src/config/paths.rs`; `src/adapters/inbound/cli/mod.rs:460`; 32 docs hand-write `<TENGU_HOME>/…` | One `paths::Layout`; `tengu paths --sandbox <s>` prints the `TENGU_HOME` source (env / `.env` path / default), state dirs, workspaces, `.tengu/` stores and log sizes. Docs point to the command. | M | ✅ |
| W2 | `TENGU_HOME` is not the one root: the memory default and 10 managed-skill sites hard-code `~/.tengu`, so a lab home still writes the operator's memory and skills; 9 tier-walk copies (eval uses the cwd). | `src/config/mod.rs:1209`; `src/application/skills/registry.rs:1144`, `src/adapters/outbound/tools/manage_skill/mod.rs:964` | One `skill_tiers(workspace)`; managed tier and memory default under `<TENGU_HOME>`. | S | ✅ |
| W3 | `tengu prune --sandbox x` deletes all of `logs/` (every sandbox's traces, `decisions.jsonl`, `risk.jsonl`, Studio maps); nothing rotates; 2 dead targets. | `src/adapters/outbound/prune.rs:72`, `:79`, `:113` | Sandbox-scoped prune; `--older-than <days>`; delete the dead targets. Keep the `touches_state` guard. | S | ✅ |
| W4 | 5 workspace naming patterns (7 bare dirs in `~`); harness state under 3 prefixes in a workspace (`memory/`, `.tengu-attachments/`, `.tengu/`). | `sandboxes/*/config.toml`; `src/config/builder/template.rs:83` | New sandboxes: `~/tengu-lab/<sandbox>/ws`; all harness state under `<ws>/.tengu/` with a one-time old-path fallback. Leave the W1-bound sandboxes. | M | ○ |

---

## 9. Docs, HTML explorers and repo root

The content is accurate (CLAUDE.md cites 29/29 live functions, `code-map.md` 568/568 live paths, 0/35 tutorial pages stale). The problem is volume: each feature is described 4–5 times, and 34 `src` commits since 2026-09-22 touched 723 doc files.

| ID | Problem | Evidence | Fix | Effort | ✓ |
|---|---|---|---|---|---|
| O1 | 86 tracked top-level docs, 62 dated, 6 with "historical" banners, 3 different "start here" orders. Dated names are used for living docs edited weekly. | `git ls-files docs` | Target tree below. Rule: living docs are undated with a "Checked against the code on <date>" footer; dated PRDs, plans, research and reports go to `docs/archive/<yyyy-mm>/` once shipped. Never move a lineage-cited doc (8 files). | M | ✏️ |
| O2 | 42 files (2.07 MB) cited by no code, test, web page or lineage record, several with dead paths; 3 runbooks break the model-spend rule. | e.g. `docs/harness-architecture.md` (30/42 live paths), `docs/xmarket-gaps-2026-09-29.md` (352 KB), `docs/IMPLEMENTATION_PLAN.md`, `docs/superpowers/*`, `docs/validation-checklist.md` | `git mv` to the archive with a one-row-per-file `docs/archive/README.md`; merge live skill-lifecycle facts into `docs/skills.md` first. | S | 🔍 |
| O3 | 7 hand-synced HTML explorers duplicate the tutorial and the generated `code-map.html`; the architecture html carries a 44.6 KB hand `FILE_MAP`; the svg embeds 116 file names. | `docs/architecture-2026-04-27.html:1074`, `:1431` | Archive 6 explorers; keep only the walk-a-turn view of the architecture page; strip file names from the svg. ~480 KB. | M | 🔍 |
| O4 | `code-map.md` § 7 is a 148 KB hand table (22 drifted line counts) although `tests/code_map.rs` already builds the same data. | `tests/code_map.rs:222` checks presence only | Generate § 7 into a GENERATED block with the existing `TENGU_REGEN_CODE_MAP` switch. | S | 🔍 |
| O5 | `docs/SESSION_HANDOFF.md` is 139 KB (65% before 2026-10-08); its "Open items" is the 2026-05-14 phase table; gate state lives in 4–5 files. | `docs/SESSION_HANDOFF.md:710`, `:808` | Resume + one Open-gates table (the only place gate state lives) + last 3 sessions, ≤ 40 KB; older sessions to `docs/archive/handoffs/`. `handoff_for_check.md` keeps the newest session only. | S | 🔍 |
| O6 | 6 private docs and `handoff_review.md` are untracked and **not ignored**; 3 plan files and an empty `memory/` dir sit in the root. The 187 MB `obs-studio-32.2.2-macos-apple.dmg` seen at the start of this audit is gone. | `git status --porcelain --ignored` | Ignore `docs/software-opportunity-*`, `docs/crypto-opportunity-*`, `/handoff_review.md`, `*.dmg` (or move the private set to `~/tengu-lab/private/`). Move `TENGU_{HANDOFF,ROADMAP,STUDIO_PLAN}.md` to `docs/plans/` with a sed over 21 src doc comments. | S | 🔍 |
| O7 | No one-screen reading order: CLAUDE.md lists 36 files, `docs/index.html` starts with 538 KB, the README has a 31-row doc table + a hand module map. | `docs/index.html`; `README.md` | Five steps on top of `docs/index.html`: README quickstart · `docs/tutorial/big-picture.html` · `code-map.md` § 2 · the area doc · SESSION_HANDOFF gates. README links instead of repeating. | S | 🔍 |
| O8 | The sandbox list is hand-kept in 5 places; venture-scan and xpower are missing from README, `docs/index.html` and `docs/tutorial/sandboxes.html`. | `rg -c` = 0 in all three | README § Sandboxes is the only list; a check in `tests/tutorial_map.rs` that every `sandboxes/<dir>` is listed; the "Open-network sandboxes" gotcha becomes a pointer. | S | 🔍 |
| O9 | Dead links are found only by hand (e.g. `sandboxes/unlimited/BENCH.md` does not exist). | `README.md:282`; `docs/index.html:435` | Extend the existing link check in `tests/tutorial_map.rs` to every living md/html doc, README and AGENTS.md. | S | 🔍 |
| O10 | `file.rs:N` citations rot (4/9 sampled hits in `orchestrator-pipeline.md`). | `docs/context-management-2026-04-27.md` has 60 | Living docs cite `path::symbol`; `:line` only in dated reports like this one. | S | ○ |
| O11 | Each config field is documented 3 times (struct doc, `config.example.toml`, a `configuration.md` row). | `docs/configuration.md` 272 rows, 59 KB | Keep struct docs + the example (test-load it fully, as `src/config/sources.rs` already does for `[sources]`); `configuration.md` keeps only load rules, validation, hardening, env vars. | M | ○ |

**Target tree (O1):**

| Path | Holds |
|---|---|
| `README.md` | what, quickstart, the one sandboxes table, link to `docs/index.html` |
| `AGENTS.md` (≤ 25 KB) · `CLAUDE.md` = `@AGENTS.md` | the agent guide |
| `handoff_for_check.md` · `config.example.toml` | newest session only · the field reference |
| `docs/index.html` | one-screen hub |
| `docs/SESSION_HANDOFF.md` · `docs/gotchas.md` · `docs/sources.toml` | resume + gates · gotchas by area · doc → src map |
| `docs/code-map.{md,html}` (generated) · `docs/architecture.{md,svg}` · `docs/context.{md,svg}` | where things live · one turn · what an LLM sees |
| `docs/{configuration,tools,skills,engine-backends,mcp-bridge,egress,webhooks,runtime,observations,memory,decision-loops,lineage,ranking,studio,builder,a2a,sealed-keys}.md` | one living doc per feature |
| lineage-cited docs under their dated names | `xlab-2026-10-01.md`, `soe-2026-10-08.md`, `xmarket-prd-2026-09-29.md`, `xmarket-feasibility-2026-09-30.md`, `p7-news-labels-2026-10-08.md`, `p9-cost-liquidity-2026-10-08.md`, `p10-w2-candidate-2026-10-08.md`, `forward-evidence-runbook-2026-10-08.md` |
| `docs/tutorial/` · `docs/plans/` · `docs/evidence/` | unchanged · the 3 root plans · Studio + sealed-check evidence |
| `docs/archive/<yyyy-mm>/` + `docs/archive/README.md` | everything dated and shipped |

---

## 10. Keep: do not "clean up" these

| Keep | Why |
|---|---|
| Planner turn stripped of tools, executor, memory, bridge (`src/bootstrap/orchestrator.rs:211-265`) + test `planner_turn_gets_no_bridge_tools` | Doctrine 1; A3 makes it structural, the test stays |
| One `build_orchestrator`; `SubprocessRunner` reused by SOE; `DagExecutor` cancel-by-drop; `run_reusing` | A1 builds on the drop path |
| `domain::backoff` as the one retry policy; `LoopDispatch` | A8 extends it instead of adding a loop framework |
| One tool catalog row per tool + schema lint + bridge conformance + engine matrix | The seam that keeps every tool working under every engine |
| `tests/layering_lint.rs` with zero exceptions; core runtime importing no vertical | Makes R7 cheap; extend it (R5), never relax it |
| `crates/tengu-seal` (native + wasm) | The template for further crates |
| SOE quarantine: `fence_untrusted` + fixpoint `clean_untrusted`, closed tool world, deny-all scopes | The model for S6 and S10 |
| Jev typed menus, `dry_run` default, Solana `simulate` default, signing only on private agents | Injected text can at most bias a choice inside the menu |
| `SanitizedToolExecutor` on every surface; Codex empty env; Telegram fail-closed | A6 makes it by construction |
| Trace envelope + JSONL store; Studio draws only what Rust folds; Studio guard | B3, B8, B9 add folds and a cache, no JS logic |
| `tengu doctor --live` and Studio health share `read_live`; prune's `touches_state` guard | W3 keeps the guard |
| Old tool results compacted to 200 chars; `local` results fit 1/8 of the window; recall lanes off by default, snippets capped at 400 chars | Already-good token hygiene |
| `tests/code_map.rs` generation, the tutorial + `sources.toml` + `tests/tutorial_map.rs` + hook | D2, O4 and O9 reuse them |

## 11. Operator decisions needed

| Decision | Rule it bends | Rows |
|---|---|---|
| Replace the `CLAUDE.md` / `AGENTS.md` twin rule with an `@AGENTS.md` import and move gotchas out | "Twin file … keep the two in sync" | D1, D2 |
| Compose may only narrow in every sandbox (the doctrine-compatible variant needs no decision) | Doctrine 3 | S4 |
| An empty `tools` list means the read-only set | "empty = every base tool" | S14 |
| Retire `compress_and_store` and use the final text | The documented step protocol | T3 |
| Retire the EVM crypto tool family (`tools/crypto/`) | none (scope only) | Y7 |
| A lower tool-result cap for W1 agents (a new generation) or keep the old value there | W1 frozen | T6 |
| Edit the operator's own `MEMORY.md` | the operator's file | D4 |

## 12. Suggested order

| Wave | Rows | Note |
|---|---|---|
| 1 · safety and quick wins (S, one small PR each) | S1, S2, S3, S5, S9, S12, A1, A2, B1, B3 quick, T1, T2, T5, R2, R3, O6 | No doctrine change; verify each with one live turn |
| 2 · measure, then merge | T7 first, then B2, B4, B5, A3, A6 + Y2, A4, A5, Y1, Y3, Y4, S6, S7, S8, S10, S11 | T7 gives the baseline for every token claim |
| 3 · docs | D1, D2, O1–O5, O7–O9 (after the § 11 answers) | Moves + deletes; lineage-cited paths never move |
| 4 · structure | R1, R4, R5, R6, R7 stages 1–3, D6, W1–W4, B8, B9 | R4 sizes appear only once cargo runs |

## Coverage and limits

| Item | Status |
|---|---|
| Cargo | not run: dead-code sizes (R4) and compile-time gains (R7) are not measured |
| Verified | 67 high/medium findings re-checked by 2 verifiers: 40 confirmed, 27 corrected, 0 refuted |
| Not verified by a verifier | 10 high/medium docs findings (rows D2, O2–O8 and the naming rule in O1): over the verifier cap, spot-checked by the author, all held |
| Low-impact rows (○) | reviewer evidence only |
| Out of scope | business logic of the trading / SOE verticals, live engine runs, anything needing secrets |
