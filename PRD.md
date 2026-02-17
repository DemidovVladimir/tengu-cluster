# Tengu Cluster — Product Requirements Document

> A model-agnostic, hardware-adaptive AI agent hub. Single binary, zero runtime dependencies.

---

## Implementation Status (2026-02-17)

This document contains both current behavior and target-state requirements.

| Area | Status Today | Notes |
|------|--------------|-------|
| CLI chat loop | Implemented | `chat`, `status`, `doctor` are usable |
| Hub daemon | Partial | `serve` command exists, daemon runtime is not implemented yet |
| Engines | Partial | `ollama` only |
| Pipes | Partial | `cli` only |
| Tools (Kit) | Planned | Traits exist, runtime tool loop not wired |
| Flows persistence | Partial | Flow index + JSONL transcripts are wired for CLI flows |
| Knowledge store | Partial | In-memory retrieval is wired to chat loop via budget-capped query |
| Skills | Planned | Config schema exists, loader/runtime not implemented |

---

## 1. Vision

A single Rust binary that lets users run AI coding agents across any messaging platform, with any model provider, on any hardware — from a Raspberry Pi to a cloud GPU server. Users have full control over their agents, models, memory, tools, and spending.

### Core Value Propositions

1. **Zero dependencies** — One compiled binary. No runtime, no package manager, no interpreter.
2. **Any model, any provider** — Claude Code, Anthropic API, OpenAI, HuggingFace, Ollama, vLLM. Swap mid-conversation.
3. **Any hardware** — Auto-detects capabilities. 30MB on a Pi, 8GB on a GPU server.
4. **Token efficiency** — Optional optimization layer reduces token usage by 20-70%. User controls whether it's on or off.
5. **Full transparency** — Users see token costs, optimization savings, context usage in real time.
6. **User owns everything** — Memory, skills, plugins, config — all plain files on disk under user control.

---

## 2. Core Principles

1. **Single binary** — Compile once, run anywhere. No Node.js, Python, or npm.
2. **User control** — Nothing is forced. Users choose models, precision, optimization, cost tradeoffs.
3. **Model agnostic** — Every LLM backend is first-class. No vendor lock-in.
4. **Hardware adaptive** — Gracefully degrades from GPU cloud to Raspberry Pi.
5. **Token efficient** — Optional. Off by default. User enables when they want savings.
6. **Transparent** — Real-time cost and context visibility.
7. **Extensible** — Pipes, engines, plugins, skills — all pluggable via traits and config.

---

## 3. Architecture

### 3.1 Crate Structure

```
tengu-cluster/
├── Cargo.toml              # Workspace root + binary entry
├── src/main.rs             # CLI entry point
├── crates/
│   ├── tengu-core/         # Shared types, traits, config, routing
│   ├── tengu-backends/     # Engine implementations (model providers)
│   ├── tengu-channels/     # Pipe implementations (messaging platforms)
│   ├── tengu-optimizer/    # Prompt compression & token optimization
│   └── tengu-memory/       # Tiered knowledge store & retrieval
├── config.example.toml
└── PRD.md
```

### 3.2 Terminology

| Tengu Term | What It Is |
|------------|-----------|
| **Hub** | Central server — manages all connections, routing, sessions |
| **Agent** | An isolated AI entity with its own workspace, memory, sessions, and engine |
| **Engine** | The AI backend powering an agent (Claude Code, Anthropic API, Ollama, etc.) |
| **Pipe** | A messaging platform connection (Telegram, Discord, CLI, WebChat) |
| **Workspace** | An agent's working directory — files, docs, project context |
| **Store** | An agent's knowledge base — indexed files, embeddings, summaries |
| **Skill** | A markdown-defined capability the agent can learn on demand |
| **Kit** | A set of tools available to an agent (file ops, shell, web, etc.) |
| **Flow** | A conversation session with history and context management |
| **Lens** | The user-controlled precision mode (eco/standard/precise) |

### 3.3 Data Flow

