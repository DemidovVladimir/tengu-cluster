# sealed-check — prove Jev, the decision loop and Studio run through the seal proxy (2026-10-09)

Sandbox `sandboxes/sealed-check/` · guard test `config::keys::tests::sealed_check_sandbox_is_harmless` · seal proxy: `docs/sealed-keys-2026-10-09.md` · set-up: `docs/cloudflare-secrets-setup.html`.

| What it proves | How |
|---|---|
| No OpenRouter key on this machine is used | `[keys.env]` exports `OPENROUTER_BASE_URL = <proxy>/openrouter` + the session; a stray local key is unset when the session fails (N1) |
| Jev (`~typesafe/jev-latest`) answers through the Worker | `tengu decide` scenarios, audit lines with `model`, `decision_id` |
| The decision loop: typed choice, tool ok, tool error, dry run, map narrowing, escalation gate | scenarios + `*.map.json` |
| The runtime: feed → loop, heartbeat, lease, graceful stop | `tengu run` + `tengu doctor --live` |
| The web UI: graph, Play / Send / Stop, timeline, replay | `tengu studio --allow-control` |
| No garbage | own root `~/tengu-sealed-check`; nothing in `~/.tengu`; one `rm -rf` |

Harmless by construction: one tool `hex_to_uint256` (pure compute) with a deny-all scope; no money, signer, memory, skills, MCP, planner, Telegram, webhooks, `[generation]`.

## Set up (once, after the Worker is deployed)

| # | Step |
|---|---|
| 1 | `sandboxes/sealed-check/config.toml` `[keys] proxy` = your `https://tengu-seal.<subdomain>.workers.dev` (default `http://127.0.0.1:8787` = `npx wrangler dev`) |
| 2 | Secretive key `tengu-unattended` in the Worker's `CLIENTS` with `"routes": ["openrouter"]` (`tengu keys setup` prints the line); Worker secrets `SESSION_KEY` + `OPENROUTER_API_KEY` |
| 3 | From the repo root: |

```bash
R="$HOME/tengu-sealed-check"; export TENGU_HOME="$R/home"         # never ~/.tengu
mkdir -p "$R/home" "$R/ws" && touch "$R/.t0"
unset OPENROUTER_API_KEY OPENROUTER_BASE_URL TENGU_KEYS_PROXY TENGU_KEYS_SESSION_TOKEN TENGU_KEYS_SESSION_EXP_MS
export SSH_AUTH_SOCK="$HOME/Library/Containers/com.maxgoedjen.Secretive.SecretAgent/Data/socket.ssh"
cargo build --release && T=target/release/tengu; SC=sandboxes/sealed-check/scenarios
```

## Terminal

Jev answers vary call to call: a scenario passes on 2 of 3 tries.

| # | Command | Expect |
|---|---|---|
| K0 | `$T keys status --sandbox sealed-check` | routes listed; `session: ok · label tengu-unattended · … · granted openrouter` (the session asks only for the routes `[keys.env]` uses) |
| K1 | `$T keys check openrouter --path v1/models --sandbox sealed-check` | `200 OK · application/json` |
| K2 | `$T keys check telegram --path botTENGU_SECRET/getMe --sandbox sealed-check` | exit 1: `seal proxy refused the session (403 Forbidden): this client key may use none of the requested routes` |
| A0 | `$T doctor --sandbox sealed-check` | `[keys] seal-proxy session started … exported=["OPENROUTER_API_KEY", "OPENROUTER_BASE_URL"]`; `checker: … endpoint=<proxy>/openrouter` |
| A1 | `$T decide --sandbox sealed-check --loop check --event $SC/normal.json` | `hold · stopped` |
| A2 | `… --event $SC/convert.json` | `convert` `{"hex":"0xff"}` → `"255"`, then `hold` |
| A3 | `… --event $SC/tool-error.json` | `convert_bad` `ok:false` `invalid hex`, then `hold` |
| A4 | `… --event $SC/dry.json` | `convert_dry` → `dry_run`, then `hold` |
| A5 | `$T decide --sandbox sealed-check --map $SC/uncertain.map.json` | `act_at` 1.0: `escalated`, or `hold` when Jev is fully sure |
| A6 | `… --map $SC/convert-seq.map.json` | `convert` `{"hex":"0x2a"}` → `"42"`, then `hold` |
| N1 | `SSH_AUTH_SOCK=/nonexistent OPENROUTER_API_KEY=sk-fake $T decide --sandbox sealed-check --loop check --event $SC/normal.json` | exit 1: `[keys] seal-proxy session not set up — the [keys.env] vars are unset` + `OPENROUTER_API_KEY is required` — **a local key never bypasses the proxy** |
| T1 | `$T trace runs --sandbox sealed-check` · `$T trace show --sandbox sealed-check --run <id>` | one line per run; `run.opened → trigger.decide → observation.read → jev.completed → action.selected → tool.* → action.completed → trigger.completed` |
| R1 | `$T run --sandbox sealed-check` (terminal A) · `$T doctor --sandbox sealed-check --live` (B) | `=> live`; `loop check … done ≥ 1`; `feed tick (required) live` |
| R2 | Ctrl-C in A | `tengu run stopped … lease_released=true`; heartbeat `stopped` |

