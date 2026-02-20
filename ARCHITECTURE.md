# 🏗️ Tengu Cluster — Architecture Deep-Dive

> Technical design documentation for all core components

---

## Current Implementation Snapshot (2026-02-20)

This document describes full target architecture. The currently running path in code is narrower:

- Inbound: `CliPipe` only
- Engine: `OllamaEngine` + `AnthropicEngine` + `OpenAIEngine` + `ClaudeCodeEngine`
- Refiner: `NoopRefiner` or `RuleRefiner`
- Runtime: `chat`, `status`, `doctor` commands
- Tool loop: partial (`ToolCallStart/Delta/End` assembly + `read_file` execution + audit trail)
- Not implemented yet: daemonized hub, external pipes, skill loader, profile/backpressure validation for event-bus subscribers

---

## Table of Contents

1. [System Overview](#1-system-overview)
2. [Crate Map](#2-crate-map)
3. [Message Lifecycle](#3-message-lifecycle)
4. [Engine Abstraction](#4-engine-abstraction)
5. [Pipe System](#5-pipe-system)
6. [Routing Engine](#6-routing-engine)
7. [Agent Isolation](#7-agent-isolation)
8. [Tool System (Kit)](#8-tool-system-kit)
9. [Knowledge Store](#9-knowledge-store)
10. [Refiner (Optimizer)](#10-refiner-optimizer)
11. [Flow Management](#11-flow-management)
12. [Configuration System](#12-configuration-system)
13. [Runtime Profiles](#13-runtime-profiles)
14. [Skills System](#14-skills-system)
15. [Security Model](#15-security-model)

---

## 1. System Overview

```
┌─────────────────────────────────────────────────────────────────────────┐
│                          TENGU CLUSTER                                  │
│                                                                         │
│  ┌──────────┐  ┌──────────┐  ┌──────────┐                              │
│  │ CLI Pipe │  │ Telegram │  │ WebChat  │   Pipes (inbound/outbound)    │
│  └────┬─────┘  └────┬─────┘  └────┬─────┘                              │
│       │              │              │                                    │
│       └──────────────┼──────────────┘                                    │
│                      ▼                                                   │
│              ┌──────────────────────┐                                    │
│              │ Ingress Router       │  sender -> entry role set           │
│              │ (pipe/peer/group/..) │                                    │
│              └──────────┬───────────┘                                    │
│                         ▼                                                │
│              ┌──────────────────────┐                                    │
│              │ Orchestrator Plane   │  single control point               │
│              │ (policy + audit +    │  for tools/adapters/skills          │
│              │ dependency dispatch) │                                    │
│              └──────────┬───────────┘                                    │
│                         ▼                                                │
│              ┌──────────────────────┐                                    │
│              │ Dependent Agents     │  flexible pool (engineering,        │
│              │ (optional)           │  product, marketing, etc.)          │
│              └──────────┬───────────┘                                    │
│                         ▼                                                │
│              ┌──────────────────────┐                                    │
│              │ Adapter Gate         │  central allow/deny + direction     │
│              │ (inter-agent comms)  │  policy enforced by orchestrator    │
│              └──────────┬───────────┘                                    │
│                         ▼                                                │
│              ┌──────────────────────┐                                    │
│              │ Runtime Audit Log    │  all approvals/executions tracked   │
│              └──────────────────────┘                                    │
│                                                                         │
│   Shared Services: FlowStore | KnowledgeStore | ToolKit | Refiner |      │
│   Runtime/Event Bus | Policy/Audit | Communication Adapters             │
│                                                                         │
│   Response Synthesizer -> orchestrator -> Pipe output                    │
└─────────────────────────────────────────────────────────────────────────┘
```

**Core Principle:** Adapter-first + event-driven runtime in one binary. Integrations compile behind traits/feature flags; orchestration evolves toward an internal domain event bus.

### 1.1 Architectural Style

1. **Adapter-first boundaries**  
   Provider/channel/tool/refiner integrations implement shared traits from `tengu-core`, keeping runtime orchestration provider-agnostic.
2. **Event-driven activity**  
   Engines emit `StreamEvent` sequences; runtime handles tool lifecycle and usage as typed events.
3. **Domain event bus migration (active backlog)**  
   `DomainEvent` + `EventBus` contracts, bounded in-process bus, runtime emitters, and audit/metrics/policy subscribers are implemented; next phase validates minimal vs multi-core backpressure behavior.
4. **Hardware-scalable execution**  
   The same architecture must run with bounded queues on single-core/minimal devices and use parallel subscribers on multi-core hosts.
5. **Topology-flexible orchestration**  
   Runtime keeps one orchestrator control plane while allowing a flexible set of dependent agents and adapter policies.
6. **Single configuration surface**  
   One config block controls orchestration, policy, adapter rules, and default model inheritance for dependents.

### 1.2 Conformance Rules (Required For Future Work)

1. New provider/channel/refiner/tool code must be introduced behind `tengu-core` adapter traits, not runtime-specific branches.
2. Runtime lifecycle transitions must use typed events (`StreamEvent` and `DomainEvent` families), not ad-hoc string protocols.
3. New side-effects must target subscriber handlers over time; if temporarily inline, they must carry explicit migration references to `E11`.

**Implemented runtime path (today):**

```
CLI stdin
  -> CliPipe
    -> (optional) Refiner.compress
      -> FlowStore load/append
        -> Budget-aware prompt assembly (history + retrieval)
          -> SelectedEngine.run
            -> CLI stdout
```

**Target coordination profile (planned runtime path):**
1. **Single-orchestrator (required)**: one orchestrator governs all dependency dispatch, adapter access, and runtime auditing.
2. **Flexible dependents**: user can add/remove dependent agents by domain without introducing extra control planes.
3. **Advanced multi-controller topologies**: explicitly deferred until after single-orchestrator path is proven stable.

---

## 2. Crate Map

```
tengu-cluster/
├── src/main.rs              # Binary entry point, event loop, CLI
├── crates/
│   ├── tengu-core/          # Traits, types, config, routing
│   │   └── src/
│   │       ├── lib.rs       # Engine, Pipe, Refiner, Tool, Lens traits
│   │       ├── config/
│   │       │   ├── mod.rs
│   │       │   ├── schema.rs    # 15+ config structs, TOML parsing, env var substitution
│   │       │   └── profile.rs   # Hardware detection, runtime profile selection
│   │       ├── events.rs        # DomainEvent schema + EventBus + bounded in-process bus
│   │       ├── routing/
│   │       │   └── mod.rs       # 4-priority cascading router
│   │       └── types/
│   │           ├── mod.rs
│   │           ├── message.rs   # Message, Role, ToolCall, ToolDef, Recipient, etc.
│   │           └── stream.rs    # StreamEvent enum (TextDelta, ToolCall*, Usage, Done)
│   │
│   ├── tengu-backends/      # AI model integrations
│   │   └── src/
│   │       ├── lib.rs           # Feature-gated module registry
│   │       ├── ollama/mod.rs    # Ollama HTTP API engine (implemented)
│   │       ├── anthropic/mod.rs # Anthropic Messages API engine (implemented)
│   │       ├── openai/mod.rs    # OpenAI Chat Completions API engine (implemented)
│   │       └── claude_code/mod.rs # Claude Code subprocess engine (implemented)
│   │
│   ├── tengu-channels/      # Messaging platform connectors
│   │   └── src/
│   │       ├── lib.rs           # Feature-gated module registry
│   │       └── cli/mod.rs       # Interactive terminal pipe (implemented)
│   │       # telegram/          # (planned – Sprint 3)
│   │       # discord/           # (planned – Phase 12)
│   │       # webchat/           # (planned – Phase 9)
│   │
│   ├── tengu-optimizer/     # Prompt compression / token savings
│   │   └── src/
│   │       ├── lib.rs           # Module registry
│   │       ├── noop/mod.rs      # Pass-through refiner
│   │       └── rules/mod.rs     # Rule-based: filler/hedging removal, extractive summary
│   │       # candle_ml/         # (planned – Phase 11)
│   │       # remote/            # (planned – Phase 11)
│   │
│   └── tengu-memory/        # Knowledge and conversation storage
│       └── src/
│           └── lib.rs           # KnowledgeStore: ingest, query, lens-aware retrieval
```

### Dependency Graph

```
main.rs
  ├── tengu-core       (always)
  ├── tengu-backends   (feature-gated engines)
  ├── tengu-channels   (feature-gated pipes)
  ├── tengu-optimizer  (feature-gated refiners)
  └── tengu-memory     (always)

tengu-backends   → tengu-core (for Engine trait, types)
tengu-channels   → tengu-core (for Pipe trait, types)
tengu-optimizer  → tengu-core (for Refiner trait)
tengu-memory     → tengu-core (for Lens, Refiner)
```

No circular dependencies. Each crate depends only on `tengu-core`.

---

## 3. Message Lifecycle

```
Target-state example (illustrative): user types in Telegram: "How much did Serik earn in February?"

Step 1: INBOUND
┌─────────────┐     InboundMessage
│ TelegramPipe │ ──► { sender: Recipient { pipe_id: "telegram", peer_id: "@erzhan_boss" },
└─────────────┘       content: "How much did Serik earn in February?",
                      timestamp: 2026-02-16T20:00:00Z }

Step 2: ROUTING
┌──────┐
│Router│ ──► resolve(sender) → "erzhan" (peer match on @erzhan_boss)
└──────┘

Step 3: REFINER (optional)
┌────────────┐
│ RuleRefiner│ ──► compress("How much did Serik earn in February?")
└────────────┘     → "How much Serik earn February?" (stripped fillers)

Step 4: KNOWLEDGE RETRIEVAL
┌──────────────┐
│KnowledgeStore│ ──► query("Serik earn February", Lens::Standard, 5)
└──────────────┘     → [{ source: "salary/february.md", content: "...", score: 1.0 }]
                     → Prepended as context block in the user message

Note: System prompt is reserved for STABLE identity (IDENTITY.md, PROFILE.md).
      Retrieved knowledge is DYNAMIC per-turn context — injected as:
      { role: "user", content: "[Context: salary/february.md]\n...\n---\nOriginal question" }
      This keeps identity and knowledge cleanly separated, and allows
      knowledge to be compacted/dropped independently during flow management.

Step 5: ENGINE
┌────────────┐     Messages: [system_prompt, ...history, user_msg]
│ OllamaEngine│ ──► POST /api/chat
└────────────┘     ← StreamEvent::TextDelta { text: "Serik earned 84,000 ₸..." }
                   ← StreamEvent::Usage { input_tokens: 450, output_tokens: 120 }
                   ← StreamEvent::Done

Step 6: TOOL LOOP (if engine requests tools)
┌──────────────┐
│ Tool: read_file │ ──► execute({ path: "salary/february.md" })
└──────────────┘       → ToolOutput { content: "...", is_error: false }
                       → Feed back into engine → get final response

Step 7: OUTBOUND
┌─────────────┐     send_text(sender, "Serik earned 84,000 ₸...", opts)
│ TelegramPipe │ ──► Telegram API → message appears in user's chat
└─────────────┘

Step 8: PERSIST
┌─────────┐     Append to flows/erzhan/telegram/@erzhan_boss.jsonl
│FlowStore│ ──► { role: "user", content: "...", timestamp: "..." }
└─────────┘     { role: "assistant", content: "...", timestamp: "..." }
```

---

## 4. Engine Abstraction

### Trait Definition (`tengu-core/src/lib.rs`)

```rust
#[async_trait]
pub trait Engine: Send + Sync {
    fn id(&self) -> &str;                     // "ollama", "anthropic", "openai"
    fn context_window(&self) -> usize;         // e.g., 128_000 for Claude
    fn supports_tool_use(&self) -> bool;       // Can handle ToolDef + ToolCall
    fn supports_streaming(&self) -> bool;      // Emits incremental text deltas
    fn manages_own_workspace(&self) -> bool;   // Future: engines that manage their own file access
    fn capabilities(&self) -> EngineCapabilities; // Runtime-discoverable capability contract
    fn diagnostics(&self) -> EngineDiagnostics; // Runtime diagnostics metadata (status/doctor)
    fn available_models(&self) -> Vec<ModelInfo>;

    async fn run(
        &self,
        messages: &[Message],
        tools: &[ToolDef],                     // Available tools for this turn
        context: &EngineContext,               // workspace path + system prompt
    ) -> Result<Pin<Box<dyn Stream<Item = StreamEvent> + Send>>>;
}
```

### Key Design Decisions

| Decision | Rationale |
|----------|-----------|
| **Streaming output** | Returns a `Stream<Item = StreamEvent>`, not a `String`. Enables real-time display and tool call interception mid-stream. |
| **Capability contract** | `Engine::capabilities()` provides one stable runtime surface for status, diagnostics, and future engine selection policies. |
| **Diagnostics contract** | `Engine::diagnostics()` standardizes endpoint/model/transport metadata surfaced by `status`, `doctor`, and `/engine`. |
| **Usage accounting contract** | `StreamEvent::Usage` is treated as a cumulative per-turn snapshot; runtime applies the latest snapshot once at turn end. |
| **Stream fixture coverage** | Ollama backend tests verify terminal event ordering for success (`TextDelta* -> Usage -> Done`) and parse-error terminal behavior (`Error`). |
| **ToolDef / ToolCall** | Tools are passed as JSON Schema definitions. Engine returns `ToolCallStart` → `ToolCallDelta` → `ToolCallEnd` events when it wants to use a tool. |
| **manages_own_workspace** | Reserved for future engines that run as subprocesses with direct filesystem access. Regular engines (Ollama, Anthropic, OpenAI) use the Kit for file access. |
| **EngineContext** | Minimal context bag — just workspace path and system prompt. Keeps the trait clean; agents add context via messages. |

### StreamEvent Enum (`types/stream.rs`)

```rust
pub enum StreamEvent {
    TextDelta { text: String },              // Incremental text
    ToolCallStart { id: String, name: String },
    ToolCallDelta { id: String, arguments_delta: String },
    ToolCallEnd { id: String },
    ThinkingDelta { text: String },          // Reasoning (Claude 3.5+)
    Usage { input_tokens: u32, output_tokens: u32 }, // cumulative per-turn snapshot
    Done,
    Error { message: String },
}
```

### Implemented vs Planned Engines

| Engine | Crate | Status | Key Behavior |
|--------|-------|--------|-------------|
| `OllamaEngine` | `tengu-backends` | ✅ Implemented | HTTP to localhost:11434 with streamed `TextDelta` output |
| `AnthropicEngine` | `tengu-backends` | ✅ Implemented | Typed REST to api.anthropic.com (`/v1/messages`), non-streaming terminal events |
| `OpenAIEngine` | `tengu-backends` | ✅ Implemented | Typed REST to api.openai.com (`/v1/chat/completions`), non-streaming terminal events |
| `ClaudeCodeEngine` | `tengu-backends` | ✅ Implemented | Subprocess CLI path (`claude --print`) with usage parsing and terminal events |
| `GoogleEngine` | `tengu-backends` | 🔲 Planned | Typed REST to Gemini API |
| `HuggingFaceEngine` | `tengu-backends` | 🔲 Planned | `hf-hub` + typed inference client |
| `CandleLocalEngine` | `tengu-backends` | 🔲 Planned | In-process local inference; prefer CUDA/Metal, CPU fallback |

### Engine Selection Flow

```
config.toml: agent.engine = "ollama" | "anthropic" | "openai" | "google" | "huggingface" | "candle-local"
                     │
                     ▼
match agent_config.engine.as_str() {
    "ollama"    => Box::new(OllamaEngine::new(url, model)),
    "anthropic" => Box::new(AnthropicEngine::new(base_url, model, api_key)),
    "openai"    => Box::new(OpenAIEngine::new(api_key, model)),
    "google"    => Box::new(GoogleEngine::new(api_key, model)),
    "huggingface" => Box::new(HuggingFaceEngine::new(token, model)),
    "candle-local" => Box::new(CandleLocalEngine::new(model)),
    // ...
}
```

---

## 5. Pipe System

### Trait Definition (`tengu-core/src/lib.rs`)

```rust
#[async_trait]
pub trait Pipe: Send + Sync {
    fn id(&self) -> &str;
    fn display_name(&self) -> &str;
    fn access_policy(&self) -> AccessPolicy;
    fn capabilities(&self) -> PipeCapabilities;

    async fn connect(&self, ctx: PipeContext) -> Result<()>;
    async fn disconnect(&self) -> Result<()>;
    async fn send_text(&self, target: &Recipient, text: &str, opts: &DeliveryOptions) -> Result<()>;
    async fn send_media(&self, target: &Recipient, media: &MediaPayload) -> Result<()>;
}
```

### Architecture

```
                    PipeContext { inbound_tx }
                           │
                    ┌──────┴──────┐
                    │             │
            ┌───────┴───┐  ┌─────┴─────┐
            │  connect() │  │ send_text()│
            │ spawn loop │  │ send_media()│
            └────────────┘  └───────────┘
                 │                 ▲
                 │                 │
    Reads from   │                 │  Writes to
    platform     ▼                 │  platform
            ┌─────────┐     ┌─────┴────┐
            │ stdin /  │     │ stdout / │
            │ Telegram │     │ Telegram │
            │ Discord  │     │ Discord  │
            └─────────┘     └──────────┘
```

### Key Types

```rust
pub struct PipeCapabilities {
    pub supports_media: bool,       // Can send images, files
    pub supports_streaming: bool,   // Can send partial responses
    pub supports_threading: bool,   // Has reply threading (Discord, Telegram reply)
    pub supports_reactions: bool,   // Can react to messages (Discord)
    pub max_text_length: Option<usize>, // Telegram: 4096, Discord: 2000
}

pub enum AccessPolicy {
    Approval,                  // New senders must be approved
    Allowlist(Vec<String>),    // Only specific sender IDs
    Open,                      // Anyone can talk (CLI)
    Disabled,                  // Pipe exists but doesn't accept messages
}

pub struct Recipient {
    pub pipe_id: String,       // "telegram", "cli", "discord"
    pub peer_id: String,       // "@erzhan_boss", "local", "user#1234"
    pub account_id: Option<String>,  // Telegram user ID
    pub thread_id: Option<String>,   // Group/thread ID
}
```

### Pipe Implementations

| Pipe | Status | Platform SDK | Notes |
|------|--------|-------------|-------|
| `CliPipe` | ✅ | tokio stdin/stdout | `AccessPolicy::Open`, spawns async read loop |
| `TelegramPipe` | 🔲 | [teloxide](https://docs.rs/teloxide) | Long-polling or webhook, `AccessPolicy::Approval` default |
| `DiscordPipe` | 🔲 | [serenity](https://docs.rs/serenity) or [twilight](https://docs.rs/twilight-gateway) | Gateway WebSocket connection |
| `WebChatPipe` | 🔲 | axum / warp | HTTP + WebSocket, serves embedded HTML |

### Message Splitting Strategy

Telegram has a 4096-character limit. Discord has 2000. Strategy:

```
if response.len() > pipe.capabilities().max_text_length {
    1. Split at markdown boundaries (---, ```blocks, ## headers)
    2. If still too long, split at paragraph boundaries (\n\n)
    3. If still too long, hard split at character limit
    4. Send as sequential messages with brief delay
}
```

---

## 6. Routing Engine

### Current Implementation (`tengu-core/src/routing/mod.rs`)

```rust
pub struct Router {
    bindings: Vec<RoutingBinding>,
    default_agent: String,
}
```

### 4-Priority Cascading Resolution

Current router resolves **sender -> entry agent/role** using a most-specific-wins strategy:

```
Priority 1: EXACT PEER MATCH
    pipe == sender.pipe_id AND binding.peer == sender.peer_id
    Example: telegram + @erzhan_boss → "erzhan"

Priority 2: GROUP MATCH
    pipe == sender.pipe_id AND binding.group_id == sender.thread_id
    Example: telegram + group "family_chat" → "family"

Priority 3: ACCOUNT MATCH
    pipe == sender.pipe_id AND binding.account_id == sender.account_id
    Example: telegram + account "123456789" → "timur"

Priority 4: PIPE-LEVEL FALLBACK
    pipe == sender.pipe_id AND no peer/group/account specified
    Example: cli + (any sender) → "main"

Fallback: DEFAULT AGENT
    If nothing matches -> default_agent (first agent with default=true)
```

This is ingress routing only. In target topology mode, ingress resolution is followed by
orchestrator dispatch (`entry role -> dependent agent plan`).

### Ingress Config Example (current)

```toml
# Priority 1: Exact peer match
[[routing]]
pipe = "telegram"
peer = "@erzhan_boss"
agent = "erzhan"

# Priority 1: Different peer
[[routing]]
pipe = "telegram"
peer = "@timur_2011"
agent = "timur"

# Priority 2: Group match
[[routing]]
pipe = "telegram"
group_id = "-1001234567890"
agent = "family"

# Priority 4: CLI fallback
[[routing]]
pipe = "cli"
agent = "main"
```

### Single-Orchestrator Topology Schema (target v1)

```toml
[topology]
mode = "single-orchestrator"
orchestrator_agent = "orchestrator"
dependent_agents = ["engineering", "marketing", "product"]
allow_direct_dependent_communication = false
max_handoff_depth = 2

[topology.defaults]
engine = "openai"
model = "gpt-4o-mini"
inherit_to_dependents = true

[topology.policy]
require_orchestrator_approval = true
tool_policy_source = "global"      # "global" | "agent-override"
adapter_policy_source = "global"   # "global" | "agent-override"

[[topology.agent_overrides]]
agent = "engineering"
engine = "anthropic"
model = "claude-sonnet-4-5-20250929"

[[topology.adapter_rules]]
from = "engineering"
to = "marketing"
adapter = "eng_to_mkt_summary"
direction = "one-way"
status = "allow"

[[topology.adapter_rules]]
from = "marketing"
to = "engineering"
direction = "one-way"
status = "deny"
reason = "prevent engineering bias from marketing directives"
```

Validation rules for this target schema:
1. Exactly one `orchestrator_agent` must be configured.
2. All `dependent_agents` must reference existing agents.
3. All tool/skill/adapter executions require orchestrator policy approval.
4. Direct dependent-to-dependent communication is blocked unless an adapter rule explicitly allows it.
5. Directional deny rules must be enforceable at runtime (for example `marketing -> engineering` blocked).
6. Capability policies cannot exceed user-defined hard boundaries.

### Resolution Diagram

```
Incoming: { pipe_id: "telegram", peer_id: "@aigul_teacher", account_id: "999", thread_id: None }

Pass 1 (peer):    @erzhan_boss ≠ @aigul_teacher → skip
                  @timur_2011  ≠ @aigul_teacher → skip
                  → No peer match

Pass 2 (group):   thread_id is None → skip all group bindings
                  → No group match

Pass 3 (account): No account bindings configured
                  → No account match

Pass 4 (pipe):    No telegram pipe-level binding
                  → No pipe match

Fallback:         → "main" (default agent)
```

### Future Enhancements

- **Regex peer matching** — `peer = "@erzhan_*"` for multiple accounts
- **Time-based routing** — business hours → one agent, after hours → another
- **Content-based routing** — keywords trigger specific agents (e.g., "urgent" → priority agent)
- **Load balancing** — round-robin across equivalent agents

---

## 7. Agent Isolation

### Conceptual Model

Each agent is an **isolated universe** with:

```
Agent "erzhan"
├── Engine:    anthropic / claude-sonnet-4-5-20250929
├── Workspace: ~/tengu/business/         ← can only see files HERE
├── Kit:       [read_file, write_file, shell]   ← allowed tools
├── Store:     [CONTEXT.md, NOTES.md, *.md]     ← indexed knowledge
├── Flow:      per-sender, 30min idle timeout   ← conversation scope
├── Limits:    500K tokens/flow, $5 max cost    ← guard rails
├── Lens:      eco (default)                     ← token optimization
└── Identity:  "Tengu Business Assistant"        ← persona
```

### Isolation Boundaries

```
┌─────────────────────┐   ┌─────────────────────┐
│    Agent: erzhan     │   │    Agent: timur      │
│                      │   │                      │
│  workspace:          │   │  workspace:          │
│  ~/tengu/business/   │ ✗ │  ~/tengu/timur/      │
│                      │   │                      │
│  Cannot access:      │   │  Cannot access:      │
│  - timur's files     │   │  - erzhan's files    │
│  - aigul's files     │   │  - salary data       │
│  - system files      │   │  - contracts         │
└─────────────────────┘   └─────────────────────┘
          ✗ No unmanaged cross-agent data flow ✗
```

Cross-agent collaboration policy (target):
1. Agent-to-agent exchange is allowed only through explicit task/result envelopes.
2. Every handoff is policy-checked by orchestrator and auditable.
3. Cross-domain exchange must pass through declared communication adapters.
4. Directional policies are enforceable (example: allow `engineering -> marketing`, block `marketing -> engineering`).
5. Direct workspace access across agents remains forbidden.

### Config Structure per Agent (`config/schema.rs`)

```rust
pub struct AgentConfig {
    pub default: bool,                  // Is this the default agent?
    pub engine: String,                 // "ollama", "anthropic"
    pub model: String,                  // "llama3.2", "claude-sonnet-4-5-20250929"
    pub workspace: Option<PathBuf>,     // Filesystem boundary
    pub default_lens: String,           // "eco", "standard", "precise"
    pub identity: IdentityConfig,       // { name: "Tengu Business" }
    pub flow: FlowConfig,              // { scope: "per-sender", idle_timeout: 30 }
    pub limits: LimitsConfig,          // { max_tokens: 500_000, max_cost: 5.00 }
    pub lens: LensConfig,             // { eco_max_tokens: 100 }
    pub kit: KitConfig,               // { allow: [...], deny: [...] }
    pub store: StoreConfig,            // { files: ["CONTEXT.md", ...] }
    pub allowed_engines: Vec<String>,  // Restrict engine switching
    pub sandbox: SandboxConfig,        // { mode: "workspace" | "docker" | "off" }
}
```

### Key Isolation Mechanisms

| Mechanism | How it Works |
|-----------|-------------|
| **Workspace path** | Tools can only read/write within `agent.workspace`. Paths are canonicalized and checked against the workspace root. |
| **Kit allow/deny** | Each agent has its own tool allowlist. Timur (kid) might have `[read_file]` only. Erzhan (business) gets `[read_file, write_file, shell]`. |
| **Flow scope** | `per-sender` means each Telegram user gets their own conversation history. Even if two users hit the same agent, they don't see each other's messages. |
| **Limits** | Token and cost limits prevent a single agent from consuming excessive resources. |
| **Sandbox** | `workspace` mode restricts shell to agent directory. `docker` runs shell commands in a container. |

---

## 8. Tool System (Kit)

### Trait Definition (`tengu-core/src/lib.rs`)

```rust
pub struct ToolContext {
    pub workspace: PathBuf,    // Agent's workspace root
    pub agent_id: String,
}

pub struct ToolOutput {
    pub content: String,       // Result text / JSON
    pub is_error: bool,        // Did the tool fail?
}

#[async_trait]
pub trait Tool: Send + Sync {
    fn name(&self) -> &str;
    fn description(&self) -> &str;
    fn parameters_schema(&self) -> serde_json::Value;  // JSON Schema
    async fn execute(&self, params: Value, ctx: &ToolContext) -> Result<ToolOutput>;
}
```

### Tool-Calling Loop (current baseline + target)

```
┌─────────────────────────────────────────────────┐
│                  TOOL LOOP                       │
│                                                  │
│  1. User message → Engine                        │
│  2. Engine responds with StreamEvents            │
│     ─── if TextDelta only ─── →  Send to user    │
│     ─── if ToolCallStart ─── ↓                   │
│  3. Accumulate ToolCallDelta until ToolCallEnd   │
│  4. Parse arguments JSON                         │
│  5. Check: is tool in agent's allow list?        │
│     ─── no ─── → Error: "tool not permitted"     │
│  6. Check: does tool need user confirmation?      │
│     ─── yes (shell) ─── → Ask user, wait         │
│  7. Execute tool (baseline: `read_file`)         │
│  8. Append: Message { role: Tool, content: ... } │
│  9. Re-run engine with updated messages          │
│  10. Repeat from step 2 (max 10 iterations)      │
│                                                  │
└─────────────────────────────────────────────────┘
```

Current implemented baseline:
1. Runtime assembles `ToolCallStart/ToolCallDelta/ToolCallEnd`.
2. Runtime enforces `kit` policy guards.
3. Runtime executes built-in `read_file` via `ToolRegistry`.
4. Runtime persists append-only JSONL audit events.

### Planned Tools

| Tool | Args | Description |
|------|------|-------------|
| `read_file` | `{ path: String }` | Read file content within workspace |
| `write_file` | `{ path: String, content: String }` | Create/overwrite file |
| `edit_file` | `{ path: String, old: String, new: String }` | Search-and-replace edit |
| `find_files` | `{ pattern: String }` | Glob-based file search |
| `search_content` | `{ query: String, path?: String }` | Grep through files |
| `shell` | `{ command: String }` | Execute shell command (requires confirmation) |
| `list_directory` | `{ path: String }` | List directory contents |

### Security: Shell Confirmation

```
Engine says: shell({ command: "rm -rf /tmp/old_data" })

Tengu:  ⚠️ Agent wants to run:
        $ rm -rf /tmp/old_data
        [Allow] [Deny] [Allow Always for this session]

Config override:
[agents.main.kit]
shell_safe_commands = ["ls", "cat", "head", "wc", "date"]  # Auto-approved
shell_blocked_commands = ["rm -rf", "sudo", "chmod"]        # Auto-denied
```

---

## 9. Knowledge Store

### Current Implementation (`tengu-memory/src/lib.rs`)

```rust
pub struct KnowledgeStore {
    entries: Vec<KnowledgeEntry>,
    workspace: PathBuf,
}

pub struct KnowledgeEntry {
    pub source: PathBuf,              // Relative path within workspace
    pub full_content: String,          // Complete file text
    pub full_token_estimate: u32,      // len/4 approximation
    pub summary: Option<String>,       // Refiner-generated summary
    pub summary_token_estimate: Option<u32>,
    pub content_hash: u64,             // For change detection
    pub last_indexed: DateTime<Utc>,
}
```

### Ingestion Pipeline

```
                         ┌──────────────────┐
config.store.files = ──► │ ingest_patterns() │
["CONTEXT.md",           │ Glob expansion   │
 "NOTES.md",             └────────┬─────────┘
 "notes/*.md"]                    │
                                  ▼
                         ┌──────────────────┐
                         │    ingest()       │
                         │ 1. Read file      │
                         │ 2. Hash content   │
                         │ 3. Skip if same   │
                         │ 4. Summarize via  │
                         │    Refiner        │
                         │ 5. Upsert entry   │
                         └──────────────────┘
```

### Lens-Aware Retrieval

```rust
pub fn query(&self, query: &str, lens: Lens, max_results: usize) -> Vec<RetrievedKnowledge>
pub fn query_with_budget(&self, query: &str, lens: Lens, max_results: usize, max_tokens: u32) -> Vec<RetrievedKnowledge>
```

```
                 ┌────────────────────────────────────┐
Query: "Serik"   │ Lens determines what gets returned: │
                 │                                      │
                 │ Eco:     summary only (cheap)        │
                 │ Standard: summary-first               │
                 │ Precise: full content (expensive)    │
                 └────────────────────────────────────┘
```

| Lens | Returns | Token Cost | Use Case |
|------|---------|-----------|----------|
| **Eco** | Summaries only | ~100 tokens/file | Quick questions, low-end hardware |
| **Standard** | Summary first | ~300 tokens/file | Day-to-day use |
| **Precise** | Full file content | Everything | Debugging, code review, critical data |

Current scoring (implemented):
- Path match + keyword match + phrase match scoring
- Non-empty queries drop zero-score entries
- Results sorted by score, then recency
- Optional `query_with_budget(...)` greedily packs results under hard `max_tokens`

### Storage Philosophy: Files = Truth, Vectors = Cache

```
                    Source of Truth              Index (disposable cache)
                    ──────────────              ────────────────────────
Knowledge      →    .md / .csv files    ──►     in-memory vector index
                         │                           │
                    User can edit these         Rebuilt on startup
                    in any text editor          or on file change
```

> **Why files, not a vector DB?**
>
> 1. **Zero dependencies** — Tengu's core promise. Adding SQLite/Qdrant/ChromaDB breaks "single binary"
> 2. **Small workspaces don't need it** — A logistics company with 30 driver files and 12 salary files? Linear scan with keyword match is <1ms
> 3. **Embeddings need a model** — Either an API call (costs money, privacy concern) or a local model (Candle, needs GPU/RAM)
> 4. **Editability** — Users can open `salary/february.md` in any text editor. The vector index is rebuilt from files, so it's always recoverable

### Phased Evolution of Retrieval

| Phase | Storage | Retrieval | Scales To | Latency |
|-------|---------|-----------|-----------|--------|
| **Current** | Plain files + in-memory `Vec<KnowledgeEntry>` | Keyword/path scoring + budgeted top-k | ~50 files | <1ms |
| **Phase 5** | Same files + in-memory TF-IDF index | `RuleRefiner.embed()` → term frequency vectors | ~500 files | <5ms |
| **Phase 11** | Same files + in-memory embeddings | `CandleRefiner.embed()` → ML vectors + cosine similarity | ~10,000 files | <50ms |

Note: The `Refiner` trait **already has** `embed()` — this was designed in from the start.
The `RuleRefiner` currently returns empty vectors (no ML), but a future `CandleRefiner`
would return real embeddings. No API changes needed.

```rust
// Phase 11: KnowledgeEntry gains an embedding field
pub struct KnowledgeEntry {
    pub source: PathBuf,
    pub full_content: String,
    pub summary: Option<String>,
    pub content_hash: u64,
    pub embedding: Option<Vec<f32>>,   // ← added in Phase 11
}

// Query becomes vector-aware:
pub fn query(&self, query: &str, lens: Lens) -> Vec<RetrievedKnowledge> {
    let query_vec = refiner.embed(query);  // Already in the Refiner trait!
    self.entries
        .iter()
        .map(|e| (e, cosine_similarity(&query_vec, &e.embedding)))
        .sorted_by_score()
        .take(top_k)
        .collect()
}
```

### Other Planned Enhancements

| Feature | Phase | Description |
|---------|-------|-------------|
| **Incremental updates** | 5 | Watch filesystem for changes, re-index only modified files |
| **Cross-reference** | 11 | Link entries that reference each other |
| **Chunk splitting** | 11 | Split large files into overlapping chunks for finer retrieval |

---

## 10. Refiner (Optimizer)

### Architecture

```
User Input → Refiner.compress() → Compressed Input → Engine

                ┌──────────────────────────────────────┐
                │          Refiner Trait                │
                │                                      │
                │  compress(input) → compressed text    │
                │  embed(text) → vector embedding       │
                │  summarize(content, max) → summary    │
                │  memory_footprint() → bytes           │
                └──────────────────────────────────────┘
                     ▲            ▲            ▲
                     │            │            │
              ┌──────┴──┐  ┌─────┴───┐  ┌─────┴─────┐
              │  Noop   │  │  Rules  │  │  Candle   │
              │ (off)   │  │ (rules) │  │  (local)  │
              └─────────┘  └─────────┘  └───────────┘
                  0ms         <1ms         ~50ms
                  0% save     20-40%       60-70%
                  0 RAM       0 RAM        ~2GB RAM
```

Acceleration policy: Candle should use GPU backends when available (CUDA/Metal), with CPU fallback on minimal hardware.

### RuleRefiner Details (implemented: `tengu-optimizer/src/rules/mod.rs`)

**Pipeline:** `strip_hedging → strip_filler → collapse_whitespace`

```
Input:  "I was wondering if you could maybe help me basically just 
         write a sort of function that actually sorts an array"

After:  "help me write function sorts array"

Savings: 17 words → 6 words (65% reduction)
```

**Extractive Summarization** for knowledge store:

```
Input: 200-line Rust file

Keeps:  pub struct, fn, impl, trait, mod, use, /// comments
Drops:  function bodies, whitespace, non-structural code

Output: ~30 lines of structural summary
```

---

## 11. Flow Management

### Concept

A **Flow** is a conversation between one sender and one agent. Flows are scoped by the `FlowConfig`:

```rust
pub struct FlowConfig {
    pub scope: String,              // "per-sender" | "per-pipe" | "global"
    pub reset_mode: String,         // "idle" | "manual" | "time"
    pub idle_timeout_minutes: u32,  // Reset after N minutes of inactivity
}
```

### Flow Scoping

| Scope | Behavior | Use Case |
|-------|----------|----------|
| `per-sender` | Each unique `peer_id` gets its own conversation | Default — each person has private chat |
| `per-pipe` | All senders on a pipe share one conversation | Shared team channel |
| `global` | One conversation for the entire agent | Simple single-user setup |

### Persistence Format (planned)

```
~/.tengu/state/flows/{agent_id}/{pipe_id}/{peer_id}.jsonl

Each line:
{"role":"user","content":"...","timestamp":"2026-02-16T20:00:00Z"}
{"role":"assistant","content":"...","timestamp":"2026-02-16T20:00:01Z"}
{"role":"tool","content":"...","tool_call_id":"tc_001","timestamp":"..."}
```

### Why JSONL, Not a Database?

| Approach | Pros | Cons |
|----------|------|------|
| **JSONL files** ✅ | Append-only, human-readable, `cat`-able, zero deps, easy backup (`cp`) | No random access, no cross-flow queries |
| **SQLite** | Structured queries, cross-flow search | Binary format, extra dependency, overkill for per-user chat |
| **Vector DB** | Semantic search across flows | Massive overkill, violates zero-dep principle |

Conversation history is **sequential by nature** — you replay it oldest-to-newest into the engine's `messages[]` array. JSONL is the natural format for sequential append-only data. If you ever need cross-flow search ("what did I discuss with the driver last week?"), the knowledge store handles that — not the flow storage.

Important operational rule:
- JSONL is a storage format, not a prompt format.
- On each turn, Tengu loads a **token-windowed subset** (recent turns + compacted summaries), never the full transcript file.

### Compaction Strategy (planned)

When flow exceeds context window:
1. Keep system prompt (always)
2. Keep last N messages verbatim
3. Summarize older messages into a single "Previously discussed:" block
4. Convert older tool interactions to just results
5. Write compacted artifacts/snapshots separately and keep raw transcript archive

Token budget split (target default):
- recent conversational window
- retrieval context
- compacted summaries
- reserved output margin (derived from output token cap + safety headroom)

Critical hardening backlog (before production):
- Add durable compaction artifact lifecycle (retention + repair hooks).
- Add oversized tool-result guards.
- Add retention/rotation and transcript corruption repair routines.

---

## 12. Configuration System

### Loading Pipeline (`config/schema.rs`)

```
1. Find config:
   $TENGU_HOME/config.toml   OR
   ~/.tengu/config.toml       OR
   (use defaults)

2. Read file content

3. Substitute env vars:
   ${TELEGRAM_BOT_TOKEN}  → actual value from environment
   ${ANTHROPIC_API_KEY}   → actual value
   (missing vars → left as-is, not an error)

4. Parse TOML → Config struct

5. Serde defaults fill any missing fields
```

Environment reference:
- `.env.example` lists implemented and planned variables with usage notes.

### Full Config Structure

Current implemented config shape:

```
Config
├── runtime_profile: String          # "auto", "cloud", "desktop", "minimal"
├── hub: HubConfig
│   ├── bind: String                 # "127.0.0.1"
│   ├── port: u16                    # 7070
│   ├── auth_mode: String            # "token"
│   ├── auth_token: Option<String>
│   └── reload: ReloadConfig
│       ├── mode: String             # "hybrid"
│       └── debounce_ms: u64         # 300
├── refiner: RefinerConfig
│   ├── mode: String                 # "off", "rules", "local", "remote"
│   ├── model: Option<String>        # for local/remote
│   └── url: Option<String>          # for remote
├── agents: HashMap<String, AgentConfig>    # "main", "erzhan", "timur"...
│   └── (see Agent Isolation section)
├── routing: Vec<RoutingBinding>     # ingress mapping only
│   └── { agent, pipe, peer?, group_id?, account_id? } 
├── pipes: PipesConfig
│   ├── cli: Option<PipeEntry>
│   ├── telegram: Option<TelegramPipeConfig>
│   ├── discord: Option<DiscordPipeConfig>
│   └── webchat: Option<WebchatPipeConfig>
└── skills: SkillsConfig
    ├── watch: bool                  # Hot-reload skills
    └── extra_dirs: Vec<String>      # Additional skill directories
```

Target extension for single-orchestrator runtime:

```
Config
└── topology: TopologyConfig
    ├── mode: String                 # "single-orchestrator"
    ├── orchestrator_agent: String   # points to agents.<id>
    ├── dependent_agents: Vec<String>
    ├── allow_direct_dependent_communication: bool
    ├── max_handoff_depth: u8
    ├── defaults: TopologyDefaults
    │   ├── engine: String
    │   ├── model: String
    │   └── inherit_to_dependents: bool
    ├── policy: TopologyPolicy
    │   ├── require_orchestrator_approval: bool
    │   ├── tool_policy_source: String    # "global" | "agent-override"
    │   └── adapter_policy_source: String # "global" | "agent-override"
    ├── agent_overrides: Vec<TopologyAgentOverride>
    │   ├── agent: String
    │   ├── engine: String
    │   └── model: String
    └── adapter_rules: Vec<TopologyAdapterRule>
        ├── from: String
        ├── to: String
        ├── adapter: Option<String>
        ├── direction: String             # "one-way" | "two-way"
        ├── status: String                # "allow" | "deny"
        └── reason: Option<String>
```

---

## 13. Runtime Profiles

### Auto-Detection (`config/profile.rs`)

```rust
pub struct SystemCapabilities {
    pub total_ram_mb: u64,
    pub available_ram_mb: u64,
    pub cpu_cores: usize,
    pub arch: String,        // "aarch64", "x86_64"
    pub has_gpu: bool,       // CUDA or Apple Silicon Metal
}
```

### Profile Resolution

```
                  available_ram > 16GB AND has_gpu?
                    ├── yes → Cloud
                    └── no
                         available_ram > 4GB?
                           ├── yes → Desktop
                           └── no  → Minimal
```

| Profile | RAM | Refiner | Binary Size | Target |
|---------|-----|---------|-------------|--------|
| **Cloud** | 32GB+ | Candle ML | ~30MB | Server, workstation |
| **Desktop** | 8GB+ | Rules or small Candle | ~30MB | Laptop, Mac |
| **Minimal** | <8GB | Rules or off | ~8MB | Raspberry Pi, ARM |

### GPU Detection

```rust
fn detect_gpu() -> bool {
    // Manual override (cpu/gpu/cuda/metal/mps)
    if env::var("TENGU_GPU_HINT").is_ok() { /* parse + return */ }
    // CUDA
    if env::var("CUDA_VISIBLE_DEVICES").is_ok() { return true; }
    // Apple Silicon Metal
    if cfg!(target_os = "macos") && ARCH == "aarch64" { return true; }
    false
}
```

---

## 14. Skills System (planned)

### Concept

Skills are **markdown files** that teach an agent how to handle specific tasks.

```
~/.tengu/skills/
├── logistics/
│   └── SKILL.md       # "When user mentions a trip/route, record it..."
├── salary/
│   └── SKILL.md       # "When user mentions advance/salary, update..."
└── monitoring/
    └── SKILL.md       # "Check these URLs every hour..."
    └── scripts/
        └── check_dns.sh
```

### SKILL.md Format

```markdown
---
name: salary-tracker
description: Track driver salaries and advances
invocable: true
---

When the user mentions giving an advance or recording a salary:

1. Read the current month's file from `salary/{month}.md`
2. If the file doesn't exist, create it with a table header
3. Add or update the driver's record
4. Calculate: earned - advances = remaining
5. If remaining is 0 or negative, warn the user
6. Save the file
```

### Loading (planned design)

```
On startup:
1. Scan skills directories recursively
2. Parse SKILL.md frontmatter (name, description, invocable)
3. Load markdown body as instructions
4. Register with agent (inject into system prompt or use as dynamic context)

If skills.watch = true:
5. Watch filesystem for changes
6. Hot-reload modified skills without restart
```

---

## 15. Security Model

### Defense Layers

```
Layer 1: ACCESS CONTROL (Pipe level)
├── AccessPolicy::Approval  → new senders wait for admin approval
├── AccessPolicy::Allowlist → only whitelisted sender IDs
├── AccessPolicy::Open      → CLI only
└── AccessPolicy::Disabled  → pipe exists but rejects all

Layer 2: ROUTING (Router level)
├── Unknown senders → default agent (restricted)
└── Known senders → specific agent with appropriate permissions

Layer 3: WORKSPACE ISOLATION (Agent level)
├── Each agent has a workspace root
├── Tools canonicalize paths and check bounds
└── No escape via symlinks or ../

Layer 4: TOOL PERMISSIONS (Kit level)
├── allow: ["read_file", "write_file"]  → only these tools available
├── deny: ["shell"]                     → explicitly blocked
└── Tools not in allow AND not in deny  → denied by default

Layer 5: SHELL SAFETY (Tool level)
├── Safe commands auto-approved: ls, cat, head, wc
├── Dangerous commands auto-blocked: rm -rf, sudo
└── Everything else → user confirmation prompt

Layer 6: RESOURCE LIMITS (Limits level)
├── max_tokens_per_flow = 500_000
├── max_cost_per_flow = $5.00
├── warn_at_cost = $2.00
├── context_window_override = 128000
└── max_output_tokens_per_turn = 4096
```

### File System Permissions

```
~/.tengu/                    (700 — owner only)
├── config.toml              (600 — contains API keys)
├── state/                   (700)
│   └── flows/               (700)
├── skills/                  (755 — readable)
└── cache/                   (700)
```

---

## Appendix: Feature Flags

Provider/channel dependency selection follows `DEPENDENCY_POLICY.md` (official-first; typed REST fallback).

```toml
[features]
default = ["ollama", "anthropic", "openai", "google", "webchat"]

# Engines
ollama    = ["tengu-backends/ollama"]       # Local models
anthropic = ["tengu-backends/anthropic"]    # Claude API
openai    = ["tengu-backends/openai"]       # OpenAI API
google    = ["tengu-backends/google"]       # Gemini API
huggingface = ["tengu-backends/huggingface"] # HF API

# Pipes
telegram = ["tengu-channels/telegram"]       # Telegram bot
discord  = ["tengu-channels/discord"]        # Discord bot
webchat  = ["tengu-channels/webchat"]        # Browser UI

# Optimizer / local ML acceleration
candle = ["tengu-optimizer/candle"]           # Candle-enabled local ML path (GPU preferred, CPU fallback)
```

### Minimal Build (Raspberry Pi)

```bash
cargo build --release --no-default-features \
  --features "ollama"
# Result: minimal currently-working runtime (CLI + Ollama)
```

### Full Build

```bash
cargo build --release
# Result: includes compile-time scaffolds for planned modules
```