```
Pipe (Telegram/Discord/CLI/WebChat)
  → Router (agent bindings, deterministic matching)
    → Flow Manager (resolve/create session, load history)
      → Refiner (optional: compress prompt, strip noise)
        → Store (optional: retrieve relevant knowledge)
          → Prompt Assembler (skills, identity, workspace context)
            → Engine (Claude Code / Anthropic / OpenAI / Ollama / HF)
              → Stream Processor (chunk for platform limits)
                → Pipe (deliver response)
```

---

## 4. Pipes (Messaging Platforms)

### 4.1 Pipe Trait

Each messaging platform implements the `Pipe` trait with optional capability sub-traits:

```rust
#[async_trait]
trait Pipe: Send + Sync {
    fn id(&self) -> &str;
    fn display_name(&self) -> &str;

    // Lifecycle
    async fn connect(&self, ctx: PipeContext) -> Result<()>;
    async fn disconnect(&self) -> Result<()>;

    // Outbound
    async fn send_text(&self, target: &Recipient, text: &str, opts: &DeliveryOptions) -> Result<()>;
    async fn send_media(&self, target: &Recipient, media: &MediaPayload) -> Result<()>;

    // Policy
    fn access_policy(&self) -> AccessPolicy;
    fn capabilities(&self) -> PipeCapabilities;

    // Optional capabilities
    fn streaming(&self) -> Option<&dyn StreamCapable> { None }
    fn threading(&self) -> Option<&dyn ThreadCapable> { None }
    fn approval(&self) -> Option<&dyn ApprovalCapable> { None }
    fn groups(&self) -> Option<&dyn GroupCapable> { None }
}
```

### 4.2 Supported Pipes

| Pipe | Status | Notes |
|------|--------|-------|
| CLI (stdin/stdout) | Implemented | `crates/tengu-channels/src/cli/mod.rs` |
| WebChat (HTTP + WS) | Planned | Config + feature flag scaffold only |
| Telegram | Planned | Config + feature flag scaffold only |
| Discord | Planned | Config + feature flag scaffold only |
| Slack | Planned | PRD target only |
| WhatsApp | Planned | PRD target only |
| Signal | Planned | PRD target only |
| Iroh | Planned | PRD target only |

### 4.3 Access Policies

| Policy | Behavior |
|--------|----------|
| `approval` (default) | Unknown senders must be approved. Codes expire after 1h. |
| `allowlist` | Only pre-approved senders |
| `open` | Accept all inbound messages |
| `disabled` | Ignore all messages |

### 4.4 Message Routing

Deterministic binding rules, most-specific wins:

1. Exact sender match
2. Parent thread match
3. Server + role match (Discord guilds)
4. Server match
5. Team match (Slack)
6. Account match
7. Pipe-level match
8. Default agent fallback

Multiple match fields require all to match (AND).

### 4.5 Response Chunking

Long responses are split for platform limits:
- Paragraph-preference soft splitting
- Code fence awareness (reopens fences across splits)
- Per-pipe text limits
- Coalescing: buffer small chunks with idle timer before delivery
- Modes: `off`, `partial` (edit-in-place for Telegram), `block` (separate messages)

---

## 5. Agents

### 5.1 Agent Isolation

Each agent is fully isolated:

```rust
struct Agent {
    id: String,
    name: String,
    workspace: PathBuf,          // Working directory
    state_dir: PathBuf,          // Auth, config, sessions
    engine: Box<dyn Engine>,     // AI backend
    flow_store: FlowStore,       // Session persistence
    store: Option<KnowledgeStore>, // Memory/RAG
    skills: Vec<Skill>,
    kit: ToolKit,                // Available tools
    config: AgentConfig,
}
```

Path layout:
```
~/.tengu/
├── config.toml
├── agents/
│   └── <agentId>/
│       ├── auth/               # Auth profiles per provider
│       ├── flows/              # Session transcripts (JSONL)
│       ├── store/              # Knowledge index (SQLite)
│       └── workspace/          # Agent's working directory
│           ├── CONTEXT.md      # Project guidelines
│           ├── IDENTITY.md     # Agent personality
│           ├── PROFILE.md      # User context
│           ├── NOTES.md        # Persistent memory
│           ├── notes/          # Daily append-only logs
│           └── skills/         # Workspace-specific skills
├── skills/                     # Shared skills
└── store/                      # Global knowledge index
```

