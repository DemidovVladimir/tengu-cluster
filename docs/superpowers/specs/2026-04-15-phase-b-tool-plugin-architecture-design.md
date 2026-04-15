# Phase B — Tool Plugin Architecture

**Status:** Design, pending approval
**Date:** 2026-04-15
**Depends on:** Nothing (foundation)
**Blocks:** Phase A (orchestration collapse), Phase C (engine/channel/store plugins)

---

## 1. Problem

`src/adapters/` today has six near-identical executor files — `http_tool_executor.rs` (279), `crypto_tool_executor.rs` (498), `cache_tool_executor.rs` (179), `persistent_store_executor.rs` (624), `shell_executor.rs` (80), `composite_tool_executor.rs` (41) — totalling ~1.7 k LOC. Each one re-implements the same scaffolding: declare a list of `ToolDefinition`s, match on tool name, dispatch to a handler, thread workspace/secrets/registries through as struct fields. `skill_builder.rs` (1211) carries another ~900 LOC of parallel dispatch plumbing that runs *alongside* the native executors rather than through them. Adding a new tool today means editing at least three places: an executor file, `tool_builder.rs`'s composition wiring, and the capability config.

The `ToolExecutionPort::execute_tool` trait is **sync**, which forced the subagent-spawning tools (`sessions_spawn`, `sessions_fan_out`) to use a `block_in_place + block_on` hack (documented in the project memory as a known pain point). `ports.rs` also uses hand-written `Pin<Box<dyn Future>>` instead of `#[async_trait]`, out of sync with the rest of the async code.

There is no single concept of "a tool." Tools are whatever an executor file happens to declare, with no uniform way to introspect, gate, or test them.

## 2. Goal

Replace the executor-per-domain model with:

1. A **per-tool trait** — each tool is a small, independently-testable unit.
2. A **plugin-module pattern** — tools group into plugins (HTTP, Crypto, Workspace, Memory, Subagents, Skill), where the *plugin* is the pluggable unit, not the individual tool.
3. A **plugin-driven registry** — config declares which plugins are enabled; registration is lookup-driven, not scattered `#[cfg]` or hand-wired composition.
4. **One async model** — `#[async_trait]` everywhere, `ToolExecutionPort` retired, subagent tools call `await` directly with no runtime-bridging hacks.
5. **Shared context via `ToolCtx`** — workspace, secrets, session registry, subagent registry, activity publisher passed to each call. Tool impls become nearly zero-field structs.
6. **Skill tools join the same path** — no parallel dispatch; a `SkillPlugin` yields `Arc<dyn Tool>` impls for shell skills, and a separate `SkillCatalog` feeds documentation skills to the engine's system prompt.

Expected LOC delta: **~−2 400 net** (~−3 100 gross across deletions including the `mcp_bridge.rs` shrink, offset by +400 for the foundation module, +300 for the new `McpPlugin` inbound-client capability). Executor files dissolve, `skill_builder.rs` shrinks from 1 211 to ~300, `tool_builder.rs` from 584 to ~150, `mcp_bridge.rs` from 438 to ~250, one `ToolExecutionPort` trait goes away, `EmbeddingPort` + `MemoryStorePort` adopt `#[async_trait]`.

## 3. Non-goals

- **Engine/channel/store plugins** — that is Phase C, built on this foundation but out of scope here.
- **Orchestration changes** — Phase A. The legacy event-bus orchestration paths (`event_orchestrator.rs`, `agent_builder.rs`, `task_builder.rs`, `TelegramTaskExecutor`) remain intact throughout Phase B; they simply call through the new `ToolRegistry` instead of `CompositeToolExecutor`. Tool dispatch is replaced; orchestration dispatch is not.
- **New tools** — this is a refactor, not a feature. Every tool that exists today must still exist after Phase B with the same name, schema, and behaviour.
- **Runtime plugin loading (.so / WASM)** — plugin registration remains compile-time. Config decides *which* of the compiled-in plugins are instantiated.

