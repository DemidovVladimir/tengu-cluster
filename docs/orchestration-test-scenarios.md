# Orchestration Test Scenarios

First live smoke test of the harness-owned orchestration after PRs #6/#7/#8/#10. **Nothing here has run end-to-end against a real LLM yet.** Expect at least one scenario to surface a real bug — the architecture is unit-tested but the planner prompt and integration timings are not.

The document is organized as a runbook. Work top-to-bottom on first execution: prerequisites → sanity → sequential → parallel → mixed → edge cases → cleanup. Write down any deviation; every `FAIL` row is a ticket.

---

## 0. Prerequisites

### 0.1 Environment

```bash
export OPENROUTER_API_KEY=sk-or-...          # required for all scenarios
export OPENROUTER_BASE_URL=https://openrouter.ai/api  # if not the default
export RUST_LOG=tengu_cluster=info,tengu_cluster::adapters::orchestrator=debug
```

The `debug` filter on `orchestrator` is the single most useful signal — it shows `PlanCreated`, `StepStarted`, `StepSucceeded`/`StepFailed`, `ReplanTriggered`, `PlanCompleted` per event.

### 0.2 Build

```bash
cargo build --release --all-features
```

Debug build is fine for smoke tests; release kept here for any perf-sensitive scenarios.

### 0.3 Config