### 5.2 Workspace Files

| File | Purpose | When Loaded |
|------|---------|-------------|
| `CONTEXT.md` | Project guidelines, repo conventions | Always at flow start |
| `IDENTITY.md` | Agent personality, behavior rules | First at flow start |
| `PROFILE.md` | Info about the user being assisted | Second at flow start |
| `NOTES.md` | Long-term curated memory | Direct sessions only |
| `notes/YYYY-MM-DD.md` | Daily logs | Today + yesterday at flow start |
| `INIT.md` | One-time bootstrap (deleted after first run) | First run only |

---

## 6. Engines (AI Backends)

### 6.1 Engine Trait

```rust
#[async_trait]
trait Engine: Send + Sync {
    fn id(&self) -> &str;
    fn context_window(&self) -> usize;
    fn supports_tool_use(&self) -> bool;
    fn manages_own_workspace(&self) -> bool;

    async fn run(
        &self,
        messages: &[Message],
        tools: &[ToolDef],
        context: &EngineContext,
    ) -> Result<Pin<Box<dyn Stream<Item = StreamEvent>>>>;
}
```

### 6.2 Supported Engines

| Engine | Status | Notes |
|--------|--------|-------|
| `ollama` | Implemented | Non-streaming request/response via `/api/chat` |
| `anthropic` | Planned | Feature scaffold only, typed REST target |
| `huggingface` | Planned | Feature scaffold only, `hf-hub` target |
| `openai` | Planned | Feature scaffold only, typed REST target |
| `google` | Planned | Feature scaffold only, typed REST target |
| `candle-local` | Planned | In-process local inference, CUDA/Metal preferred with CPU fallback |

### 6.3 Engine Switching Mid-Chat

Current behavior:
- `/engine` shows current engine/model and context window.
- Runtime switching via `/engine <name>` is not implemented yet.

Planned behavior:
- Mid-chat engine/model switching with context preservation and compaction safeguards.

### 6.4 Model Providers

```rust
#[async_trait]
trait ModelProvider: Send + Sync {
    fn provider_id(&self) -> &str;
    fn list_models(&self) -> Vec<ModelInfo>;

    async fn chat(
        &self,
        model: &str,
        messages: &[Message],
        tools: &[ToolDef],
        auth: &AuthConfig,
    ) -> Result<Pin<Box<dyn Stream<Item = StreamEvent>>>>;
}
```

Provider format: `provider/model` (e.g., `anthropic/claude-sonnet-4-5`, `ollama/deepseek-coder-v2`).

Dependency policy:
- Prefer official provider SDKs when available and maintained.
- If not available, use typed direct REST against official API docs.
- Avoid third-party multi-provider abstraction crates in core runtime.

### 6.5 Auth Profiles

Per-agent, per-provider auth with:
- Profile rotation on failure
- Cooldown tracking for failed credentials
- Multiple auth methods: API key, OAuth, AWS SDK
- Environment variable resolution

### 6.6 Failover

1. Primary model with first auth profile
2. Rotate auth profiles within provider
3. Fall back to next model in list
4. Downgrade thinking level on context overflow

---

## 7. Kit (Tool System)

### 7.1 Tool Trait

```rust
#[async_trait]
trait Tool: Send + Sync {
    fn name(&self) -> &str;
    fn description(&self) -> &str;
    fn parameters_schema(&self) -> serde_json::Value;
    async fn execute(&self, params: serde_json::Value, ctx: &ToolContext) -> Result<ToolOutput>;
}
```

### 7.2 Built-in Tools

Current behavior:
- No built-in tools are currently executable in the chat runtime.
- `Tool` trait and message/tool event types are implemented as core interfaces.

Planned built-in tools:
- `read_file`, `write_file`, `edit_file`, `find_files`, `search_content`, `shell`, `recall`, `fetch_url`, `web_search`.

### 7.3 Tool Security

- Per-agent allow/deny lists
- Safe command allowlist for shell
- Approval system for dangerous operations
- Workspace-only file access when sandboxing enabled
- Background execution with timeouts (default: 30 minutes)