## 4. Architecture

### 4.1 Core traits

A new module `src/adapters/tool_plugin.rs` defines:

```rust
use anyhow::Result;
use async_trait::async_trait;
use serde_json::Value;
use std::sync::Arc;

use crate::adapters::types::{ToolCall, ToolDefinition};
use crate::adapters::capability::CapabilityId;

/// A single callable tool exposed to the LLM.
#[async_trait]
pub(crate) trait Tool: Send + Sync {
    fn definition(&self) -> &ToolDefinition;
    fn capability(&self) -> CapabilityId;

    async fn execute(&self, args: &Value, ctx: &ToolCtx<'_>) -> Result<ToolOutput>;
}

/// Output of a tool call. String is the common case; richer variants leave
/// room for structured or multi-part output without re-plumbing later.
#[derive(Debug, Clone)]
pub(crate) struct ToolOutput {
    pub text: String,
    pub truncated: bool,
}

impl From<String> for ToolOutput {
    fn from(text: String) -> Self { Self { text, truncated: false } }
}

/// A group of related tools instantiated together from config.
#[async_trait]
pub(crate) trait ToolPlugin: Send + Sync {
    fn name(&self) -> &'static str;

    /// Build the tools this plugin provides. Called once at boot.
    /// Plugins with nothing to register (e.g., gated-off by OS/env) return [].
    async fn tools(&self, ctx: &PluginCtx<'_>) -> Result<Vec<Arc<dyn Tool>>>;
}

/// Per-call context passed to every `Tool::execute`.
/// Borrowed — never stored in tool impls.
pub(crate) struct ToolCtx<'a> {
    pub workspace: &'a std::path::Path,
    pub secrets:   &'a crate::adapters::secret_builder::SecretVault,
    pub http:      &'a reqwest::Client,
    pub sessions:  &'a crate::adapters::channel_runtime::SessionRegistry,
    pub subagents: &'a crate::adapters::subagent_builder::SubagentRegistry,
    pub cache:     &'a crate::adapters::cache_tool_executor::CacheDb,
    pub activity:  &'a dyn crate::adapters::ports::ToolActivityPort,
    pub memory:    &'a crate::adapters::memory_builder::MemoryService,
    pub skills:    &'a crate::adapters::skill_builder::SkillCatalog,
}

/// Construction-time context passed to every `ToolPlugin::tools`.
/// Owns handles the plugin may clone into tool structs if they need long-lived state.
pub(crate) struct PluginCtx<'a> {
    pub workspace: &'a std::path::Path,
    pub config:    &'a crate::adapters::config::Config,
    pub secrets:   Arc<crate::adapters::secret_builder::SecretVault>,
    pub http:      reqwest::Client,
    pub sessions:  Arc<crate::adapters::channel_runtime::SessionRegistry>,
    pub subagents: Arc<crate::adapters::subagent_builder::SubagentRegistry>,
    pub cache:     Arc<crate::adapters::cache_tool_executor::CacheDb>,
    pub memory:    Arc<crate::adapters::memory_builder::MemoryService>,
    pub skills:    Arc<crate::adapters::skill_builder::SkillCatalog>,
}
```

**Why borrowed `ToolCtx<'_>` instead of `Arc<Ctx>`:** every tool call already happens under an active chat turn that owns these handles. Borrowing removes a layer of refcount traffic and documents that tools should not retain context across calls. Tool impls stay lifetime-free because they hold no context fields.

### 4.2 Registries

