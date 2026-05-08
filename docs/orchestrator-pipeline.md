# Orchestrator Pipeline — Diagrams + Code References

Precise map of how orchestration was built, how it runs, and where every piece lives in source. Mermaid diagrams render on GitHub; ASCII mirrors for terminal viewing.

Covers: construction history, runtime pipeline, parallel + sequential + diamond execution, retention.

---

## 1. How this was built — PR timeline

```mermaid
%%{init: { 'gitGraph': { 'mainBranchName': 'main' }}}%%
gitGraph
  commit id: "pre-harness" tag: "skill-based orch"
  commit id: "PR #9" tag: "ci fix + rustfmt baseline"
  commit id: "PR #6" tag: "foundation"
  commit id: "PR #7" tag: "memory port"
  commit id: "PR #8" tag: "channel wiring"
  commit id: "PR #10" tag: "polish (6 fixes)"
  commit id: "PR #11" tag: "eval dispatch + prose JSON"
  commit id: "PR #12" tag: "skill lifecycle"
  commit id: "PR #13" tag: "retention + --no-persist"
```

| PR | SHA on main | Net LOC | What it delivered |
|---|---|---|---|
| #9 | `6139d78` | −3 | Removed dead CI step + rustfmt baseline |
| #6 | `e76c08d` | +10,252 / −12,226 | `orchestrator/` + `memory/` subsystems, Phases 0–9 |
| #7 | `9667ea3` | +575 / −1,175 | Retired `MemoryService` + 3 old files |
| #8 | `1533c48` | +570 / −58 | Telegram + TUI route through `Orchestrator::handle` |
| #10 | `18711e1` | +~400 / −~50 | Fixed 6 limitations (batch embed, delete, clear_all, stats, `@role:` toggle, activity) |
| #11 | `1002957` | +~500 | 20-scenario runbook + eval orchestrator dispatch + prose-JSON parser |
| #12 | `f14bfc6` | +~1200 | Skill lifecycle (metrics + distill + evolve) |
| #13 | open | +~600 | Retention, `--no-persist`, TUI memory config, validation checklist |

---

## 2. System architecture (top-level)

```mermaid
flowchart TB
    subgraph Channel["Channel layer"]
        TG["Telegram<br/><code>telegram_builder.rs</code>"]
        TUI["TUI<br/><code>tui/mod.rs</code>"]
        Eval["Eval runner<br/><code>eval_builder.rs</code>"]
    end

    subgraph Orchestration["Orchestrator subsystem<br/><code>src/adapters/orchestrator/</code>"]
        Handle["Orchestrator::handle<br/><code>mod.rs:93</code>"]
        Replan["replan::drive<br/><code>replan.rs</code>"]
        Planner["OrchestratorAgentPlanner<br/><code>planner.rs</code>"]
        Executor["DagExecutor::run<br/><code>executor.rs:64</code>"]
        Retry["RetryPolicy<br/><code>retry.rs</code>"]
        Events["EventBus<br/><code>events.rs</code>"]
    end

    subgraph Workers["Worker step dispatch"]
        Factory["ChatServiceFactory<br/><code>wiring.rs:30</code>"]
        ChatWorker["ChatWorker::run_step<br/><code>wiring.rs:47</code>"]
        Inject["MemoryInjector<br/><code>memory/injector.rs</code>"]
        Write["MemoryWriter<br/><code>memory/writer.rs</code>"]
    end

    subgraph Engine["Per-agent LLM loop"]
        ChatRT["ChatRuntimeService<br/><code>chat_builder.rs</code>"]
        EngineCall["collect_engine_response<br/><code>engine_builder.rs:598</code>"]
    end

    TG -->|per-message snapshot| Handle
    TUI -->|per-turn snapshot| Handle
    Eval -->|EvalChatServiceFactory| Handle

    Handle --> Replan
    Replan --> Planner
    Planner -->|JSON plan| Replan
    Replan --> Executor
    Executor --> Retry
    Retry --> ChatWorker
    Executor -.broadcast.-> Events

    ChatWorker --> Inject
    Inject -->|"<memory-context>"| Factory
    Factory --> ChatRT
    ChatRT --> EngineCall
    EngineCall -->|assistant text| Factory
    Factory --> Write
    Write -.spawned.-> ChatWorker

    Events -.subscribers.-> TG
    Events -.subscribers.-> TUI
    Events -.subscribers.-> Eval
```