---

## 8. Flows (Sessions)

Current status: flow state is in-memory per process; history is lost on restart.

### 8.1 Flow Persistence

Current runtime:
- In-memory only (no persistence yet).

Target persistence model:
- **Flow index (`flows/index.json`)**:
  - maps `flow_key -> metadata` (`transcript_path`, `updated_at`, `message_count`, `token_estimate`, `last_compacted_at`).
- **Transcript files (`*.jsonl`)**:
  - append-only event log per flow (user/assistant/tool/system lines).
  - optimized for sequential writes, easy backups, and crash-safe recovery.

Important: transcript file size does **not** directly determine prompt token usage.
Only a budgeted subset is loaded per turn.

### 8.2 Flow Scoping

| Scope | Key Pattern |
|-------|-------------|
| `main` (default) | `<agentId>:main` — all DMs share one flow |
| `per-sender` | `<agentId>:dm:<senderId>` |
| `per-pipe-sender` | `<agentId>:<pipe>:dm:<senderId>` |
| `per-group` | `<agentId>:<pipe>:group:<groupId>` |

### 8.3 Flow Reset

| Mode | Behavior |
|------|----------|
| `manual` | Only via `/reset` command |
| `daily` | Auto-reset every 24h |
| `idle` | Auto-reset after N minutes of inactivity |

### 8.4 Compaction

Goal: keep prompts bounded even when transcript/history grows forever.

Per-turn prompt assembly (target):
1. Reserve response budget (for model output).
2. Allocate fixed input budget buckets:
   - system/static context
   - recent turns window
   - retrieved knowledge
   - compaction summaries
3. Load newest messages first until the window budget is reached.
4. If needed, include compacted summary blocks instead of old raw turns.
5. Never load full transcript JSONL into a prompt.

Compaction strategy (target):
1. Keep recent N turns verbatim.
2. Summarize older contiguous ranges into compact blocks.
3. Replace old ranges in active window with references to summary blocks.
4. Recompute flow token estimate in index metadata.
5. Preserve raw transcript on disk for audit/replay.

### 8.5 Flow Storage Policy

Target operational rules:
- Rotate large transcript files (size-based) while keeping one active append file.
- Keep flow index small and O(1) to load at startup.
- Apply retention/pruning to archived transcripts by age and activity.
- Store compaction artifacts as separate files (do not overwrite raw history).

### 8.6 Critical Gaps vs OpenClaw (Storage)

Current gaps that block production-grade flow storage:
Reference deep-dive: `STORAGE_RETRIEVAL_GAP_ANALYSIS.md`.

| Gap | Why It Matters | Priority |
|-----|----------------|----------|
| No compaction execution path (overflow/threshold triggers) | Long flows eventually exceed model context | P0 |
| No history-turn limit policy per flow scope | Context can grow too fast and unpredictably | P0 |
| No transcript retention/rotation jobs | Storage grows without lifecycle control | P1 |
| No corruption detection/repair path for transcript files | Single broken transcript can break flow continuity | P1 |

---

## 9. Store (Knowledge System)

Current status: `KnowledgeStore` is in-memory and wired into the CLI chat loop with budget-capped retrieval.

### 9.1 Knowledge Indexing

Current implementation:
- In-memory `Vec<KnowledgeEntry>` storage.
- File ingestion and summary generation APIs.
- Query relevance scoring (path + keyword match).
- Lens-aware content selection.
- Token-budgeted retrieval API (`query_with_budget`).

Planned evolution:
- Persistent index, semantic retrieval, auto-reindexing, and richer ranking.

### 9.2 Two-Tier Retrieval (Tengu Innovation)

Each indexed file has two representations:

| Tier | Content | Use Case |
|------|---------|----------|
| **Summary** | Compressed version (10-20% of original) | Fast, cheap retrieval |
| **Full** | Original content | Precise, detailed retrieval |

Retrieval contract (target):
- Retrieval must be **bounded by both `top_k` and `max_tokens`**.
- Results are ranked first, then greedily packed into budget.
- If budget is exhausted, lower-ranked results are dropped (never overflow prompt budget).