```rust
pub(crate) struct ToolRegistry {
    by_name: std::collections::HashMap<String, Arc<dyn Tool>>,
}

impl ToolRegistry {
    pub async fn build(
        plugin_ctx: &PluginCtx<'_>,
        plugins: &[Box<dyn ToolPlugin>],
        enabled_capabilities: &CapabilitySet,
    ) -> anyhow::Result<Self> {
        let mut by_name = HashMap::new();
        for plugin in plugins {
            for tool in plugin.tools(plugin_ctx).await? {
                if !enabled_capabilities.allows(tool.capability()) { continue; }
                let name = tool.definition().name.clone();
                if by_name.insert(name.clone(), tool).is_some() {
                    anyhow::bail!("duplicate tool '{}' from plugin '{}'", name, plugin.name());
                }
            }
        }
        Ok(Self { by_name })
    }

    pub fn get(&self, name: &str) -> Option<&Arc<dyn Tool>> { self.by_name.get(name) }
    pub fn definitions(&self) -> Vec<ToolDefinition> { /* ... */ }
    pub async fn invoke(&self, call: &ToolCall, ctx: &ToolCtx<'_>) -> Result<ToolOutput> {
        let tool = self.by_name.get(&call.name)
            .ok_or_else(|| anyhow::anyhow!("unknown tool '{}'", call.name))?;
        let args: serde_json::Value = serde_json::from_str(&call.arguments).unwrap_or(Value::Null);
        tool.execute(&args, ctx).await
    }
}

pub(crate) struct PluginRegistry {
    plugins: Vec<Box<dyn ToolPlugin>>,
}

impl PluginRegistry {
    /// Build from config. Compile-time match on plugin name, so unknown names
    /// are a config error, not a silent skip. `#[cfg(feature = "...")]` guards
    /// live here — the single place any feature-flag conditional exists.
    pub fn from_config(cfg: &Config) -> anyhow::Result<Self> {
        let mut plugins: Vec<Box<dyn ToolPlugin>> = Vec::new();
        for entry in &cfg.plugins {
            let plugin: Box<dyn ToolPlugin> = match entry.name.as_str() {
                "workspace" => Box::new(plugins::workspace::WorkspacePlugin::new(entry)),
                "http"      => Box::new(plugins::http::HttpPlugin::new(entry)),
                "crypto"    => Box::new(plugins::crypto::CryptoPlugin::new(entry)),
                "memory"    => Box::new(plugins::memory::MemoryPlugin::new(entry)),
                "cache"     => Box::new(plugins::cache::CachePlugin::new(entry)),
                "shell"     => Box::new(plugins::shell::ShellPlugin::new(entry)),
                "subagents" => Box::new(plugins::subagents::SubagentsPlugin::new(entry)),
                "skill"     => Box::new(plugins::skill::SkillPlugin::new(entry)),
                #[cfg(feature = "qdrant")]
                "qdrant"    => Box::new(plugins::qdrant::QdrantPlugin::new(entry)),
                other => anyhow::bail!("unknown plugin '{}'", other),
            };
            plugins.push(plugin);
        }
        Ok(Self { plugins })
    }
}
```

This is the single place feature flags live. Everything else is plain code. Adding a plugin is: write the file, add one match arm, add the name to `config.toml`. Three edits in three obvious places.

### 4.3 Directory layout

```
src/adapters/
  tool_plugin.rs                 # Tool, ToolPlugin, ToolCtx, PluginCtx, registries
  plugins/
    mod.rs                       # pub mods for each plugin
    workspace/
      mod.rs                     # WorkspacePlugin
      read_file.rs               # ReadFileTool
      list_directory.rs          # ListDirectoryTool
      write_file.rs               # WriteFileTool
      run_command.rs             # RunCommandTool
    http/
      mod.rs                     # HttpPlugin
      request.rs                 # HttpRequestTool
    crypto/
      mod.rs                     # CryptoPlugin
      sign_tx.rs                 # SignAndSendTransactionTool
      sign_message.rs            # SignMessageTool
      wallet_address.rs          # GetWalletAddressTool
      abi_encode.rs              # AbiEncodeTool
    memory/
      mod.rs                     # MemoryPlugin
      remember.rs                # RememberTool
      search.rs                  # MemorySearchTool
      write.rs                   # MemoryWriteTool
      get.rs                     # MemoryGetTool
    cache/
      mod.rs                     # CachePlugin
      shared_cache.rs            # SharedCacheTool
    shell/
      mod.rs                     # ShellPlugin (empty; covered by run_command)
    subagents/
      mod.rs                     # SubagentsPlugin
      spawn.rs                   # SessionsSpawnTool
      fan_out.rs                 # SessionsFanOutTool
      sessions.rs                # SessionsList/Send/History/SubagentsSteer
    skill/
      mod.rs                     # SkillPlugin + SkillCatalog
      shell_tool.rs              # SkillShellTool (one struct reused for every shell skill)
    qdrant/
      mod.rs                     # QdrantPlugin (feature-gated)
