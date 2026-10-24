# Phase A — Tool Plugin Architecture Implementation Plan

> **Archived (2026-09-18)** — historical; current behaviour: see `README.md` / `docs/architecture-2026-04-27.md`.

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace the executor-per-domain model with a per-tool trait + plugin-module pattern, making tools independently testable, async-native, and scope-enforced.

**Architecture:** Each tool becomes a small struct implementing an async `Tool` trait. Tools group into `ToolPlugin`s (workspace, http, crypto, etc.). A `ToolRegistry` collects all tools at boot and dispatches calls. During migration, a `LegacyToolBridge` wraps old sync executors so both old and new tools coexist in the same registry.

**Tech Stack:** Rust, async-trait, serde_json, tokio, reqwest, rusqlite

**Spec:** `docs/superpowers/specs/2026-04-15-phase-a-tool-plugin-architecture-design.md`

**Adaptations from spec:**
- No `CapabilityId` enum — keep existing `ToolAllowList` (tool names as strings)
- A6 (shell) dropped — `shell_executor.rs` implements `ShellExecutionPort` (infrastructure), not a tool executor
- A7 reframed — build new subagent spawning tools (code doesn't exist yet), not convert existing

---

## File Structure

### New files

```
src/adapters/
  tool_plugin.rs                    # Tool, ToolPlugin, ToolOutput, ToolCtx, PluginCtx,
                                    # ToolRegistry, LegacyToolBridge, PluginToolExecutor
  plugins/
    mod.rs                          # pub(crate) mod declarations
    workspace/
      mod.rs                        # WorkspacePlugin
      read_file.rs                  # ReadFileTool
      list_directory.rs             # ListDirectoryTool
      write_file.rs                 # WriteFileTool
      run_command.rs                # RunCommandTool
    http/
      mod.rs                        # HttpPlugin
      request.rs                    # HttpRequestTool
    crypto/
      mod.rs                        # CryptoPlugin
      sign_tx.rs                    # SignAndSendTransactionTool
      sign_message.rs               # SignMessageTool
      wallet_address.rs             # GetWalletAddressTool
      abi_encode.rs                 # AbiEncodeTool
      hex_to_uint256.rs             # HexToUint256Tool
    cache/
      mod.rs                        # CachePlugin
      shared_cache.rs               # SharedCacheTool
    memory/
      mod.rs                        # MemoryPlugin
      remember.rs                   # RememberTool
      persistent_store.rs           # PersistentStoreTool
    subagents/
      mod.rs                        # SubagentsPlugin + SubagentRegistry
      spawn.rs                      # SessionsSpawnTool
      fan_out.rs                    # SessionsFanOutTool
      manage.rs                     # SubagentsTool (list/kill/steer)
    skill/
      mod.rs                        # SkillPlugin + SkillCatalog
      shell_tool.rs                 # SkillShellTool
    mcp/
      mod.rs                        # McpPlugin
      protocol.rs                   # Shared JSON-RPC protocol (inbound + outbound)
      client.rs                     # McpClient (stdio + http transports)
      proxy_tool.rs                 # McpProxyTool
```

### Modified files

- `src/adapters/mod.rs` — add `tool_plugin`, `plugins` modules
- `src/adapters/engine_builder.rs` — make `ToolExecutor` async via `#[async_trait]`
- `src/adapters/channel_runtime.rs` — rewrite `build_tool_executor` to use `ToolRegistry`
- `src/adapters/ports.rs` — delete `ToolExecutionPort`; modernize `EmbeddingPort`, `MemoryStorePort` to `#[async_trait]`
- `src/adapters/tool_builder.rs` — shrink to path utilities + UI helpers only
- `src/adapters/skill_builder.rs` — remove `SkillToolExecutionAdapter`, keep parser + prompt building
- `src/adapters/mcp_bridge.rs` — rewrite to dispatch through `ToolRegistry`
- `src/adapters/orchestrator.rs` — update `NoopRuntimeToolExecutor` impl to async
- `tests/scope_lint.rs` — activate real lint for `plugins/**/*.rs`

### Deleted files (in A9)

- `src/adapters/http_tool_executor.rs`
- `src/adapters/crypto_tool_executor.rs`
- `src/adapters/cache_tool_executor.rs`
- `src/adapters/persistent_store_executor.rs`
- `src/adapters/composite_tool_executor.rs`

---

## Task 1: A0 — Foundation

**Goal:** Define the core traits, registry, and legacy bridge. Make `ToolExecutor` async. No behavior change.

**Files:**
- Create: `src/adapters/tool_plugin.rs`
- Create: `src/adapters/plugins/mod.rs`
- Modify: `src/adapters/mod.rs`
- Modify: `src/adapters/engine_builder.rs:531-554`
- Modify: `src/adapters/channel_runtime.rs:63-66`
- Modify: `src/adapters/orchestrator.rs:574-578`

### Step 1: Create `tool_plugin.rs`

- [ ] **1.1: Write the core types and traits**

```rust
// src/adapters/tool_plugin.rs
//! Tool plugin architecture — per-tool trait, plugin grouping, and registry.

use anyhow::Result;
use async_trait::async_trait;
use serde_json::Value;
use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use crate::adapters::memory_builder::MemoryServiceHandle;
use crate::adapters::ports::{ShellExecutionPort, ToolActivityPort, ToolExecutionPort, ToolScope};
use crate::adapters::secret_builder::SecretRegistry;
use crate::adapters::types::{ToolCall, ToolDef};

// ---------------------------------------------------------------------------
// Core trait
// ---------------------------------------------------------------------------

/// A single callable tool exposed to the LLM.
#[async_trait]
pub(crate) trait Tool: Send + Sync {
    fn definition(&self) -> &ToolDef;
    async fn execute(&self, args: &Value, ctx: &ToolCtx<'_>) -> Result<ToolOutput>;
}

/// Output of a tool call.
#[derive(Debug, Clone)]
pub(crate) struct ToolOutput {
    pub text: String,
}

impl From<String> for ToolOutput {
    fn from(text: String) -> Self {
        Self { text }
    }
}

// ---------------------------------------------------------------------------
// Plugin trait
// ---------------------------------------------------------------------------

/// A group of related tools instantiated together.
#[async_trait]
pub(crate) trait ToolPlugin: Send + Sync {
    fn name(&self) -> &'static str;
    async fn tools(&self, ctx: &PluginCtx<'_>) -> Result<Vec<Arc<dyn Tool>>>;
}

// ---------------------------------------------------------------------------
// Contexts
// ---------------------------------------------------------------------------

/// Per-call context passed to every `Tool::execute`. Borrowed, never stored.
pub(crate) struct ToolCtx<'a> {
    pub workspace: &'a Path,
    pub scope: &'a ToolScope,
    pub shell: &'a dyn ShellExecutionPort,
    pub http: &'a reqwest::Client,
    pub memory: Option<&'a MemoryServiceHandle>,
    pub secret_registry: &'a SecretRegistry,
    pub activity: &'a dyn ToolActivityPort,
}

/// Construction-time context passed to `ToolPlugin::tools()`.
pub(crate) struct PluginCtx<'a> {
    pub workspace: &'a Path,
    pub config: &'a crate::adapters::config::AgentConfig,
    pub http: reqwest::Client,
    pub shell: Arc<dyn ShellExecutionPort>,
    pub memory: Option<Arc<MemoryServiceHandle>>,
    pub secret_registry: Arc<SecretRegistry>,
}

// ---------------------------------------------------------------------------
// Registry
// ---------------------------------------------------------------------------

pub(crate) struct ToolRegistry {
    by_name: HashMap<String, Arc<dyn Tool>>,
}

impl ToolRegistry {
    pub(crate) fn new() -> Self {
        Self {
            by_name: HashMap::new(),
        }
    }

    /// Register tools from a plugin, filtered by the allow list.
    pub(crate) async fn register_plugin(
        &mut self,
        plugin: &dyn ToolPlugin,
        ctx: &PluginCtx<'_>,
        allowed: &[String],
    ) -> Result<()> {
        for tool in plugin.tools(ctx).await? {
            let name = tool.definition().name.clone();
            if !allowed.is_empty() && !allowed.contains(&name) {
                continue;
            }
            if self.by_name.contains_key(&name) {
                anyhow::bail!("duplicate tool '{}' from plugin '{}'", name, plugin.name());
            }
            self.by_name.insert(name, tool);
        }
        Ok(())
    }

    /// Register a single tool directly.
    pub(crate) fn register_tool(&mut self, tool: Arc<dyn Tool>) {
        let name = tool.definition().name.clone();
        self.by_name.insert(name, tool);
    }

    pub(crate) fn definitions(&self) -> Vec<ToolDef> {
        self.by_name.values().map(|t| t.definition().clone()).collect()
    }

    pub(crate) fn get(&self, name: &str) -> Option<&Arc<dyn Tool>> {
        self.by_name.get(name)
    }

    pub(crate) fn tool_names(&self) -> Vec<String> {
        self.by_name.keys().cloned().collect()
    }

    pub(crate) async fn invoke(
        &self,
        name: &str,
        args: &Value,
        ctx: &ToolCtx<'_>,
    ) -> Result<ToolOutput> {
        let tool = self
            .by_name
            .get(name)
            .ok_or_else(|| anyhow::anyhow!("unknown tool '{}'", name))?;
        tool.execute(args, ctx).await
    }
}

// ---------------------------------------------------------------------------
// Legacy bridge — wraps sync ToolExecutionPort as async Tool
// ---------------------------------------------------------------------------

/// Adapts an old sync executor + tool definition into the new async Tool trait.
/// Used during incremental migration (A1–A8). Deleted in A9.
pub(crate) struct LegacyToolBridge {
    def: ToolDef,
    executor: Arc<dyn ToolExecutionPort>,
}

impl LegacyToolBridge {
    pub(crate) fn new(def: ToolDef, executor: Arc<dyn ToolExecutionPort>) -> Self {
        Self { def, executor }
    }
}

#[async_trait]
impl Tool for LegacyToolBridge {
    fn definition(&self) -> &ToolDef {
        &self.def
    }

    async fn execute(&self, args: &Value, _ctx: &ToolCtx<'_>) -> Result<ToolOutput> {
        let call = ToolCall {
            id: String::new(),
            name: self.def.name.clone(),
            arguments: args.clone(),
        };
        let result = self.executor.execute_tool(&call)?;
        Ok(ToolOutput::from(result))
    }
}

// ---------------------------------------------------------------------------
// PluginToolExecutor — bridges ToolRegistry into the engine's ToolExecutor
// ---------------------------------------------------------------------------

use crate::adapters::engine_builder::ToolExecutor;

/// Wraps a ToolRegistry + context handles to implement the engine's ToolExecutor.
pub(crate) struct PluginToolExecutor {
    pub registry: ToolRegistry,
    pub workspace: std::path::PathBuf,
    pub shell: Arc<dyn ShellExecutionPort>,
    pub http: reqwest::Client,
    pub memory: Option<Arc<MemoryServiceHandle>>,
    pub secret_registry: Arc<SecretRegistry>,
    pub activity: Arc<dyn ToolActivityPort>,
    pub scopes: HashMap<String, ToolScope>,
}

#[async_trait]
impl ToolExecutor for PluginToolExecutor {
    async fn execute(&self, call: &ToolCall) -> Result<String> {
        self.activity.publish_tool_activity(call);

        if self.registry.get(&call.name).is_none() {
            anyhow::bail!("Tool '{}' is not available to this agent.", call.name);
        }

        let scope = self.scopes.get(&call.name).cloned().unwrap_or_default();
        let ctx = ToolCtx {
            workspace: &self.workspace,
            scope: &scope,
            shell: self.shell.as_ref(),
            http: &self.http,
            memory: self.memory.as_ref().map(|m| m.as_ref()),
            secret_registry: &self.secret_registry,
            activity: self.activity.as_ref(),
        };

        let output = self.registry.invoke(&call.name, &call.arguments, &ctx).await?;
        Ok(output.text)
    }
}
```

- [ ] **1.2: Create empty `plugins/mod.rs`**

```rust
// src/adapters/plugins/mod.rs
//! Tool plugins — each subdirectory groups related tools.
```

- [ ] **1.3: Wire into `mod.rs`**

Add to `src/adapters/mod.rs`:

```rust
pub(crate) mod tool_plugin;
pub(crate) mod plugins;
```

### Step 2: Make `ToolExecutor` async

- [ ] **2.1: Update trait definition in `engine_builder.rs:531-533`**

```rust
// Before:
pub(crate) trait ToolExecutor: Send + Sync {
    fn execute(&self, call: &ToolCall) -> Result<String>;
}

// After:
#[async_trait]
pub(crate) trait ToolExecutor: Send + Sync {
    async fn execute(&self, call: &ToolCall) -> Result<String>;
}
```

Add `use async_trait::async_trait;` to the imports (already present for Engine trait).

- [ ] **2.2: Update `SanitizedToolExecutor` in `engine_builder.rs:550-554`**

```rust
// Before:
impl<'a> ToolExecutor for SanitizedToolExecutor<'a> {
    fn execute(&self, call: &ToolCall) -> Result<String> {
        let result = self.inner.execute(call)?;
        Ok(self.registry.redact(&result))
    }
}

// After:
#[async_trait]
impl<'a> ToolExecutor for SanitizedToolExecutor<'a> {
    async fn execute(&self, call: &ToolCall) -> Result<String> {
        let result = self.inner.execute(call).await?;
        Ok(self.registry.redact(&result))
    }
}
```

- [ ] **2.3: Update tool call site in `collect_engine_response` (~`engine_builder.rs:678`)**

```rust
// Before:
let result = match executor.execute(tc) {

// After:
let result = match executor.execute(tc).await {
```

- [ ] **2.4: Update `ToolServiceExecutor` in `channel_runtime.rs:63-66`**

```rust
// Before:
impl ToolExecutor for ToolServiceExecutor {
    fn execute(&self, call: &ToolCall) -> Result<String> {
        self.service.execute(call)
    }
}

// After:
#[async_trait]
impl ToolExecutor for ToolServiceExecutor {
    async fn execute(&self, call: &ToolCall) -> Result<String> {
        self.service.execute(call)
    }
}
```

Add `use async_trait::async_trait;` to channel_runtime.rs imports.

- [ ] **2.5: Update `NoopRuntimeToolExecutor` in `orchestrator.rs:574-578`**

```rust
// Before:
impl ToolExecutor for NoopRuntimeToolExecutor {
    fn execute(&self, call: &ToolCall) -> Result<String> {
        anyhow::bail!("No tools available (agent has no workspace): {}", call.name)
    }
}

// After:
#[async_trait]
impl ToolExecutor for NoopRuntimeToolExecutor {
    async fn execute(&self, call: &ToolCall) -> Result<String> {
        anyhow::bail!("No tools available (agent has no workspace): {}", call.name)
    }
}
```

Add `use async_trait::async_trait;` to orchestrator.rs imports.

### Step 3: Verify

- [ ] **3.1: Compile**

Run: `cargo build 2>&1 | head -50`
Expected: compiles with no errors. Existing warnings unchanged.

- [ ] **3.2: Run tests**

Run: `cargo test 2>&1 | tail -20`
Expected: all 35 tests pass.

- [ ] **3.3: Commit**

```bash
git add src/adapters/tool_plugin.rs src/adapters/plugins/mod.rs src/adapters/mod.rs \
        src/adapters/engine_builder.rs src/adapters/channel_runtime.rs src/adapters/orchestrator.rs
git commit -m "feat(phase-a): A0 foundation — Tool trait, ToolRegistry, async ToolExecutor"
```

---

## Task 2: A1 — Workspace Plugin + Registry Wiring

**Goal:** Migrate workspace tools (read_file, list_directory, write_file, run_command) to new plugin architecture. Wire `ToolRegistry` + `PluginToolExecutor` into `channel_runtime.rs`, wrapping all unmigrated tools via `LegacyToolBridge`.

**Files:**
- Create: `src/adapters/plugins/workspace/mod.rs`
- Create: `src/adapters/plugins/workspace/read_file.rs`
- Create: `src/adapters/plugins/workspace/list_directory.rs`
- Create: `src/adapters/plugins/workspace/write_file.rs`
- Create: `src/adapters/plugins/workspace/run_command.rs`
- Modify: `src/adapters/plugins/mod.rs`
- Modify: `src/adapters/channel_runtime.rs`
- Modify: `src/adapters/tool_builder.rs` (delete workspace tool code)
- Modify: `tests/scope_lint.rs` (activate lint for plugins dir)

### Step 1: Create workspace tool files

- [ ] **1.1: Create `read_file.rs`**

Move the read_file logic from `tool_builder.rs:247-288` into a standalone `Tool` impl. The tool struct holds only its definition. Filesystem access uses `ctx.workspace` and `validate_path` (stays in `tool_builder.rs` as a utility).

```rust
// src/adapters/plugins/workspace/read_file.rs
use anyhow::{bail, Result};
use async_trait::async_trait;
use serde_json::{json, Value};

use crate::adapters::tool_builder::validate_path;
use crate::adapters::tool_plugin::{Tool, ToolCtx, ToolOutput};
use crate::adapters::types::ToolDef;

pub(crate) struct ReadFileTool {
    def: ToolDef,
}

impl ReadFileTool {
    pub(crate) fn new() -> Self {
        Self {
            def: ToolDef::new(
                "read_file",
                "Read a file.",
                json!({
                    "type": "object",
                    "properties": {
                        "path": {
                            "type": "string",
                            "description": "File path relative to the workspace root"
                        }
                    },
                    "required": ["path"]
                }),
            ),
        }
    }
}

#[async_trait]
impl Tool for ReadFileTool {
    fn definition(&self) -> &ToolDef { &self.def }

    async fn execute(&self, args: &Value, ctx: &ToolCtx<'_>) -> Result<ToolOutput> {
        ctx.scope.check_fs_read(ctx.workspace)?;

        let path_str = args.get("path").and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("read_file: missing 'path' argument"))?;
        let target = validate_path(ctx.workspace, path_str)?;

        let metadata = std::fs::metadata(&target)
            .map_err(|e| anyhow::anyhow!("Cannot read file '{}': {}", path_str, e))?;
        if metadata.is_dir() {
            bail!("'{}' is a directory, not a file. Use list_directory instead.", path_str);
        }

        let is_pdf = target.extension()
            .map(|e| e.eq_ignore_ascii_case("pdf"))
            .unwrap_or(false);

        if is_pdf {
            let text = pdf_extract::extract_text(&target)
                .map_err(|e| anyhow::anyhow!("Cannot extract text from PDF '{}': {}", path_str, e))?;
            if text.trim().is_empty() {
                bail!("PDF '{}' contains no extractable text (may be image-only)", path_str);
            }
            Ok(ToolOutput::from(text))
        } else {
            let content = std::fs::read_to_string(&target)
                .map_err(|e| anyhow::anyhow!("Cannot read file '{}': {}", path_str, e))?;
            Ok(ToolOutput::from(content))
        }
    }
}
```

- [ ] **1.2: Create `list_directory.rs`**

Same pattern — move logic from `tool_builder.rs:290-326`. Scope enforcement with `ctx.scope.check_fs_read()`.

- [ ] **1.3: Create `write_file.rs`**

Move logic from `tool_builder.rs:328-368`. Uses `ctx.scope.check_fs_write()`. Preserves the skill-directory write block.

- [ ] **1.4: Create `run_command.rs`**

Move logic from `tool_builder.rs:229-242`. Uses `ctx.scope.check_shell_bin()` to enforce allowed binaries. Delegates to `ctx.shell.execute_shell()`.

- [ ] **1.5: Create `plugins/workspace/mod.rs`**

```rust
// src/adapters/plugins/workspace/mod.rs
mod read_file;
mod list_directory;
mod write_file;
mod run_command;

use anyhow::Result;
use async_trait::async_trait;
use std::sync::Arc;

use crate::adapters::tool_plugin::{Tool, ToolPlugin, PluginCtx};

pub(crate) struct WorkspacePlugin;

#[async_trait]
impl ToolPlugin for WorkspacePlugin {
    fn name(&self) -> &'static str { "workspace" }

    async fn tools(&self, _ctx: &PluginCtx<'_>) -> Result<Vec<Arc<dyn Tool>>> {
        Ok(vec![
            Arc::new(read_file::ReadFileTool::new()),
            Arc::new(list_directory::ListDirectoryTool::new()),
            Arc::new(write_file::WriteFileTool::new()),
            Arc::new(run_command::RunCommandTool::new()),
        ])
    }
}
```

- [ ] **1.6: Update `plugins/mod.rs`**

```rust
pub(crate) mod workspace;
```

### Step 2: Wire ToolRegistry into channel_runtime.rs

- [ ] **2.1: Rewrite `build_tool_executor` to return `PluginToolExecutor`**

Replace the current `build_tool_executor` function. The new version:
1. Creates a `ToolRegistry`
2. Registers workspace tools via `WorkspacePlugin`
3. Wraps all other tools (skills, memory, cache, http, crypto, persistent_store) via `LegacyToolBridge` using the same executor construction logic that exists today
4. Returns `PluginToolExecutor` wrapping the registry

The function signature changes from returning `Option<ToolServiceExecutor>` to `Option<PluginToolExecutor>`. All callers pass it as `&dyn ToolExecutor` (unchanged since both implement `ToolExecutor`).

Key: keep `ToolUseService` and `CompositeToolExecutionAdapter` alive for legacy tools. Wrap each legacy executor's tools with `LegacyToolBridge` and register them in the `ToolRegistry`.

- [ ] **2.2: Update `channel_runtime.rs` imports**

Remove: `WorkspaceToolExecutionAdapter` from imports.
Add: `PluginToolExecutor`, `ToolRegistry`, `LegacyToolBridge`, `WorkspacePlugin` from the new modules.

### Step 3: Shrink tool_builder.rs

- [ ] **3.1: Delete `WorkspaceToolExecutionAdapter` and `execute_workspace_tool`**

Remove `tool_builder.rs:211-377` (WorkspaceToolExecutionAdapter, execute_workspace_tool, ToolExecutionPort impl). Keep:
- `expand_tilde`, `validate_path` (used by plugins)
- `build_tool_activity_text`, `prettify_tool_name`, `summarize_tool_args_for`, `truncate_detail` (UI helpers)
- `ToolUseService` (temporary — still used by LegacyToolBridge path until A9)
- `build_workspace_tools` — delete (definitions now in plugin tools)
- `build_platform_tools` — keep (still used by legacy bridge for http/crypto/etc.)

### Step 4: Activate scope lint

- [ ] **4.1: Uncomment the real lint in `tests/scope_lint.rs`**

Uncomment `every_tool_execute_checks_scope` test (lines 59-98). Update the search path to `src/adapters/plugins/` instead of `src/adapters/`.

- [ ] **4.2: Add `regex` to dev-dependencies if not present**

Check `Cargo.toml` — the commented-out test uses `regex::Regex`.

### Step 5: Write unit tests for workspace tools

- [ ] **5.1: Create `tests/plugins_workspace.rs`**

Test each tool by constructing a `ToolCtx` with a tempdir workspace and a permissive `ToolScope`:

```rust
use tempfile::TempDir;

fn test_ctx(tmp: &TempDir) -> ToolCtx<'_> {
    // Build ToolCtx with tmp.path() as workspace,
    // ToolScope with fs_roots = [tmp.path()],
    // a LocalShellExecutor, stub activity port, etc.
}

#[tokio::test]
async fn read_file_returns_content() {
    let tmp = TempDir::new().unwrap();
    std::fs::write(tmp.path().join("hello.txt"), "world").unwrap();
    let tool = ReadFileTool::new();
    let args = json!({"path": "hello.txt"});
    let ctx = test_ctx(&tmp);
    let output = tool.execute(&args, &ctx).await.unwrap();
    assert_eq!(output.text, "world");
}

#[tokio::test]
async fn read_file_scope_denies_outside_root() {
    let tmp = TempDir::new().unwrap();
    let tool = ReadFileTool::new();
    let args = json!({"path": "/etc/passwd"});
    let ctx = test_ctx(&tmp); // scope.fs_roots = [tmp.path()]
    assert!(tool.execute(&args, &ctx).await.is_err());
}
```

Write similar tests for list_directory, write_file, run_command.

- [ ] **5.2: Create golden regression test `tests/tool_registry_golden.rs`**

```rust
#[test]
fn tool_names_match_pre_migration() {
    let expected: std::collections::HashSet<&str> = [
        "read_file", "list_directory", "write_file", "run_command",
        "http_request",
        "sign_and_send_transaction", "sign_message", "get_wallet_address",
            "abi_encode", "hex_to_uint256",
        "shared_cache", "remember", "persistent_store",
    ].into_iter().collect();
    // Build registry from test config and compare tool names.
    // Exact wiring depends on test harness, but assert:
    // registry.tool_names().collect::<HashSet>() == expected
}
```

This test runs on every A-series PR and asserts the LLM-facing tool surface is unchanged.

### Step 6: Verify

- [ ] **6.1: Compile**

Run: `cargo build 2>&1 | head -50`
Expected: compiles. No new warnings.

- [ ] **6.2: Run tests**

Run: `cargo test 2>&1 | tail -30`
Expected: all tests pass, including new scope lint and workspace tool unit tests.

- [ ] **6.3: Manual smoke test**

Run: `cargo run -- chat` (or the TUI). Verify read_file, list_directory, write_file, run_command work identically.

- [ ] **6.4: Commit**

```bash
git add src/adapters/plugins/workspace/ src/adapters/plugins/mod.rs \
        src/adapters/channel_runtime.rs src/adapters/tool_builder.rs \
        src/adapters/tool_plugin.rs tests/scope_lint.rs \
        tests/plugins_workspace.rs tests/tool_registry_golden.rs
git commit -m "feat(phase-a): A1 workspace plugin — first 4 tools migrated to Tool trait"
```

---

## Task 3: A2 — HTTP Plugin

**Goal:** Migrate `http_request` tool. Delete `http_tool_executor.rs`. Eliminate `block_in_place` hack.

**Files:**
- Create: `src/adapters/plugins/http/mod.rs`
- Create: `src/adapters/plugins/http/request.rs`
- Modify: `src/adapters/plugins/mod.rs`
- Modify: `src/adapters/channel_runtime.rs` (remove http from legacy bridge, add HttpPlugin)
- Delete: `src/adapters/http_tool_executor.rs`
- Modify: `src/adapters/mod.rs` (remove http_tool_executor module)

### Step 1: Create HTTP plugin

- [ ] **1.1: Create `plugins/http/request.rs`**

Move logic from `http_tool_executor.rs:58-163`. Key changes:
- `execute` is now `async fn` — call `ctx.http.request(...).send().await` directly, no `block_in_place`
- Env var expansion uses existing `expand_env_refs` helper (move to a shared utility or inline)
- Auth resolution, header parsing, multipart handling: copy as-is
- Scope enforcement: `ctx.scope.check_net_host(&parsed_url.host())` as first line
- Secret/env var resolution: `ctx.scope.check_env_read()` before reading env vars

- [ ] **1.2: Create `plugins/http/mod.rs`**

```rust
mod request;

use anyhow::Result;
use async_trait::async_trait;
use std::sync::Arc;
use crate::adapters::tool_plugin::{Tool, ToolPlugin, PluginCtx};

pub(crate) struct HttpPlugin;

#[async_trait]
impl ToolPlugin for HttpPlugin {
    fn name(&self) -> &'static str { "http" }

    async fn tools(&self, _ctx: &PluginCtx<'_>) -> Result<Vec<Arc<dyn Tool>>> {
        Ok(vec![Arc::new(request::HttpRequestTool::new())])
    }
}
```

- [ ] **1.3: Move `expand_env_refs`, `parse_headers`, `resolve_auth`, `apply_auth`, `format_response`, `mime_from_filename` into `request.rs`** (or a shared `http/helpers.rs` if reuse is needed later)

### Step 2: Wire and cleanup

- [ ] **2.1: Update `channel_runtime.rs`** — register `HttpPlugin` in the registry. Remove the `HttpToolExecutionAdapter` legacy bridge path.

- [ ] **2.2: Delete `src/adapters/http_tool_executor.rs`**

- [ ] **2.3: Remove `pub(crate) mod http_tool_executor;` from `src/adapters/mod.rs`**

- [ ] **2.4: Remove `HttpToolExecutionAdapter` import from `channel_runtime.rs` and `mcp_bridge.rs`** (mcp_bridge.rs still compiles because it still builds its own executor — it won't use the plugin yet, just remove the import if unused)

- [ ] **2.5: Update `plugins/mod.rs`** — add `pub(crate) mod http;`

- [ ] **2.6: Delete `build_platform_tools` http_request entry from `tool_builder.rs`** (the definition now lives in HttpRequestTool)

### Step 3: Verify

- [ ] **3.1: Compile and test**

Run: `cargo build && cargo test 2>&1 | tail -20`
Expected: pass. Scope lint now covers http/request.rs.

- [ ] **3.2: Commit**

```bash
git commit -m "feat(phase-a): A2 http plugin — http_request migrated, block_in_place eliminated"
```

---

## Task 4: A3 — Crypto Plugin

**Goal:** Migrate 5 crypto tools. Delete `crypto_tool_executor.rs`. Eliminate its `block_in_place` hack.

**Files:**
- Create: `src/adapters/plugins/crypto/mod.rs`
- Create: `src/adapters/plugins/crypto/sign_tx.rs`
- Create: `src/adapters/plugins/crypto/sign_message.rs`
- Create: `src/adapters/plugins/crypto/wallet_address.rs`
- Create: `src/adapters/plugins/crypto/abi_encode.rs`
- Create: `src/adapters/plugins/crypto/hex_to_uint256.rs`
- Modify: `src/adapters/plugins/mod.rs`
- Modify: `src/adapters/channel_runtime.rs`
- Delete: `src/adapters/crypto_tool_executor.rs`
- Modify: `src/adapters/mod.rs`

### Step 1: Create crypto tool files

- [ ] **1.1: Create each tool file**

Move logic from `crypto_tool_executor.rs`. Each tool becomes a struct with:
- `definition: ToolDef` (from `build_platform_tools()` entries)
- `client: reqwest::Client` (cloned from `PluginCtx.http` at construction)
- `async fn execute` — Privy API calls use `.await` directly, no `block_in_place`

Scope enforcement: `ctx.scope.check_wallet()` for sign_tx and sign_message. `abi_encode` and `hex_to_uint256` are pure computation — no scope needed (but still include a comment explaining why).

- [ ] **1.2: Move `WALLET_ADDRESS_CACHE` static into `wallet_address.rs`**

The `Mutex<Option<String>>` cache stays as-is — it's a cross-call optimization.

- [ ] **1.3: Create `plugins/crypto/mod.rs`**

CryptoPlugin::tools() creates all 5 tools. Each tool clones `ctx.http` for Privy API calls.

### Step 2: Wire and cleanup

- [ ] **2.1: Register CryptoPlugin in channel_runtime.rs, remove legacy bridge for crypto tools**
- [ ] **2.2: Delete `crypto_tool_executor.rs`, remove from `mod.rs`**
- [ ] **2.3: Delete crypto entries from `build_platform_tools()` in `tool_builder.rs`**
- [ ] **2.4: Update `plugins/mod.rs`** — add `pub(crate) mod crypto;`

### Step 3: Verify

- [ ] **3.1: Compile and test**

Run: `cargo build && cargo test`
Expected: pass.

- [ ] **3.2: Commit**

```bash
git commit -m "feat(phase-a): A3 crypto plugin — 5 tools migrated, block_in_place eliminated"
```

---

## Task 5: A4 — Cache Plugin

**Goal:** Migrate `shared_cache` tool. Delete `cache_tool_executor.rs`.

**Files:**
- Create: `src/adapters/plugins/cache/mod.rs`
- Create: `src/adapters/plugins/cache/shared_cache.rs`
- Modify: `src/adapters/plugins/mod.rs`
- Modify: `src/adapters/channel_runtime.rs`
- Delete: `src/adapters/cache_tool_executor.rs`
- Modify: `src/adapters/mod.rs`

### Step 1: Create cache plugin

- [ ] **1.1: Create `shared_cache.rs`**

Move logic from `cache_tool_executor.rs`. The `SharedCacheTool` struct holds:
- `def: ToolDef`
- `db: Arc<Mutex<Connection>>` — opened at construction via `PluginCtx.workspace`

The `execute` method dispatches on `operation` (get/put/delete/list) same as before. All SQLite calls are sync (rusqlite is sync) — that's fine in an async fn since they're fast local I/O.

No scope enforcement needed for cache — it's workspace-scoped by construction (DB at `workspace/.tengu/cache.db`).

- [ ] **1.2: Create `plugins/cache/mod.rs`**

CachePlugin::tools() opens the DB and creates SharedCacheTool. Returns empty vec if DB open fails (with warning log, matching current behavior).

### Step 2: Wire and cleanup

- [ ] **2.1: Register CachePlugin, remove legacy bridge for shared_cache**
- [ ] **2.2: Delete `cache_tool_executor.rs`, remove from `mod.rs`**
- [ ] **2.3: Update `plugins/mod.rs`**

### Step 3: Verify

- [ ] **3.1: Compile and test**

Run: `cargo build && cargo test`
Expected: pass.

- [ ] **3.2: Commit**

```bash
git commit -m "feat(phase-a): A4 cache plugin — shared_cache migrated"
```

---

## Task 6: A5 — Memory Plugin

**Goal:** Migrate `remember` tool and `persistent_store` tool. Delete `persistent_store_executor.rs`. Remove `MemoryToolExecutionAdapter` from `memory_builder.rs`.

**Files:**
- Create: `src/adapters/plugins/memory/mod.rs`
- Create: `src/adapters/plugins/memory/remember.rs`
- Create: `src/adapters/plugins/memory/persistent_store.rs`
- Modify: `src/adapters/plugins/mod.rs`
- Modify: `src/adapters/channel_runtime.rs`
- Modify: `src/adapters/memory_builder.rs` (delete MemoryToolExecutionAdapter + run_async)
- Delete: `src/adapters/persistent_store_executor.rs`
- Modify: `src/adapters/mod.rs`

### Step 1: Create memory plugin

- [ ] **1.1: Create `remember.rs`**

Move logic from `memory_builder.rs:369-408`. The tool uses `ctx.memory` (MemoryServiceHandle) to embed and store. Secret redaction via `ctx.secret_registry.redact()`. With async execute, the `run_async` / `block_in_place` hack disappears — call `service.remember_with_metadata(...).await` directly.

- [ ] **1.2: Create `persistent_store.rs`**

Move logic from `persistent_store_executor.rs`. The PersistentStoreTool struct holds:
- `def: ToolDef`
- `workspace: PathBuf`
- `memory: Arc<MemoryServiceHandle>`
- `chunk_size: usize`
- `chunk_overlap: usize`

Same operation dispatch (store/search/list/delete). All async calls use `.await` directly.

Move the chunking helpers (`chunk_text`, `FileManifest`, etc.) into the file.

- [ ] **1.3: Create `plugins/memory/mod.rs`**

MemoryPlugin::tools() creates RememberTool (always) and PersistentStoreTool (only if `workspace_tools` includes "persistent_store"). Uses `PluginCtx.memory` and `PluginCtx.config.workspace_tools`.

### Step 2: Wire and cleanup

- [ ] **2.1: Register MemoryPlugin, remove legacy bridges for memory and persistent_store**
- [ ] **2.2: Delete `MemoryToolExecutionAdapter`, `run_async` from `memory_builder.rs`**. Keep `MemoryService`, `MemoryServiceHandle`, `DiskVectorMemoryStore`, `memory_tool_defs()`, and all non-tool code.
- [ ] **2.3: Delete `persistent_store_executor.rs`, remove from `mod.rs`**
- [ ] **2.4: Update `plugins/mod.rs`**

### Step 3: Verify

- [ ] **3.1: Compile and test**

Run: `cargo build && cargo test`
Expected: all tests pass including the 5 persistent_store tests.

- [ ] **3.2: Commit**

```bash
git commit -m "feat(phase-a): A5 memory plugin — remember + persistent_store migrated, block_in_place eliminated"
```

---

## Task 7: A7 — Subagents Plugin (NEW)

**Goal:** Build LLM-driven subagent spawning tools. This is new functionality — the tools and SubagentRegistry don't exist yet. This gives Phase B its foundation.

**Files:**
- Create: `src/adapters/plugins/subagents/mod.rs`
- Create: `src/adapters/plugins/subagents/spawn.rs`
- Create: `src/adapters/plugins/subagents/fan_out.rs`
- Create: `src/adapters/plugins/subagents/manage.rs`
- Modify: `src/adapters/plugins/mod.rs`
- Modify: `src/adapters/tool_plugin.rs` (add SubagentRegistry to ToolCtx/PluginCtx)
- Modify: `src/adapters/channel_runtime.rs`

### Step 1: Design SubagentRegistry

- [ ] **1.1: Define `SubagentRegistry` in `plugins/subagents/mod.rs`**

```rust
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::Mutex;

/// Tracks running subagents. Enforces max_concurrent.
pub(crate) struct SubagentRegistry {
    max_concurrent: usize,
    running: Mutex<HashMap<String, SubagentHandle>>,
}

pub(crate) struct SubagentHandle {
    pub agent_name: String,
    pub cancel: Arc<std::sync::atomic::AtomicBool>,
    pub join: tokio::task::JoinHandle<anyhow::Result<String>>,
}
```

The registry provides:
- `spawn(agent_name, prompt, runtime) -> Result<SubagentHandle>` — spawns an agent task
- `fan_out(agents, prompts, runtimes) -> Result<Vec<(String, String)>>` — parallel spawn + await
- `list() -> Vec<SubagentInfo>` — currently running agents
- `kill(agent_name)` — cancel a running agent
- `steer(agent_name, guidance)` — send mid-run guidance (via a channel on the handle)

### Step 2: Create spawn tools

- [ ] **2.1: Create `spawn.rs` — SessionsSpawnTool**

Tool schema: `{ agent: string, prompt: string }`. Looks up agent config, builds engine + tools + executor, runs a full chat turn, returns result. Uses `ctx.subagents.spawn(...)`.await — true async, no `block_in_place`.

Result format: `<<<BEGIN_SUBAGENT_RESULT agent=NAME>>>\n{result}\n<<<END_SUBAGENT_RESULT>>>`

- [ ] **2.2: Create `fan_out.rs` — SessionsFanOutTool**

Tool schema: `{ tasks: [{ agent: string, prompt: string }] }`. Spawns multiple agents via `tokio::JoinSet`, returns combined results.

- [ ] **2.3: Create `manage.rs` — SubagentsTool**

Tool schema: `{ action: "list"|"kill"|"steer", agent?: string, guidance?: string }`. Dispatches to `SubagentRegistry` methods.

### Step 3: Wire

- [ ] **3.1: Add `subagents: Option<&'a SubagentRegistry>` to `ToolCtx`**

- [ ] **3.2: Add `subagents: Option<Arc<SubagentRegistry>>` to `PluginCtx`**

- [ ] **3.3: Register SubagentsPlugin in channel_runtime.rs**

Only registered when orchestrator is enabled (check `config.orchestrator.enabled`). The SubagentRegistry is constructed in `build_tool_executor` and passed via PluginCtx.

- [ ] **3.4: Update `plugins/mod.rs`** — add `pub(crate) mod subagents;`

### Step 4: Verify

- [ ] **4.1: Compile and test**

Run: `cargo build && cargo test`
Expected: pass. New tools are registered but not called in tests (they require full engine setup).

- [ ] **4.2: Write a unit test for SubagentRegistry**

Test max_concurrent enforcement, list/kill operations.

- [ ] **4.3: Commit**

```bash
git commit -m "feat(phase-a): A7 subagents plugin — sessions_spawn, sessions_fan_out, subagents tools"
```

---

## Task 8: A8 — Skill Plugin

**Goal:** Extract skill tool dispatch into `SkillPlugin` + `SkillShellTool`. Shrink `skill_builder.rs` from ~1211 to ~300 LOC.

**Files:**
- Create: `src/adapters/plugins/skill/mod.rs`
- Create: `src/adapters/plugins/skill/shell_tool.rs`
- Modify: `src/adapters/plugins/mod.rs`
- Modify: `src/adapters/skill_builder.rs` (delete SkillToolExecutionAdapter)
- Modify: `src/adapters/channel_runtime.rs`

### Step 1: Create skill plugin

- [ ] **1.1: Create `shell_tool.rs` — SkillShellTool**

One struct reused for every shell skill:

```rust
pub(crate) struct SkillShellTool {
    def: ToolDef,
    template: String,
}

#[async_trait]
impl Tool for SkillShellTool {
    fn definition(&self) -> &ToolDef { &self.def }

    async fn execute(&self, args: &Value, ctx: &ToolCtx<'_>) -> Result<ToolOutput> {
        let command = render_command(&self.template, args)?;
        let result = ctx.shell.execute_shell(&command, ctx.workspace)?;
        Ok(ToolOutput::from(result))
    }
}
```

Move `render_command` from `skill_builder.rs` into this file.

- [ ] **1.2: Create `plugins/skill/mod.rs` — SkillPlugin**

```rust
pub(crate) struct SkillPlugin;

#[async_trait]
impl ToolPlugin for SkillPlugin {
    fn name(&self) -> &'static str { "skill" }

    async fn tools(&self, ctx: &PluginCtx<'_>) -> Result<Vec<Arc<dyn Tool>>> {
        let registry = SkillRegistry::from_config(ctx.config, ctx.workspace);
        let mut tools: Vec<Arc<dyn Tool>> = Vec::new();
        for skill in registry.active_skill_definitions() {
            if let SkillExecution::Shell { template } = &skill.execution {
                tools.push(Arc::new(SkillShellTool {
                    def: skill.tool_def.clone(),
                    template: template.clone(),
                }));
            }
        }
        Ok(tools)
    }
}
```

The SkillPlugin constructs a `SkillRegistry` (the existing one from `skill_builder.rs`), iterates shell skills, and yields `SkillShellTool` instances.

Documentation skills continue to flow through the system prompt building path (unchanged).

### Step 2: Cleanup skill_builder.rs

- [ ] **2.1: Delete `SkillToolExecutionAdapter` and its `ToolExecutionPort` impl** (~lines 1033-1085)

- [ ] **2.2: Move `render_command` to `plugins/skill/shell_tool.rs`**

- [ ] **2.3: Keep in `skill_builder.rs`**: `SkillRegistry`, `SkillDefinition`, `SkillExecution`, `SkillStatus`, frontmatter parsing, `build_system_prompt`, `build_system_prompt_with_tools`, `active_context_fragments`, `active_skill_definitions`.

### Step 3: Wire

- [ ] **3.1: Register SkillPlugin in channel_runtime.rs**
- [ ] **3.2: Remove skill shell tool construction from `build_tool_executor`** (currently lines 139-158)
- [ ] **3.3: Update `plugins/mod.rs`**

### Step 4: Verify

- [ ] **4.1: Compile and test**

Run: `cargo build && cargo test`
Expected: pass.

- [ ] **4.2: Commit**

```bash
git commit -m "feat(phase-a): A8 skill plugin — shell skills unified under SkillShellTool"
```

---

## Task 9: A9 — Final Cleanup

**Goal:** Delete all legacy infrastructure: `CompositeToolExecutionAdapter`, `ToolExecutionPort`, `ToolUseService`, `LegacyToolBridge`. Modernize `EmbeddingPort` + `MemoryStorePort` to `#[async_trait]`. Rewrite `mcp_bridge.rs` outbound server to dispatch through `ToolRegistry`.

**Files:**
- Delete: `src/adapters/composite_tool_executor.rs`
- Modify: `src/adapters/mod.rs` (remove deleted modules)
- Modify: `src/adapters/ports.rs` (delete ToolExecutionPort, modernize EmbeddingPort + MemoryStorePort)
- Modify: `src/adapters/tool_builder.rs` (delete ToolUseService, build_platform_tools, build_workspace_tools)
- Modify: `src/adapters/tool_plugin.rs` (delete LegacyToolBridge)
- Modify: `src/adapters/channel_runtime.rs` (remove all legacy imports + code)
- Modify: `src/adapters/mcp_bridge.rs` (dispatch through ToolRegistry)
- Modify: `src/adapters/memory_builder.rs` (DiskVectorMemoryStore → async_trait)
- Modify: `src/adapters/embedding.rs` (OpenRouterEmbeddingAdapter → async_trait)
- Modify: `src/adapters/qdrant_memory_store.rs` (QdrantMemoryStore → async_trait, if feature=qdrant)

### Step 1: Delete legacy types

- [ ] **1.1: Delete `CompositeToolExecutionAdapter`**

Remove `src/adapters/composite_tool_executor.rs`. Remove `pub(crate) mod composite_tool_executor;` from `mod.rs`.

- [ ] **1.2: Delete `ToolExecutionPort` from `ports.rs`**

Remove `ports.rs:22-24`:
```rust
pub(crate) trait ToolExecutionPort: Send + Sync {
    fn execute_tool(&self, call: &ToolCall) -> Result<String>;
}
```

- [ ] **1.3: Delete `LegacyToolBridge` from `tool_plugin.rs`**

By this point, no tools use the bridge — all have migrated to `Tool` impls.

- [ ] **1.4: Delete `ToolUseService` from `tool_builder.rs`**

Remove `tool_builder.rs:179-207`. Also delete `build_workspace_tools()` and `build_platform_tools()` if not yet deleted.

`tool_builder.rs` retains only: `expand_tilde`, `validate_path`, `build_tool_activity_text`, `prettify_tool_name`, `summarize_tool_args_for`, `truncate_detail`.

### Step 2: Modernize async ports

- [ ] **2.1: Modernize `EmbeddingPort` to `#[async_trait]`**

```rust
// Before (ports.rs:44-49):
pub(crate) trait EmbeddingPort: Send + Sync {
    fn embed(&self, texts: &[&str])
        -> Pin<Box<dyn Future<Output = Result<Vec<Vec<f32>>>> + Send + '_>>;
}

// After:
#[async_trait]
pub(crate) trait EmbeddingPort: Send + Sync {
    async fn embed(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>>;
}
```

Update `OpenRouterEmbeddingAdapter` in `embedding.rs`: change `fn embed(...)` → `async fn embed(...)`. Remove `Pin<Box<dyn Future>>` wrapper. The body stays the same (it was already async internally).

- [ ] **2.2: Modernize `MemoryStorePort` to `#[async_trait]`**

```rust
// After:
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

Update `DiskVectorMemoryStore` in `memory_builder.rs` and `QdrantMemoryStore` in `qdrant_memory_store.rs` (`#[cfg(feature = "qdrant")]`): mechanical change from `Pin<Box<Future>>` to `async fn`.

### Step 3: Rewrite mcp_bridge.rs

- [ ] **3.1: Rewrite `run_mcp_bridge` to use ToolRegistry**

Replace the manual executor construction (which imports every executor type) with:
1. Build a `ToolRegistry` from plugins (same as channel_runtime)
2. Build a `PluginToolExecutor`
3. Dispatch incoming MCP `tools/call` requests through `executor.execute(&call).await`

Since `run_mcp_bridge` is sync (blocking stdin loop), wrap the async tool execution in `tokio::runtime::Runtime::block_on()` (the bridge runs in its own process, so this is fine).

This shrinks `mcp_bridge.rs` from ~438 to ~250 LOC by removing 7 executor import+construction blocks.

### Step 4: Verify

- [ ] **4.1: Compile with all feature combinations**

Run each independently — a failure in one must not be masked by another:

```bash
cargo build
cargo build --features qdrant
cargo build --features telegram
cargo build --features claude_code
cargo build --features "qdrant,telegram,claude_code"
```

Expected: all 5 build successfully. This verifies the `#[cfg(feature)]` guards in plugin registration are correct.

- [ ] **4.2: Run all tests**

Run: `cargo test`
Expected: all pass, including golden regression test (tool names unchanged).

- [ ] **4.3: Run scope lint**

Verify the lint catches all tools under `plugins/**/*.rs`.

- [ ] **4.4: Verify no dead imports**

Run: `cargo build 2>&1 | rg 'unused import'`
Expected: no unused imports from deleted modules.

- [ ] **4.5: Commit**

```bash
git commit -m "feat(phase-a): A9 cleanup — delete legacy executors, modernize async ports, rewrite MCP bridge"
```

---

## Task 10: A10 — MCP Plugin (Inbound Client)

**Goal:** Add inbound MCP client that connects to external MCP servers at boot, imports their tool manifests, and surfaces each remote tool as a `Tool` in the agent's toolkit.

**Files:**
- Create: `src/adapters/plugins/mcp/mod.rs`
- Create: `src/adapters/plugins/mcp/protocol.rs`
- Create: `src/adapters/plugins/mcp/client.rs`
- Create: `src/adapters/plugins/mcp/proxy_tool.rs`
- Modify: `src/adapters/plugins/mod.rs`
- Modify: `src/adapters/config.rs` (add `[[mcp_servers]]` config section)
- Modify: `src/adapters/tool_plugin.rs` (add mcp_servers to PluginCtx)
- Modify: `src/adapters/channel_runtime.rs` (register McpPlugin)

### Step 1: Define config

- [ ] **1.1: Add `McpServerConfig` to `config.rs`**

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpServerConfig {
    pub name: String,
    pub transport: String, // "stdio" or "http"
    #[serde(default)]
    pub command: Vec<String>, // for stdio transport
    #[serde(default)]
    pub url: Option<String>, // for http transport
    #[serde(default)]
    pub env: HashMap<String, String>, // env vars for subprocess ($VAR refs resolved)
    #[serde(default)]
    pub auth: Option<McpAuthConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpAuthConfig {
    #[serde(rename = "type")]
    pub auth_type: String, // "bearer"
    pub token: String, // "$ENV_VAR" reference
}
```

- [ ] **1.2: Add `mcp_servers: Vec<McpServerConfig>` to `Config`**

```rust
#[serde(default)]
pub mcp_servers: Vec<McpServerConfig>,
```

### Step 2: Build MCP client

- [ ] **2.1: Create `protocol.rs`**

Shared JSON-RPC 2.0 types used by both inbound client and outbound bridge:
- `JsonRpcRequest`, `JsonRpcResponse`, `JsonRpcError` — factor these out of `mcp_bridge.rs`

- [ ] **2.2: Create `client.rs` — McpClient**

Two transports:
- **Stdio**: spawn subprocess, pipe JSON-RPC over stdin/stdout
- **HTTP**: POST JSON-RPC to a URL

Methods:
- `connect(config: &McpServerConfig) -> Result<Self>` — establish connection
- `list_tools() -> Result<Vec<McpRemoteTool>>` — call `tools/list`
- `call_tool(name: &str, args: &Value) -> Result<String>` — call `tools/call`

Env var resolution uses the existing `expand_env_refs` helper.

- [ ] **2.3: Create `proxy_tool.rs` — McpProxyTool**

```rust
pub(crate) struct McpProxyTool {
    def: ToolDef,
    remote_name: String,
    client: Arc<McpClient>,
}

#[async_trait]
impl Tool for McpProxyTool {
    fn definition(&self) -> &ToolDef { &self.def }

    async fn execute(&self, args: &Value, _ctx: &ToolCtx<'_>) -> Result<ToolOutput> {
        let result = self.client.call_tool(&self.remote_name, args).await?;
        Ok(ToolOutput::from(result))
    }
}
```

Tool names are prefixed: `"{server_name}.{tool_name}"` to prevent collisions.

### Step 3: Create McpPlugin

- [ ] **3.1: Create `plugins/mcp/mod.rs`**

```rust
pub(crate) struct McpPlugin {
    servers: Vec<McpServerConfig>,
}

#[async_trait]
impl ToolPlugin for McpPlugin {
    fn name(&self) -> &'static str { "mcp" }

    async fn tools(&self, _ctx: &PluginCtx<'_>) -> Result<Vec<Arc<dyn Tool>>> {
        let mut out: Vec<Arc<dyn Tool>> = Vec::new();
        for cfg in &self.servers {
            match McpClient::connect(cfg).await {
                Ok(client) => {
                    let manifest = client.list_tools().await?;
                    let client = Arc::new(client);
                    for remote in manifest {
                        out.push(Arc::new(McpProxyTool {
                            def: ToolDef::new(
                                &format!("{}.{}", cfg.name, remote.name),
                                &remote.description,
                                remote.input_schema.clone(),
                            ),
                            remote_name: remote.name,
                            client: client.clone(),
                        }));
                    }
                }
                Err(e) => {
                    tracing::warn!(server = %cfg.name, error = %e,
                        "MCP server connection failed, skipping");
                }
            }
        }
        Ok(out)
    }
}
```

### Step 4: Wire

- [ ] **4.1: Add `mcp_servers` to `PluginCtx`** (or pass directly to McpPlugin constructor)

- [ ] **4.2: Register McpPlugin in channel_runtime.rs** — only if `config.mcp_servers` is non-empty.

- [ ] **4.3: Update `plugins/mod.rs`** — add `pub(crate) mod mcp;`

### Step 5: Verify

- [ ] **5.1: Compile and test**

Run: `cargo build && cargo test`
Expected: pass. MCP plugin registers zero tools when no `[[mcp_servers]]` configured (default).

- [ ] **5.2: Integration test with a simple MCP server** (manual)

Set up a test config with a stdio MCP server (e.g. `npx -y @anthropic-ai/claude-code-mcp-server` or a simple echo server). Verify tools appear in the agent's toolkit.

- [ ] **5.3: Commit**

```bash
git commit -m "feat(phase-a): A10 MCP plugin — inbound MCP client for custom tools without Rust"
```

---

## Regression Guard

Throughout A1–A10, maintain a golden test that asserts the set of tool names is identical to the pre-migration set:

- [ ] **Create `tests/tool_registry_golden.rs`** (in A1)

```rust
#[test]
fn tool_names_match_pre_migration() {
    // Hard-coded set of tool names from before Phase A:
    let expected: HashSet<&str> = [
        "read_file", "list_directory", "write_file", "run_command",
        "http_request",
        "sign_and_send_transaction", "sign_message", "get_wallet_address",
            "abi_encode", "hex_to_uint256",
        "shared_cache",
        "remember",
        "persistent_store",
        // Skills are dynamic — not in this set
    ].into_iter().collect();

    // Build registry from default config and compare.
    // Exact implementation depends on test harness setup.
}
```

Each A-series PR asserts the golden set is preserved. A7 (subagents) and A10 (MCP) add to the set.

---

## Summary

| Task | PR | What | Deletes | LOC Δ (est.) |
|------|-----|------|---------|-------------|
| 1 | A0 | Foundation — traits, registry, async ToolExecutor | — | +400 |
| 2 | A1 | Workspace plugin (4 tools) + registry wiring | workspace code from tool_builder.rs | −150 |
| 3 | A2 | HTTP plugin | http_tool_executor.rs | −200 |
| 4 | A3 | Crypto plugin (5 tools) | crypto_tool_executor.rs | −350 |
| 5 | A4 | Cache plugin | cache_tool_executor.rs | −100 |
| 6 | A5 | Memory plugin (remember + persistent_store) | persistent_store_executor.rs, MemoryToolExecutionAdapter | −500 |
| 7 | A7 | Subagents plugin (NEW — spawn/fan_out/manage) | — | +350 |
| 8 | A8 | Skill plugin (SkillShellTool) | SkillToolExecutionAdapter | −900 |
| 9 | A9 | Cleanup — delete legacy, modernize ports, rewrite MCP bridge | composite, ToolExecutionPort, ToolUseService | −600 |
| 10 | A10 | MCP plugin (inbound client) | — | +300 |

**Expected cumulative:** ~−1,750 net LOC