### 9.3 Lens (Precision Modes)

Users control which tier is used:

| Command | Lens | Behavior |
|---------|------|----------|
| `/eco` | Eco (default) | Summaries only. Cheapest. |
| `/standard` | Standard | Summary-first (same retrieval behavior as Eco today). |
| `/precise` | Precise | Full content always. Maximum tokens. |

When refiner is `off`, everything is full content (no summaries generated).

### 9.4 Embedding Providers

Phased retrieval plan:

| Phase | Retrieval Mode | Cost Profile | Notes |
|-------|----------------|--------------|-------|
| Current | Keyword/path scoring | Lowest | Zero external deps, fast for small/medium workspaces |
| Next | TF-IDF / lexical ranking | Low | Better relevance without ML dependency |
| Optional | Embeddings (local or API) | Higher | Only when required by scale/quality |

Embedding providers (optional phase):
- OpenAI/Gemini APIs, local Candle, or pure-Rust fallback strategies.

### 9.5 Token Guard Rails

Non-negotiable rules for cost control:
- Retrieval budget is independent from flow history budget.
- `Lens::Eco` defaults to summary-only retrieval.
- Full-content retrieval is explicit (`Lens::Precise`) and still budget-capped.
- Prompt assembly fails closed (drop low-priority context) rather than exceeding budget.

### 9.6 Critical Gaps vs OpenClaw (Retrieval)

Current gaps that block robust retrieval:
Reference deep-dive: `STORAGE_RETRIEVAL_GAP_ANALYSIS.md`.

| Gap | Why It Matters | Priority |
|-----|----------------|----------|
| No incremental retrieval refresh | Workspace changes after startup are not reflected automatically | P1 |
| No persisted retrieval index | Re-indexing on every restart wastes startup time | P1 |
| No chunking for large files | Single large documents can dominate retrieval budget | P1 |
| No retrieval citation metadata (line/offset references) | Low auditability and weaker explainability | P1 |
| No retrieval quality telemetry (hit rate, dropped-by-budget) | Hard to tune token/cost behavior with data | P1 |

---

## 10. Skills

Current status: skills configuration schema exists, but skill discovery/loading/execution is planned.

### 10.1 Skill Format

Skills are markdown files with YAML frontmatter:

```
skills/<skill-name>/
  SKILL.md         — Frontmatter + instructions
  references/      — Reference files
  scripts/         — Helper scripts
```

```yaml
---
name: skill-identifier
description: Brief description
invocable: true
---

<Instructions for the agent in markdown>
```

### 10.2 Loading Hierarchy

1. Bundled skills (compiled into binary)
2. Shared skills (`~/.tengu/skills/`)
3. Workspace skills (`<workspace>/skills/`) — highest priority

### 10.3 Lazy Loading

Skills are listed in the prompt by name + description only (~100 tokens each). The agent reads the full SKILL.md via `read_file` when it determines a skill is relevant.

---

## 11. Refiner (Optimization Layer)

### 11.1 Refiner Modes

| Mode | Description | Resource Usage |
|------|-------------|---------------|
| `off` | No processing. Raw passthrough. | 0 |
| `rules` | Regex + heuristic compression. No ML. | ~0 |
| `local` | Candle in-process ML models. | 300MB-8GB |
| `remote` | Offload to another Tengu instance on the network. | ~0 (client) |

### 11.2 Refiner Trait

```rust
#[async_trait]
trait Refiner: Send + Sync {
    async fn compress(&self, input: &str) -> Result<String>;
    async fn embed(&self, text: &str) -> Result<Vec<f32>>;
    async fn summarize(&self, content: &str, max_tokens: u32) -> Result<String>;
    fn memory_footprint(&self) -> usize;
}
```

Implementations: `NoopRefiner`, `RuleRefiner`, `CandleRefiner`, `RemoteRefiner`.

### 11.3 Rule-Based Compression (Zero Cost)

- Strip filler words ("basically", "just", "maybe", "perhaps")
- Remove hedging ("I think", "could you maybe")
- Collapse whitespace, deduplicate phrases
- Compiled regex patterns — microsecond latency
- Saves 20-40% tokens

