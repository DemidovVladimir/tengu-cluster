# Tengu Cluster — Product Requirements Document

> A model-agnostic, hardware-adaptive AI agent hub. Single binary, zero runtime dependencies.

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

| Pipe | Rust Crate | Priority |
|------|-----------|----------|
| CLI (stdin/stdout) | Built-in | Phase 1 |
| WebChat (HTTP + WS) | `axum` + `tokio-tungstenite` | Phase 2 |
| Telegram | `teloxide` | Phase 3 |
| Discord | `serenity` / `poise` | Phase 4 |
| Slack | `slack-morphism` | Future |
| WhatsApp | TBD | Future |
| Signal | CLI bridge | Future |

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

| Engine | Description | Tool Use | Workspace |
|--------|-------------|----------|-----------|
| `claude-code` | Spawns `claude` CLI subprocess | Native (own tools) | Self-managed |
| `anthropic` | Direct Anthropic Messages API | Native `tool_use` | Hub-provided |
| `openai` | OpenAI or any compatible API | Function calling | Hub-provided |
| `huggingface` | HF Inference API or TGI | Prompt-based or FC | Hub-provided |
| `ollama` | Local Ollama instance | Model-dependent | Hub-provided |

### 6.3 Engine Switching Mid-Chat

Users switch engines/models mid-conversation via `/engine <name>`:

- Recent messages preserved verbatim
- Older messages compressed if new engine has smaller context
- Knowledge retrieval adjusts (mini/full) based on available context
- User warned of any context truncation

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

| Tool | Description |
|------|-------------|
| `read_file` | Read file content (workspace-sandboxed) |
| `write_file` | Write file content |
| `edit_file` | String replacement edits |
| `find_files` | Glob pattern matching |
| `search_content` | Regex content search |
| `shell` | Execute shell commands (with approval system) |
| `recall` | Search knowledge store |
| `fetch_url` | Fetch web page with content extraction |
| `web_search` | Web search |

### 7.3 Tool Security

- Per-agent allow/deny lists
- Safe command allowlist for shell
- Approval system for dangerous operations
- Workspace-only file access when sandboxing enabled
- Background execution with timeouts (default: 30 minutes)

---

## 8. Flows (Sessions)

### 8.1 Flow Persistence

- **Flow index**: JSON file mapping flow keys to metadata
- **Transcripts**: JSONL files with full message history

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

When context window fills:
1. Split messages by token share
2. Summarize oldest chunk (via current or cheaper model)
3. Replace oldest messages with summary
4. 20% safety margin for token estimation
5. Configurable chunk ratio (40% default)

---

## 9. Store (Knowledge System)

### 9.1 Knowledge Indexing

Files from the agent workspace are indexed for semantic retrieval:
- SQLite database with vector embeddings
- Chunking: ~400 tokens per chunk, 80-token overlap
- Hybrid search: BM25 text search + vector similarity
- File watching for auto-reindex on changes

### 9.2 Two-Tier Retrieval (Tengu Innovation)

Each indexed file has two representations:

| Tier | Content | Use Case |
|------|---------|----------|
| **Summary** | Compressed version (10-20% of original) | Fast, cheap retrieval |
| **Full** | Original content | Precise, detailed retrieval |

### 9.3 Lens (Precision Modes)

Users control which tier is used:

| Command | Lens | Behavior |
|---------|------|----------|
| `/eco` | Eco (default) | Summaries only. Cheapest. |
| `/standard` | Standard | Summary first, auto-expand when confidence low. |
| `/precise` | Precise | Full content always. Maximum tokens. |

When refiner is `off`, everything is full content (no summaries generated).

### 9.4 Embedding Providers

| Provider | How | When |
|----------|-----|------|
| OpenAI `text-embedding-3-small` | API call | Remote, high quality |
| Gemini `gemini-embedding-001` | API call | Remote, alternative |
| Candle (in-process) | GGUF model | Local, no network |
| TF-IDF | Pure Rust | Zero-ML fallback (minimal builds) |

Auto-selection: try configured provider → fall back to local → fall back to TF-IDF.

---

## 10. Skills

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

`axum` HTTP server + `tokio-tungstenite` WebSocket on configurable port (default: 7070).

### 12.2 RPC Protocol

JSON frame protocol over WebSocket:

```json
{"type": "req", "id": "abc", "method": "flow.send", "params": {...}}
{"type": "res", "id": "abc", "ok": true, "payload": {...}}
{"type": "event", "event": "stream.delta", "payload": {...}}
```

### 12.3 HTTP Endpoints

| Endpoint | Purpose |
|----------|---------|
| `GET /` | Web UI (embedded static assets) |
| `POST /v1/chat` | Chat API |
| `GET /health` | Health check |
| `WS /ws` | WebSocket for real-time |

### 12.4 Auth

- Token-based and password-based
- Bind modes: loopback (default), LAN, custom
- Rate limiting on auth failures

### 12.5 Config Hot Reload

File watching via `notify`:
- Safe changes (pipes, agents, routing) — apply immediately
- Structural changes (port, bind, TLS) — require restart

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
    "huggingface/meta-llama/Llama-3.3-70B-Instruct",
    "claude-code",
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

# Minimal for Pi — no ML, only Telegram + API engines
cargo build --release --no-default-features \
  --features "telegram,anthropic,huggingface"
```

### Remote Refinement

Minimal devices offload to a beefy machine:

```toml
[refiner]
mode = "remote"
url = "http://192.168.1.100:7070"
```

---

## 15. User Commands

| Command | Description |
|---------|-------------|
| `/eco` | Switch to eco lens (summaries only) |
| `/standard` | Switch to standard lens (auto-expand) |
| `/precise` | Switch to precise lens (full content) |
| `/engine` | Show/switch engine and model |
| `/engine <name>` | Switch to specific engine |
| `/engines` | List available engines |
| `/cost` | Token usage + savings breakdown |
| `/context` | Context window usage |
| `/reset` | Clear flow history |
| `/export` | Export conversation as markdown |
| `/store` | Show indexed knowledge files |
| `/store add <path>` | Add file to knowledge store |
| `/store rm <path>` | Remove from store |
| `/agent` | Show/switch agent |
| `/kit` | List available tools |
| `/status` | System overview |
| `/stop` | Abort current generation |
| `/compact` | Force context compaction |
| `/help` | List commands |

---

## 16. CLI Commands

```bash
tengu                          # Start interactive CLI
tengu chat                     # Alias for interactive CLI
tengu serve                    # Start hub daemon
tengu serve-refiner            # Serve refiner API for remote mode
tengu config get <key>
tengu config set <key> <val>
tengu agents list
tengu agents add <id>
tengu pipes status
tengu engines list
tengu engines scan
tengu doctor
tengu status
tengu security check
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