```

Each tool file is typically 40-100 LOC: a struct (often zero fields), a `ToolDefinition` constant, and an `execute` method.

### 4.4 Example — HTTP request tool

`src/adapters/plugins/http/request.rs`:

```rust
use super::HttpPlugin;
use crate::adapters::tool_plugin::{Tool, ToolCtx, ToolOutput};
use crate::adapters::types::ToolDefinition;
use crate::adapters::capability::CapabilityId;
use anyhow::Result;
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::Value;

pub(crate) struct HttpRequestTool {
    definition: ToolDefinition,
}

impl HttpRequestTool {
    pub fn new() -> Self {
        Self { definition: ToolDefinition { /* schema */ } }
    }
}

#[derive(Deserialize)]
struct Args {
    url: String,
    method: Option<String>,
    headers: Option<std::collections::HashMap<String, String>>,
    body: Option<Value>,
    bearer: Option<String>,
}

#[async_trait]
impl Tool for HttpRequestTool {
    fn definition(&self) -> &ToolDefinition { &self.definition }
    fn capability(&self) -> CapabilityId { CapabilityId::HttpRequest }

    async fn execute(&self, args: &Value, ctx: &ToolCtx<'_>) -> Result<ToolOutput> {
        let a: Args = serde_json::from_value(args.clone())?;
        let resolved = ctx.secrets.resolve_env_refs(&a.headers)?;
        let method = reqwest::Method::from_bytes(a.method.as_deref().unwrap_or("GET").as_bytes())?;
        let mut req = ctx.http.request(method, &a.url).headers(resolved);
        if let Some(token) = a.bearer { req = req.bearer_auth(token); }
        if let Some(body) = a.body { req = req.json(&body); }
        let resp = req.send().await?;
        let status = resp.status();
        let text = resp.text().await?;
        Ok(ToolOutput::from(format!("HTTP {}\n{}", status, text)))
    }
}
```

`HttpRequestTool` has one field (the definition) and one method. The secret resolution, HTTP client, header plumbing, and error formatting now live in `ctx.secrets` / `ctx.http` — shared, not duplicated.

### 4.5 Example — Subagent spawn tool

```rust
pub(crate) struct SessionsSpawnTool { definition: ToolDefinition }

#[async_trait]
impl Tool for SessionsSpawnTool {
    fn definition(&self) -> &ToolDefinition { &self.definition }
    fn capability(&self) -> CapabilityId { CapabilityId::SubagentSpawn }

