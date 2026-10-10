# Studio builder — drag-and-drop sandboxes (2026-10-10)

n8n-style canvas inside Tengu Studio: drag agents, tools, skills, workspaces, secrets and channels, wire them, **Finalise** → `sandboxes/<name>/config.toml`. Rust decides every verdict (palette, wire colour, TOML, load check); the page draws. Built from the ST-40 design (`docs/studio-editor-design-2026-10-08.md`). Tutorial: `docs/tutorial/builder.html`. Code: `docs/code-map.md` § Studio builder.

## Start

| Step | Command / action |
|---|---|
| 1. New sandbox | `tengu sandbox new` → asks **name** (a-z 0-9 - _, ≤ 40) + template (`blank` · `team` · `telegram`) → writes `sandboxes/<name>/builder.json` + `config.toml` (checked by the real loader first; an existing dir is refused) → opens the builder in the browser |
| 2. Compose | drag cards from the left palette, drag from a card's right port to another card's left port; edit fields in the right inspector (autosaved to `builder.json`) |
| 3. Validate | **Validate** = compile + `Config::load` on a temp copy + diff vs `config.toml` + secrets checklist |
| 4. Finalise | **Write config.toml** (only when the loader accepts it; old file → `config.toml.prev`) |
| 5. Run | `tengu chat --sandbox <name>` (· `tengu telegram` · `tengu webhooks` · `tengu studio`) |
| Re-open | `tengu studio --sandbox <name> --allow-edit [--open]` → prints `Builder: http://127.0.0.1:<port>/builder#t=<token>` |

Flags: `tengu sandbox new --name <n> --template <t> [--no-open] [--port <p>]` (no prompts).

## Cards → TOML

| Card | Writes | Wires |
|---|---|---|
| Sandbox (always one) | `[egress] network` / `allow_hosts`, `[memory] enabled`, `[studio] control` | — |
| Agent | `[agents.<name>]` engine, model, default, identity, limits, `local` / `claude_code` / `codex` knobs | → tool, skill, workspace, agent (delegates) |
| Orchestrator (one) | `[orchestrator] agent`, attempts, replans | → the planner agent |
| Tool | the agent's `tools`; Restrict on → `[agents.<a>.scopes.<tool>]` (deny by default per field) | ← agent, ← secret |
| Skill | `skill_packages` | ← agent |
| Workspace | `workspace`; Confine → file-tool scopes `fs_roots = [path]` | ← agent |
| Secret | **nothing** (name only) · Cloudflare store: `[keys.env]`, `strip` | → agent (engine key), tool (`env_reads`), seal proxy, Telegram, webhook |
| Cloudflare seal proxy (one) | `[keys] proxy`, `client`, `agent_socket` | ← secret |
| Telegram (one) | `[telegram] enabled`, `allowed_users` | → agent (= default) |
| Webhook | `[webhooks.endpoints.<name>]` agent, `secret_env`, goal | → agent |

Wire colours: **green current** = Rust compiled it · amber = compiles, read the note · red = refused (inspector says why) · grey = not checked yet. Table of every wire: `config/builder/palette.rs::CONNECTIONS`.

## Engines

| Engine | Card | Auth | Notes |
|---|---|---|---|
| `openrouter` | Agent · OpenRouter API | `OPENROUTER_API_KEY` (vault / env / Cloudflare) | planner: use this one |
| `claude_code` | Agent · Claude (subscription) | `claude` CLI login | build `--features claude_code`; built-in tools profile |
| `codex` | Agent · OpenAI (ChatGPT subscription) | `codex login` | default feature; `sandbox` read-only / workspace-write; refused in hardened sandboxes (`docs/engine-backends.md` § Codex) |
| `local` | Agent · Local model | optional key env | server URL (LAN ok) + context window |

Live check 2026-10-10 (one canvas-built sandbox, `tengu tool turn`, list_directory + read_file through tengu): openrouter ✅ · claude_code ✅ · codex ✅ · local: not run here (operator's PC).

## Secrets

| Store | Where the value lives | Command (Secrets tab, copy button) |
|---|---|---|
| Local vault | `~/.tengu/secrets.vault` (AES-256-GCM, master password) | `tengu secret set NAME <value>` |
| Env / `.env` | the shell or `.env` | `export NAME=<value>` |
| Cloudflare seal proxy | a Worker secret; tengu gets a route + session | `npx wrangler secret put NAME` |

The Cloudflare store needs a build with `[keys]` (PR #50). The builder probes it (`config::builder::keys_supported`), so it switches on once #50 merges, with no builder change. Checked on #50 + #51 merged in: the composed `[keys]` loads.

## Rules

| Rule | Where |
|---|---|
| Only sandboxes made by `tengu sandbox new` (with `builder.json`) are editable; hand-written ones are view-only | `application/builder.rs::why_not` |
| `[generation]`-bound (W1) or hardened (`[risk]`, `[paper]`, `[soe]`, `[xmarket]`, a Solana signer) = view-only | `config/builder::editable` |
| Finalise refused while the sandbox's `tengu run` heartbeat is fresh (409) or the preview is stale (409, sha256) | `Builder::finalise` |
| Owned sections rewritten; every other top-level key of `config.toml` kept as it is | `compile::OWNED` |
| Never offered: `run_command`, wallet signing, Solana sends, exec tools | `palette::offered` |
| Keys never in TOML or `builder.json` | `compile` |
| Loopback only, token + CSRF guard (same as Studio) | `adapters/inbound/studio/guard.rs` |

## Debug

| Need | How |
|---|---|
| What would Finalise write? | `tengu sandbox compile --sandbox <n>` — TOML on stdout, every issue (`[card <id> · <field>]`, `[wire <id>]`) + loader verdict on stderr, exit 1 on an error |
| Finalise without a browser | `tengu sandbox finalise --sandbox <n>` |
| The palette | `tengu sandbox palette` (JSON) |
| What the page sent / Rust answered | **Debug** drawer: issues, blueprint JSON, last response, request log |
| Server side | one `builder:` line per request (route, sandbox, ms, refusal) in stderr + `<TENGU_HOME>/logs/tengu.log` |
| The canvas itself | `sandboxes/<n>/builder.json` (plain JSON; `deny_unknown_fields`) |

## Not yet

| Gap | Note |
|---|---|
| Decision loops, feeds, `[[mcp_servers]]` cards | kept as hand-written TOML sections (not lost) |
| Import a hand-written sandbox onto the canvas | view-only today |
| Setting a secret value from the page | by design: the page shows the command |
| Local engine live check | operator's PC |