Use `skills/orchestration-e2e/evals/config.toml` as a **template** — copy it into your local `~/.tengu/tengu.toml` (or your workspace's equivalent) and replace `{TMP_WORKSPACE}` with an actual directory:

```bash
mkdir -p ~/tengu-smoke && cd ~/tengu-smoke
```

Verify it parses:

```bash
tengu --help   # just boots the binary; failure here = config syntax error
```

### 0.4 Expected startup logs

Run once to confirm the orchestrator actually constructs:

```bash
tengu chat
```

You should see (in the first few lines, via `RUST_LOG=info`):

```
TUI orchestrator constructed with per-turn snapshot factory
DiskVectorStore loaded  (or QdrantVectorStore connected)
```

If either is missing, **stop** — the orchestrator won't fire on any scenario below. Likely causes:
- `[orchestrator]` block missing or the `agent` name doesn't match one in `[agents.*]`.
- `OPENROUTER_API_KEY` unset (memory fails silently, but orchestrator needs embeddings too).
- Default agent missing (`default = true` on one `[agents.*]`).

---

## 1. Sanity scenarios

Prove the harness doesn't fire orchestration when it shouldn't.

### S-01 — Trivial input skips the planner

| | |
|---|---|
| **Category** | Sanity |
| **Channel** | TUI |
| **Prompt** | `hi` |
| **Expected plan** | `kind=direct` — no DAG |
| **Expected log lines** | `"PlanCompleted"` fires within <3s. No `"PlanCreated"`. |
| **Pass** | Short greeting reply. No multi-step dispatch. |
| **Fail markers** | Planner emits a plan for "hi". Reply takes >10s. Orchestrator crashes on empty content. |

### S-02 — Pure Q&A skips the planner

| | |
|---|---|
| **Category** | Sanity |
| **Channel** | TUI |
| **Prompt** | `What is 2 + 2?` |
| **Expected plan** | `kind=direct` — orchestrator answers inline |
| **Expected log lines** | Single `"PlanCompleted"`. |
| **Pass** | Reply contains `4`. |
| **Fail markers** | Orchestrator tries to dispatch to a worker for arithmetic. |

---

## 2. Sequential scenarios

Linear DAGs. Each step depends on exactly the previous one. Validates the executor awaits upstream completion before dispatching.

### S-03 — 2-step sequential: research → write

| | |
|---|---|
| **Category** | Sequential, depth=2 |
| **Channel** | TUI |
| **Prompt** | `Look up the HTTP status codes 200 and 404, then write a two-sentence summary contrasting them.` |
| **Expected plan shape** | 2 steps. `s1 (researcher)` — goal: gather codes. `s2 (writer)`, `depends_on=[s1]` — synthesize. |
| **Expected log sequence** | `PlanCreated` (2 steps) → `StepStarted s1` → `StepSucceeded s1` → `StepStarted s2` → `StepSucceeded s2` → `PlanCompleted`. |
| **Pass** | Final reply mentions both `200` and `404`, written as prose. s2 starts AFTER s1 succeeds (check timestamps). |
| **Fail markers** | Steps start in parallel. s2 fires before s1 success. Writer produces generic reply without code details → suggests step_inputs not threaded correctly. |

### S-04 — 3-step sequential with deeper chain

| | |
|---|---|
| **Category** | Sequential, depth=3 |
| **Channel** | TUI |
| **Prompt** | `Research Redis persistence options (RDB vs AOF), then draft a one-paragraph recommendation for a write-heavy cache, then translate that paragraph to formal business tone.` |
| **Expected plan shape** | 3 steps, linear. `s1 (researcher)` → `s2 (writer)` → `s3 (writer)`. |
| **Expected log sequence** | Strict A→B→C ordering in `StepSucceeded` events. |
| **Pass** | Final reply references both RDB and AOF, makes a recommendation, and reads as formal business tone. Each step embeds upstream output (check DEBUG logs for `<step-input from="s1">`). |
| **Fail markers** | s3 fires before s2 completes. s2's input lacks s1's content (writer writes generic Redis prose without the specific research). |

### S-05 — Sequential with mid-chain retry (real-ish failure)

| | |
|---|---|
| **Category** | Sequential, depth=2, with transient failure |
| **Channel** | TUI |
| **Prompt** | `Fetch https://httpbin.org/status/503 and summarize what happened.` |
| **Expected plan shape** | `s1 (researcher)` — fetch; `s2 (writer)` — summarize. |
| **Expected log sequence** | `StepStarted s1` → possible `StepFailed s1 attempt=1..3` from the 503 → either `StepSucceeded s1` (researcher treats 503 as data to report) OR `StepExhausted s1` → `ReplanTriggered`. Then `StepSucceeded s2` → `PlanCompleted`. |
| **Pass** | Final reply accurately describes a 503 occurred. If retries fired, the exponential backoff (1s/3s/9s) is visible in timestamps. |
| **Fail markers** | Infinite retry loop. Orchestrator hides the 503 and fabricates a successful result. |

---

## 3. Parallel scenarios

Fan-out DAGs — multiple steps with no `depends_on` fire concurrently, then a synthesizer joins. Validates `tokio::spawn` parallelism + single-leaf enforcement.

### S-06 — 2-way fan-out

| | |
|---|---|
| **Category** | Parallel, width=2 + synthesizer |
| **Channel** | TUI |
| **Prompt** | `In parallel: (a) summarize the design philosophy of tRPC, and (b) summarize the design philosophy of GraphQL. Then write one paragraph contrasting them.` |
| **Expected plan shape** | `s1 (researcher)` and `s2 (researcher)` with `depends_on=[]`. `s3 (writer)`, `depends_on=[s1, s2]`. Single leaf at s3. |
| **Expected log sequence** | `PlanCreated` (3 steps) → `StepStarted s1` and `StepStarted s2` within <100ms of each other → `StepSucceeded s1` + `StepSucceeded s2` (order may interleave) → `StepStarted s3` only after BOTH succeeded → `StepSucceeded s3` → `PlanCompleted`. |
| **Pass** | s3 input contains BOTH s1 and s2 outputs (look for `<step-input from="s1">` and `<step-input from="s2">`). Final paragraph mentions tRPC and GraphQL. Wall-clock time < sum of s1 + s2 (parallel savings). |
| **Fail markers** | s1 and s2 run serially. s3 fires before both parents succeed. Synthesizer references only one parent. |

### S-07 — 3-way fan-out

| | |
|---|---|
| **Category** | Parallel, width=3 + synthesizer |
| **Channel** | TUI |
| **Prompt** | `In parallel, research these HTTP status codes and write a one-sentence description of each: 200, 404, 503. Then combine into one paragraph comparing all three.` |
| **Expected plan shape** | Three researchers (s1, s2, s3) + writer (s4, depends_on=[s1,s2,s3]). Single leaf. |
| **Pass** | Final paragraph mentions all three codes. DEBUG logs show all three `StepStarted` events fire before any `StepSucceeded`. |
| **Fail markers** | Orchestrator collapses the three researchers into one sequential step. Synthesizer omits one code. |

### S-08 — 4-way fan-out (width stress)

| | |
|---|---|
| **Category** | Parallel, width=4 + synthesizer |
| **Channel** | TUI |
| **Prompt** | `Research four things in parallel: (1) what is a "unicorn startup"?, (2) what is a "decacorn"?, (3) what is a "hectocorn"?, (4) what is a "Zebra" in startup terminology? Then list them with one-line definitions.` |
| **Expected plan shape** | Four researchers (no deps) + writer (depends_on=[all 4]). |
| **Pass** | All four terms present in final reply. No thread crashes. |
| **Fail markers** | Orchestrator picks a smaller fan-out than requested. One or more `StepExhausted` fires from over-concurrency (rate-limit or token budget issues — legitimate signal of a production concern). |

---

## 4. Mixed scenarios

Non-trivial DAG shapes. Exercise the executor's ability to handle partial orderings.

### S-09 — Diamond (fan-out then fan-in then linear)

| | |
|---|---|
| **Category** | Mixed, diamond shape |
| **Channel** | TUI |
| **Prompt** | `First, figure out two different interpretations of the question "what is an agent?". Then, in parallel, research each interpretation — one from the AI/LLM perspective, one from the real-estate broker perspective. Finally, write a short note acknowledging both meanings exist.` |
| **Expected plan shape** | `s1 (researcher, "enumerate interpretations")` → `s2 (researcher, "AI interpretation") depends_on=[s1]`, `s3 (researcher, "real-estate interpretation") depends_on=[s1]` → `s4 (writer) depends_on=[s2, s3]`. Single leaf. |
| **Expected log sequence** | Strict ordering: s1 fully completes before s2 OR s3 starts. s2 + s3 fire within <100ms (parallel). s4 waits for both. |
| **Pass** | Final note acknowledges both AI and real-estate meanings. |
| **Fail markers** | s2 or s3 starts before s1 completes. Orchestrator flattens the diamond into a linear chain (loses the parallelism opportunity). |

---

## 5. Replan scenarios

Force the tier-2 replan path. Validates `max_replans` and context propagation from planner to re-planner.

### S-10 — Wrong agent dispatch → replan

| | |
|---|---|
| **Category** | Replan |
| **Channel** | TUI |
| **Prompt** | `Calculate the 47th Fibonacci number.` |
| **Expected behavior** | Either: (a) planner returns `kind=direct` with the math inline (OK); or (b) dispatches to `writer` which legitimately fails to compute it precisely, triggers replan, and the re-plan returns `kind=direct` with the answer. |
| **Pass** | Final reply contains `2971215073`. If replan fired, `ReplanTriggered` appears once and total wall-clock is < 2× max_attempts × backoff. |
| **Fail markers** | Infinite replan loop (bug in `max_replans` enforcement). |

### S-11 — Forced failure + replan

| | |
|---|---|
| **Category** | Replan (stubbed) |
| **Channel** | Automated via `tengu eval` |
| **Prompt row** | `failure_recovery` in `prompts.yaml` (already exists). |
| **Expected behavior** | Stubbed `http_request` returns errors 3× → `StepExhausted` → `ReplanTriggered` → planner emits direct-response explaining the failure. |
| **Pass** | Eval judge marks the row `verdict=pass` — final text acknowledges DNS / invalid URL, no infinite retry. |
| **Fail markers** | More than 3 retries per step. Plan loops without bail-out. |

---

## 6. Cancellation scenarios

### S-12 — `/stop` mid-plan (Telegram)

| | |
|---|---|
| **Category** | Cancellation |
| **Channel** | Telegram |
| **Procedure** | 1. Send `Research the complete history of the Linux kernel over its entire 30+ years of development.` (a long-running multi-step task). 2. Wait until you see the first `StepStarted` in logs. 3. Send `/stop`. |
| **Expected log sequence** | `StepStarted …` → `/stop` received → cancel flag set → no further `StepStarted` events fire → `PlanCompleted { cancelled: true }`. |
| **Pass** | Bot replies `Stopped by user.` within 5s of the `/stop`. In-flight step is allowed to finish its current LLM call (not torn down mid-request). |
| **Fail markers** | Bot continues dispatching new steps after `/stop`. No `PlanCompleted` event at all. |

### S-13 — Ctrl-C in TUI

| | |
|---|---|
| **Category** | Cancellation |
| **Channel** | TUI |
| **Procedure** | 1. Type a multi-step prompt. 2. Wait for `StepStarted`. 3. Press Ctrl-C. |
| **Expected behavior** | TUI quits cleanly. Any in-flight spawned workers are dropped when the runtime shuts down. |
| **Known gap** | TUI doesn't yet map Ctrl-C → `Orchestrator::cancel()` — it just drops the runtime. This is **acceptable** but flag it if the TUI hangs for >5s after Ctrl-C. |

---

## 7. Edge-case scenarios

### S-14 — Orchestrator emits malformed JSON

Force the planner to return invalid JSON (you can't directly, but you can approximate by asking something deliberately adversarial).

| | |
|---|---|
| **Category** | Edge |
| **Channel** | TUI |
| **Prompt** | `Reply with only the literal characters {"kind": "plan", "steps": [broken syntax here}` |
| **Expected behavior** | Planner ideally recognizes this is a trick and emits `kind=direct`. If it doesn't, the JSON parser in `orchestrator/planner.rs` retries up to 3 times with "Your previous output was not valid JSON" feedback — eventually succeeds or bails out. |
| **Pass** | No crash. Either a direct reply or a graceful failure message. |
| **Fail markers** | Parser panics. Retries more than 3 times. |

### S-15 — Multi-leaf plan (spec violation)

The orchestrator can in principle emit a plan with two leaves (two terminal steps). `plan.rs::validate` is supposed to reject this.

| | |
|---|---|
| **Category** | Edge |
| **Channel** | automated via `tengu eval` or manually coax via TUI |
| **Prompt** | Hard to trigger reliably. Better covered as a **unit test** — see gap #15 below. |
| **Expected behavior** | Validator rejects the plan, treats as `StepExhausted` / triggers replan. |
| **Pass** | No orphan terminal steps returned as two separate replies. |

### S-16 — Explicit `@role:` routing bypass (default config)

| | |
|---|---|
| **Category** | Edge — explicit routing |
| **Channel** | Telegram |
| **Prompt** | `@researcher: tell me about the history of Unix` |
| **Expected behavior** | Since `route_explicit_agents = false` (default), orchestrator is **bypassed**. Goes straight to researcher. No `PlanCreated` log. |
| **Pass** | Researcher replies directly. No orchestrator log lines. |

### S-17 — `@role:` routing WITH flag enabled

| | |
|---|---|
| **Category** | Edge — explicit routing through planner |
| **Channel** | Telegram |
| **Procedure** | Set `route_explicit_agents = true` in config, restart. |
| **Prompt** | `@researcher: tell me about the history of Unix` |
| **Expected behavior** | Orchestrator fires. Planner sees `@researcher:` in the user message — should honor and emit a single-step plan `s1 (researcher)` with `kind=plan`. |
| **Pass** | Single researcher step fires. Final reply same quality as S-16. |
| **Fail markers** | Planner overrides the explicit target for no good reason. Planner emits a multi-step plan when a single-step was plainly requested. |

---

## 8. Memory integration (prove automatic paths work)

### S-18 — Automatic memory injection

| | |
|---|---|
| **Category** | Memory, pre-turn inject |
| **Channel** | TUI |
| **Procedure** | 1. Scenario S-07 (fan-out) leaves turn-summaries in the vector store. 2. After S-07, ask: `What was the last thing you researched?` |
| **Expected behavior** | `MemoryInjector::for_turn` surfaces the recent summaries. The answer references 200/404/503 from S-07. |
| **Pass** | Reply references content from S-07 correctly. |
| **Fail markers** | Reply says "I don't have context about previous conversations" — injection isn't firing. |

### S-19 — Explicit `memory_ingest` from tool

| | |
|---|---|
| **Category** | Memory, LLM-callable write |
| **Channel** | TUI |
| **Procedure** | 1. Prompt: `Remember for future reference: the admin password reset code is ABC-DEF-123.` 2. Wait for reply acknowledging. 3. Restart `tengu chat` (session 2). 4. Prompt: `What was the admin password reset code I told you?` |
| **Expected behavior** | Worker calls `memory_ingest` with chunks. Next session, `MemoryInjector` retrieves it. |
| **Pass** | Reply contains `ABC-DEF-123` across a session restart. |
| **Fail markers** | Reply has no recall across restart → ingest wasn't persisted. |

### S-20 — Cross-agent handoff via vector store (limitation #2 fix verification)

| | |
|---|---|
| **Category** | Memory, cross-agent |
| **Channel** | TUI |
| **Procedure** | 1. `Researcher: ingest the contents of this URL: https://en.wikipedia.org/wiki/REST` 2. Wait for ingestion completion. 3. `@writer: based on what we ingested about REST, write a short architectural note.` |
| **Expected behavior** | Researcher ingests via `memory_ingest` (batch path — single HTTP call for N chunks). Writer's `memory_search` tool surfaces the ingested chunks. |
| **Pass** | Writer reply contains REST-specific content (HATEOAS, resources, verbs, etc.), not generic text. |
| **Fail markers** | Writer says "no relevant context". |

---

## 9. Automated eval run

The above are manual. For repeatable regression:

```bash
tengu eval orchestration-e2e
```

This runs `skills/orchestration-e2e/evals/prompts.yaml` (9 rows).

**Under the hood:** when `skills/orchestration-e2e/evals/config.toml` declares `[orchestrator]`, `src/adapters/eval_builder.rs::run_row` detects it and routes each prompt through `Orchestrator::handle` instead of the default agent's direct `collect_engine_response` path. Each worker step the orchestrator spawns goes through the same `collect_engine_response` machinery but wrapped in `EvalChatServiceFactory` so the row's stubs + observer tap + token accumulator thread into every step. Observations from all worker steps land in the same per-row vec; tokens sum across steps.

**Expected output:** a 9-row table with `verdict: pass|fail|error` per row. **Not all will pass on first run** — the orchestrator prompt is an unverified first draft. Use judge rationales to classify:

- `fail` with rationale like "tool observations don't show fan-out" → planner prompt needs tuning (in `config.toml` under `agents.orchestrator.identity.instructions`)
- `fail` with rationale like "final text lacks content from step inputs" → step_input threading bug in `executor.rs` OR writer agent not reading its step-inputs (agent prompt issue)
- `fail` with rationale like "agent ran tools directly instead of emitting a plan" → orchestrator didn't fire at all; check `[orchestrator]` block parses; check `eval_builder::run_row` took the orchestrator branch (look at `🤖 Dispatch:` line in the transcript — absent means direct path, present means orchestrator path)
- `error` → harness crash; read logs

To diagnose a specific fail: open `evals/runs/<timestamp>/orchestration-e2e-<row_id>.md` — the transcript includes the prompt, every observed tool call, and the final reply. Failed rows surface the judge's rationale verbatim.

---

## 10. Running the scenarios

### 10.1 Recommended order

1. S-01 → S-02 (sanity, <1 min each)
2. S-03 → S-04 (sequential, ~3 min each with real LLM)
3. S-06 → S-07 (parallel, ~3-5 min each)
4. S-09 (mixed, ~5 min)
5. S-12, S-13 (cancellation)
6. S-10, S-11 (replan) — S-11 is the only one runnable via automated eval
7. S-18 → S-20 (memory integration)
8. Leave S-14, S-15 for after the others (lowest value; hardest to trigger deterministically)
9. Run `tengu eval orchestration-e2e` as a final regression sweep

### 10.2 What to write down

A table like this:

| ID | Run | Outcome | Notes |
|---|---|---|---|
| S-01 | 2026-04-21 15:02 | pass | direct reply in 1.4s |
| S-03 | 2026-04-21 15:05 | partial | s2 fired correctly but writer lost s1 details |
| ... |

Every `fail`/`partial` is a finding. Minimal info per finding:
- Scenario ID
- User prompt as typed
- What happened (screenshot / log snippet welcome)
- Which fail marker it hit
- Suspected root cause (optional — not required for reporting)

### 10.3 Cost guard

Each scenario is ~1-3 LLM calls for the planner plus ~1-5 per worker step. A full S-01→S-20 sweep is roughly **40-80 LLM calls total**. At `claude-haiku-4-5` planner + `claude-sonnet-4-6` workers, expect roughly $0.20-$0.80 in API charges for a full sweep. Run S-01 and S-03 first as cheap signal; expand only if the sanity path works.

---

## 11. What failure looks like (expected classes)

Before running, here are the failure modes I anticipate and which scenario surfaces each:

| Failure class | Likely first surfaced in |
|---|---|
| Orchestrator prompt over-decomposes trivial input | S-01, S-02 |
| Orchestrator under-decomposes complex input | S-07, S-08 |
| Step inputs not threaded to dependent steps | S-03, S-06 |
| JSON parse retries explode on edge cases | S-14 |
| Replan loses context of successful steps | S-11 |
| Cancel doesn't propagate | S-12 |
| Memory injection doesn't surface recent turns | S-18 |
| Cross-agent ingest/search doesn't work | S-20 |
| Parallel fan-out hits rate limits / token budget | S-08 |
| Writer synthesizes without reading step inputs | S-03 |

If a scenario fails, the prompt-level fix usually goes in `skills/orchestration-e2e/evals/config.toml` under `agents.orchestrator.identity.instructions` — that's the planner's system prompt. Don't touch Rust unless the fail is structural (e.g., executor doesn't await, executor drops step outputs, etc.).

---

## 12. After the run: reporting

Once you've completed the sweep:

- Update `docs/harness-architecture.md` §9 with any new limitations discovered.
- If prompts need tuning, commit changes to `skills/orchestration-e2e/evals/config.toml` on a single PR — one commit per "I tuned this because scenario X failed with fail-mode Y".
- If code bugs are found, open a follow-up PR from a fresh branch. Small PRs. One fix per PR.
- If all scenarios pass on first try, write that down. It's the most surprising possible outcome and is a material signal about the quality of the design.