### 11.4 ML Compression (Candle)

- Small quantized model in-process (Phi-3-mini, SmolLM2, Qwen2.5-1.5B)
- Semantic compression preserving intent
- Hardware acceleration path: prefer CUDA/Metal when available, CPU fallback otherwise
- Explicit goal: saturate available local compute for faster/cheaper optimization passes
- ~5-50ms per message
- Saves 60-70% tokens

### 11.5 Cost Transparency

```
/cost

Session Stats (47 messages)
─────────────────────────────
 Tokens sent:       12,400
 Tokens received:    8,200
 Total:             20,600

 Saved by refiner:
   Prompt compression:    -4,100
   Summary retrieval:     -31,000
   History compression:   -8,900
   Total saved:          -44,000 (68%)

 Estimated cost:
   Actual:     $0.062
   Unrefined:  $0.194
   Saved:      $0.132
```

---

## 12. Hub (Central Server)

### 12.1 Architecture

Current behavior:
- `serve` command is present but does not start a daemon yet.

Planned behavior:
- HTTP + WebSocket hub daemon with RPC/event protocol and web surfaces.

### 12.2 RPC Protocol

JSON frame protocol over WebSocket:

```json
{"type": "req", "id": "abc", "method": "flow.send", "params": {...}}
{"type": "res", "id": "abc", "ok": true, "payload": {...}}
{"type": "event", "event": "stream.delta", "payload": {...}}
```

### 12.3 HTTP Endpoints

Status: planned (not implemented in current runtime).

### 12.4 Auth

Status: planned for hub daemon. Current executable has no remote hub endpoint.

### 12.5 Config Hot Reload

Status: planned for hub daemon.

---

## 13. Configuration

```toml
runtime_profile = "auto"       # auto | cloud | desktop | minimal

[hub]
bind = "127.0.0.1"
port = 7070
auth_mode = "token"
auth_token = "${TENGU_TOKEN}"

[hub.reload]
mode = "hybrid"                # hybrid | hot | restart | off

[refiner]
mode = "off"                   # off | rules | local | remote

[agents.main]
default = true
engine = "ollama"
model = "deepseek-coder-v2:16b"
workspace = "~/projects/my-app"
default_lens = "eco"

[agents.main.identity]
name = "Tengu"

[agents.main.flow]
scope = "per-sender"
reset_mode = "idle"
idle_timeout_minutes = 30

[agents.main.limits]
max_tokens_per_flow = 500_000
max_cost_per_flow = 5.00
warn_at_cost = 2.00

[agents.main.lens]
eco_max_tokens = 100
standard_threshold = 0.7
precise_budget = 0.5

[agents.main.kit]
allow = ["read_file", "write_file", "edit_file", "find_files", "search_content", "shell"]
deny = []

[agents.main.store]
files = ["CONTEXT.md", "IDENTITY.md", "PROFILE.md", "NOTES.md", "notes/*.md"]

[agents.main.allowed_engines]
list = [
    "ollama/deepseek-coder-v2:16b",
    "anthropic/claude-sonnet-4-5-20250929",
    "openai/gpt-4o-mini",
    "google/gemini-2.0-flash",
    "huggingface/meta-llama/Llama-3.3-70B-Instruct",
]

[[routing]]
agent = "main"
pipe = "cli"

[pipes.cli]
enabled = true

[pipes.telegram]
enabled = false
token = "${TELEGRAM_BOT_TOKEN}"
access_policy = "approval"

[pipes.discord]
enabled = false
token = "${DISCORD_BOT_TOKEN}"

[pipes.webchat]
enabled = false
bind = "127.0.0.1:7071"

[skills]
watch = true
extra_dirs = []
```

### Environment Variables

| Variable | Purpose |
|----------|---------|
| `TENGU_HOME` | Base directory (default: `~/.tengu`) |
| `TENGU_CONFIG_PATH` | Config file override |
| `TENGU_LOG_LEVEL` | Log level |
| `TENGU_HUB_PORT` | Port override |
| `TENGU_TOKEN` | Auth token |
| `ANTHROPIC_API_KEY` | Anthropic API key |
| `OPENAI_API_KEY` | OpenAI API key |
| `HF_TOKEN` | HuggingFace token |
| `TELEGRAM_BOT_TOKEN` | Telegram bot token |