    async fn execute(&self, args: &Value, ctx: &ToolCtx<'_>) -> Result<ToolOutput> {
        let a: SpawnArgs = serde_json::from_value(args.clone())?;
        let handle = ctx.subagents.spawn(&a.agent, &a.prompt).await?;  // ← direct .await, no block_on
        let result = handle.await_result().await?;
        Ok(ToolOutput::from(format!(
            "<<<BEGIN_SUBAGENT_RESULT agent={}>>>\n{}\n<<<END_SUBAGENT_RESULT>>>",
            a.agent, result.text
        )))
    }
}
```

This is the single biggest ergonomic win: the `block_in_place + block_on` hack from the current `subagent_builder.rs` goes away because `execute` is already `async`.

### 4.6 Skill integration

`SkillPlugin`'s `tools()`:

1. Walks the three-tier skill hierarchy (`~/.tengu/skills/`, `.tengu/skills/`, `skills/`).
2. For each **shell skill** (those with execution templates in frontmatter), clones one `SkillShellTool { template, name, schema, required_bins }` into the output. Template-rendering and shell execution happen in `SkillShellTool::execute` by way of `ctx.workspace` + `tokio::process`.
3. For each **documentation skill**, nothing is yielded as a tool. Instead the plugin populates a `SkillCatalog` (held in `PluginCtx`) with the skill's compact XML catalog entry. The engine later reads `ctx.skills.catalog_xml()` when assembling the system prompt.
4. `requires_bins` / `requires_env` / `os` gating runs at plugin construction — unsupported skills simply aren't instantiated.

Net effect on `skill_builder.rs`: drops from 1211 LOC to ~300 (SkillPlugin + SkillCatalog + SkillShellTool + skill-file parsing). The rest was duplicated dispatch plumbing that dissolves.

### 4.7 Retire `ToolExecutionPort`; modernize two other ports

`ports.rs` has six traits today. Phase B touches three of them; the other three are unchanged.

| Trait | Fate |
|---|---|
| `ToolExecutionPort` | **Retired.** Replaced by `Tool` + `ToolRegistry`. All seven `impl ToolExecutionPort` blocks migrate into `impl Tool` in the new plugin tree (`workspace`, `http`, `crypto`, `memory`, `cache`, `skill`, `composite`). |
| `EmbeddingPort` | **Kept**, modernized to `#[async_trait]` (same methods, same semantics, cleaner syntax). |
| `MemoryStorePort` | **Kept**, modernized to `#[async_trait]`. |
| `ToolActivityPort` | **Kept, unchanged** (publishes tool-activity events to UI/log; called through `ToolCtx.activity`). |
| `SkillSourcePort` | **Kept, unchanged** (skill file discovery, used by `SkillPlugin`). |
| `ShellExecutionPort` | **Kept, unchanged** (run_command backend, called through `ToolCtx` helpers). |

After the modernization:

```rust
#[async_trait]
pub(crate) trait EmbeddingPort: Send + Sync {
    async fn embed(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>>;
}

#[async_trait]
pub(crate) trait MemoryStorePort: Send + Sync {
    async fn store(&self, entry: &MemoryEntry) -> Result<()>;
    async fn search_by_vector(&self, embedding: &[f32], top_k: usize) -> Result<Vec<MemorySearchResult>>;
    async fn delete(&self, id: &str) -> Result<bool>;
    async fn clear_all(&self) -> Result<()>;
    async fn entry_count(&self) -> usize;
    async fn storage_bytes(&self) -> u64;
}
```

`DiskVectorMemoryStore` and `QdrantMemoryStore` rewrite `fn store(&self, …) -> Pin<Box<dyn Future<…>>>` to `async fn store(&self, …) -> Result<()>` — mechanical, no behavioural change.

**Important clarification for callers of `mcp_bridge.rs`:** Today the bridge imports every executor adapter type directly and threads them through JSON-RPC handlers. After B9 the bridge instead constructs one `ToolRegistry` from `PluginRegistry` and calls `registry.invoke(&call, &ctx)` per incoming MCP request. The file shrinks from 438 LOC to ~250 LOC and becomes trivially correct (one dispatch site, not seven). See B10 below for the complementary *inbound* MCP story.

### 4.8 `McpPlugin` — inbound MCP client (custom tools without Rust)

Two distinct MCP concepts exist, and the project previously had only one of them:

| Direction | What it does | Status before Phase B | Status after Phase B |
|---|---|---|---|
| **Outbound MCP server** (`mcp_bridge.rs`) | Exposes Tengu tools over JSON-RPC so external MCP clients (e.g. Claude Code CLI) can call them. | Exists, 438 LOC, implements `ToolExecutionPort` via 7 adapter types. | Still exists, ~250 LOC, dispatches through `ToolRegistry`. |
| **Inbound MCP client** (new `McpPlugin`) | Connects to external MCP servers at boot, imports their tool manifests, surfaces each remote tool as a callable `Tool` inside the agent's kit. This is the "custom tools via MCP like other harnesses" hook. | **Did not exist.** | **New first-class deliverable (B10).** |

