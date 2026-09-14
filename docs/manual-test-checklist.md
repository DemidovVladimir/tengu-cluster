# Tengu-cluster manual smoke test

Run this at the end of every implementation phase. Both the TUI and Telegram channels must pass before a phase is considered landed. Phase 0 captures the baseline outputs in the "Regression baselines" section at the bottom so later phases have a concrete diff target; if a later phase changes wording or behaviour, record whether the delta is expected and only update the baseline when the new behaviour is the intended one.

## Pre-flight

- [ ] `cargo build --release` completes with no errors or warnings
- [ ] `cargo test` passes with no failures
- [ ] `cargo clippy --all-targets -- -D warnings` passes
- [ ] `tests/scope_lint.rs` passes (required structural test for the repo)
- [ ] (with `--features postgres_memory`) Postgres reachable: `psql "$TENGU_MEMORY_DATABASE_URL" -c 'select count(*) from agentic_memory'` succeeds
- [ ] `git status --short` shows only the changes expected for the current phase

## TUI smoke (default agent)

Launch with `cargo run -- chat`. The TUI must come up without a panic and present a prompt.

- [ ] `> hello` produces a direct conversational response in under 5 seconds
- [ ] `> what agents do you have?` lists agents from the roster (static mode) or returns Open Brain / Karpathy LLM Wiki hits (legacy planner mode), matching the mode configured for this phase
- [ ] `> research what LLMs were released in April 2026 and summarise the top 3` produces a visible plan, step events fire as the plan executes, and a final synthesised response is delivered
- [ ] `> /quit` exits the TUI cleanly with no panic and no dangling processes

## TUI smoke (aura sandbox)

Launch with `cargo run -- chat --sandbox aura`. The multi-agent DeSci roster defined in `sandboxes/aura/config.toml` must load.

- [ ] TUI starts without panic and the aura roster is visible (agent list or startup log)
- [ ] `> summarize what this sandbox is for` returns a coherent response grounded in the aura sandbox context
- [ ] `> /quit` exits cleanly

## Telegram smoke (default)

Launch with `cargo run -- telegram`. From the phone client associated with the configured bot token:

- [ ] `/start` returns the welcome message
- [ ] `hello` returns a direct reply
- [ ] `research what LLMs were released in April 2026` streams status updates during planning and execution, then delivers a final plan response
- [ ] Bot shuts down cleanly on Ctrl-C with no panic

## Telegram smoke (aura)

Launch with `cargo run -- telegram --sandbox aura`. The sandbox flag must take effect for the Telegram channel too.

- [ ] Bot starts against the aura roster without panic
- [ ] A short message from the phone gets a coherent reply grounded in the aura sandbox context
- [ ] Ctrl-C shuts down cleanly

## Debug probes

- [ ] `TENGU_PLANNER_REGISTRY.md` at the repo root is regenerated on a planner turn and lists agents + skills + core tools
- [ ] `TENGU_PLAN.md` at the repo root holds the last accepted plan
- [ ] (with `--features postgres_memory`) step summaries landed: `psql "$TENGU_MEMORY_DATABASE_URL" -c "select session_id, agent, left(content, 80) from agentic_memory order by created_at desc limit 5"`

## Regression baselines

Fill these in on the first run of Phase 0, then treat them as the diff target for every later phase. Record any intentional change inline as a comment.

### Phase 0 baseline — `cargo run -- chat` `> hello`

```
<!-- paste actual output here -->
```

### Phase 0 baseline — `cargo run -- telegram` + `/start`

```
<!-- paste actual output here -->
```

### Phase 0 baseline — stderr on clean startup (first 20 lines)

```
<!-- paste actual output here -->
```

## Skill lifecycle smoke

```
SKL-15  tengu skill seed german-teacher ./materials/ → skills/german-teacher/ exists with SKILL.md (frontmatter editable_by_learner=true, learner_facing=true) + resources/<copied files> + evals/prompts.yaml stub.
SKL-16  tengu skill seed against an existing skill name → refuses with clear error.
```

## Rollback procedure

1. `git checkout main` — restore the last known-good state.
2. Re-run this checklist against `main` to confirm it is healthy.
3. Debug the broken phase branch off `main` with a clean slate.