## Web UI (Studio)

| # | Step | Expect |
|---|---|---|
| S1 | `$T studio --sandbox sealed-check --allow-control > "$R/studio.out"`; open the printed URL | graph 14 nodes · 15 edges; runtime `idle`; scenarios `convert`, `dry`, `normal`, `tool-error` |
| S2 | Play | runtime `running`, model `typesafe/jev-…`, health `live`; tick → `check` → Jev → `hold` |
| S3 | Send `convert`, `tool-error`, `dry` | `convert` green, `convert_bad` red, `convert_dry` dashed (logged); loop `failed 0` |
| S4 | Stop → Replay | runtime `stopped`, health `not live`, run closed; Replay slides through it |
| S5 | Ctrl-C Studio | exit 0; `trace runs` shows kinds `decide`, `run`, `studio` |

Headless alternative: the curl helper in `docs/studio-2026-10-08.md` § Play / Stop.

## Leak + isolation checks, then clean up

```bash
rg -l 'tss1\.|sk-or-' "$R" || echo none                 # no session token / key in any file
find ~/.tengu -newer "$R/.t0" -type f                  # only other runs' files (e.g. a weekend run), none from sealed-check
rg -c 'sealed-check' ~/.tengu/logs/*.jsonl ~/.tengu/logs/tengu.log || echo none
pgrep -fl 'tengu (run|studio) --sandbox sealed-check' || rm -rf -- "$R"
```

Outside this machine only Cloudflare Workers Logs keep a line per call (3 days).

## Recorded run (2026-10-09, local `wrangler dev`, real OpenRouter key loaded into the local Worker only)

| Check | Result |
|---|---|
| K0 · K1 · K2 | session ok, may use `openrouter` · `v1/models` 200 (782 050 bytes) · telegram 403 |
| A0 | session started, exported `OPENROUTER_API_KEY`, `OPENROUTER_BASE_URL`; endpoint `http://127.0.0.1:8787/openrouter` |
| A1–A6 | hold · `0xff`→`255` · `0xzz` failed then hold · dry_run · hold (Jev sure) · `0x2a`→`42` |
| N1 | exit 1, vars unset, `OPENROUTER_API_KEY is required` — stray key not used |
| Audit | 10 Jev lines for A1–A6, model `typesafe/jev-1.13-20260917` |
| R1 · R2 | `=> live`, tick → Jev → hold · SIGINT, `lease_released=true` |
| S1–S5 | guards: no token 401, POST without proofs 403; Play → 3 events → Stop; loop `accepted 9 · completed 9 · failed 0`; screenshots below |
| Isolation | no `tss1.` / key value in any sandbox file; `~/.tengu/logs/decisions.jsonl` sha256 unchanged; no `sealed-check` line under `~/.tengu` |
| Hardened Worker (same local Worker) | key-exfiltration requests refused; `allow` refused `openrouter` `v1/keys`; a 6.5 MB reply streamed through |

Evidence: `docs/sealed-check-evidence/` — `01-studio-running.png`, `02-studio-stopped.png`, `trace-runs.jsonl` (10 runs: 7 decide, 2 run, 1 studio), `decisions-summary.jsonl` (23 Jev decisions). Sessions longer than the client's `session_hours` end with Jev 401 — restart the command.