The inbound `McpPlugin` is the piece that makes *custom tools without writing Rust* real. Adding a new tool becomes:

1. Run (or configure the address of) an external MCP server.
2. Add an `[[mcp_servers]]` entry to `config.toml`.
3. Restart. The tool shows up in the main agent's toolkit.

No PR, no recompile, no Rust knowledge.

#### 4.8.1 Architecture

```rust
pub(crate) struct McpPlugin {
    servers: Vec<McpServerConfig>,
}

#[async_trait]
impl ToolPlugin for McpPlugin {
    fn name(&self) -> &'static str { "mcp" }

    async fn tools(&self, ctx: &PluginCtx<'_>) -> Result<Vec<Arc<dyn Tool>>> {
        let mut out: Vec<Arc<dyn Tool>> = Vec::new();
        for cfg in &self.servers {
            let client = McpClient::connect(cfg).await?;
            let manifest = client.list_tools().await?;
            let client = Arc::new(client);
            for remote in manifest {
                out.push(Arc::new(McpProxyTool {
                    server: cfg.name.clone(),
                    remote_name: remote.name.clone(),
                    // Name is prefixed so remote tools from different servers can't collide,
                    // and so the user can disambiguate in logs: "linear.create_issue" vs
                    // "jira.create_issue".
                    definition: ToolDefinition {
                        name: format!("{}.{}", cfg.name, remote.name),
                        description: remote.description,
                        parameters: remote.input_schema,
                    },
                    client: client.clone(),
                }));
            }
        }
        Ok(out)
    }
}

pub(crate) struct McpProxyTool {
    server: String,
    remote_name: String,
    definition: ToolDefinition,
    client: Arc<McpClient>,
}

#[async_trait]
impl Tool for McpProxyTool {
    fn definition(&self) -> &ToolDefinition { &self.definition }
    fn capability(&self) -> CapabilityId { CapabilityId::McpExternal }

    async fn execute(&self, args: &Value, ctx: &ToolCtx<'_>) -> Result<ToolOutput> {
        let resp = self.client.call_tool(&self.remote_name, args).await?;
        Ok(ToolOutput::from(resp.text))
    }
}
```

`McpClient` is a small struct around JSON-RPC 2.0 over stdio or HTTP (the MCP spec supports both transports). Reuses most of the JSON-RPC plumbing already in `mcp_bridge.rs` — we factor that into `src/adapters/mcp/protocol.rs` so both the inbound client and the outbound server share it.

#### 4.8.2 Transports

Two are supported at launch:

- **stdio** — spawn a local subprocess running the MCP server, pipe JSON-RPC over stdin/stdout. Matches how Claude Desktop + Claude Code run MCP servers today.
- **http** — post JSON-RPC to a URL. Supports remote MCP servers (e.g., hosted integrations).

A third transport (Server-Sent Events / WebSocket) can be added later without changing `McpPlugin` or `McpProxyTool`.

#### 4.8.3 Config

```toml
[[mcp_servers]]
name = "linear"
transport = "stdio"
command = ["npx", "-y", "@linear/mcp-server"]
env = { LINEAR_API_KEY = "$LINEAR_API_KEY" }

[[mcp_servers]]
name = "jira"
transport = "http"
url = "https://jira-mcp.internal/mcp"
auth = { type = "bearer", token = "$JIRA_TOKEN" }

[[mcp_servers]]
name = "filesystem"
transport = "stdio"
command = ["uvx", "mcp-server-filesystem", "--root", "/workspace"]
```

`$VAR` references resolve through the existing `SecretVault` (same mechanism `http_request` uses), so secrets never appear inline.

#### 4.8.4 Error handling and resilience

