# Sealed keys — no provider key on this machine (2026-10-09)

Operator decision 2026-10-09 (local k8s / Helm, VPS cluster and encrypted USB rejected). Provider keys are secrets of the `tengu-seal` Cloudflare Worker, set by the operator; tengu names a route, the Worker adds the key and forwards. Operator walkthrough: `docs/cloudflare-secrets-setup.html`. Tutorial: `docs/tutorial/sealed-keys.html`. End-to-end proof: `sandboxes/sealed-check` (`docs/sealed-check-2026-10-09.md`).

| Piece | Lives | Code |
|---|---|---|
| Provider keys, one per route | Worker secrets — dashboard or `npx wrangler secret put <NAME>`; never on this machine | — |
| `SESSION_KEY` (random, ≥ 32 chars) | Worker secret; HMAC key of every session | `crates/tengu-seal/src/session.rs` |
| `ROUTES` `name → {upstream, secret, header \| path \| query, allow?}` | `wrangler.toml` `[vars]`, no secrets | `crates/tengu-seal/src/route.rs` |
| `CLIENTS` `SHA256:<fp> → {label, session_hours, routes}` | `wrangler.toml` `[vars]`; fingerprints are public | `route.rs` |
| Client key (`ecdsa-sha2-nistp256` / `ssh-ed25519`) | ssh-agent — Secretive keeps it in the Secure Enclave (Touch ID) | `crates/tengu-seal/src/ssh.rs` · `src/adapters/outbound/keys/agent.rs` |
| Session `tss1.<claims>.<hmac>` (24 h default, 1–168 per client), claims carry the granted routes | tengu process env only (`TENGU_KEYS_SESSION_TOKEN`) | `session.rs` |
| Upstream target, key position, path checks, reply masking | the Worker | `crates/tengu-seal/src/target.rs` · `cloudflare/seal-worker/src/lib.rs` |

## Worker

| Endpoint | Auth | Does |
|---|---|---|
| `GET /healthz` | none | `ok` + route names |
| `POST /session` | SSH signature over `tengu-seal/session/v1` + fp + ts (±60 s) + nonce + Worker origin + `routes:` (the asked routes, signed); fp in `CLIENTS` | grants asked ∩ the client's routes (403 if none) → token + `exp_ms`, fp, label, `routes` |
| `GET /whoami` | session | fp, label, expiry, granted routes |
| `ANY /<route>/<rest>` | session; the route granted to it AND still in its key's `CLIENTS` routes | key in → fetch (`redirect: manual`) → key masked out → back |
| `ANY <path>` + `Tengu-Route: <route>` | same | same, for clients that build absolute paths (teloxide) |

Each route sets exactly one key position — anything else is 403, so a session can use a key but never copy it out (webhook URL, message text):

| Field | Key goes | Caller sends |
|---|---|---|
| `"header": "<Name>: <value with {secret}>"` | that header | no `TENGU_SECRET` anywhere |
| `"path": ["bot{secret}/", …]` | the start of the path, one template | `TENGU_SECRET` exactly once, where the template has `{secret}` |
| `"query": "<param>"` | that query parameter | `?<param>=TENGU_SECRET`, exactly once |

| Config field | Rule |
|---|---|
| route name | 1–32 chars `[a-z0-9_-]`, not `healthz` / `session` / `whoami` |
| `upstream` | `https://host[:port][/base]`; no query, fragment or userinfo |
| `secret` | Worker secret name, `[A-Z0-9_]`, never `SESSION_KEY` |
| `header` | `{secret}` once in the value; not host, cookie, content-length, hop-by-hop, `cf-*`, `tengu-*` |
| `path` | each template: `{secret}` once, relative plain path (no `/` start, `..`, `//`) |
| `query` | parameter name `[A-Za-z0-9_-]`, ≤ 64 |
| `allow` | optional path prefixes (after a `path` key part), whole segments; empty = any; else 403 `this path is not allowed on this route` |
| key with `/ ? # @ \ % & =` or a space | header route only (path / query: 500) |
| `CLIENTS` key | full OpenSSH fingerprint (`SHA256:` + 43 chars, what `ssh-add -l` prints) |
| `session_hours` · `routes` | 1–168, default 24 · route names or `["*"]`; **empty = none**; a name not in `ROUTES` = 500 on `/session` and every call |

