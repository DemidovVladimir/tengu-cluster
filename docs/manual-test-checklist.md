# Tengu-cluster manual smoke test

Run this at the end of every implementation phase. Both the TUI and Telegram channels must pass before a phase is considered landed. Phase 0 captures the baseline outputs in the "Regression baselines" section at the bottom so later phases have a concrete diff target; if a later phase changes wording or behaviour, record whether the delta is expected and only update the baseline when the new behaviour is the intended one.

## Pre-flight

- [ ] `bash scripts/phase-0-checks.sh` exits 0
- [ ] `cargo build --release` completes with no errors or warnings
- [ ] `cargo test` passes with no failures
- [ ] `cargo clippy --all-targets -- -D warnings` passes
- [ ] `tests/scope_lint.rs` passes (required structural test for the repo)
- [ ] Qdrant reachable: `curl http://localhost:6334/collections` returns HTTP 200
- [ ] `git status --short` shows only the changes expected for the current phase

## TUI smoke (default agent)

Launch with `cargo run -- chat`. The TUI must come up without a panic and present a prompt.

- [ ] `> hello` produces a direct conversational response in under 5 seconds
- [ ] `> what agents do you have?` lists agents from the roster (static mode) or returns RAG hits (rag mode), matching the mode configured for this phase
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

## Debug probes (become available in later phases)

- [ ] `tengu registry list` (Phase 1+): lists compiled-in tools + MCP tools + agents + skills
- [ ] `tengu registry search "<query>"` (Phase 1+): returns top-10 ranked entries
- [ ] Qdrant dashboard at http://localhost:6334/dashboard shows `tengu_registry`, `tengu_messages`, `tengu_outputs` (Phase 1–2)

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

## Rollback procedure

1. `git checkout main` — restore the last known-good state.
2. Re-run this checklist against `main` to confirm it is healthy.
3. Debug the broken phase branch off `main` with a clean slate.