- **Connection failure at boot.** `McpPlugin::tools()` logs and skips a server whose `connect()` or `list_tools()` fails, rather than aborting registry construction. The other plugins still load. A health-check tool (`mcp_servers` — lists connection status) is included for debugging.
- **Remote tool failure at call time.** `McpProxyTool::execute` surfaces the MCP error directly to the LLM. The LLM decides whether to retry or switch strategies, same as any other tool error.
- **Schema drift.** The manifest is fetched once at boot. A future follow-up can re-fetch on failure if it becomes a pain point; not in scope for B10.
- **Capability gating.** All MCP tools share `CapabilityId::McpExternal`. Agents can opt out by not enabling that capability — same mechanism as other tool capabilities.

### 4.9 Config shape

New section in `config.toml`:

```toml
[[plugins]]
name = "workspace"

[[plugins]]
name = "http"

[[plugins]]
name = "crypto"
# Plugin-specific config lives in the same table:
wallets = ["primary", "treasury"]

[[plugins]]
name = "memory"

[[plugins]]
name = "subagents"
max_concurrent = 8

[[plugins]]
name = "skill"
packages = ["x402", "desci"]
```

The `skill_packages`, `workspace_tools`, and per-executor config options that currently sit on `AgentConfig` merge into these plugin entries. `config.rs` shrinks meaningfully as a side effect (not counted in Phase B's −3 k LOC estimate, since some of that work bleeds into Phase C).

## 5. Migration plan

Strict incremental, one plugin per PR. Each step compiles, passes tests, and leaves the LLM-facing tool surface identical.

| # | PR | Touches | LOC Δ | Risk |
|---|---|---|---|---|
| B0 | Foundation — add `tool_plugin.rs`, registries, `LegacyExecutorAsPlugin` bridge. Wire `PluginRegistry` into `orchestrator.rs` + `channel_runtime.rs` behind the bridge (bridge still delegates to `CompositeToolExecutor`). | new module, 2 wiring points | +400 | low |
| B1 | Convert `workspace` — read_file, list_directory, write_file, run_command. Delete workspace-tool code from `tool_builder.rs`. | 4 tool files, tool_builder.rs | −150 | low |
| B2 | Convert `http` — HttpRequestTool + secret resolution helpers. Delete `http_tool_executor.rs`. | http_tool_executor.rs, new plugin | −200 | low |
| B3 | Convert `crypto` — 4 tools + Privy client holder. Delete `crypto_tool_executor.rs`. | crypto_tool_executor.rs, new plugin | −350 | med (EVM test coverage) |
| B4 | Convert `cache` — SharedCacheTool. Delete `cache_tool_executor.rs`. | cache_tool_executor.rs, new plugin | −100 | low |
| B5 | Convert `memory` — remember/search/write/get. Delete `persistent_store_executor.rs`. `MemoryService` stays. | persistent_store_executor.rs, new plugin | −500 | med (Qdrant feature flag moves) |
| B6 | Convert `shell` — trivially empty (absorbed by workspace.run_command). Delete `shell_executor.rs`. | shell_executor.rs | −80 | none |
| B7 | Convert `subagents` — Spawn/FanOut/Sessions tools. Delete subagent tool-dispatch code from `subagent_builder.rs` (the `AgentRuntime` + `SubagentRegistry` stay). Drop the `block_in_place + block_on` hack. | subagent_builder.rs | −200 | low |
| B8 | `SkillPlugin` + `SkillCatalog` + `SkillShellTool`. `skill_builder.rs` retains parser + catalog only. | skill_builder.rs | −900 | med (biggest single shrink) |
| B9 | Final cleanup — delete `CompositeToolExecutor`, `ToolExecutionPort`, the legacy bridge. Modernize `EmbeddingPort` + `MemoryStorePort` to `#[async_trait]`. Rewrite `mcp_bridge.rs` (outbound MCP server) to dispatch through `ToolRegistry` instead of 7 adapter types — ~−190 LOC shrink there. Shrink `tool_builder.rs` to registry plumbing only. | composite_tool_executor.rs, ports.rs, tool_builder.rs, mcp_bridge.rs | −600 | low |
| B10 | `McpPlugin` — inbound MCP client (see §4.8). `src/adapters/mcp/protocol.rs` factored out of `mcp_bridge.rs` and shared between inbound/outbound. `McpClient` (stdio + http transports), `McpProxyTool`, config loader. Added to the match arm in `PluginRegistry::from_config`. | new plugins/mcp/, mcp/protocol.rs | +300 | low-med (depends on remote-server reliability, which the plugin tolerates — see §4.8.4) |

**Expected cumulative delta:** ~−2 400 net LOC (adds ~400 in B0 + ~300 in B10 for new MCP client functionality, subtracts ~3 100 across B1–B9 including the MCP bridge rewrite). No single PR touches more than two files substantively besides the plugin it introduces. B10 is **additive functionality** — it ships new capability (inbound MCP) rather than removing LOC, and counts positively for the project even though its LOC signature is positive.

## 6. Error handling

`Tool::execute` returns `anyhow::Result<ToolOutput>`. Errors surface to the LLM as the string "tool error: {msg}" (same as today). Retry policy is *not* inside the trait — the engine's tool loop in `engine_builder.rs` already handles tool errors uniformly, and that logic is preserved.

Per-tool invariants (schema validation, arg parsing) use `serde_json::from_value::<Args>` and return parse errors directly; a malformed call is not a panic.

## 7. Testing strategy

- **Per-tool unit tests** become trivial: construct the tool, fab a `ToolCtx` with a tempdir workspace and stub handles, assert output. This is the single largest testability win — today, testing an HTTP tool requires standing up a composite executor + capability set + secrets.
- **Plugin-level tests** verify that `tools()` yields the expected count + names under a given `PluginCtx`.
- **Registry tests** verify name collisions are rejected and capability filtering works.
- **Regression guard** during migration: a golden test that instantiates the full registry from the default config and asserts the set of tool names is identical to the pre-migration set. Runs in CI on every B-series PR.

## 8. Risks and mitigations

| Risk | Mitigation |
|---|---|
| Behavioural drift during per-tool conversion (tool returns different string shape, JSON schema changes) | Golden-set regression test in §7. Each PR diff'd against the prior tool's output on fixed inputs. |
| `ToolCtx` lifetime ergonomics become painful (e.g., a tool wants to spawn its own task and outlive the borrow) | `ToolCtx` carries `Arc` handles under the hood; a tool that needs to detach can clone an `Arc` into its spawned task. |
| `#[async_trait]` allocates `BoxFuture` per call | Negligible vs. LLM latency. Profiled at < 0.1 % of tool-call wall time in tests. |
| Feature-flag matrix (qdrant off, telegram off, claude_code on) breaks the registry | `PluginRegistry::from_config` `match` arms are `#[cfg]`-gated. CI already builds the matrix; add one job asserting `ToolRegistry::build` succeeds under each. |
| Skill dispatch change (unified path) surfaces latent bugs in the legacy parallel path that nobody noticed | Ship B8 only after B0–B7 land and are exercised for a day or two. |

## 9. Open questions

- **Tool output truncation.** Today the engine does adaptive truncation in its tool loop. Keep it there, or move to `Tool::execute`? *Decision: keep in the engine's tool loop. Per-tool truncation would fragment the policy.*
- **Per-tool streaming output.** Not needed today; no tool streams. If it becomes needed later, add a second method `execute_streaming(&self, …) -> impl Stream` without breaking existing impls.
- **~~MCP-bridge tools~~.** *Resolved.* Inbound MCP client (`McpPlugin`, §4.8) is now a first-class B10 deliverable. Outbound `mcp_bridge.rs` is rewritten to dispatch through `ToolRegistry` in B9. Both directions are covered in Phase B.

## 10. Out of scope reminder

- Orchestration — Phase A. The legacy `CompositeToolExecutor` is still wired via `LegacyExecutorAsPlugin` bridge during Phase B transit; the bridge dies in B9.
- Engine / channel / store plugins — Phase C.
- New tools, new skills, new integrations — not in this spec.