| Shipped route | Upstream | Secret | Key at | `allow` | `[keys.env]` |
|---|---|---|---|---|---|
| `openrouter` | `https://openrouter.ai/api` | `OPENROUTER_API_KEY` | header `Authorization: Bearer {secret}` | `v1/chat/completions`, `v1/embeddings`, `v1/models`, `alpha/decisions` | `OPENROUTER_BASE_URL = "openrouter"`, `OPENROUTER_API_KEY = "@session"` |
| `telegram` | `https://api.telegram.org` | `TELEGRAM_BOT_TOKEN` | path `bot{secret}/`, `file/bot{secret}/` | — | `TELEGRAM_API_URL = "telegram"` |
| `solana-rpc` | `https://mainnet.helius-rpc.com` | `HELIUS_API_KEY` | query `api-key` | — | `SOLANA_RPC_URL = "solana-rpc/?api-key=TENGU_SECRET"` |
| `evm-rpc` | `https://eth-sepolia.g.alchemy.com/v2` | `ALCHEMY_API_KEY` | path `{secret}` | — | `EVM_RPC_URL = "evm-rpc/TENGU_SECRET"` |

`crates/tengu-seal` test `shipped_wrangler_toml_parses` parses the shipped `ROUTES` / `CLIENTS`.

Replies: the route's key (≥ 8 chars) → `[REDACTED]` in every header value; text / JSON / XML / JavaScript bodies read in chunks up to 2 MiB and masked on bytes (invalid UTF-8 survives); a larger one streams on unmasked from there (log `masked` = -1); `text/event-stream` and binary pass through; HEAD and 101 / 204 / 205 / 304 stay body-less. Dropped before forwarding: `authorization` (the session), `cookie`, `host`, hop-by-hop, `accept-encoding`, `cf-*`, `tengu-*`, `x-forwarded-*`, `forwarded`. Log: one JSON line — client, route, upstream host, status, masked count, ms.

| Status | When |
|---|---|
| 400 | unknown route, a session request with no / an unknown route, malformed request |
| 401 | no / forged / expired session, `SESSION_KEY` rotated, key not (or no longer) in `CLIENTS`, clock > 60 s off |
| 403 | key may use none of the asked routes · route not in the session or no longer in `CLIENTS` · `TENGU_SECRET` not exactly at the route's key position · path outside `allow` · path or query leaves the upstream |
| 500 | misconfigured: a route's secret or `SESSION_KEY` missing / short, `ROUTES` / `CLIENTS` do not parse, a bad route entry, a `CLIENTS` route not in `ROUTES`, a URL-unsafe key on a path / query route |
| 502 | upstream unreachable (its URL never in the message) |

## tengu side

