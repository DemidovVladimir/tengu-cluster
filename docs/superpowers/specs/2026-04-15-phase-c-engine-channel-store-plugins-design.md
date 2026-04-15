# Phase C — Engine / Channel / Store Plugins (design stub)

**Status:** Design sketch, not implementation-ready. Intentionally thin — will be expanded into a full spec once Phase A + Phase B have landed and we have real usage feedback on the plugin pattern.
**Date:** 2026-04-15
**Depends on:** Phase A (tool plugin architecture) and Phase B (orchestration collapse) — both must be complete.
**Blocks:** Nothing. This is the final consolidating phase.

---

## 1. Problem

After Phase A, tool extensibility is clean: adding a new tool plugin or a new MCP server is a one-file change. After Phase B, orchestration behaviour lives in skills rather than Rust.

But three other extension points are still hand-wired with `#[cfg(feature = "…")]` scattered through the code:

- **Engines** — `OpenRouter` (in `engine_builder.rs`, 893 LOC, feature-gated on `openrouter`) and `ClaudeCode` (in `claude_code_engine.rs`, 653 LOC, feature-gated on `claude_code`). Adding a new backend (OpenAI, Gemini, local llama.cpp) today means editing `engine_builder.rs`, adding a feature flag, threading config, and updating every caller that matches on engine kind.
- **Channels** — `CliChannel` (in `chat_builder.rs`) and `TelegramChannel` (in `telegram_builder.rs`, feature-gated on `telegram`). After Phase B both drop through `channel_runtime.rs`, but the construction sites are still hard-coded. Adding Slack, Discord, Matrix, or mail today means editing several files.
- **Memory stores and embedders** — `DiskVectorMemoryStore` (default) and `QdrantMemoryStore` (feature-gated on `qdrant`). Adding pgvector, Redis, or a new embedder today means editing `memory_builder.rs` and the config loader.

Each extension point has its own ad-hoc pattern. None of them use the plugin + registry pattern Phase A established for tools, even though they would fit it exactly.

## 2. Goal

Apply the Phase A plugin pattern uniformly to engines, channels, memory stores, and embedders. Feature flags collapse to a single `#[cfg]` per `match` arm in each registry — the same place the tool plugins live. Adding a new backend becomes a one-file, one-config-entry change, identical in shape to adding a tool plugin.

**Expected LOC delta:** ~−800 to −1 200 (feature-flag scatter consolidates, construction sites dedupe). Depends on how much fat the OpenRouter + ClaudeCode engine implementations shed once they share a common registry lifecycle — to be measured during the detailed spec pass.

## 3. Non-goals

- **New engines, channels, stores, or embedders.** Phase C is a refactor. If a new backend is added at the same time it should be a follow-up PR after Phase C lands, so the refactor and the new code are reviewed independently.
- **Dynamic runtime plugin loading** (.so / WASM). Everything stays compile-time. Config selects which compiled-in plugins to instantiate.
- **Hot-reload of channels.** A channel is started once at boot and lives for the process lifetime. Restart to reconfigure.

## 4. Architecture sketch

Four new plugin traits, each mirroring `ToolPlugin` from Phase A:

### 4.1 `EnginePlugin`

```rust
#[async_trait]
pub(crate) trait EnginePlugin: Send + Sync {
    fn name(&self) -> &'static str;
    async fn build(&self, ctx: &EngineBuildCtx<'_>) -> Result<Arc<dyn Engine>>;
}
```

Impls: `OpenRouterEnginePlugin`, `ClaudeCodeEnginePlugin`, future `OpenAiEnginePlugin`, `GeminiEnginePlugin`, `LocalLlamaCppEnginePlugin`.

`Engine` itself is the existing trait in `types.rs` — unchanged. The plugin layer is just how engines are *constructed*, not how they're *called*.

Config:

```toml
[[engines]]
name = "openrouter"
api_key = "$OPENROUTER_API_KEY"
default_model = "anthropic/claude-opus-4-6"

[[engines]]
name = "claude_code"
# spawns the claude CLI as a subprocess
```

Agents pick an engine by name in their config:

```toml
[[agents]]
name = "researcher"
engine = "openrouter"
model = "anthropic/claude-opus-4-6"
```

### 4.2 `ChannelPlugin`

```rust
#[async_trait]
pub(crate) trait ChannelPlugin: Send + Sync {
    fn name(&self) -> &'static str;

    /// Start the channel listener. Long-running — the plugin owns its lifetime.
    /// Returns when the channel is shut down via the cancellation token.
    async fn start(&self, ctx: &ChannelCtx<'_>) -> Result<()>;
}

pub(crate) struct ChannelCtx<'a> {
    pub runtime:  &'a ChannelRuntime,   // session management
    pub tools:    &'a ToolRegistry,     // from Phase A
    pub plugins:  &'a PluginRegistry,   // from Phase A
    pub engines:  &'a EngineRegistry,   // from §4.1
    pub shutdown: tokio_util::sync::CancellationToken,
}
```

Impls:
- `CliChannel` — wraps today's `chat_builder.rs` entry point.
- `TelegramChannel` — wraps the post-B3 Telegram adapter.
- Future: `SlackChannel`, `DiscordChannel`, `MatrixChannel`, `MailChannel` (SMTP/IMAP).

Each channel's `start()` is spawned into a `tokio::JoinSet`; the runtime shuts them all down on `Ctrl+C` via the shared `CancellationToken`.

Config:

```toml
[[channels]]
name = "cli"

[[channels]]
name = "telegram"
token_env = "TELEGRAM_BOT_TOKEN"

[[channels]]
name = "slack"
webhook_url = "$SLACK_WEBHOOK"
bot_user = "@tengu"

[[channels]]
name = "mail"
imap_host = "imap.fastmail.com"
imap_user = "$IMAP_USER"
imap_password = "$IMAP_PASSWORD"
send_as = "tengu@yourdomain.com"
```

**Phase B §11.1 is the explicit contract that makes this possible.** Telegram and CLI must already share the same `open_session → send_user_message → stream_output` flow before Phase C can factor them into `ChannelPlugin` trivially.

### 4.3 `MemoryStorePlugin`

```rust
#[async_trait]
pub(crate) trait MemoryStorePlugin: Send + Sync {
    fn name(&self) -> &'static str;
    async fn build(&self, ctx: &PluginCtx<'_>) -> Result<Arc<dyn MemoryStorePort>>;
}
```

Impls: `DiskMemoryStorePlugin`, `QdrantMemoryStorePlugin`, future `PgVectorMemoryStorePlugin`, `RedisMemoryStorePlugin`.

`MemoryStorePort` is the existing trait in `ports.rs` (modernized to `#[async_trait]` in Phase A §4.7).

Config:

```toml
[memory]
store = "qdrant"      # references the plugin name
# store-specific options in the same table
qdrant_url = "http://localhost:6334"
qdrant_collection = "tengu-memory"
```

### 4.4 `EmbedderPlugin`

```rust
#[async_trait]
pub(crate) trait EmbedderPlugin: Send + Sync {
    fn name(&self) -> &'static str;
    async fn build(&self, ctx: &PluginCtx<'_>) -> Result<Arc<dyn EmbeddingPort>>;
}
```

Impls: `OpenRouterEmbedderPlugin`, future `LocalSentenceTransformersEmbedderPlugin`, `VoyageEmbedderPlugin`.

Config:

```toml
[memory]
embedder = "openrouter"
embedder_model = "text-embedding-3-small"
```

### 4.5 Registries

One registry per plugin kind, each built from config in `PluginRegistry::from_config`:

```rust
pub(crate) struct PluginRegistry {
    pub tools:    ToolRegistry,            // from Phase A
    pub engines:  EngineRegistry,          // from §4.1
    pub channels: ChannelRegistry,         // from §4.2
    pub stores:   MemoryStoreRegistry,     // from §4.3
    pub embedders: EmbedderRegistry,       // from §4.4
}
```

Each sub-registry has its own `from_config` + `match` arms, and each match arm is the single `#[cfg(feature = "…")]` site for its backend.

### 4.6 Feature-flag consolidation

Before Phase C: `#[cfg(feature = "…")]` scattered across `config.rs`, `engine_builder.rs`, `telegram_builder.rs`, `tool_builder.rs`, `memory_builder.rs`, `mod.rs`, `main.rs`.

After Phase C: exactly one `#[cfg]` per backend, all in `plugins/*/mod.rs` `PluginRegistry::from_config` match arms. Everywhere else is plain code.

## 5. Migration plan (rough)

To be detailed in the full Phase C spec after Phase A + B land. Rough sketch:

| # | Step | Risk |
|---|---|---|
| C1 | `EnginePlugin` trait + `EngineRegistry` + `OpenRouterEnginePlugin` + `ClaudeCodeEnginePlugin`. Wire into boot. Feature flags move. | med |
| C2 | `ChannelPlugin` trait + `ChannelRegistry` + `CliChannel` + `TelegramChannel` (extraction from post-B3 code). | low |
| C3 | `MemoryStorePlugin` + `DiskMemoryStorePlugin` + `QdrantMemoryStorePlugin`. Feature flag moves. | low |
| C4 | `EmbedderPlugin` + `OpenRouterEmbedderPlugin`. | low |
| C5 | Config schema cleanup — `[[engines]]`, `[[channels]]`, `[memory]` subtables. Legacy config keys retired. | low |
| C6 | Documentation pass — add a "how to add a channel" / "how to add an engine" / "how to add a store" guide to docs/. | none |

Each step is self-contained; each leaves the system working. Target total: ~−800 to −1 200 net LOC.

## 6. Risks and open questions (to be refined)

- **Engine construction is fatter than expected.** `OpenRouterEnginePlugin::build` may need to thread a lot of state (token budgeter, prompt budget, streaming config). Mitigation: keep the plugin thin; put heavy state on the `Engine` impl itself, same as today.
- **Channel shutdown coordination.** Multiple channels running concurrently must all drain cleanly on `Ctrl+C`. The `CancellationToken` pattern handles this; explicit test needed.
- **Per-engine feature gating of tools.** Some tools may not make sense on some engines (e.g., a tool that assumes Anthropic-style tool IDs). Decide: gate at the engine layer, at the tool layer, or leave it up to the engine's tool-use loop to reject? Leaning "leave it to the engine's tool-use loop" since it already handles this today.
- **Scope boundary with Phase A's `McpPlugin`.** Both `McpPlugin` (Phase A) and `ChannelPlugin` (Phase C) can expose Tengu to external clients in different ways. `McpPlugin` lets Tengu *call* external tools; Phase A's rewritten `mcp_bridge.rs` lets external MCP clients *call* Tengu tools; `ChannelPlugin` is how users interact with Tengu agents via messengers. Three different directions — keep them distinct in docs.

## 7. What's intentionally left for the full spec

- Exact trait signatures after we've built Phase A and learned what the `ToolCtx` pattern is actually ergonomic for.
- LOC numbers per PR.
- Detailed capability-gating semantics for engine- or channel-specific tools.
- Migration plan for the existing Telegram config keys that users may have in their `config.toml` today.
- Test strategy for multi-channel boot + shutdown ordering.

## 8. Guardrail

**Do not start Phase C until Phase A and Phase B are both merged and exercised for at least a week.** The plugin pattern may need small adjustments based on what Phase A surfaces; doing C too early means rewriting it. The stub exists only so the trajectory is visible and Phase B's Telegram migration can be shaped deliberately to unblock C.
