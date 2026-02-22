# Rust Patterns Playbook (Tengu)

This file is a practical guide to advanced Rust patterns used in this repository.

Use it for:
- onboarding
- code reviews
- picking the right pattern before implementing new features

## 1. Lock File + RAII Cleanup

Pattern:
- acquire lock by creating a file with `create_new(true)`
- release lock automatically in `Drop`

Where:
- `src/flow_store.rs` (`IndexLockGuard`, `acquire_index_lock`)

Why:
- prevents concurrent writers from corrupting shared index state
- guarantees unlock even on early-return paths

## 2. Atomic File Replace (`.tmp` + `rename`)

Pattern:
- write full content to temp file
- `rename` temp file over target

Where:
- `src/flow_store.rs` (`write_index_atomic`)

Why:
- avoids partial file state if process crashes during write
- keeps index file valid-or-old, never half-written

## 3. Cooperative Stream Termination (`oneshot` + `tokio::select!`)

Pattern:
- background task listens to both input stream and shutdown signal
- shutdown sender is stored and triggered on disconnect

Where:
- `crates/tengu-channels/src/cli/mod.rs` (`connect`, `disconnect`)

Why:
- explicit lifecycle control for long-running async loops
- clean stop without orphaned tasks

## 4. Streamed Engine Contract via Trait Objects

Pattern:
- engine returns `Pin<Box<dyn Stream<Item = StreamEvent> + Send>>`

Where:
- `crates/tengu-core/src/lib.rs` (`Engine::run`)
- `crates/tengu-core/src/types/stream.rs` (`StreamEvent`)
- `crates/tengu-backends/src/ollama/mod.rs`

Why:
- one runtime loop can consume different backends uniformly
- supports text deltas, usage events, errors, done signals

## 5. Saturating Arithmetic for Budget Safety

Pattern:
- use `saturating_add`, `saturating_sub`, `min`, `max`

Where:
- `src/runtime_prompt.rs` (prompt/history/retrieval budget calculations)
- `src/main.rs` (budget integration into chat loop)
- `crates/tengu-memory/src/lib.rs` (`query_with_budget`)

Why:
- prevents underflow/overflow edge bugs in budget logic
- keeps behavior deterministic under tight limits

## 6. Borrow Without Clone (`as_deref`)

Pattern:
- convert `Option<String>` to `Option<&str>` with `as_deref()`

Where:
- `src/main.rs`
- `src/flow_store.rs`
- `crates/tengu-core/src/routing/mod.rs`

Why:
- avoids unnecessary allocations
- keeps APIs ergonomic for borrowed lookups/comparisons

## 7. Deterministic Matching with Iterator Chains

Pattern:
- priority matching via chained `.find(...).or_else(...)`

Where:
- `crates/tengu-core/src/routing/mod.rs` (`resolve`)

Why:
- explicit precedence rules in one expression
- no hidden branch order surprises

## 8. Trait-Object Runtime Polymorphism

Pattern:
- `Box<dyn Engine>`, `Box<dyn Refiner>` selected by config at runtime

Where:
- `src/main.rs`

Why:
- plug new providers/optimizers without changing orchestration code

## 9. Serde Defaults for Config Evolution

Pattern:
- `#[serde(default)]` + explicit default functions

Where:
- `crates/tengu-core/src/config/schema.rs`

Why:
- forward/backward-friendly config parsing
- adding new fields does not break existing user config files

## 10. Context-Rich Errors (`anyhow::Context`)

Pattern:
- wrap IO/parse errors with operation-specific context

Where:
- `src/flow_store.rs`

Why:
- faster debugging in production
- clearer diagnostics from nested failures

## 11. Content-Addressed Upsert

Pattern:
- hash content and skip re-index if unchanged
- upsert by key (`retain` + `push`)

Where:
- `crates/tengu-memory/src/lib.rs` (`ingest`)

Why:
- avoids redundant work
- keeps in-memory index fresh with simple logic

## 12. `Cow` Use Cases (Important for Future Optimizations)

`Cow` is not heavily used yet, but it is useful for zero-copy paths.

Current related usage:
- `to_string_lossy()` returns `Cow<str>` in `crates/tengu-memory/src/lib.rs`

Recommended use cases:
1. String normalization that is often no-op:
   - return `Cow<'a, str>` so unchanged input stays borrowed
2. Path/display conversion:
   - keep borrowed path text unless invalid UTF-8 forces allocation
3. Pre-prompt transforms:
   - avoid cloning large content unless modification is required

Minimal example:
```rust
use std::borrow::Cow;

fn trim_if_needed(input: &str) -> Cow<'_, str> {
    if input.starts_with(' ') || input.ends_with(' ') {
        Cow::Owned(input.trim().to_string())
    } else {
        Cow::Borrowed(input)
    }
}
```

## 13. What to Prefer in This Repo

- Prefer deterministic behavior over cleverness.
- Prefer RAII cleanup for resources/locks.
- Prefer explicit budget math with saturating operations.
- Prefer typed contracts (`enum`, trait methods) over ad-hoc maps/strings.
- Prefer borrowed data (`&str`, `as_deref`, `Cow`) before cloning.

## 14. Review Checklist for New Rust Code

Before merging, check:
1. Does this need a lock/atomic write pattern?
2. Can this overrun token/file/resource budgets?
3. Can this avoid allocations with borrowing or `Cow`?
4. Is the async task lifecycle cancellable/terminable?
5. Are error contexts descriptive enough for production debugging?