**ASCII mirror** (same graph, text only):

```
┌─────────── Channels ───────────┐
│ Telegram │ TUI │ Eval runner   │
└────┬───────┬──────┬────────────┘
     │       │      │
     ▼       ▼      ▼   (each provides a ChatServiceFactory closure)
┌─────────────────────────────────┐
│   Orchestrator::handle          │   orchestrator/mod.rs:93
│    └─▶ replan::drive            │   orchestrator/replan.rs
│         ├─▶ Planner::plan       │   orchestrator/planner.rs
│         │   (returns verdict)   │
│         └─▶ DagExecutor::run    │   orchestrator/executor.rs:64
│              ├─▶ RetryPolicy    │   orchestrator/retry.rs
│              └─▶ ChatWorker     │   orchestrator/wiring.rs:47
│                   ├ Inject ─┐   │   memory/injector.rs
│                   ├ Factory │   │   orchestrator/wiring.rs:30
│                   │    └ ChatRuntimeService → collect_engine_response
│                   └ Write    │   memory/writer.rs  (spawned)
└─────────────────────────────────┘
        │
        └─▶ EventBus (broadcast) ─▶ subscribers render progress
```

---

## 3. Request lifecycle — a single user message

```mermaid
sequenceDiagram
    participant User
    participant Channel as Channel<br/>(TG/TUI/Eval)
    participant Orch as Orchestrator
    participant Plan as Planner
    participant Exec as DagExecutor
    participant Retry as RetryPolicy
    participant W as ChatWorker
    participant Mem as MemoryManager
    participant LLM as LLM engine

    User->>Channel: prompt
    Channel->>Channel: build orchestrator_snapshots<br/>(one ChatTurnInputs per agent)
    Channel->>Orch: handle(prompt)
    Orch->>Plan: plan(prompt)
    Plan->>LLM: orchestrator agent turn<br/>(tools=[])
    LLM-->>Plan: JSON verdict
    Plan->>Plan: parse_verdict<br/>(raw → fences → prose)
    Plan-->>Orch: PlannerVerdict::Plan{steps}
    Orch->>Exec: run(plan, worker, cancel)
    Orch-->>Channel: emit PlanCreated

    loop Each ready step
        Exec->>Exec: plan.ready_steps(&completed)
        Exec->>Exec: tokio::spawn per ready step
        Exec-->>Channel: emit StepStarted
        Exec->>Retry: run_step_with_retry
        Retry->>W: run_step(step, step_inputs)
        W->>Mem: injector::for_turn(agent, goal)
        Mem-->>W: <memory-context> block
        W->>W: compose content = memblock + inputs + goal
        W->>LLM: ChatServiceFactory.run_turn(agent, content)
        LLM-->>W: assistant text
        W->>Mem: writer::sync_turn (spawned)
        W-->>Retry: Ok(output) or Err
        Retry-->>Exec: StepOutcome::Ok | Exhausted
        Exec-->>Channel: emit StepSucceeded/Failed
    end

    alt all succeed
        Exec-->>Orch: ExecResult::Done{leaf_output}
        Orch-->>Channel: emit PlanCompleted
    else step exhausted
        Exec-->>Orch: ExecResult::NeedsReplan
        Orch-->>Channel: emit ReplanTriggered
        Orch->>Plan: replan(prompt, prior_plan, failed_id, error)
        Plan-->>Orch: new verdict
        Orch->>Exec: re-run (bounded by max_replans)
    end

    Channel-->>User: final_response
```

**Reference points (file:line):**

| Step in diagram | File | Line |
|---|---|---|
| Channel builds snapshots | `telegram_builder.rs::execute_orchestrator_turn` | 1397 |
|  | `tui/mod.rs` snapshot block | ~138 |
| `Orchestrator::handle` | `orchestrator/mod.rs` | 93 |
| `replan::drive` | `orchestrator/replan.rs` | — |
| `Planner::plan` (verdict parsing) | `orchestrator/planner.rs` | 77 (parse_verdict) |
| `DagExecutor::run` | `orchestrator/executor.rs` | 64 |
| `ready_steps` | `orchestrator/plan.rs` | 94 |
| `tokio::spawn` per step | `orchestrator/executor.rs` | ~85 |
| `run_step_with_retry` | `orchestrator/retry.rs` | — |
| `ChatWorker::run_step` | `orchestrator/wiring.rs` | 47 |
| `MemoryInjector::for_turn` | `memory/injector.rs` | — |
| `ChatServiceFactory::run_turn` | `orchestrator/wiring.rs` | 30 (trait) |
| `MemoryWriter::sync_turn` | `memory/writer.rs` | — |
| `collect_engine_response` | `engine_builder.rs` | 598 |

