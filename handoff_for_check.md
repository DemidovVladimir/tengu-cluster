# Handoff for check

> For the operator's architecture agent. Newest session first; earlier sessions below, unchanged. Ids and hashes are full. Everything is verifiable from git, GitHub and the commands in each session's last section.

# Session 2026-10-12 → 2026-10-13: weekend #2 stop + grade, PRs #50–#54 merged, cleanup, docs sweep

## T1. What was asked → what was delivered

| Ask (operator) | Delivered | Where |
|---|---|---|
| Watch the weekend #2 run every 15 min | `doctor --live` every 15 min Fri → Mon; operator stopped it (SIGINT 2026-10-12T14:50:41Z, clean) | incidents below |
| "go" (grade weekend #2) | vaults, grade, regrade, backfill, secondaries, graded record, 3 incidents | PR #54 |
| Merge every PR and branch into main; rebase; fix conflicts | #52 → #53 → #50 → #51 rebased in that order and squash-merged on green CI; 0 open PRs; only `main` | § T2 |
| Remove `~/.cache/tengu-xm.noindex/agents/` if weekend research is unaffected; clean up; keep weekend research | `agents/` (90 GB), `seed/`, `a2a/`, `weekend/post-stop-backup-2026-10-12/` deleted; weekend binaries, vaults, `xmarket-weekend` / `xlab` state kept | § T3 |
| Update all docs / tutorial to the code | docs sweep PR (this handoff's PR) | § T4 |

## T2. Merges (squash, `rust-quality` green on each final head)

| PR | Merge commit on `main` | Final head (rebased) | CI run | Original tip → tag |
|---|---|---|---|---|
| #54 weekend #2 graded | `43ce6384745b126d4478b083ea0491a25fbfb929` | `59c5fa19cbff9075858e9d621739e6cb9b007ac5` | 38271122120 | `archive/fwd/grade-weekend-2-2026-10-12` = `59c5fa19cbff9075858e9d621739e6cb9b007ac5` |
| #52 Studio builder + codex | `23a586d1d85cd8a037e23fa3c57ab1c9b949f7ea` | `fb190f76431e2e8822d479b2ef5ff22e62e07c0a` | 38381272786 | `archive/feature/sandbox-builder` = `7d420d0be5743c4365e7ae238952505906c36a17` |
| #53 A2A | `3e16a8694535859a20d97ab52687532b1cc3571b` | `9c1ce529b5dd51e13639539026d5c7b7eec8f1ab` | 38382655012 | `archive/feature/a2a` = `917ead814baeaf2b2368c76deffe11bdf2388bf1` |
| #50 seal proxy | `0316f4698699d5c636d13cd5bf7e73d038fbeb33` | `6275ca74535392d708b2120742c893ff370f8915` | 38384097897 | `archive/feature/sealed-keys` = `71b7338201b8683baa6bd232282d697bf42023cd` |
| #51 Privy Solana signer | `099ceb6fdf1e217293b614b6d232d74ab29d123c` | `309650dffbd5c901037c0040cfe9b8342c377b27` | 38385329953 | `archive/feature/privy-solana-signer` = `b4c1c17a7a66ecfbcc36ae9fbfb312cf03abf081` |

| Conflict | Resolution |
|---|---|
| #53 on #52: `Cargo.toml` default features, `src/bootstrap/mod.rs` modules, `tests/engine_matrix.rs` legs | union: `default = ["openrouter", "telegram", "studio", "codex", "a2a"]`; both modules; both leg lists |
| #53 on #52: codex had no A2A leg | added `codex_a2a => CODEX, Set::A2a` + the `[a2a.remotes.matrix]` / `[default_scopes.a2a]` block and `"a2a"` tool in `tests/fixtures/engine_matrix/open/codex.toml` (`fixtures_load_and_agree`) — commit `9c1ce529b5dd51e13639539026d5c7b7eec8f1ab` |
| #53 on #52: architecture svg (both drew "section 9") | builder = section 9, A2A = section 10 (+308 px), footer → y 4408, viewBox 4536 |
| #50: 4 commits incl. a superseded first design + a merge | squashed to one commit on its merge base (tree identical to `71b7338201b8683baa6bd232282d697bf42023cd`), then rebased |
| #50 / #51 on #53: footers of CLAUDE.md / AGENTS.md, handoff, docs hub, tutorial sandboxes page, code map rows; `src/config/{mod,sections}.rs` (`a2a` vs `privy_wallet_id` fields) | additive, both kept; code map regenerated (`TENGU_REGEN_CODE_MAP=1`) |
| `handoff_for_check.md` session for #50 / #51 (was uncommitted in the sealed worktree) | committed in #51 |

Local checks before each push: `cargo fmt --check`; `cargo test --test code_map --test tutorial_map --test layering_lint --test scope_lint --test engine_matrix` (offline legs); `cargo test --bin tengu -- builder hardening solana keys codex a2a` on the #51 tree (335 passed).

Deleted: branches `feature/{sandbox-builder,a2a,sealed-keys,privy-solana-signer}`, `fwd/grade-weekend-2-2026-10-12` (origin + local); worktrees `~/development/{tengu-builder,tengu-a2a,tengu-sealed}`, `.claude/worktrees/studio-view`. Not touched: the `mol` remote (Molecule org) and its branches.

## T3. Local cleanup (operator 2026-10-13)

| Removed | Size | Why allowed |
|---|---|---|
| `~/.cache/tengu-xm.noindex/agents/` (a2a, builder, grading, grading-test, seal-worker, sealed-crate, sealed-tengu, studio-view) | 90 GB | operator ask; build caches only (no process open; nothing cites them) |
| `~/.cache/tengu-xm.noindex/seed/` | 6.1 GB | handoff row E4 "any time" |
| `~/.cache/tengu-xm.noindex/a2a/` (pre-merge `tengu-a2a-08bb144`) | 46 MB | #53 merged |
| `~/.cache/tengu-xm.noindex/weekend/post-stop-backup-2026-10-12/` | 112 KB | stale copies of files tracked upstream |
| stale worktree entries (`git worktree prune`) | — | their dirs were gone |

Kept (weekend research, operator rule): `~/.cache/tengu-xm.noindex/weekend/{tengu-6fcb455,tengu-acdef66,tengu-desk-2026-10-02}`, `~/.cache/tengu-xm.noindex/tengu-xlab-f554db0`, `~/.tengu/state/{evidence,xmarket-weekend,xlab,xmarket,research}`, logs. Not ours: `~/development/muliagent-tengu-cluster` (repo `moleculeprotocol/multiagent-tengu-cluster`, 17 GB `target/`); `/private/tmp/tengu-review-target` (8.2 GB, the reviewer's cargo cache).

## T4. Docs sweep + record fix (this handoff's PR)

| Item | Note |
|---|---|
| `fwd.rule_w.2026-10-09.graded` | review P1 "fails its declared verifier": the `ledger:` source is cumulative — the weekend-only rows lose `extract`; new rows `capped_cumulative` (n 8, 172.89980107619598 bps, $3.429751754824897) and `shadow_cumulative` (n 149, 58.72003458464617, $86.9575096438379) carry it. `tengu lineage verify --evidence`: 0 errors |
| Docs | agent guides, handoff, hub, code map, feature docs, architecture, tutorial pages swept against `main` @ `099ceb6fdf1e217293b614b6d232d74ab29d123c` |

## T5. Open (from `handoff_review.md` 2026-10-13T11:06:01Z — not fixed in this session)

| Finding | Area |
|---|---|
| P1 seal proxy returns oversized / SSE / binary bodies unmasked | `cloudflare/seal-worker/src/lib.rs`, `crates/tengu-seal/src/target.rs` |
| P1 codex inherits vault secrets + the seal-proxy session env | `src/adapters/outbound/engines/codex.rs` |
| P1 `tengu sandbox compile` / `finalise` path traversal | `src/adapters/inbound/cli/sandbox.rs`, `src/adapters/outbound/builder_store.rs` |
| P1 webhook trace closure races a detached planner turn | `src/adapters/inbound/webhooks.rs` |
| P2 malformed TOML fails open in builder finalise; eval trace metrics barrier | `src/config/builder/`, eval |

# Session 2026-10-09 (afternoon → night): seal proxy + Privy signers — PRs #50, #51

> **Status 2026-10-13:** both MERGED — #50 `0316f4698699d5c636d13cd5bf7e73d038fbeb33` (rebased head `6275ca74535392d708b2120742c893ff370f8915`), #51 `099ceb6fdf1e217293b614b6d232d74ab29d123c` (head `309650dffbd5c901037c0040cfe9b8342c377b27`); see § T2. The OPEN / head / "not pulled" lines of this section are as of 2026-10-09.

## S1. What was asked → what was delivered

| Ask (operator, 2026-10-09) | Delivered | Where |
|---|---|---|
| Review the Linear "[BE] AC" tickets (IP-3150…IP-3157, IP-3289, IP-3114) and make a much simpler plan; no keys / secrets / envs on the machine; Cloudflare compute + secrets only; free-tier limits | Plan (Cloudflare Worker as key-injecting proxy; free-tier table) — first as HPKE-sealed blobs, then "simplify even more": keys are Worker secrets behind routes | `docs/sealed-keys-2026-10-09.md` (design, free tier, verified · open) |
| "Start with it till complete … do not use any molecule infrastructure … I will create Cloudflare and add secrets myself" | Rust Worker `cloudflare/seal-worker` + shared crate `crates/tengu-seal`; tengu `[keys]` / `[keys.env]` / `strip`, `tengu keys setup / status / check`; fail-closed install in every process; Telegram / RPC / OpenRouter / Jev through the proxy | PR #50 |
| "A concise html page with detailed instructions" to put every `.env` secret into Cloudflare | Operator guide, 11 sections, every one of the 24 `.env` names classified (Molecule names excluded) | `docs/cloudflare-secrets-setup.html` (PR #50) |
| "A separate sandbox … prove JEV, decision loop and validation loop via the web UI and terminal work … no garbage" | `sandboxes/sealed-check` + runbook + recorded run (real Jev through a local `wrangler dev` Worker; Studio screenshots) | `docs/sealed-check-2026-10-09.md`, `docs/sealed-check-evidence/` (PR #50) |
| "Why not simply use the existing Privy agentic wallet with app id and the rest stored in Cloudflare?" | Route `privy` (Basic header, secret `PRIVY_BASIC_AUTH`); EVM Privy tools send the session, never the app secret | PR #50, commit `71b7338201b8683baa6bd232282d697bf42023cd` |
| "After Privy and Cloudflare are in place we would be able to remove the Solana signer?" → "yes" | `[solana] signer_key_file` removed; `[solana] privy_wallet_id` — Solana write tools sign with a Privy wallet through the same route | PR #51 |
| "Leave it in PR for now" (weekend trading validation running) | Both PRs OPEN, not merged | § S3 |

## S2. Merge model (this session)

| Rule | Applied |
|---|---|
| Isolation | one worktree `/Users/vladimirdemidov/development/tengu-sealed` (branch `feature/sealed-keys`, then `feature/privy-solana-signer` on top); own `CARGO_TARGET_DIR`s under `~/.cache/tengu-xm.noindex/agents/` |
| Merge | **none** — operator asked to keep both in PR during the weekend validation. Order when approved: #50 into `main`, then retarget #51 to `main` and merge |
| Main checkout | not pulled, not built (weekend freeze): `/Users/vladimirdemidov/development/tengu-cluster` stays at `10bdbb53a27390f6a67fd73826740befe05ababe` |
| Remotes | pushed to `origin` (DemidovVladimir/tengu-cluster) only; nothing pushed to the `mol` remote; no Molecule service used |
| Secrets | no real secret written to any file; tests used throwaway values in the scratchpad (deleted). The one real OpenRouter key run (sealed-check) loaded it into the local `wrangler dev` Worker only |

## S3. Pull requests (both OPEN)

| PR | Title | Base ← head | Head SHA | CI `rust-quality` |
|---|---|---|---|---|
| #50 | Seal proxy: provider keys live only in a Cloudflare Worker (+ sealed-check sandbox) | `main` ← `feature/sealed-keys` | `71b7338201b8683baa6bd232282d697bf42023cd` | success, run 37973350643 |
| #51 | Solana write tools sign with a Privy wallet through the seal proxy (stacked on #50) | `feature/sealed-keys` ← `feature/privy-solana-signer` | `b4c1c17a7a66ecfbcc36ae9fbfb312cf03abf081` | success, run 37986183258 |

`origin/main` at review time: `91b2a2198bf29403aaa81c5d63760a71e3f6cf40`.

### S3.1 Branch commits (full ids)

| Commit | Date (+02:00) | PR | What |
|---|---|---|---|
| `7378ae24cdd2cabd6ae17b8bc71ea3ff156c35aa` | 2026-10-09T16:02:44 | #50 | first design: HPKE-sealed blobs + Durable Object key — **superseded by the next commit** |
| `a0644fbe44636f21a1a0dd3fdee75463a463e915` | 2026-10-09T17:50:20 | #50 | simplified + hardened seal proxy (routes, clients, route-scoped sessions, masking), `tengu keys`, `[keys]`, sealed-check sandbox, docs |
| `58aa1beac083e60d6cf28890683b9232568334ee` | 2026-10-09T17:53:27 | #50 | merge `origin/main` (#49); code map regenerated |
| `71b7338201b8683baa6bd232282d697bf42023cd` | 2026-10-09T20:26:53 | #50 | route `privy` + Privy EVM tools via the session (`FALLBACK_KEYS`: `PRIVY_API_URL` needs `PRIVY_APP_SECRET` stripped); Worker trims secrets |
| `b4c1c17a7a66ecfbcc36ae9fbfb312cf03abf081` | 2026-10-09T22:18:59 | #51 | `PrivySigner` (Solana), `signer_key_file` removed, async `SolanaSigner`, session env_reads rule, docs |
| (this handoff) | — | #51 | `handoff_for_check.md` |

Diff sizes: #50 = 69 files, +6142 / −81 vs `main`; #51 = 47 files, +1054 / −457 vs #50.

## S4. What to look at (security-relevant code)

| Area | Files | Claim to verify |
|---|---|---|
| Key position | `crates/tengu-seal/src/target.rs` (`build`, `check_rest`, `after_key`, `split_path`), `route.rs` | a route's key goes to exactly one place (header / path template / query); `TENGU_SECRET` anywhere else → 403; path escapes (`..`, `//`, `%2f`, `%25`, `\`, `;`) refused; `allow` prefixes enforced |
| Sessions | `crates/tengu-seal/src/{session,ssh}.rs`, `cloudflare/seal-worker/src/lib.rs` (`authorize`) | HMAC token under `SESSION_KEY`; SSH signature over fp + ts (±60 s) + nonce + Worker origin + routes; granted = asked ∩ `CLIENTS`; every call re-checks `CLIENTS` |
| Reply masking | `target.rs` (`mask`, `mask_bytes`, `maskable`, `null_body`), `lib.rs` (`mask_reply`) | key masked in headers and text bodies ≤ 2 MiB; larger bodies stream unmasked (`masked` = -1) — documented limit → fixed 2026-10-13: fail-closed `tengu-seal::mask` (every byte) |
| Fail closed | `src/adapters/outbound/keys/mod.rs` (`install`, `DISPLACED` / `REMOVED`), `src/adapters/outbound/secrets.rs`, `src/adapters/inbound/telegram.rs` | no session → every `[keys.env]` var unset; `strip` names removed in every process; displaced / removed values stay redacted; vault never re-injects a removed name; Telegram refuses to start |
| Privy EVM | `src/adapters/outbound/tools/crypto/helpers.rs::privy_request`, `src/config/keys.rs::FALLBACK_KEYS` | through the proxy the app secret is never read; config without `strip` `PRIVY_APP_SECRET` does not load |
| Privy Solana | `src/adapters/outbound/solana/privy.rs`, `src/adapters/outbound/tools/solana/write_common.rs::signer_for`, `src/adapters/outbound/solana/signer.rs::sign_transaction` | only via the seal proxy with a session; reply message byte-identical; only the wallet's slot taken; ed25519 `verify_strict` for the `wallet` arg; all slots signed before any is written |
| Load rules | `src/config/solana.rs::signer_rules`, `src/config/hardening.rs` | `privy_wallet_id` 1-64 `[A-Za-z0-9_-]`; needs `[keys.env] PRIVY_API_URL`; no scope `env_reads` of `TENGU_KEYS_SESSION_TOKEN`, an `@session` var or `"*"` beside a signer; hardening otherwise unchanged (key-path rule removed — no key) |

## S5. Local state (operator's Mac)

| Item | State |
|---|---|
| Worktree | `/Users/vladimirdemidov/development/tengu-sealed` on `feature/privy-solana-signer` (`b4c1c17a7a66ecfbcc36ae9fbfb312cf03abf081` + this handoff); remove after merge (`docs/SESSION_HANDOFF.md` E5 row) |
| Build caches (this session) | `~/.cache/tengu-xm.noindex/agents/sealed-tengu` (24G), `…/seal-worker` (1.1G), `…/sealed-crate` (273M); remove after merge |
| Test leftovers | none: `~/tengu-sealed-check` removed; no `wrangler` / `workerd` / test `ssh-agent` running; scratch files deleted; nothing written to `~/.tengu` by the sandbox |
| Cloudflare | nothing deployed by the agent; no account touched |

## S6. Verification evidence

| Check | Result |
|---|---|
| Unit (`cargo test --bin tengu`, worktree at #51) | 2147 passed, 0 failed |
| Crate `crates/tengu-seal` | 20 passed (incl. `shipped_wrangler_toml_parses`) |
| Worker | `cargo clippy --target wasm32-unknown-unknown --release -- -D warnings` + `cargo fmt --check` clean |
| Gates | `bridge_conformance`, `run_agent_ipc`, `layering_lint`, `scope_lint`, `language_policy`, `tutorial_map`, `code_map` green; clippy: no warning in touched files |
| Local `wrangler dev`, real OpenRouter key in the local Worker only | sealed-check: Jev `0xff`→`255`, tool error, dry run, maps, `tengu run` + `doctor --live`, Studio Play / Send / Stop (`accepted 9 · completed 9 · failed 0`), N1 fail closed; exfiltration requests 403; `allow` refused `v1/keys`; 6.5 MB reply streamed — `docs/sealed-check-2026-10-09.md` § Recorded run |
| Local `wrangler dev`, route `privy`, throwaway secret, httpbin upstream | `get_wallet_address` through the Worker with the session, Basic key added and masked (`masked` 1); `v1/apps` 403; `TENGU_SECRET` in the path 403; no session → `Missing environment variable PRIVY_APP_SECRET`, 0 calls to `api.privy.io`; config without `strip` refused |
| Privy Solana (tests) | `write_tokens::tests::send_signs_each_batch_through_privy` (fake Privy over HTTP: 2 batches sent, each signature verifies, Bearer session, no Basic); `privy::tests::*` (other wallet, changed message, bad base64, HTTP 400, net_host denial → nothing signed); `signer::tests::one_failing_signer_writes_nothing` |
| CI flake (not this change) | run 37955084254 first attempt failed `bridge_conformance` backtest case at 16:00:38Z (hour boundary); rerun green; passes locally |

## S7. Operator gates and open items

| Gate / item | Owner |
|---|---|
| Merge #50 then #51 (after the weekend validation) | operator's word |
| Cloudflare: Secretive keys, `npx wrangler login` + `deploy`, `SESSION_KEY`, provider secrets (fresh keys), `CLIENTS`, `[keys]` per sandbox — `docs/cloudflare-secrets-setup.html` | operator |
| Privy: `PRIVY_BASIC_AUTH`; a dedicated Privy Solana wallet + `[solana] privy_wallet_id` + write-tool scopes; Privy wallet policies | operator |
| First live Solana `send` (real money) | separate go |
| Not verified live: a real Privy call, Tor reach of `workers.dev`, Worker CPU p95 on the free plan | after deploy |

## S8. Known limits (not fixed on purpose)

| Limit | Note |
|---|---|
| Sessions are not refreshed in-process | restart a long run before `session_hours` (≤ 168 h) |
| A session holder can USE a route's key, never READ it | e.g. any Telegram Bot API method as the bot; `allow` narrows; a Solana-signer sandbox forbids `env_reads` of the session |
| Reply bodies > 2 MiB stream unmasked → fixed 2026-10-13 (every byte masked; CPU cost on large replies not measured on `wrangler dev`) | free-plan CPU budget |
| Privy keeping other partial signatures | irrelevant by design: tengu takes only the wallet's slot and fills its own one-shot key |
| Earlier review findings | `handoff_review.md` 2026-10-09T14:55:11Z P1 (webhook planner turns detached at shutdown) / P2 (eval timed sleep before `run.closed`) — earlier sessions' code, not touched here, still open |

## S9. How to check (commands)

```sh
gh pr view 50 --repo DemidovVladimir/tengu-cluster --json state,headRefOid,baseRefName,mergeable
gh pr view 51 --repo DemidovVladimir/tengu-cluster --json state,headRefOid,baseRefName,mergeable
git fetch origin
git log --format='%H %s' origin/main..origin/feature/sealed-keys
git log --format='%H %s' origin/feature/sealed-keys..origin/feature/privy-solana-signer
git worktree add /private/tmp/check-privy origin/feature/privy-solana-signer
cd /private/tmp/check-privy
export CARGO_TARGET_DIR=~/.cache/tengu-xm.noindex/agents/check-privy CARGO_BUILD_JOBS=2
cargo test --bin tengu -- solana privy signer config::solana config::keys keys
cargo test --test bridge_conformance --test run_agent_ipc --test layering_lint --test scope_lint --test tutorial_map --test code_map
cargo test --manifest-path crates/tengu-seal/Cargo.toml
cargo clippy --manifest-path cloudflare/seal-worker/Cargo.toml --target wasm32-unknown-unknown --release -- -D warnings
rg -n 'signer_key_file' src tests sandboxes config.example.toml     # → nothing
rg -n 'sk-or-v1-[0-9a-f]{64}|tss1\.[A-Za-z0-9_-]{40,}\.' --glob '!target' .   # → nothing: no real OpenRouter key or session token
```

---

# Earlier sessions

# Session 2026-10-08 → 2026-10-09 (PRs #39–#45; original handoff, unchanged below this title)

## Addendum 2026-10-10 — A2A (PR #53, open on purpose until the weekend #2 stop)

> **Status 2026-10-13:** MERGED as #53 `3e16a8694535859a20d97ab52687532b1cc3571b` (head `9c1ce529b5dd51e13639539026d5c7b7eec8f1ab`, + the `codex_a2a` leg); see § T2.

| Field | Value |
|---|---|
| Ask (operator, 2026-10-10) | check for A2A; if missing, implement it till finished so agents (when the sandbox config says so) talk to other agent harnesses; keep the weekend validation running; separate branch + PR; a separate binary if needed |
| Found | no A2A anywhere (main, origin/main, every feature branch) |
| Branch · worktree | `feature/a2a` · `~/development/tengu-a2a` (base `origin/main` `91b2a2198bf29403aaa81c5d63760a71e3f6cf40`) |
| Commits | `08bb144fcdb6b647b8b4fa643872b9b504eb0523` (feature) + the evidence / handoff commit after it (`git log origin/main..origin/feature/a2a`) |
| PR | #53 `A2A: agents talk to other agent harnesses (a2a tool + tengu a2a serve)` — not merged: main checkout frozen until Mon 2026-10-12; merge after #52 (operator order), rebase first (docs + additive config conflicts with #50 / #51 / #52) |
| Binary | `~/.cache/tengu-xm.noindex/a2a/tengu-a2a-08bb144` sha256 `83d2a7bb5208e1db75001c32ee2ab4ca5de6ea16da50717d97354975d71bd35b`; build cache `~/.cache/tengu-xm.noindex/agents/a2a` |
| Weekend run | untouched: pid 11624 `~/.cache/tengu-xm.noindex/weekend/tengu-acdef66` (sha256 `fdcf2c270dd94347b2786d2b1a0b6dda162d91c1a4f02dc37408ce497555b445`) kept running; main checkout not touched |
| Doc · tutorial · example | `docs/a2a-2026-10-10.md` · `docs/tutorial/a2a.html` · `sandboxes/a2a-lab` |
| Evidence | `docs/a2a-2026-10-10.md` § 4 and `docs/SESSION_HANDOFF.md` top section: unit, `tests/a2a_e2e.rs`, bridge conformance, engine matrix live (openrouter gemini + haiku, claude_code), `a2a-sdk` 1.2.2 interop 52/52, live `a2a-lab` chain, `cargo test --workspace` 2264 passed / 0 failed |
| Check | `cargo test --bin tengu a2a && cargo test --test a2a_e2e && TENGU_CONFORMANCE_ONLY=a2a cargo test --test bridge_conformance && cargo test --test engine_matrix -- offline_local_a2a` |

> For the operator's architecture agent. Everything below is verifiable from git, GitHub and the commands at the end. Ids and hashes are full.

## 1. What was asked → what was delivered

| Ask (operator, 2026-10-08) | Delivered | Where |
|---|---|---|
| Deep-compare `docs/software-opportunity-{prd,roadmap,compatibility}-2026-10-04.md` + `docs/strategy-ranking-automation-2026-10-08.md` with the code; implement what is lacking (Rust, project rules) | Strategy ranking SR-1…SR-8; Software Opportunity Engine O0–O4 (stops at Operator Review #2) | PRs #40, #41, #43 |
| Update every doc so the code is the source of truth (tutorial + agent markdown) | Two doc sweeps (A: README, architecture, context, research, memory/skills, 22 tutorial pages; B: CLAUDE.md/AGENTS.md, handoff, hub, code map, runtime/tool docs, remaining tutorial pages, feature docs) | PRs #42, #45 |
| Implement `TENGU_STUDIO_PLAN.md` with workflows; validate `control-loop-lab` via Studio | ST-00…ST-40 (+ ST-90 clean room); live validation with real Jev + screenshots | PR #44, `docs/studio-evidence/`, PR #45 |
| PR + merge everything into main, no review wait; no stale branches / PRs left alone | 7 PRs squash-merged, 0 open PRs; this session's branches deleted (tips kept as `archive/*` tags) | § 4 |
| A handoff with all commits, PRs and branches | this file | repo root |

## 2. Merge model

| Rule | Applied |
|---|---|
| Work isolation | every lane in its own git worktree under `.claude/worktrees/` with its own `CARGO_TARGET_DIR` (`~/.cache/tengu-xm.noindex/agents/<lane>`); worktrees + target dirs removed after merge |
| Merge | one PR per feature, CI `rust-quality` green (fmt, check --all-features, clippy, `cargo test --workspace`, + `cargo test --features studio --bin tengu studio` added in #44), squash-merged |
| Reviews | every lane ended with an adversarial review agent (fixes committed in the same branch); integration merges re-ran the full suites |
| Weekend freeze | **the local main checkout was NOT pulled** (it is at `10bdbb53a27390f6a67fd73826740befe05ababe` + a 1-line local runbook edit) — see § 6 |

## 3. Pull requests (all squash-merged into `main`; 0 open PRs)

| PR | Title | Squash commit on main | Merged (UTC) |
|---|---|---|---|
| #39 | xlab-w2: keep every backtest run dir | `10bdbb53a27390f6a67fd73826740befe05ababe` | 2026-10-08T17:09:12Z |
| #40 | Strategy ranking: sealed contracts, deterministic ranker, publisher, xlab-w2 feeds | `117b2595b0864066502d156aca42c7180494d67a` | 2026-10-08T20:48:47Z |
| #41 | SOE O0–O2: opportunity domain, deterministic economics + gates, source evidence layer (SEC, TED) | `bc9bc12a038654ac4941c714abcfa777c99af631` | 2026-10-08T21:07:29Z |
| #42 | Docs sweep A | `e84e886c6d3d4dc985e408991fe66d1773140c01` | 2026-10-09T01:54:09Z |
| #43 | SOE O3/O4: weekly portfolio cycle, replay + grading, Review #2 packet, soe tools, feed job, SOE-G0 | `7cae230f61eddcc014b94521564b15a7b3ac817f` | 2026-10-09T07:24:11Z |
| #44 | Tengu Studio + control-loop-lab (TENGU_STUDIO_PLAN ST-00…ST-40) | `f7ac9a5636e2493e3839e896cba9951a2d41aefb` | 2026-10-09T08:18:13Z |
| #45 | Docs sweep B + ST-90 clean room + this handoff | see `git log origin/main -1` after merge | 2026-10-09 |

### 3.1 Branch commits per PR (full ids; branch tips kept as `archive/<branch>` tags)

## PR #39 — xlab-w2: keep every backtest run dir

| Field | Value |
|---|---|
| URL | https://github.com/DemidovVladimir/tengu-cluster/pull/39 |
| Branch | `fix/xlab-w2-retention` — deleted after merge; tip kept as tag `archive/fix/xlab-w2-retention` |
| Squash commit on main | `10bdbb53a27390f6a67fd73826740befe05ababe` |
| Merged at (UTC) | 2026-10-08T17:09:12Z |

Branch commits (1):

- `7f508389dc56907eacf5857faeac4bf2ccd4a6d2` xlab-w2: keep every backtest run dir (keep_runs = 0)

## PR #40 — Strategy ranking: sealed contracts, deterministic ranker, publisher, xlab-w2 feeds

| Field | Value |
|---|---|
| URL | https://github.com/DemidovVladimir/tengu-cluster/pull/40 |
| Branch | `feature/strategy-ranking` — deleted after merge; tip kept as tag `archive/feature/strategy-ranking` |
| Squash commit on main | `117b2595b0864066502d156aca42c7180494d67a` |
| Merged at (UTC) | 2026-10-08T20:48:47Z |

Branch commits (9):

- `caacdc0be446ce9d7628beacbc31ef0992855c1f` Strategy ranking SR-1/SR-2 prerequisites: ranking contracts, report cohort identity, [strategy_ranking]
- `42584f85ca739b7ee6a0df408726e05488d5deec` Strategy ranking SR-2/SR-3: pure selection + ranker (domain/backtest/ranking.rs)
- `5a4682e8f0c7fce262f94895e2fe9b134f71e4ae` Strategy ranking SR-5: coordinator (application/ranking) + `tengu ranking run|show`
- `7c7a7cdc5df01f1b117abbc8b0c3a30dcff5b78d` config/xmarket.rs: the strategy-rankings row names its writer (application/ranking/) and files
- `6573e24f44da1eb35ff2f7930348cbd884fa1239` Strategy ranking SR-4 + SR-7: `strategy_ranking` tool on every engine + binary acceptance
- `6a99fe212062036833b8a4bb28e3a6457573262d` Strategy ranking SR-6: xlab-w2 runs the daily + weekend rankings (unsealed, unbound)
- `e84f438ef9a545a242c1576c4bcaa5e1491b02fe` docs: strategy-ranking automation doc, as built (SR-8)
- `dc466c170521e68b716392db8e89db429f68264a` Strategy ranking SR-8: tutorial page, agent guides and operator docs as built
- `88b86dde45227dade1d67e2f60c5716041c7747a` Strategy ranking review: as-of standing, lost-lease writes, published-only show

## PR #41 — SOE O0–O2: opportunity domain, deterministic economics + gates, source evidence layer (SEC, TED)

| Field | Value |
|---|---|
| URL | https://github.com/DemidovVladimir/tengu-cluster/pull/41 |
| Branch | `feature/soe-o0-o2` — deleted after merge; tip kept as tag `archive/feature/soe-o0-o2` |
| Squash commit on main | `bc9bc12a038654ac4941c714abcfa777c99af631` |
| Merged at (UTC) | 2026-10-08T21:07:29Z |

Branch commits (17):

- `740e1e97fda76df3d41d8167c184a7d89bcfd29c` O2 record model: domain::source pure source records + provenance
- `872de9dbb2b69fc3fbb229aa52d1ed23e3f3de4a` SOE O0: public contract, sources and threat model doc
- `ef4a6a70aaf660a4e359cf9aec41a1045b93fcba` SOE O1: domain/soe value types — exact money, FX, bps, Est, schema tags
- `4d2827c8502483a690d94c4c24c91f16d4637560` SOE O1: versioned records + private profile / record-dir loader
- `a798640a1bf06fa3dd9c1b5d1c4f0797b9d047c7` O2 sources: [sources] registry + soe sandbox, as-of view, evidence packet, split-world checks
- `6c0fe6d3dfccc100abefed83cf074849fe1c6203` SOE O1: deterministic economics + PRD § 7.2 hard gates
- `8172f4ee713cefa8d1b158131b6ff94332444114` O2 sources: append-only sources.db store + SEC EDGAR source keyed by CIK
- `32ba0f1825b4ba5520de70092b28c70bfcb3ec7d` SOE O1 W5 + O0 W8: ranking primitives, capability fit, HOLD week + dated eval set
- `e3aaf7dc72229b5cf7b4dfebe8eb9e7a919c8f6a` SOE O1 W7 + W9: offline `tengu soe init|check|portfolio|sensitivity|eval`
- `faa8d238f044d7888e21065cea409aab42549dfb` O2 sources: EU TED Search source + `tengu sources` CLI with a runtime kill switch
- `5209f6470ee1a82bf35caab7228b0b727e9bcb8d` O2 sources: read-only `source_evidence` tool + 12 dated replay cases (O2 exit)
- `982fd19c24f7ca4bc79c03c949594d5e425c41cd` O2 sources docs: source-evidence doc, tutorial picture end to end, tool + config rows
- `48e2454e7c8521b9d9f620027d1ad692cb616760` review-C: sources.db WAL/SHM owner-only; kill switch by append order
- `206e22f345a256fe8414b6c646bf563a44b3e876` SOE O1 review: void experiments, payback scan bound, atomic init
- `9e199b77e88e1ce5c2767525e4939fffe1beed26` Merge branch 'feature/soe-sources' into feature/soe-o0-o2
- `4e3b2a47e7bce2ff770ed06ea241130f3ce06501` soe init: the four money values are REQUIRED placeholders, never in public source
- `e0951c001170ed2da1eac5eab0153e1c31647026` Merge remote-tracking branch 'origin/main' into feature/soe-o0-o2

## PR #42 — Docs sweep A: docs and tutorial pages match the code (README, architecture, context, research, memory/skills, 22 tutorial pages)

| Field | Value |
|---|---|
| URL | https://github.com/DemidovVladimir/tengu-cluster/pull/42 |
| Branch | `docs/sweep-2026-10-08` — deleted after merge; tip kept as tag `archive/docs/sweep-2026-10-08` |
| Squash commit on main | `e84e886c6d3d4dc985e408991fe66d1773140c01` |
| Merged at (UTC) | 2026-10-09T01:54:09Z |

Branch commits (7):

- `7fcbacd93d6bfd84c525c5e4a62f0d7299fdc927` docs sweep: readme — catalog count 47, lineage/ranking/sources/soe commands + sandboxes, config sections, module map
- `31c0929d0e5e0ba7dbcfd7a028021a0c866acefb` docs sweep: architecture — lineage/evidence, ranking, sources, SOE subsystems; executor abort + leaf join, retry backoff, memory port
- `efde4875f770ef69c093dbab074ef493efd8003e` docs sweep: context — mechanism count, compress_and_store paths + line refs, budget floor, comparison rows for lineage/ranking/sources
- `1ff4df8b2c3bdc906c61f3088ec29305caa77ad1` docs sweep: research — P0–P11 status, registry counts, xlab-w2 + SEC events, evidence verify/evaluate flags, no-pull note for weekend #2
- `d09f905b7015b8fbacae70c0d08fe45414c69897` docs sweep: memory-skills — skill gating/tiers/editability, learning-agent caveat, conformance + engine-matrix counts, engine recipe paths
- `70efdffad9fc7bed6a5bd72c006359f3d3f75b5d` docs sweep: tutorial-1 — beyond-chat commands, soe egress, generation tool filter, offline commands skip the vault
- `999cb7a8f2bacf2292da2ca1e2f59eb69f584180` docs sweep: tutorial-2 — every top-level command, soe + xlab-w2 sandboxes, lineage/backtest/channel fixes

## PR #43 — SOE O3/O4: weekly portfolio cycle, replay + grading, Review #2 packet, soe tools, feed job, SOE-G0

| Field | Value |
|---|---|
| URL | https://github.com/DemidovVladimir/tengu-cluster/pull/43 |
| Branch | `feature/soe-o3-infra` — deleted after merge; tip kept as tag `archive/feature/soe-o3-infra` |
| Squash commit on main | `7cae230f61eddcc014b94521564b15a7b3ac817f` |
| Merged at (UTC) | 2026-10-09T07:24:11Z |

Branch commits (29):

- `740e1e97fda76df3d41d8167c184a7d89bcfd29c` O2 record model: domain::source pure source records + provenance
- `872de9dbb2b69fc3fbb229aa52d1ed23e3f3de4a` SOE O0: public contract, sources and threat model doc
- `ef4a6a70aaf660a4e359cf9aec41a1045b93fcba` SOE O1: domain/soe value types — exact money, FX, bps, Est, schema tags
- `4d2827c8502483a690d94c4c24c91f16d4637560` SOE O1: versioned records + private profile / record-dir loader
- `a798640a1bf06fa3dd9c1b5d1c4f0797b9d047c7` O2 sources: [sources] registry + soe sandbox, as-of view, evidence packet, split-world checks
- `6c0fe6d3dfccc100abefed83cf074849fe1c6203` SOE O1: deterministic economics + PRD § 7.2 hard gates
- `8172f4ee713cefa8d1b158131b6ff94332444114` O2 sources: append-only sources.db store + SEC EDGAR source keyed by CIK
- `32ba0f1825b4ba5520de70092b28c70bfcb3ec7d` SOE O1 W5 + O0 W8: ranking primitives, capability fit, HOLD week + dated eval set
- `e3aaf7dc72229b5cf7b4dfebe8eb9e7a919c8f6a` SOE O1 W7 + W9: offline `tengu soe init|check|portfolio|sensitivity|eval`
- `faa8d238f044d7888e21065cea409aab42549dfb` O2 sources: EU TED Search source + `tengu sources` CLI with a runtime kill switch
- `2e6b78fb5e93d63a1cdf49844cfb876f4d81397c` Merge branch 'feature/soe-sources' into feature/soe-o3
- `86beb09e19db5843135e2a7ed96568acb98c09a8` SOE O3/O4 pure domain: observe, proposals, challenges, allocation, memo, forecast log, review packet, ops
- `f32f54fbe55102b1b0caa2e07294c3592c8d6c34` SOE O3 ports: stage runner + cycle store
- `3853f14bbdf7d33d5802bbd55d012ee1e96dd92c` SOE O3 weekly cycle: run_cycle over the ports + synthetic cycle cases
- `8617286770cbd001d592f202521f8312c1c75346` SOE O4: replay, operator grades, forecast resolution, Operator Review #2 packet
- `d65b30cb4615542ba818a973c9aa432c11ac8f66` SOE O3 infra: [soe] section, cycle adapters, feed kind `job` + soe_cycle job, SOE state lease
- `a841dbf6ee1470a6e03ff28b537f6a5634b0bb9e` Merge branch 'feature/soe-o3-infra' into feature/soe-o3
- `35a29bd4b3798a761f008651fd6c7723ddbf0f0d` Merge branch 'feature/soe-o3' into feature/soe-o3-infra
- `ed34247824f4c63a881893744ec5e8195069dcfe` Merge branch 'feature/soe-o3' into feature/soe-o3-infra
- `cc660b53d5009a44a5f6389d5e00ec1adeebc978` SOE O4 CLI: tengu soe cycle|replay|grade|resolve|review|verify|show
- `14b2dbac5d67ec72cb503a9179d0485d8db1e405` SOE O3 stage tools, sandbox cycle, skills, SOE-G0 + § 13 guards
- `2c221e8093eed294a3ea7dd59aab1fd45013b6c1` Merge branch 'feature/soe-o3' into feature/soe-o3-infra
- `dc22415ebcfbed988e776945759e7302c3c85957` SOE O3/O4 docs: the operator's path to Review #2, gates, invariants → tests
- `35598f0b18e085d2ab687ad93d3645910d5f6401` SOE O3/O4 review fixes: holdout count first, unknown tokens, log resume, cycle lease, cache key
- `c6bf1b467eab9e7faee5c6abd1d07f710e0bca36` SOE O3/O4 review: freeze manifest and stage-cache recordings whole or absent
- `a547fc64fc7d0f2f09e354dfdb77001514311818` Merge origin/main (strategy ranking #40, reviewed SOE O0–O2 #41, docs sweep #42) into SOE O3/O4
- `cd366e64122879501970ade16325b0222a2ba040` SOE-G0 lists intel.source_evidence: the soe sandbox's reader loads again
- `13100f3c0de9c619930ce1bbac30f9f24894a96a` SOE docs: init's template leaves the four money values REQUIRED (no PRD figures)
- `8fac057ab6b6f10b02e10a2d420e36b96ec14a06` SOE doc: O2 row and § 15 step 1 — main merged in, SOE-G0 lists source_evidence

## PR #44 — Tengu Studio + control-loop-lab: workflow graph, execution trace, local browser UI, replay, Play/Stop (TENGU_STUDIO_PLAN ST-00…ST-40)

| Field | Value |
|---|---|
| URL | https://github.com/DemidovVladimir/tengu-cluster/pull/44 |
| Branch | `feature/studio` — deleted after merge; tip kept as tag `archive/feature/studio` |
| Squash commit on main | `f7ac9a5636e2493e3839e896cba9951a2d41aefb` |
| Merged at (UTC) | 2026-10-09T08:18:13Z |

Branch commits (23):

- `52cad0cf2badb4ec9a11457e862775dc242575a6` Studio ST-00..ST-02: Phase 0 audit, control-loop-lab sandbox, runbook
- `02c13268b2961086315c9b2c53d45aaffebe1602` Studio tracker: ST-00, ST-01, ST-02 done (52cad0cf2badb4ec9a11457e862775dc242575a6)
- `7062b4c6bbe302b78f76d13b7c33b5af5aa6896a` Studio ST-03: baseline real-Jev run of control-loop-lab; Gate 1 waived
- `285826452d8a160c542274ceeeda47695fa66072` Studio tracker: ST-03 done, Gate 1 waived (7062b4c6bbe302b78f76d13b7c33b5af5aa6896a)
- `dfec6877cabf2d9e1e533ca0ff714a3a4bd1f506` Studio ST-10 + ST-11: workflow graph read model, execution trace store
- `9b956fa4fb2ea748d0c32a429bdc964f29d6701c` Studio tracker: ST-10, ST-11 done (dfec6877cabf2d9e1e533ca0ff714a3a4bd1f506)
- `d6438a4294a74d1be4b3aa65d5722b7d29c3d5eb` Studio ST-12: instrument the lab path end to end; Gate 2 evidence
- `5a01d657b0a80e4234065f1918a6119cf96f7475` Studio tracker: ST-12 done, Gate 2 waived (d6438a4294a74d1be4b3aa65d5722b7d29c3d5eb)
- `c9eeabc1c94e28d0e9819bd94e5126cf095b7c40` Studio review ST-10..ST-12: scrub keys + graph URLs, one pass/fail rule, close stopped feed runs
- `5fdd82c110a9c7f26707fe8f18c5855804c77044` Studio tracker: ST-10..ST-12 review fixes (c9eeabc1c94e28d0e9819bd94e5126cf095b7c40)
- `1d8223fd4ed2c0384c20db27bee45625fb71c636` Studio ST-20 (1/2): Studio-web exception to Rust only + tests/language_policy.rs
- `fe5115bc31c0bb381ccb4ea80998cba459ccbf2a` Studio ST-20 (2/2): tengu studio — local read-only server, API + SSE
- `77bf750a4a53e3113a9bb53cf37aae46c6a93af6` Studio tracker: ST-20 done (fe5115bc31c0bb381ccb4ea80998cba459ccbf2a)
- `a0ecdb5490975eeb61865047186f73dbb18a0d9c` Studio ST-21 + ST-22: graph, inspector, timeline, runs + replay
- `3e6cf7898da8de8651798f8d9df60db4d5471da0` Studio tracker: ST-21, ST-22 done, Gate 3 waived (a0ecdb5490975eeb61865047186f73dbb18a0d9c)
- `b1d51fc05801a1bb29e654b21d94e3d55a45c26b` Studio ST-30 + ST-31: Play / Attach / graceful Stop through the tengu run start; control hardening
- `658589d05e0022c4b4ebb1efc0118874bd96c8d4` Studio tracker: ST-30, ST-31 done, Gate 4 waived (b1d51fc05801a1bb29e654b21d94e3d55a45c26b)
- `f56b1b45325c3a94a002569337f53512c41d335e` Studio ST-40 (editor design only) + operator docs for ST-20..ST-31
- `ffa850b2a10da71ba4287a307190fa57e005d978` Studio tracker: ST-40 done (design only), ST-90 docs landed (f56b1b45325c3a94a002569337f53512c41d335e)
- `7a69f79e51c46b54cfdaa0123da5b18c00610d7e` Studio review ST-20..ST-40: path-independent CSRF guard, served verdict tone, bounded control names, server logs like tengu run
- `9fee64f2a3cc22cb6f3a2117a27b28f5b6d1dcd0` Studio tracker: ST-20..ST-40 review (7a69f79e51c46b54cfdaa0123da5b18c00610d7e) — live guards, drain, kill -9
- `7217b209a9d09cea69d508b0cd5d2e1a4cb99586` Merge origin/main into feature/studio (#39–#43: keep_runs, strategy ranking, SOE O0–O4, docs sweep)
- `0ff7fcb6fa7d5d355709a70ae8a42ade93a013fe` config.example.toml: [studio] note names [soe] among hardened sandboxes

## PR #45 — Docs sweep B + ST-90 clean room + handoff

| Field | Value |
|---|---|
| Branch | `docs/sweep-b-2026-10-09` — deleted after merge; tip kept as tag `archive/docs/sweep-b-2026-10-09` |

Branch commits (the authoritative list, incl. follow-ups after this file was written: `git log --oneline f7ac9a5636e2493e3839e896cba9951a2d41aefb..archive/docs/sweep-b-2026-10-09`):

- `37abe0ce62083ea83578e3aa8c7b91078c14691e` docs: studio validation screenshots (run cbd8cd05-30fb-4177-aa99-0b978f0e83e5)
- `f23db47766f28035d7cea2e982ff8fb9299bcb6c` docs: agent guides (CLAUDE.md + AGENTS.md) match main #39-#44
- `ea1df6ea1897e60dae02d03e33d7751d988df64e` docs: handoff + docs hub for #39-#44
- `1541725793c5e4d724922321166092527e419645` docs: code map, configuration, tools, engines, bridge, skills match main
- `6719b8fdab3312a639d61fced786a79bb51009f1` docs: runtime, decision loops, observations, egress, webhooks, lping match main
- `0dacf40d832e43d5a16958c67e5152d545679713` docs(tutorial): tools, lineage, ranking, sources, soe, studio pages match main
- `eb2a2d401b3756f8c01589f0a7a7e1de03f59b1b` docs(tutorial): nav, index, big picture, sandboxes, ops, secrets, egress, metrics, observations match main
- `88460f661fa1b16f24fb9f2ac4a0a8694b834062` docs: README, architecture, comparison match main #39-#44
- `dac8d9c7689750d775e19ff1acf33f145b008435` docs: Studio, lab, ranking, lineage, xlab, SOE, source docs match main
- `5234bdd992f06988255e812ecc2929bbbc8ac729` Studio ST-90: clean-room acceptance passed; its 8 doc fixes applied
- `367bfa4282d3ccc89ccff80f71250ee919e9f39a` handoff_for_check.md: commits, PRs, branches, gates for the architecture check
- `f9056da70b879da49ff11c9270c42c891e37a93c` docs: full commit ids instead of short ones (handoff, clean-room record, tracker)

## 4. Branches and tags

### 4.1 Deleted (this session's branches, all merged)

| Branch | PR | Tip (kept as tag `archive/<branch>`) |
|---|---|---|
| `fix/xlab-w2-retention` | #39 | `7f508389dc56907eacf5857faeac4bf2ccd4a6d2` |
| `feature/strategy-ranking` | #40 | `88b86dde45227dade1d67e2f60c5716041c7747a` |
| `feature/soe-o0-o2` | #41 | `e0951c001170ed2da1eac5eab0153e1c31647026` |
| `docs/sweep-2026-10-08` | #42 | `999cb7a8f2bacf2292da2ca1e2f59eb69f584180` |
| `feature/soe-o3-infra` | #43 | `8fac057ab6b6f10b02e10a2d420e36b96ec14a06` |
| `feature/studio` | #44 | `0ff7fcb6fa7d5d355709a70ae8a42ade93a013fe` |
| `docs/sweep-b-2026-10-09` | #45 | its tip (tag `archive/docs/sweep-b-2026-10-09`) |
| local only, never pushed: `feature/soe-domain` (tip `206e22f345a256fe8414b6c646bf563a44b3e876`), `feature/soe-sources` (tip `48e2454e7c8521b9d9f620027d1ad692cb616760`), `feature/soe-o3` (tip `cc660b53d5009a44a5f6389d5e00ec1adeebc978`) | via #41 / #43 | contained in the archived tips `archive/feature/soe-o0-o2` and `archive/feature/soe-o3-infra` |
| `feature/remove-aura` + worktree `.claude/worktrees/jev-chains` + its 8.8 GB target dir | merged earlier as #30 (`9f0e98f96704eca960575d6c453c5dabb5487d65`; tip `8c169b924e5fba2cf8d30cd6c0bcecc28b9d18b5` had no unmerged diff) | — |

### 4.2 Archive tags pushed to origin (every branch tip existing on 2026-10-09)

| Tag | Commit |
|---|---|
| `archive/backup/docs-pipeline-html` | `a3b3d22b9b2084fdbe7bca55787388a44443ed6a` |
| `archive/backup/phase-0-baseline-and-config-scaffold` | `038d609aed106eb3ecac97e7500d9f25a7484193` |
| `archive/backup/refactor-hexagonal` | `d502565cc7a4af5f52b42d0723e1c3d8ee3c202e` |
| `archive/backup/stash-2026-03-22-event-bus-memory-switch` | `7e4855c13a86fc8ffbf910953c33a3794f96a9ad` |
| `archive/backup/stash-2026-04-20-phase-b-orchestration-collapse` | `6459966c7d0ef26986e52d7af5f4d09b569994d9` |
| `archive/docs/sweep-2026-10-08` | `999cb7a8f2bacf2292da2ca1e2f59eb69f584180` |
| `archive/evolve-skill-creator-2026-04-21T13-56-05Z` | `f16d2efdaaf0955cf4e426a0c38a7fc523d51e94` |
| `archive/feature/execution-map` | `07fca913da947786a06728c46b336fab5669e4bc` |
| `archive/feature/forward-evidence` | `c5e3b719cd3b4788ea90df28db2c0ccc4e489310` |
| `archive/feature/p6-candidate-table` | `0f0f0a2b154dceecf2facf72c76ef8267cb943e6` |
| `archive/feature/p6-decision-eval` | `e989fe011c9495cdeb9c9dbe7a585e751744d5ba` |
| `archive/feature/p7-news-labels` | `0c8eb49df908196bafff34489acbda19e33ac292` |
| `archive/feature/phase-b-orchestration-collapse` | `372a020b47a1ef0770a55625f79f7607004f7ef4` |
| `archive/feature/soe-o0-o2` | `e0951c001170ed2da1eac5eab0153e1c31647026` |
| `archive/feature/soe-o3-infra` | `8fac057ab6b6f10b02e10a2d420e36b96ec14a06` |
| `archive/feature/strategy-ranking` | `88b86dde45227dade1d67e2f60c5716041c7747a` |
| `archive/feature/studio` | `0ff7fcb6fa7d5d355709a70ae8a42ade93a013fe` |
| `archive/feature/w1-lineage` | `492ef59eaa1a0e4b1515ec8c4061cd4055c9978d` |
| `archive/fix/code-findings-and-tutorial` | `aa5240eee48f05a054b07158e63491dc59e1049f` |
| `archive/fix/code-findings-gaps-1` | `723fc48133852dc529b3c88f18542b070fdfa657` |
| `archive/fix/xlab-w2-retention` | `7f508389dc56907eacf5857faeac4bf2ccd4a6d2` |
| `archive/fwd/rule-w-2026-10-09` | `9101646f8f5ffe228bd3135cdb1f295db9825f5b` |
| `archive/weekend/xmarket-weekend-2026-10-02` | `e5daae83febea00e549ce9b8cac83bddce12a0a2` |
| `archive/xlab/weeknight-fade-test` | `868dabab6edb253125040a09c134e81c91b00ca4` |

Lineage check before tagging: all 12 full commit ids cited by `lineage/` + `docs/SESSION_HANDOFF.md` + `docs/w1-*.md` are on `main` or inside an existing tag (`w1-forward-2026-10-02`, `w1-forward-config-2026-10-02`, `w1-weeknight-specs-2026-10-03`), so no cited commit depends on a branch.

### 4.3 Still present — OPERATOR DECISION (older than this session)

Bulk deletion of these was **blocked by Claude Code's auto-mode classifier**, so they were left for the operator. Every tip is already preserved as `archive/<name>` (§ 4.2).

| Remote branches on origin (17) |
|---|
| `backup/docs-pipeline-html` · `backup/phase-0-baseline-and-config-scaffold` · `backup/refactor-hexagonal` · `backup/stash-2026-03-22-event-bus-memory-switch` · `backup/stash-2026-04-20-phase-b-orchestration-collapse` · `evolve-skill-creator-2026-04-21T13-56-05Z` · `feature/forward-evidence` (#36) · `feature/p6-candidate-table` (#34) · `feature/p6-decision-eval` (#33) · `feature/p7-news-labels` (#35) · `feature/phase-b-orchestration-collapse` (#5, closed unmerged) · `feature/w1-lineage` (#32) · `fix/code-findings-and-tutorial` (#31) · `fix/code-findings-gaps-1` (#38) · `fwd/rule-w-2026-10-09` (#37) · `weekend/xmarket-weekend-2026-10-02` (#25) · `xlab/weeknight-fade-test` (#26) |

Local branches in the main checkout (11): `feature/execution-map`, `feature/forward-evidence`, `feature/p6-candidate-table`, `feature/p6-decision-eval`, `feature/p7-news-labels`, `feature/w1-lineage`, `fix/code-findings-and-tutorial`, `fix/code-findings-gaps-1`, `fwd/rule-w-2026-10-09`, `weekend/xmarket-weekend-2026-10-02`, `xlab/weeknight-fade-test`.

To delete them (tips stay reachable through the `archive/*` tags):

```bash
cd ~/development/tengu-cluster
git push origin --delete backup/docs-pipeline-html backup/phase-0-baseline-and-config-scaffold backup/refactor-hexagonal backup/stash-2026-03-22-event-bus-memory-switch backup/stash-2026-04-20-phase-b-orchestration-collapse evolve-skill-creator-2026-04-21T13-56-05Z feature/forward-evidence feature/p6-candidate-table feature/p6-decision-eval feature/p7-news-labels feature/phase-b-orchestration-collapse feature/w1-lineage fix/code-findings-and-tutorial fix/code-findings-gaps-1 fwd/rule-w-2026-10-09 weekend/xmarket-weekend-2026-10-02 xlab/weeknight-fade-test
git branch -D feature/execution-map feature/forward-evidence feature/p6-candidate-table feature/p6-decision-eval feature/p7-news-labels feature/w1-lineage fix/code-findings-and-tutorial fix/code-findings-gaps-1 fwd/rule-w-2026-10-09 weekend/xmarket-weekend-2026-10-02 xlab/weeknight-fade-test
```

The `mol` remote (another repository) was not touched.

## 5. Local state (operator's Mac)

| Item | State |
|---|---|
| Main checkout `~/development/tengu-cluster` | **at `10bdbb53a27390f6a67fd73826740befe05ababe` (#39), NOT pulled** — see § 6. One tracked local edit: `docs/forward-evidence-runbook-2026-10-08.md` (+1 no-pull row). Untracked: `TENGU_STUDIO_PLAN.md`, `docs/strategy-ranking-automation-2026-10-08.md` (both tracked upstream now; local copies = the upstream text), `handoff_for_check.md` (this file), the 3 private `docs/software-opportunity-*-2026-10-04.md` (status sections updated locally, never committed — the repo is public), the 2 `docs/crypto-opportunity-*` research docs (untouched) |
| Worktrees under `.claude/worktrees/` | none left (all removed after merge) |
| Build dirs `~/.cache/tengu-xm.noindex/agents/*` | removed after each merge; `seed` refreshed 2026-10-08; `weekend/` (frozen binaries) untouched |
| `~/.tengu` | untouched by this work (`logs/decisions.jsonl` sha256 `7deaacb257f43d3dc3e622f6d08adddad72e24f7cc812f7722a98ec25e0039d5` before and after every lab run) |
| `~/tengu-lab/` | lab data: `home/` (clean-room traces), `home-validation-2026-10-09/` (traces behind `docs/studio-evidence/`), `control-loop-lab/` (lab workspace, empty `in/` `out/`) — registered in `docs/SESSION_HANDOFF.md` local-data table |

## 6. Weekend #2 freeze — do this after Monday's stop

The forward run (Fri 2026-10-09 19:30 ET → Mon 2026-10-12) runs the frozen binary `~/.cache/tengu-xm.noindex/weekend/tengu-acdef66` from the main checkout. Verified 2026-10-08: that binary refuses `origin/main`'s `lineage/rankings/` directory (`not a registry entry` → the W1 load of `xmarket-weekend` fails). So the main checkout must not be pulled before the stop.

After the stop (Mon ≥ 10:00 ET) and the Monday snapshot:

```bash
cd ~/development/tengu-cluster
git checkout -- docs/forward-evidence-runbook-2026-10-08.md
rm TENGU_STUDIO_PLAN.md docs/strategy-ranking-automation-2026-10-08.md handoff_for_check.md   # tracked upstream
git pull --ff-only
```

## 7. Verification evidence

| Check | Result | Where |
|---|---|---|
| CI `rust-quality` on every PR | green (fmt, `cargo check --all-features`, clippy, `cargo test --workspace`, + `cargo test --features studio --bin tengu studio` from #44) | GitHub checks of #39–#45 |
| Full suites on the final code | `cargo test --workspace` 2130 passed / 0 failed / 101 ignored; `--features studio` 2176 passed / 0 failed | integration merge of #44 + ST-90 clean room |
| Clean room (ST-90) | fresh clone of `f7ac9a5636e2493e3839e896cba9951a2d41aefb`: release build, lab A0–A15, G1–G8, Studio Play / events / Stop on real Jev — PASS | `docs/studio-clean-room-2026-10-09.md` |
| Studio live validation (coordinator) | real Jev (`typesafe/jev-1.13-20260917`), run `cbd8cd05-30fb-4177-aa99-0b978f0e83e5`, 117 events; guards 401 / 403 / 421; second runner refused; Stop drained, lease released; replay byte-identical; `tengu trace show` = API | `docs/studio-2026-10-08.md` § Live validation, `docs/studio-evidence/*.png` |
| Baselines | lab baseline 32 Jev calls ($0.000799134); Gate 2 trace evidence; ST-30 acceptance | `docs/control-loop-lab-baseline-2026-10-08.md`, `docs/studio-trace-evidence-2026-10-08.md`, `docs/studio-acceptance-2026-10-08.md` |
| Weekend freeze | `git diff --stat <base>...<branch> -- sandboxes/xmarket-weekend sandboxes/xlab lineage/generations/W1.toml lineage/locks.toml skills/xlab-research` empty for every PR; `tengu lineage verify --pins` 0 errors | review agents' reports |
| Privacy | no operator money figures in public source (`tengu soe init` leaves them `REQUIRED`); fixtures `synthetic = true`; TED fixtures scrubbed; key value found in no file | PR #41 / #43 notes |

## 8. Operator gates and open items

| Gate / item | What the operator does | Doc |
|---|---|---|
| G-SR1 — ranking contracts | review, then `tengu lineage seal ranking:rank.xlab-w2.daily.v1` and `ranking:rank.xlab-w2.weekend.v1` (the publisher refuses unsealed contracts; nothing ran on real state) | `docs/strategy-ranking-automation-2026-10-08.md` |
| G-O0 — SOE | `tengu soe init`, set the four `REQUIRED` money values, add capabilities, sign; approve SEC / TED terms + hashes and enable the rows | `docs/soe-2026-10-08.md` § 15 |
| G-R2 — SOE Operator Review #2 | after 4 weekly cycles: `tengu soe review`; nothing from O5–O8 was built | `docs/soe-2026-10-08.md` |
| SOE-G0 lock | the lock row in `lineage/locks.toml` is the operator's (after signing) | `docs/lineage-2026-10-06.md` |
| Operator Review #3 — Studio editor | ST-40 is design only (no save) | `docs/studio-editor-design-2026-10-08.md` |
| Live engine-matrix legs | `xlab_rank`, `sources`, `soe` live legs not run (need keys / claude login / the local PC); offline legs pass | `tests/engine_matrix.rs` |
| Studio gates 1–4 | recorded as waived by the operator's 2026-10-08 instruction (never as reviewed) | `TENGU_STUDIO_PLAN.md` § 8 |

## 9. Known limits (from the review agents; not fixed on purpose)

| Area | Limit |
|---|---|
| Ranking | variant status (verdict) is not as-of (schema has no status time); daily contract reads the whole window as development data |
| SOE | DILIGENCE costs no weekly budget (operator decision); a torn last forecast-log line fails closed until fixed by hand; `replay` / `grade` / `resolve` take no lease |
| Sources | `parsed_ms` is stamped before batch commit (live vs replay may differ within a fetch); raw snapshots are decoded UTF-8, not wire bytes |
| Studio / trace | `tengu webhooks` and planner / subagent steps record no trace; a `kill -9` leaves a run `open`; Play runs the config loaded at Studio start |
| Pre-existing warnings | unused imports `tools/solana/write_tokens.rs`, dead `claude_cli_env` without `claude_code`, older clippy findings (none in the new SOE / source / ranking / studio / trace files) |

## 10. How to check (commands)

```bash
git fetch origin --tags
git log --oneline 10bdbb53a27390f6a67fd73826740befe05ababe~1..origin/main          # #39 … #45
gh pr list --state open                                                            # expect: none
git ls-remote --tags origin 'refs/tags/archive/*' | wc -l                          # 25 (24 + docs/sweep-b)
# in a fresh clone of origin/main (NOT the main checkout before Monday):
cargo fmt --all --check && cargo test --workspace && cargo test --workspace --features studio
cargo test --test layering_lint --test scope_lint --test tutorial_map --test code_map --test language_policy --test lineage_cli
export TENGU_HOME=$HOME/tengu-lab/home   # never ~/.tengu
cargo build --release --features studio && target/release/tengu studio --sandbox control-loop-lab   # docs/studio-2026-10-08.md
target/release/tengu lineage verify --pins --registry lineage                     # 0 errors
```

## 11. 2026-10-10 session — Studio builder + `engine = "codex"` (PR #52, OPEN)

> **Status 2026-10-13:** MERGED as #52 `23a586d1d85cd8a037e23fa3c57ab1c9b949f7ea` (head `fb190f76431e2e8822d479b2ef5ff22e62e07c0a`); see § T2.

| Ask (operator, 2026-10-10) | Delivered |
|---|---|
| CLI makes a sandbox template, name asked in the CLI | `tengu sandbox new` (asks name + template; `--name` / `--template` / `--no-open`) |
| n8n-like web dashboard: draggable tools / agents / sandbox configs with fields, wire relationships, Finalise composes the config | `/builder` (`tengu studio --sandbox <s> --allow-edit`): 10 card kinds, 12 wires, Rust palette + compiler, Validate (real `Config::load` on a temp copy + diff), Finalise → `config.toml` (+ `.prev`) |
| Secrets pluggable: local + Cloudflare (PR #50) | vault · env · Cloudflare seal proxy (probed `[keys]`: on once #50 merges; checked on a #50 + #51 test merge) |
| Claude, OpenAI subscription, local model, OpenRouter | `engine = "codex"` added (ChatGPT subscription); live on a canvas-built sandbox: openrouter ✅ claude_code ✅ codex ✅; local not run here (operator's PC) |
| Green "electricity" on connected wires, animations | live wires: glowing flowing current + particles; red refused wire shakes; Finalise burst |
| Debuggable | `tengu sandbox compile\|finalise\|palette`, Debug drawer, one `builder:` log line per request |
| Check PRs / branches for conflicts | #50, #51 mergeable; test merge found one test-only conflict (codex hardening test vs #51's `with_signer()`), fixed in `57553a4f724a3cb8f6d86b70df5c2b2986038b03` |
| Don't touch the weekend lab binary | untouched: pid 11624 (`~/.cache/tengu-xm.noindex/weekend/tengu-acdef66`) ran throughout; all builds in `~/.cache/tengu-xm.noindex/agents/builder`, live runs with a scratch `TENGU_HOME` |

| PR | Branch | Base | State |
|---|---|---|---|
| #52 | `feature/sandbox-builder` | `main` @ `91b2a2198bf29403aaa81c5d63760a71e3f6cf40` | OPEN — not merged (main checkout frozen until the weekend #2 stop; merge is the operator's word) |

| Commit (branch `feature/sandbox-builder`) | Subject |
|---|---|
| `3914de4b88080ba6de6ae6f1c262f7a6ba6fbfeb` | config: one ENGINES list for engine validation |
| `a0aecf42fc5b7a7fb72fc557d92059b0e59b845d` | engine = "codex" (branch `feature/codex-engine`, merged in, branch deleted) |
| `cb9ea333b43269898e083ef8ad739881b5b709de` | Studio builder: blueprint, palette, compiler, routes, CLI, page |
| `3f203ffe8d9e3c64826911d86eb1364701937411` | merge `feature/codex-engine` |
| `ed990fe66fcbd09995d52a970325a5befc6be9ff` | fit on first load, templates, copyable commands |
| `57553a4f724a3cb8f6d86b70df5c2b2986038b03` | codex hardening test without the signer helper (#51-proof) |
| `dedf602970b21f29f6fba576974b8621db8905b0` | cargo fmt |
| `4578fbe8b362128c897e798b2a5c1e879ce35f98` | docs |
| `493b217d494c14c720fe3cf258121ca42bd7b4a9` | lint-clean in every feature set, disabled items, Studio → Builder link |
| `d69141f43d17eb0516673e7d97f0f6c6a37adab8` | browser evidence `docs/studio-evidence/builder-0{1..6}-*.png` |
| `275e56d5179eb9cd0129768634458521d3216712` | tutorial pages, `builder.js` URL literals, wire wording |

| Check | Command |
|---|---|
| Unit | `cargo test --bin tengu -- builder blueprint codex hardening studio` |
| Lints | `cargo test --test layering_lint --test scope_lint --test code_map --test tutorial_map --test language_policy` |
| Builder without a browser | `tengu sandbox new --name t1 --template team --no-open && tengu sandbox compile --sandbox t1` |
| Builder in a browser | `tengu studio --sandbox t1 --allow-edit --open` |

Local state: worktree `~/development/tengu-builder` (branch `feature/sandbox-builder`), build cache `~/.cache/tengu-xm.noindex/agents/builder` (21 GB; delete after merge). Removed: worktrees `tengu-codex`, `tengu-mergecheck` and their caches. The main checkout `~/development/tengu-cluster` was not modified.
