# Tengu Cluster — Claude Project Instructions

Read this before making changes. These are the project-specific rules that override default Claude behaviour.

## Project shape

- Rust binary crate. Single `tengu` binary at `target/debug/tengu` (or `target/release/tengu`).
- Flat layout: all code in `src/adapters/*.rs` + `src/main.rs`. No sub-crates. Per-subsystem `*_builder.rs` files (~25 of them).
- Dual engine backends behind feature flags: `openrouter` (default) and `claude_code` (opt-in). Also: `telegram`, `qdrant`.
- Orchestration is skill-driven, not Rust-driven. Rust owns primitives (`sessions_spawn`, `sessions_fan_out`, tools, memory, cache); the LLM reads `skills/orchestration/SKILL.md` and decides when to decompose.
- Tool plugins live under `src/adapters/plugins/{workspace,http,crypto,cache,memory,skill,subagents,mcp}/`. Every `Tool::execute` body starts with a `ctx.scope.check_*()` call — `tests/scope_lint.rs` enforces this.

## Hard rules (non-negotiable)

- **Cap every `cargo` Bash call at ≤ 30 seconds.** Never run bare `cargo test` — it builds the entire workspace with all features and can take minutes. Always scope: `cargo test --bin tengu -- <module>::tests` or `cargo check --bin tengu`.
- **Small changes, verify.** One change at a time. Compile + test before moving on. Don't batch edits.
- **Don't oversell fixes.** A "fix" that doesn't enforce anything at runtime is documentation, not protection. Say so.
- **Skills are portable.** Never modify a skill's body to fit the platform. Skills copy-paste between Tengu, OpenClaw, etc. If something's wrong, either fix the platform or replace the skill file.
- **Keep agent names visible.** Tool-call UI shows which agent fired each call. Don't suppress agent names in the name of "clean UX".
- **Use `rg` instead of `grep`.**

## Testing this project

| Purpose | Command |
|---|---|
| Compile-only check | `cargo check --bin tengu` |
| Scoped unit tests | `cargo test --bin tengu -- <module>::tests` |
| Scope-enforcement lint | `cargo test --test scope_lint` |
| Skill eval (requires `OPENROUTER_API_KEY`) | `cargo run -- eval <skill>` |
| End-to-end eval smoke | `cargo test --bin tengu --features eval-integration -- eval_builder::tests::orchestration_evals_smoke --nocapture` |

If a failure is flaky, don't retry in a loop — find the root cause.

## Skill evals

Every skill that wants automated testing drops two files:

```
skills/<skill>/evals/prompts.md     # | Prompt | Expected behaviour | table
skills/<skill>/evals/config.toml    # agent config with `workspace = "{TMP_WORKSPACE}"`
```

`tengu eval <skill>` replays the prompts, scores each row pass/fail with an LLM judge (default: opus-4-7 via OpenRouter), and writes `evals/runs/<ts>/report.json` + per-row markdown transcripts. Exit codes: 0 pass / 1 row failure / 2 runner error. Full spec at `docs/superpowers/specs/2026-04-19-eval-runner-design.md`. `skills/orchestration/evals/` is the canonical example. `evals/runs/` is gitignored.

## Branch conventions

- Feature work goes on `feature/<name>` branches off `main`.
- Design specs live at `docs/superpowers/specs/YYYY-MM-DD-<topic>-design.md`. Implementation plans at `docs/superpowers/plans/YYYY-MM-DD-<topic>.md`.
- When implementing a plan, use `superpowers:subagent-driven-development` — one fresh subagent per task, two-stage review (spec compliance, then code quality).
- Never force-push to `main`. Never skip git hooks (`--no-verify`, `--no-gpg-sign`).
- Commit messages follow the existing style — `feat(scope):`, `fix(scope):`, `refactor(scope):`, `docs(scope):`. Include `Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>` trailer.

## Workspace hygiene

- Secrets: encrypted vault at `~/.tengu/secrets.vault`, master password in `TENGU_MASTER_PASSWORD` env var.
- Don't commit: `.env`, anything in `~/.tengu/`, anything in `evals/runs/`, anything in `target/`.
- Config example is `config.example.toml`; user configs live at `~/.tengu/config.toml` or `sandboxes/<name>/config.toml`.

## Architecture

- **Plugin architecture** (Phase A, merged 2026-04-17): tools live in `src/adapters/plugins/<name>/` as `Tool` trait impls, registered via `ToolRegistry`, dispatched through `PluginToolExecutor`. `ToolScope` in `ports.rs` gates per-call resource access.
- **Orchestration** (Phase B, open on PR #5): main agent loads `skills/orchestration/SKILL.md` and decides when to call `sessions_spawn` / `sessions_fan_out`. No central Plan/DAG state machine.
- **Memory** is OpenClaw-aligned: `remember` tool + optional `persistent_store` + MEMORY.md / daily-log bootstrap injection + pre-compaction flush. Vector backend is bincode by default, Qdrant with `--features qdrant`.

## Where things live

| Subsystem | File |
|---|---|
| CLI entry point | `src/main.rs` |
| Engine trait + `collect_engine_response` tool loop | `src/adapters/engine_builder.rs` |
| Tool plugin core | `src/adapters/tool_plugin.rs` |
| Channel runtime (tool executor + system prompt builders) | `src/adapters/channel_runtime.rs` |
| Chat runtime (per-turn flow, 2-phase pruning) | `src/adapters/chat_builder.rs` |
| Memory service + disk store | `src/adapters/memory_builder.rs` |
| Skills registry + system prompt composition | `src/adapters/skill_builder.rs` |
| Orchestrator (CLI fleet boot) | `src/adapters/orchestrator.rs` |
| Telegram adapter | `src/adapters/telegram_builder.rs` |
| Eval runner (`tengu eval`) | `src/adapters/eval_builder.rs` |
| MCP bridge (exposes Tengu tools to external Claude Code) | `src/adapters/mcp_bridge.rs` |
| Shell executor | `src/adapters/shell_executor.rs` |