---

## 4. Sequential execution — example

### Prompt
> "Look up the HTTP status codes for 200 and 404, then write a two-sentence summary contrasting them."

### Plan emitted

```json
{
  "kind": "plan",
  "steps": [
    {"id": "s1", "agent": "researcher", "goal": "Gather concise descriptions of HTTP status codes 200 and 404", "depends_on": []},
    {"id": "s2", "agent": "writer",     "goal": "Write a two-sentence summary contrasting 200 and 404 from s1 output", "depends_on": ["s1"]}
  ]
}
```

### DAG shape

```mermaid
flowchart LR
    s1["s1: researcher<br/>deps=[]"]:::done --> s2["s2: writer<br/>deps=[s1]"]:::done
    s2 --> leaf(["final_response"]):::leaf

    classDef done fill:#dfe,stroke:#0a0,color:#060;
    classDef leaf fill:#fff,stroke:#666,stroke-dasharray:4 4;
```

### Timeline (as observed in logs)

```
T+0.0s   orchestrator:plan_created  steps=[s1:researcher(deps=[]), s2:writer(deps=[s1])]
T+0.1s   orchestrator:step_started  s1:researcher
T+0.1s   http_request  GET https://developer.mozilla.org/.../Status/200
T+1.4s   http_request  GET https://developer.mozilla.org/.../Status/404
T+14.2s  orchestrator:step_succeeded  s1   ← waits for s1 before starting s2
T+14.3s  orchestrator:step_started  s2:writer
T+22.8s  orchestrator:step_succeeded  s2
T+22.9s  orchestrator:plan_completed  cancelled=false
```

### Key invariant proved

`s2` starts **only after** `s1 succeeds` (gap between lines 4 and 5 above). `DagExecutor::run` at `executor.rs:64` loops:

1. `let ready = plan.ready_steps(&completed)` — `s2` is NOT in ready while `s1` is in-flight (its dep isn't completed).
2. When s1 succeeds, `completed.insert(s1)`, next iteration includes s2 in ready.
3. `render_step_inputs` at `executor.rs:~175` builds `<step-input from="s1">...</step-input>` and prepends to s2's user message.

### Code reference

`orchestrator/plan.rs:94` — `Plan::ready_steps`:
```rust
pub fn ready_steps(&self, completed: &HashSet<StepId>) -> Vec<&Step> {
    self.steps.iter().filter(|s|
        !completed.contains(&s.id)
        && s.depends_on.iter().all(|d| completed.contains(d))
    ).collect()
}
```

This is THE entire sequential-vs-parallel logic. Sequential works because `s2.depends_on = [s1]` — `ready_steps` returns `[s1]` only while `s1` runs, then `[s2]` after it completes.

---

## 5. Parallel execution — example

### Prompt
> "In parallel, give a one-line definition of each of: unicorn startup, decacorn startup, hectocorn startup, zebra startup. Then list all four with their definitions."

### Plan emitted

```json
{
  "kind": "plan",
  "steps": [
    {"id": "s1", "agent": "researcher", "goal": "Define 'unicorn startup' in one sentence", "depends_on": []},
    {"id": "s2", "agent": "researcher", "goal": "Define 'decacorn startup' in one sentence", "depends_on": []},
    {"id": "s3", "agent": "researcher", "goal": "Define 'hectocorn startup' in one sentence", "depends_on": []},
    {"id": "s4", "agent": "researcher", "goal": "Define 'zebra startup' in one sentence", "depends_on": []},
    {"id": "s5", "agent": "writer",     "goal": "List all four terms with their definitions from s1-s4", "depends_on": ["s1","s2","s3","s4"]}
  ]
}
```

### DAG shape

```mermaid
flowchart LR
    s1["s1: researcher<br/>unicorn"]:::done --> s5
    s2["s2: researcher<br/>decacorn"]:::done --> s5
    s3["s3: researcher<br/>hectocorn"]:::done --> s5
    s4["s4: researcher<br/>zebra"]:::done --> s5
    s5["s5: writer<br/>synthesizer<br/>deps=[s1,s2,s3,s4]"]:::done --> leaf(["final_response"]):::leaf

    classDef done fill:#dfe,stroke:#0a0,color:#060;
    classDef leaf fill:#fff,stroke:#666,stroke-dasharray:4 4;
```

### Timeline — proof of parallelism

```
T+0.0s   orchestrator:plan_created  steps=[s1:r(deps=[]), s2:r(deps=[]), s3:r(deps=[]), s4:r(deps=[]), s5:w(deps=[s1,s2,s3,s4])]
T+0.1s   orchestrator:step_started  s1:researcher  ┐
T+0.1s   orchestrator:step_started  s2:researcher  │ all four start
T+0.1s   orchestrator:step_started  s3:researcher  │ within ~100ms
T+0.1s   orchestrator:step_started  s4:researcher  ┘
T+0.2s   http_request  ... (for s3, interleaved)
T+0.3s   http_request  ... (for s1, interleaved)
T+0.3s   http_request  ... (for s4, interleaved)
T+0.4s   http_request  ... (for s2, interleaved)
...
T+8.9s   orchestrator:step_succeeded  s2
T+9.2s   orchestrator:step_succeeded  s3
T+9.5s   orchestrator:step_succeeded  s1
T+9.8s   orchestrator:step_succeeded  s4  ← s5 can now start
T+9.9s   orchestrator:step_started  s5:writer
T+15.1s  orchestrator:step_succeeded  s5
T+15.2s  orchestrator:plan_completed
```

### Key invariant proved

**Interleaving of tool calls** is the signature of true concurrency. If one researcher had done all four sequentially, tool calls would be grouped by researcher (all of s1's calls, then all of s2's, etc.). The interleaving visible in timeline rows 5–8 = `tokio::spawn` ran them concurrently.

### Code reference

`orchestrator/executor.rs:64` — the loop:
```rust
for step in ready {
    if !in_flight.contains(&step.id) {
        in_flight.insert(step.id.clone());
        futures.push(tokio::spawn(async move {
            let outcome = run_step_with_retry(&step_clone, &step_inputs, worker, &policy, &events).await;
            (step_clone.id, outcome)
        }));
    }
}
```

Every ready step gets its own `tokio::spawn`. `FuturesUnordered` awaits whichever completes first — no ordering between parallel siblings.

---

## 6. Mixed (diamond) execution — example

### Prompt
> "First enumerate two plausible interpretations of the question 'what is an agent?'. Then, in parallel, research each interpretation — one from the AI/LLM perspective, one from the real-estate broker perspective. Finally, write a short note acknowledging both meanings exist."

### Plan emitted

```json
{
  "kind": "plan",
  "steps": [
    {"id": "s1", "agent": "researcher", "goal": "Enumerate two plausible interpretations of 'what is an agent?'", "depends_on": []},
    {"id": "s2", "agent": "researcher", "goal": "Research the AI/LLM interpretation from s1 output", "depends_on": ["s1"]},
    {"id": "s3", "agent": "researcher", "goal": "Research the real-estate broker interpretation from s1 output", "depends_on": ["s1"]},
    {"id": "s4", "agent": "writer",     "goal": "Write a short note acknowledging both meanings based on s2 and s3", "depends_on": ["s2","s3"]}
  ]
}
```

### DAG shape (the diamond)

```mermaid
flowchart LR
    s1["s1: researcher<br/>enumerate"] --> s2["s2: researcher<br/>AI view"]
    s1 --> s3["s3: researcher<br/>real-estate view"]
    s2 --> s4["s4: writer<br/>synthesizer"]
    s3 --> s4
    s4 --> leaf([final_response]):::leaf

    classDef leaf fill:#fff,stroke:#666,stroke-dasharray:4 4;
```

### Timeline

```
T+0.0s   plan_created  s1→(s2,s3)→s4
T+0.1s   step_started  s1
         (s2 and s3 NOT yet started — waiting for s1)
T+6.3s   step_succeeded  s1
T+6.4s   step_started  s2     ┐ parallel fan-out
T+6.4s   step_started  s3     ┘ after s1 complete
T+14.1s  step_succeeded  s2
T+14.9s  step_succeeded  s3
T+15.0s  step_started  s4     ← waits for BOTH s2 and s3
T+20.2s  step_succeeded  s4
T+20.3s  plan_completed
```

### Two invariants proved

1. **s2 and s3 don't start until s1 succeeds** — `ready_steps` excludes them while `s1.id ∉ completed`.
2. **s4 doesn't start until both s2 AND s3 succeed** — `depends_on.iter().all(...)` requires ALL deps completed.

### Code reference

Same `ready_steps` logic as before, but exercising the `.all()` predicate:

```rust
s.depends_on.iter().all(|d| completed.contains(d))  // s4 waits for both
```

---

## 7. Replan — example

### Prompt
> (Same as diamond above)

### Observed replan scenario

The planner's first plan was valid topology but used an invented agent name `analyst`:

```json
{"steps": [
  {"id": "s1", "agent": "analyst", "goal": "...", "depends_on": []},  ← "analyst" not in roster
  ...
]}
```

### Flow

```mermaid
flowchart TB
    A["Plan #1 — s1:analyst ..."] --> B{"plan.validate<br/>known_agents=[orchestrator, researcher, writer]"}
    B -->|PlanError::UnknownAgent| C["run_step_with_retry<br/>3 attempts"]
    C -->|all fail| D["StepExhausted"]
    D --> E["ReplanTriggered"]
    E --> F["planner.replan with failure context"]
    F --> G["Plan #2 — s1:researcher ..."]
    G --> H{"plan.validate"}
    H -->|Ok| I["DagExecutor::run"]
    I --> J["PlanCompleted"]
```

### Timeline

```
T+0.0s   plan_created  steps=[s1:analyst(deps=[]), ...]   ← bad agent
T+0.1s   step_started  s1:analyst
T+0.2s   step_failed   s1 attempt=1 err=unknown agent: analyst
T+0.3s   step_failed   s1 attempt=2 err=unknown agent: analyst
T+0.4s   step_failed   s1 attempt=3 err=unknown agent: analyst
T+0.5s   step_exhausted  s1
T+0.5s   replan_triggered  unknown agent: analyst
T+3.2s   plan_created  steps=[s1:researcher(deps=[]), ...]   ← repaired plan
...continues normally
```

### Code reference

- Validation rejects: `orchestrator/plan.rs::validate` (lines ~60)
- Retry at step level: `orchestrator/retry.rs::run_step_with_retry` — 3 attempts then `StepExhausted`
- Replan driver: `orchestrator/replan.rs::drive`:

```rust
ExecResult::NeedsReplan { failed, error } => {
    if replans_left == 0 { /* bail */ }
    replans_left -= 1;
    events.emit(ReplanTriggered { reason: error.clone() });
    plan = planner.replan(user_msg, plan, failed, error).await?;
}
```

Bounded by `OrchestratorConfig.max_replans` (default 2).

---

## 8. How each channel provides the factory

The same `Orchestrator::handle` works for all three channels. They differ only in **how** they build the `ChatInputsFn` closure that `RuntimeChatServiceFactory::run_turn` calls.

### 8.1 Telegram (per-message snapshot)

```mermaid
flowchart TB
    Msg["user message arrives"] --> HotReload["hot-reload every agent<br/>(skill registry)"]
    HotReload --> BuildAll["for each agent in agent_states:<br/>build ChatTurnInputs snapshot"]
    BuildAll --> Publish["orchestrator_snapshots.write(snapshot_map)"]
    Publish --> Handle["orchestrator.handle(user_content)"]

    Handle -.during execution.-> Factory["factory.run_turn(agent, content)"]
    Factory --> Read["snapshots.read().get(agent).clone()"]
    Read --> BuildService["build ChatRuntimeService&lt;'a&gt; borrowing the snapshot"]
    BuildService --> Loop["collect_engine_response + tool loop"]
```

**Code:** `telegram_builder.rs:1397` — `execute_orchestrator_turn`. Snapshot loop at lines 1416–1486. Publish at 1488. Handle at 1521.

### 8.2 TUI (per-turn snapshot)

Same shape; the engine thread's `ChatRequest::UserMessage` branch writes snapshots before `rt.block_on(orch.handle(text))`.

**Code:** `tui/mod.rs:~540+` — same snapshot pattern.

### 8.3 Eval (per-step factory closure)

The eval runner doesn't need snapshots — it rebuilds the per-agent ChatRuntimeService inside each `run_turn` call, using the `EvalRowAccum` to thread the row's stubs + observation tap across every worker step.

**Code:** `eval_builder.rs::EvalChatServiceFactory::run_turn` — search for the comment "Shared state threaded through every worker step".

---

## 9. Retention pipeline

How disk artifacts get bounded across many runs.

### 9.1 Sources of disk writes

```mermaid
flowchart TB
    subgraph EvalRun["Single eval run"]
        Run["tengu eval <skill>"] --> Top["top-level run dir:<br/><code>evals/runs/&lt;ts&gt;/</code>"]
        Top --> PerRow["per row: <code>&lt;skill&gt;-&lt;id&gt;.md</code><br/>(transcript)"]
        Top --> Report["<code>report.json</code>"]
    end

    subgraph SkillLifecycle["Per-skill lifecycle artifacts"]
        Finalize["finalize_run"] --> Metrics["<code>metrics.json</code> (rolling)"]
        Finalize --> History["<code>history.jsonl</code>"]
        Finalize --> PerSkillRun["per-skill per-run detail:<br/><code>skills/&lt;name&gt;/metrics/runs/&lt;ts&gt;/</code>"]
    end

    Run -.also triggers.-> Finalize

    classDef bounded fill:#dfe,stroke:#0a0;
    class Top,PerSkillRun bounded;
```

### 9.2 Pruning rules

| Artifact | Retention knob | Default | Location |
|---|---|---|---|
| `evals/runs/<ts>/` | `--keep-runs N` | 10 | `eval_builder.rs::prune_old_run_dirs` |
| `skills/<name>/metrics/runs/<ts>/` | `--max-runs N` | 10 | `eval_builder.rs::finalize_run` (passed via `RunSkillOptions.max_per_run_reports`) |
| `metrics.json` | always kept (rolling) | — | — |
| `history.jsonl` | always appended | — | — |

### 9.3 `--no-persist` — skip all writes

```mermaid
flowchart LR
    Args["--no-persist"] --> Skip["RunSkillOptions.persist = false"]
    Skip --> Skip1["run_row: skip write_transcript"]
    Skip --> Skip2["run_skill: skip report.json"]
    Skip --> Skip3["finalize_run: early-return (no metrics.json / history.jsonl write)"]

    Summary["in-memory SkillReport still returned"] -.prints.-> Terminal["table/JSON summary to stdout"]

    Skip --> Summary
```

### 9.4 Example — 100 runs, bounded

Without retention: 100 runs × ~9 rows × 1 transcript each + 100 report.json + 100 per-skill run dirs = ~1000+ files.

With default retention:
- `evals/runs/` → 10 dirs × 9 transcripts + 10 report.json = 100 files max
- `skills/<name>/metrics/runs/` → 10 dirs max
- `metrics.json` + `history.jsonl` → 2 files (rolling)

**Total bound: ~112 files regardless of run count.**

With `--no-persist`: **0 files written**.

### 9.5 `.gitignore` rules

```
/evals/runs/
/skills/*/metrics/runs/
```

So even if retention fires AFTER a run (pruning last time), nothing between runs shows up in `git status`.

---

## 10. Quick reference — how to use

### Just validate it works
```bash
./target/release/tengu eval orchestration-e2e --keep-runs 3
# Expect 9/9 pass, ~5 min, ~$0.40
```

### Iterate without polluting repo
```bash
./target/release/tengu eval orchestration-e2e --no-persist --filter parallel_fan_out
# Zero files written, single row, ~$0.05
```

### Interactive (TUI with memory)
```bash
# See docs/configs/tui-memory-smoke.toml for setup
cd ~/tengu-memory-smoke
/Users/vladimirdemidov/development/tengu-cluster/target/release/tengu chat
# Then manually test scenarios from docs/orchestration-test-scenarios.md
```

### Inspect observations post-run
```bash
ls evals/runs/                                                    # retained dirs
cat evals/runs/<ts>/orchestration-e2e-parallel_fan_out.md         # tool calls + events + final text
```

---

## 11. Complete code reference table

| Concept | File | Key functions/types |
|---|---|---|
| **Orchestrator public API** | `src/adapters/orchestrator/mod.rs` | `Orchestrator::new`, `::handle`, `::subscribe`, `::cancel` |
| **Replan loop** | `src/adapters/orchestrator/replan.rs` | `drive(planner, msg, worker, policy, max_replans, events, cancel)` |
| **Planner** | `src/adapters/orchestrator/planner.rs` | `Planner` trait, `OrchestratorAgentPlanner`, `parse_verdict`, `extract_balanced_json_object` |
| **DAG executor** | `src/adapters/orchestrator/executor.rs` | `DagExecutor::run`, `WorkerHandle` trait, `ExecResult` |
| **Retry** | `src/adapters/orchestrator/retry.rs` | `RetryPolicy`, `run_step_with_retry`, backoff `{1s, 3s, 9s}` |
| **Plan types + topology** | `src/adapters/orchestrator/plan.rs` | `Step`, `StepId`, `Plan::ready_steps`, `Plan::validate`, `Plan::single_leaf` |
| **Events** | `src/adapters/orchestrator/events.rs` | `OrchestratorEvent`, `EventBus`, `new_bus()` |
| **ChatServiceFactory wiring** | `src/adapters/orchestrator/wiring.rs` | `ChatServiceFactory` trait, `ChatWorker`, `ChatOrchestratorPortImpl` |
| **Roster rendering** | `src/adapters/orchestrator/roster.rs` | `render_roster(agents, exclude)`, `substitute_roster(template, md)` |
| **Telegram channel dispatch** | `src/adapters/telegram_builder.rs` | `execute_orchestrator_turn` (line 1397), orchestrator_snapshots field (line 497) |
| **TUI channel dispatch** | `src/adapters/tui/mod.rs` | orchestrator branch in engine thread (~line 540) |
| **Eval channel dispatch** | `src/adapters/eval_builder.rs` | `run_row_via_orchestrator`, `EvalChatServiceFactory` |
| **Eval retention** | `src/adapters/eval_builder.rs` | `prune_old_run_dirs`, `EvalArgs::keep_runs` / `no_persist` / `max_per_run_reports` |
| **Memory provider** | `src/adapters/memory/provider.rs` | `MemoryProvider` trait |
| **Memory manager** | `src/adapters/memory/manager.rs` | `MemoryManager::{add_provider, prefetch_all, sync_all, ingest_one, ingest_batch, search, delete_entry, clear_all, stats}` |
| **Pre-turn injection** | `src/adapters/memory/injector.rs` | `for_turn(mgr, agent, query) -> PinnedMemoryBlock` |
| **Post-turn write** | `src/adapters/memory/writer.rs` | `sync_turn(mgr, agent, user, asst)` — spawned |
| **Fenced block** | `src/adapters/memory/fencing.rs` | `build_memory_context_block`, `sanitize_context` |
| **Builtin provider** | `src/adapters/memory/builtin.rs` | `BuiltinMemoryProvider` |
| **Vector store trait** | `src/adapters/memory/vector.rs` | `VectorStore` trait: `write`, `search`, `delete`, `clear_all`, `entry_count`, `storage_bytes` |
| **Disk store** | `src/adapters/memory/vector/disk.rs` | `DiskVectorStore`, bincode-backed |
| **Qdrant store** | `src/adapters/memory/vector/qdrant.rs` | `QdrantVectorStore`, optional feature |
| **Embedder** | `src/adapters/memory/vector/embedder.rs` | `Embedder::embed`, `::embed_batch` (single HTTP call for N inputs) |
| **Channel runtime helpers** | `src/adapters/channel_runtime.rs` | `build_orchestrator`, `build_memory_manager`, `RuntimeChatServiceFactory`, `OrchestratorSnapshots`, `snapshots_inputs_fn` |
| **Config schema** | `src/adapters/config.rs` | `OrchestratorConfig` (agent, max_attempts_per_step, max_replans, route_explicit_agents) |

---

## 12. Related docs

| Doc | Purpose |
|---|---|
| `docs/architecture.md` | Top-level doctrine (what the harness owns) |
| `docs/harness-architecture.md` | Full subsystem map, file tables, request lifecycle |
| `docs/orchestration-test-scenarios.md` | 20 manual TUI/Telegram test scenarios (S-01 through S-20) |
| `docs/validation-checklist.md` | Top-to-bottom validation runbook (~30 min pass) |
| `docs/configs/tui-memory-smoke.toml` | Ready-to-paste config for memory scenarios |
| `docs/superpowers/specs/2026-04-20-harness-orchestration-memory-design.md` | Original design spec |
| `docs/superpowers/plans/2026-04-20-harness-orchestration-memory.md` | Original implementation plan (40 tasks) |
| `skills/orchestration-e2e/evals/prompts.yaml` | 9 automated eval rows |
| `skills/orchestration-e2e/evals/config.toml` | Eval-runner config (prompt + agents) |