| Item | Behaviour |
|---|---|
| `[keys]` (`src/config/keys.rs`) | `proxy` (https origin, no user / password; `http://127.0.0.1` / `localhost` for `wrangler dev`) · `client` (agent key comment or full fp; unset = the agent's only key) · `agent_socket` (unset = `$SSH_AUTH_SOCK`) · `strip` · `[keys.env]`; every field needs `proxy`; its host must be in `[egress] allow_hosts` when set |
| `[keys.env]` | `VAR = "<route>[/path][?query]"` → `<proxy>/<value>`; `VAR = "@session"` → the token — only `OPENROUTER_API_KEY`, and only next to an `OPENROUTER_BASE_URL` route value; overrides inherited values. `KeysConfig::routes()` = the route names used = what the session asks for |
| `[keys] strip` | env var names removed at load (`TELEGRAM_BOT_TOKEN`, `ALCHEMY_API_KEY`, …: local copies of Worker keys), on success and on failure; a name `[keys.env]` sets is a load error |
| `install` | at `load_sandbox_or` (base config too) and again in `run-agent` / `mcp-bridge` children (reuse the parent's session: same canonical origin, > 5 min left; strip again — `.env` was re-read). Proxy canonical: lowercase host, no default port, no trailing slash. Exports `TENGU_KEYS_PROXY`, `TENGU_KEYS_SESSION_TOKEN`, `TENGU_KEYS_SESSION_EXP_MS` + `[keys.env]`; warns naming routes the Worker did not grant. On failure every `[keys.env]` var and the session vars are UNSET (warn) — a stray local key never bypasses the proxy |
| Redaction | token name ends in `_TOKEN`; `cli/mod.rs::registry_after_keys` EXTENDS the startup registry after the export (never rebuilt: overwritten / stripped `.env` and vault values stay masked) |
| Clients | OpenRouter engine, embeddings, Jev, wiki compiler: no code change. Solana RPC / EVM receipt poll: `auth_header_for` (proxy origin only). Telegram: `header_mode` (`<proxy>/` + `Tengu-Route`), placeholder token; with `TELEGRAM_API_URL` in `[keys.env]` and no session `tengu telegram` refuses to start — never a local token |
| `tengu keys` (no vault unlock) | `setup [--proxy] [--agent-socket]` → `CLIENTS` lines with the routes this config uses (none: `["openrouter"]`) + `[keys]` block · `status` → routes, `[keys.env]` vs routes, session for `KeysConfig::routes()`, `/whoami`, `granted …` · `check <route> [--path p]` → a session for that one route, one GET: status, type, size, ms. All take `--sandbox` |
| Not moved | wallet signing (`signer_key_file`, Privy) — separate "go"; local Postgres; Claude Code login; public HL / Gecko |

## Set up (once) · revoke

| # | Step |
|---|---|
| 1 | Secretive: `tengu-attended` (Touch ID) + `tengu-unattended` (no prompt); `SSH_AUTH_SOCK` = its socket, or `[keys] agent_socket` |
| 2 | `tengu keys setup [--sandbox <s>]` → paste the `CLIENTS` lines into `cloudflare/seal-worker/wrangler.toml` (168 h for the unattended key; explicit `routes` per key) |
| 3 | `cd cloudflare/seal-worker && npx wrangler login && npx wrangler deploy` |
| 4 | Secrets (dashboard or `npx wrangler secret put`): `SESSION_KEY` + each route's secret — rotate each provider key first |
| 5 | Paste the `[keys]` block (`proxy` = the `workers.dev` origin, `strip` = the moved local names) into the sandbox config |
| 6 | `tengu keys status` · `tengu keys check openrouter --path v1/models`; then delete the keys from `.env` / the vault |

| Revoke | How |
|---|---|
| one machine | delete its `CLIENTS` line, `npx wrangler deploy` → next request 401 (`CLIENTS` is read on every request) |
| one route of a machine | drop it from that key's `routes`, deploy → next call on it 403, even inside an open session |
| every session | rotate `SESSION_KEY` |
| a provider key | `npx wrangler secret put <NAME>`; nothing changes on the Mac |

## Free tier (developers.cloudflare.com, 2026-10)

| Limit | Free | Fit |
|---|---|---|
| Requests | 100k/day | 5–20k/day expected; `sealed-check`'s tick feed ≈ 2.9k/day while it runs; watch xlab Jev replays |
| CPU | 10 ms/request (fetch wait free) | HMAC + target ≈ ms; masking a body ≤ 2 MiB is the cost — measure in the deploy check |
| Subrequests | 50/request | 1 |
| Workers Logs | 200k events/day, 3 days | 1 line/request |
| Durable Objects · KV | — | not used |
| Not free | Containers (tengu itself on CF), > 10 ms CPU | $5/mo Workers Paid |

## Verified · open

| Verified on local `wrangler dev` | Result |
|---|---|
| fake secrets through httpbin (before the key-position hardening) | bearer 200, header template 200, wrong creds 401, echoed key masked, missing secret 500, dead upstream 502, path escape never forwarded, unknown route 400 |
| hardened Worker, real OpenRouter key, `sandboxes/sealed-check` | Jev `convert` `0xff` → `255`, tool error, dry run, maps, `tengu run` + `doctor --live`, Studio Play / Send / Stop; N1 fail closed; key-exfiltration requests refused; `allow` refused `v1/keys`; a 6.5 MB reply streamed |

| Open | Note |
|---|---|
| sessions are not refreshed in-process | a long run must restart before `session_hours` (unattended key: up to 168 h) |
| a session holder can USE a route's key, never READ it | e.g. call any Telegram Bot API method as the bot; `allow` narrows what a route accepts |
| live deploy · Tor reach of workers.dev · CPU p95 | operator steps, `docs/cloudflare-secrets-setup.html` |

Residual: while tengu runs, an agent with a shell on this machine can USE the session or the agent socket; it can never READ a provider key — there is none here.
