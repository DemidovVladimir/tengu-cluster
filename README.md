# Tengu Cluster

Fast, low-cost Rust agent hub for business workflows across chat channels.

Architecture style:
- Adapter-first runtime boundaries (`Engine`, `Pipe`, `Refiner`, `Tool`) for plug-and-play providers/channels/tools.
- Event-driven runtime processing (stream events + channel message queues), with `DomainEvent`/`EventBus` contracts, bounded in-process bus, runtime emitters, audit/metrics/policy subscribers, and profile-aware backpressure validation implemented.

Architecture guardrail:
- New providers/channels/tools/refiners must be added via `tengu-core` adapter traits and must not introduce provider-specific orchestration coupling in `src/main.rs`.
- New runtime side-effects should be introduced as domain-event subscribers (or marked explicitly as temporary with linked follow-up tasks).

## What It Is

Tengu Cluster is a single Rust application that routes messages to AI models and keeps strict control over:
- token usage
- storage/retrieval behavior
- operational cost

Current working baseline:
- CLI chat runtime
- Ollama backend (streaming)
- Anthropic backend (typed REST, non-streaming)
- OpenAI backend (typed REST, non-streaming)
- Claude Code backend (subprocess, non-streaming)
- single-orchestrator-first topology direction with one central policy/audit control plane (dependent agents remain flexible)
- backend diagnostics metadata surfaced in `status`, `doctor`, and `/engine`
- prompt reserve aligned to engine output caps (avoids over-reserve on large-context models)
- in-memory knowledge retrieval with budget-capped query API (`query_with_budget`)
- append-only tool audit trail (`~/.tengu/state/audit/tool_calls.jsonl`) persisted by event subscriber from runtime tool lifecycle events
- config-driven tool approval gates (`kit.approval_required` + `kit.approved`) plus pre-execution allow/deny policy re-checks
- typed inter-agent handoff task/result envelopes in `tengu-core` for orchestrator/dependent workflows
- capability governance enforcement in runtime (`user` vs `delegated` actor gate) plus handoff policy evaluator with bounded tool/skill/engine checks
- delegated orchestrator control-plane baseline in chat runtime (`/assign`, `/assignments`) with user-boundary checks
- official-first dependency policy for providers/channels
- Candle as planned local acceleration path (CUDA/Metal when available, CPU fallback)

## Why Use It

- Lower running cost through explicit token budgeting
- Predictable behavior for long-running business chats
- Rust-first architecture for speed and deploy simplicity
- Adapter + event-driven design keeps integrations modular and auditable
- Runtime is designed to run both on minimal single-core devices and higher-core machines
- Single configuration surface keeps orchestration/policy/model defaults simple for users
- Clear path to durable flows, compaction, and retrieval guard rails

## How To Use

### 1. Build

```bash
cargo build
```

### 2. Configure

Copy and edit config:

```bash
mkdir -p ~/.tengu
cp config.example.toml ~/.tengu/config.toml
```

Export environment variables as needed:

```bash
cp .env.example .env
# then export variables from .env using your shell tool of choice
# (or export directly, e.g. `export OLLAMA_HOST=http://localhost:11434`)
# for Anthropic/OpenAI engines set provider API keys
```

To run with OpenAI, set your agent config to:

```toml
[agents.main]
engine = "openai"
model = "gpt-4o-mini"
```

To run with Claude Code CLI, set your agent config to:

```toml
[agents.main]
engine = "claude-code"
model = "claude-sonnet-4-5-20250929"
```

Claude Code backend notes:
- ensure `claude` is installed and authenticated in your shell profile
- optional binary override via `CLAUDE_CODE_BIN`

Optional per-agent provider tuning:

```toml
[agents.main.limits]
context_window_override = 128000
max_output_tokens_per_turn = 4096
```

Environment variable details:
- `.env.example`

### 3. Run

```bash
cargo run -- chat
```

Useful commands inside chat:
- `/help`
- `/cost`
- `/context`
- `/assign <dependent> <cap1,cap2,...> [objective...]` (delegated mode)
- `/assignments` (delegated mode)
- `/eco`, `/standard`, `/precise`
- `/reset`

## Manual Smoke Test

Preconditions before chat testing:
- `~/.tengu/` is writable (or `TENGU_HOME` points to a writable directory)
- Ollama is running at `OLLAMA_HOST`
- the configured model is available locally (for example `llama3.2`)

Suggested check sequence:

```bash
mkdir -p ~/.tengu
cp config.example.toml ~/.tengu/config.toml

# Ensure model is available in local Ollama registry
ollama pull llama3.2

# Optional: load env vars
set -a
source .env
set +a

cargo run -- status
cargo run -- doctor
cargo run -- chat
```

If `doctor` prints:
- `probe /api/tags... Unreachable`: start Ollama or fix `OLLAMA_HOST`
- `Flow store ... Error`: ensure `TENGU_HOME` parent is writable

## Development Checks

Run local quality checks:

```bash
scripts/check_rust_file_descriptions.sh
cargo fmt --all
cargo test --workspace
```

Enable the pre-commit hook:

```bash
git config core.hooksPath .githooks
```

## Key Docs

- `PRD.md`
- `USER_STORIES.md`
- `ARCHITECTURE.md`
- `ROADMAP_MVP.md`
- `EVENT_BUS_MIGRATION_PLAN.md`
- `STORAGE_RETRIEVAL_GAP_ANALYSIS.md`
- `DEPENDENCY_POLICY.md`
- `RUST_PATTERNS_PLAYBOOK.md`
