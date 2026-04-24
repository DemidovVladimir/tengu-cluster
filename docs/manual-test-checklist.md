# Tengu-cluster manual smoke test

Run this at the end of every implementation phase. Both the TUI and Telegram channels must pass before a phase is considered landed. Phase 0 captures the baseline outputs in the "Regression baselines" section at the bottom so that later phases have a concrete diff target; if a later phase changes wording or behaviour, record whether the delta is expected and update the baseline only when the new behaviour is the intended one.

## Pre-flight

- [ ] `cargo build --release` completes with no errors or warnings
- [ ] `cargo test` passes with no failures
- [ ] `cargo clippy --all-targets -- -D warnings` passes with no warnings
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

Launch with `cargo run -- telegram --sandbox aura`. This minimal probe confirms the `--sandbox` flag is respected in Telegram mode as well as TUI mode.

- [ ] Bot starts without panic and logs indicate the aura sandbox config was loaded
- [ ] `/start` returns a welcome message
- [ ] A single aura-scoped probe (e.g. `summarize what this sandbox is for`) returns a coherent response

## Debug probes (land in Phase 1+)

These commands do not yet exist in Phase 0. Leave them unchecked with the noted phase annotation until the corresponding phase has landed, then flip them on during that phase's checklist run.

- [ ] `tengu registry list` shows agents + skills + tools — Phase 1+
- [ ] `tengu registry search "<query>"` returns sensible rankings — Phase 1+
- [ ] Qdrant dashboard at http://localhost:6334/dashboard shows the `tengu_registry`, `tengu_messages`, and `tengu_outputs` collections — Phase 1-2+

## Regression baselines

Fill these once during Phase 0. On every later phase, re-run the same commands and compare. Minor wording differences from the model are acceptable; missing events, new warnings, panics, or dropped functionality are not.

### Phase 0 baseline: `cargo run -- chat` with `> hello`

```
<!-- paste actual output here -->
```

### Phase 0 baseline: `cargo run -- telegram` with `/start`

```
<!-- paste actual output here -->
```

### Phase 0 baseline: stderr on clean startup (first 20 lines)

```
<!-- paste actual output here -->
```

## Rollback procedure

If a new phase breaks a check above, `git checkout main` to return to the last known-good state. Re-run this checklist against `main` to confirm the baseline is still healthy and the regression is specific to the in-progress branch. Then debug the branch in isolation.
