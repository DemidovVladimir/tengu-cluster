# Sealed keys — no provider key on this machine (2026-10-09)

Operator decision 2026-10-09 (plan: the Molecule `[BE] AC` series was the example; local k8s / Helm, VPS cluster and encrypted USB rejected). Tutorial: `docs/tutorial/sealed-keys.html`.

| Piece | Lives | Code |
|---|---|---|
| Vault seed → X25519 key + session key | `KeyVault` Durable Object (Cloudflare), never exported | `cloudflare/seal-worker/src/vault_do.rs` |
| Sealed blob `tsb1.<meta>.<enc>.<ct>` (HPKE, meta = AAD) | `<TENGU_HOME>/sealed/<name>.sealed` — ciphertext, safe in git | `crates/tengu-seal/src/blob.rs` |
| Client key (P-256 / Ed25519) | ssh-agent — Secretive keeps it in the Secure Enclave (Touch ID) | `src/adapters/outbound/keys/agent.rs` |
| Allow-list `clients:<fp>` → `{label, session_hours}` | `CLIENTS` KV | Worker `client()` |
| Session `tss1.…` (24 h default, 1–168 per key) | process env only (`TENGU_KEYS_SESSION_TOKEN`) | `crates/tengu-seal/src/session.rs` |

## Worker routes

| Route | Auth | Does |
|---|---|---|
| `GET /pubkey` | none | public key + `kid` (seed made on first call) |
| `POST /session` | SSH signature over `tengu-seal/session/v1` + fp + ts (±60 s) + nonce + Worker origin | session |
| `GET /whoami` | session | fp, label, expiry |
| `ANY /s/<blob>/<rest>` · `/fwd/<rest>` + `Tengu-Sealed` · any other path + `Tengu-Sealed` | session | open blob → client allowed? → path inside upstream? → inject → fetch (`redirect: manual`) → stream back |

Inject modes: `bearer` · `header:<Name>` · `basic` (`user:pass`) · `placeholder[:<TOKEN>]` (default `TENGU_SECRET`, path or query) · `url` (secret = whole URL, same host). Errors: 400 malformed · 401 not allow-listed / expired / forged · 403 tampered blob, `kid` mismatch, client not allowed, path escape.

## tengu side

| Item | Behaviour |
|---|---|
| `[keys]` (`src/config/keys.rs`) | `proxy` (https origin; `http://127.0.0.1` for `wrangler dev`), `pubkey`, `client`, `agent_socket`, `sealed_dir`, `[keys.env]`; proxy host must be in `[egress] allow_hosts` when set |
| `install` at `load_sandbox_or` | reuse the parent's session (same proxy, > 5 min left) or sign a new one; export `TENGU_KEYS_PROXY`, `TENGU_KEYS_SESSION_TOKEN`, `TENGU_KEYS_SESSION_EXP_MS` + `[keys.env]`; fail-soft (warn) |
| `[keys.env]` | `VAR = "<blob>"` → `<proxy>/s/<blob>`; `VAR = "@session"` → token. OpenRouter engine, embeddings, Jev, wiki compiler: no code change |
| Header-less clients | Solana RPC / EVM receipt: `auth_header_for` (proxy origin only). Telegram: `TELEGRAM_API_URL` blob → `header_mode`, placeholder token, no `TELEGRAM_BOT_TOKEN` needed |
| Not moved | wallet signing (`signer_key_file`, Privy) — separate "go"; local Postgres; Claude Code login; public HL / Gecko |

## Set up (once)

| # | Step |
|---|---|
| 1 | Secretive: create `tengu-attended` (Touch ID) and `tengu-unattended` (no prompt); `export SSH_AUTH_SOCK=~/Library/Containers/com.maxgoedjen.Secretive.SecretAgent/Data/socket.ssh` |
| 2 | `cd cloudflare/seal-worker && npx wrangler login && npx wrangler kv namespace create CLIENTS` → paste the id into `wrangler.toml`; `npx wrangler deploy` |
| 3 | `tengu keys setup --proxy https://tengu-seal.<sub>.workers.dev` → paste `[keys]`; run the printed `wrangler kv key put … clients:<fp>` lines (`session_hours` 168 for the unattended key) |
| 4 | Rotate each provider key, then `tengu keys seal openrouter --upstream https://openrouter.ai/api` (hidden prompt or `--stdin`); Telegram: `--upstream https://api.telegram.org --inject placeholder`; Helius: `--inject url` |
| 5 | `tengu keys status`, `tengu keys check openrouter --path v1/models`; then delete the keys from `.env` / the vault |

Revoke: `npx wrangler kv key delete --binding CLIENTS --remote "clients:<fp>"` — new sessions fail at once, live ones within 60 s (isolate cache). Worker key lost / rotated: every blob shows `kid STALE` in `tengu keys status` → reseal.

## Free tier (developers.cloudflare.com, 2026-10)

| Limit | Free | Fit |
|---|---|---|
| Requests | 100k/day | 5–20k/day expected; watch xlab Jev replays |
| CPU | 10 ms/request (fetch wait free) | session open + HPKE open ≈ ms; measure in the deploy check |
| Subrequests | 50/request | 1–2 |
| Durable Objects (SQLite) | 100k req/day | seed read once per isolate |
| KV | 100k reads / 1k writes per day | allow-list cached 60 s |
| Workers Logs | 200k events/day, 3 days | 1 line/request |
| Not free | Containers (tengu itself on CF), > 10 ms CPU | $5/mo Workers Paid |

Verified 2026-10-09 against a local `wrangler dev` (no deploy yet): setup, seal, status, session, `/whoami`, and every inject mode through httpbin (basic 200 / wrong creds 401, bearer 200, placeholder 204, url 202, path escape 400, url + path 403). Open: live deploy, Tor reachability of workers.dev, CPU p95.

Residual: while tengu runs, an agent with a shell on this machine can use the session or the agent socket; it cannot read a provider key.