---

## 14. Hardware Profiles

### Auto-Detection

On startup, detect: RAM, CPU cores, architecture, GPU availability.

| Profile | RAM | GPU | Refiner | Binary Size |
|---------|-----|-----|---------|-------------|
| `cloud` | 32GB+ | Yes | Candle (large) | ~30MB |
| `desktop` | 8GB+ | Maybe | Candle (small) | ~30MB |
| `minimal` | <8GB | No | Rules or off | ~8MB |

### Feature Flags

```bash
# Full build
cargo build --release

# Minimal currently-working runtime (CLI + Ollama)
cargo build --release --no-default-features \
  --features "ollama"

# Optional compile-time scaffolds (not fully implemented yet)
# telegram, discord, webchat, anthropic, openai, google, huggingface, candle
```

### Remote Refinement

Planned mode: minimal devices offload refinement to another machine.

```toml
[refiner]
mode = "remote"
url = "http://192.168.1.100:7070"
```

---

## 15. User Commands

Implemented now:

| Command | Description |
|---------|-------------|
| `/eco` | Switch to eco lens |
| `/standard` | Switch to standard lens |
| `/precise` | Switch to precise lens |
| `/engine` | Show current engine/model |
| `/cost` | Token usage stats |
| `/context` | Context window usage estimate |
| `/reset` | Clear current in-memory flow |
| `/help` | List chat commands |

Planned:
- `/engine <name>`, `/engines`, `/store*`, `/agent`, `/kit`, `/status`, `/stop`, `/compact`, `/export`.

---

## 16. CLI Commands

```bash
# Current binary name in this repo:
tengu-cluster chat             # Interactive CLI chat
tengu-cluster status           # Show config/profile summary
tengu-cluster doctor           # Ollama connectivity check
tengu-cluster serve            # Placeholder (daemon not implemented yet)

# If renamed/installed as `tengu`, same subcommands apply.
```

---

## 17. Security

- File permissions: `~/.tengu/` (700), config/credentials (600)
- Per-agent sandboxing: `off`, `workspace`, `docker`
- Per-agent tool allow/deny lists
- Safe command allowlist for shell
- Approval system for dangerous operations
- Bind to loopback by default
- Token auth for remote access

---

## 18. Phased Delivery

| Phase | Scope | Outcome |
|-------|-------|---------|
| **1** | CLI + config + `Engine` trait + Ollama engine + CLI pipe | Chat with Ollama from terminal |
| **2** | Anthropic + HuggingFace engines + `/engine` switching | Multi-model support |
| **3** | Kit (read, write, edit, shell) + tool calling loop | Agents can modify files |
| **4** | Flow management (JSONL, compaction) | Persistent conversations |
| **5** | Store (workspace files, vector search) | Knowledge-aware agents |
| **6** | RuleRefiner + `/cost` + lens modes | Token optimization (no ML) |
| **7** | Claude Code engine (subprocess) | Full Claude Code integration |
| **8** | Telegram pipe + access policies + routing | First external platform |
| **9** | WebChat pipe + web UI | Browser interface |
| **10** | Skills system (markdown, lazy loading) | Extensible capabilities |
| **11** | Candle integration (local ML refiner) | ML-powered optimization |
| **12** | Discord pipe + multi-agent routing | Full platform support |
| **13** | Sandboxing, Docker, security audit | Production hardening |

---

## 19. Success Metrics

| Metric | Target |
|--------|--------|
| Binary size (minimal) | <10MB |
| Binary size (full) | <30MB |
| Startup (no models) | <100ms |
| Startup (with models) | <5s |
| RAM (minimal, refiner off) | <50MB |
| RAM (desktop, rules refiner) | <200MB |
| RAM (cloud, Candle) | <8GB |
| Token savings (rules) | 20-40% |
| Token savings (ML) | 60-70% |
| Refiner latency (rules) | <1ms |
| Refiner latency (ML) | <50ms |
