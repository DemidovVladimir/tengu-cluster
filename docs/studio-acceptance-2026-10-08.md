# Studio Play / Stop — live acceptance (ST-30 / ST-31, run 2026-10-09)

Real Jev, release build `cargo build --release --features studio` of the ST-30 / ST-31 tree
(`feature/studio` at `9a71ed6b0f563904b889dac7ca7c5e0b65406a0e` + this change, built after the
last code edit; working-tree diff sha256 `642c23400a617c3848a86dfb0670208d26421af7e1ffbf6d736cfd7eff69de79`).
Lab home `TENGU_HOME=$HOME/tengu-lab/home`, sandbox `control-loop-lab` (`[studio] control = true`).
Sanitized: the token is never shown (64 hex, length checked); the key never printed. Plan:
`TENGU_STUDIO_PLAN.md` § 6, § 8 ST-30 / ST-31. Window 2026-10-09T06:58:03Z → 06:58:29Z.

## Commands

```bash
export TENGU_HOME="$HOME/tengu-lab/home"          # first; never ~/.tengu
T=<target>/release/tengu
$T studio --sandbox control-loop-lab --port 0 &    # stdout: Studio: http://127.0.0.1:60224/#t=<64 hex>
G() { curl -sS -H "X-Studio-Token: $TOK" "$BASE$1"; }
P() { curl -sS -X POST -H "X-Studio-Token: $TOK" -H "Origin: $BASE" -H 'Sec-Fetch-Site: same-origin' \
      -H 'Content-Type: application/json' --data "$2" "$BASE$1"; }
G /api/v1/control; P /api/v1/control/play '{}'
$T run --sandbox control-loop-lab                  # a second runner: refused
P /api/v1/control/event '{"scenario":"act"}'; P /api/v1/control/event '{"scenario":"tool-error"}'
G /api/v1/runs; G /api/v1/runs/<run_id>/events?limit=1000
P /api/v1/control/stop ''; G /api/v1/control
kill -INT <studio pid>                             # after a second Play: drains it first
```

Script: session scratchpad `lane-S/st30-lab.sh` (watchdog 300 s, runbook cleanup at the end); page smoke `lane-S/st30-ui.sh`.

## Results

| # | Step | Result |
|---|---|---|
| 1 | `GET /api/v1/meta` · `GET /api/v1/control` | `read_only: false`, `control_enabled: true`; state `idle` (tone plain), why `on: [studio] control = true in this sandbox`; play ok, stop `nothing to stop: this Studio runs no runtime`, event `no runtime: Play first`; scenarios `act`, `normal`, `tool-error` (the `*.map.json` maps not listed); loops `demo` |
| 2 | ST-31 guards on `POST …/play` / `…/event` | see § Guards — none started anything (state still `idle`) |
| 3 | `POST /api/v1/control/play {}` | **200**, holder `Vladimirs-MacBook-Pro-2.local:31302:64331316-a1f3-4a4f-a2c0-948380f5f8a8` (the Studio pid), run `93668d55-372f-45a3-9b45-cfec64055960`; state `running` (green) |
| 4 | second runner `tengu run --sandbox control-loop-lab` | **exit 1**: ``sandbox `control-loop-lab` is already running: lease `runtime:control-loop-lab` in ~/tengu-lab/home/state/runtime.db is held by `Vladimirs-MacBook-Pro-2.local:31302:64331316-a1f3-4a4f-a2c0-948380f5f8a8` for 30 s more.`` |
| 5 | `tengu doctor --live` while Studio runs it | `=> live` (exit 0): heartbeat holder = Studio's, loop `demo`, feeds `tick` (required) + `probe` (optional) |
| 6 | two ticks | `loop.completed` for `tick:1791529084164` (run_on_start) and `tick:1791529100000` (the 20 s grid) |
| 7 | `POST …/event {"scenario":"act"}` | **202**, session `studio-act-759ba3d9-dc8a-4bc3-8d44-1cb9b00c7898` → Jev `write_marker` (executed, `ok: true`, `out/marker.txt` = `scenario=act`) → `hold`; `loop.completed` |
| 8 | `POST …/event {"scenario":"tool-error"}` | **202**, session `studio-tool-error-935a4723-dfb1-4cc6-a170-198fabbe62ec` → `read_probe` (executed, `ok: false`: the absent file) → `hold` at confidence 0.66 < `act_at` 0.8 = `escalated` (real Jev; the stop is explicit); `loop.completed` |
| 9 | `POST …/event {"scenario":"nope"}` · a second `POST …/play` | **422** `no scenario nope (… act, normal, tool-error)` · **409** `already running: this Studio started it — Stop it first` |
| 10 | `GET /api/v1/runs` · `/runs/<run_id>/events?limit=1000` | the run `live` while it ran; after Stop its 49 events = `tengu trace show` line for line (the page minus `view`); board header: runtime `stopped`, model `typesafe/jev-1.13-20260917`, loop `demo` accepted 4 · completed 4 · failed 0 · dropped 0 |
| 11 | `POST /api/v1/control/stop` | **200** after the drain: `reason: studio stop`, `failed: false`, drain finished 0 · dropped 0 · aborted 0, `aborted_tasks: []`, `lease_released: true`; `GET /api/v1/control` = `stopped`, `last_run.end.reason` = `studio stop` |
| 12 | heartbeat after Stop | `state: stopped`, holder as in 3, pid 31302, `stop_reason: studio stop`; `/api/v1/health` `live: false` (red); runtime run ends `runtime.stopping` (48, `studio stop`) → `runtime.stopped` (49, ok, `lease_released: true`) |
| 13 | CLI `tengu run` starts (lease free), Studio = `attached` | heartbeat holder `Vladimirs-MacBook-Pro-2.local:31582:1272713b-edb1-4d3b-bac6-894fee23efc0` (pid 31582); every action `ok: false`; Play / Stop / event each **409**, the holder named in full (`… holds this sandbox's runtime lease — stop it where it runs, then Play` · `this Studio did not start … — stop it where it runs (Ctrl-C, or SIGTERM to its pid)` · `events go to a runtime this Studio started; … is another process's`); CLI SIGINT → exit 0 |
| 14 | Play again, then SIGINT to Studio | **200**, run `94b57d6c-da8d-42cf-b68f-2260767cc2e8`, holder `Vladimirs-MacBook-Pro-2.local:31302:740d8d9b-b7ac-4a7a-af21-38b62f496539`; SIGINT ⇒ Studio drained it first (heartbeat `stopped`, `stop_reason: studio SIGINT`; run ends `runtime.stopping` → `runtime.stopped` ok) and exited **0**; `doctor --live` after: `NOT live` |
| 15 | `decisions.jsonl` of run `93668d55-372f-45a3-9b45-cfec64055960` | 6 lines, each with `run_id` + `runtime_id`; model `typesafe/jev-1.13-20260917`, 340–551 ms, cost $0.000171318 in all |
| 16 | after | `pgrep -fl 'tengu (run\|studio)'` = nothing; key value: 0 matches in the output dir and the lab home; token: 0 matches outside the one stdout line (deleted); `~/.tengu/logs/decisions.jsonl` sha256 unchanged (`7deaacb257f43d3dc3e622f6d08adddad72e24f7cc812f7722a98ec25e0039d5`); lab paths cleaned (runbook block) |

## Guards (ST-31, live, the release binary)

| Request to `POST /api/v1/control/play` (or `…/event`) | Code |
|---|---|
| no token header | 403 |
| token in the query (`?token=`) only | 403 |
| `Origin: http://evil.example` + `Sec-Fetch-Site: cross-site` | 403 |
| no `Origin` | 403 |
| no `Sec-Fetch-Site` | 403 |
| `Host: evil.example` (DNS rebinding) | 421 |
| `Content-Type: text/plain` (a form-like POST) | 415 |
| body 1 100 000 bytes (> 1 MiB) | 413 |
| `{"event": {…}}` (an event body: the page sends names only) | 400 |
| CORS preflight `OPTIONS` from `http://evil.example` | 403, no `Access-Control-*` header |

## Studio's own recording (`kind = "studio"`, run `7c6e4998-2077-4ea2-9a86-17a1c2a47427`, 27 events)

| seq | kind | status | action · detail |
|---|---|---|---|
| 1–2 | `run.opened` · `studio.started` | ok · running | — |
| 3 → 4 | `studio.control` | pending → ok | play · running, holder + run as in step 3 |
| 5 → 6 · 7 → 8 | `studio.control` | pending → ok | event act · event tool-error (queued, session ids as in 7 / 8) |
| 9 → 10 · 11 → 12 | `studio.control` | pending → refused | event `nope` · play while running |
| 13 → 14 | `studio.control` | pending → ok | stop (`studio stop`): draining |
| 15 | `studio.runtime` | ok | the runtime ended: `studio stop` |
| 16 → 21 | `studio.control` × 3 | pending → refused | play · stop · event while attached (holder pid 31582) |
| 22 → 23 | `studio.control` | pending → ok | play (run `94b57d6c-da8d-42cf-b68f-2260767cc2e8`) |
| 24 → 26 | `studio.control` · `studio.runtime` | pending → ok · ok | stop `studio SIGINT` (Studio's shutdown) · ended |
| 27 | `studio.stopped` | ok | `SIGINT` — closes the run |

`tengu trace runs`: the studio run (27, `studio.stopped`) + three `run` recordings, each its own `run_id` and holder, each ending `runtime.stopped` ok: `93668d55-372f-45a3-9b45-cfec64055960` (Studio, 49 events), `d148fb06-f043-44b8-9c76-68d6f657c130` (CLI, 17), `94b57d6c-da8d-42cf-b68f-2260767cc2e8` (Studio, 17).

## Page smoke (headless Chrome, temp profile, the same binary)

| Sandbox | `#controls` |
|---|---|
| `control-loop-lab` (control on) | shown: badge `idle` (plain), Play enabled, Stop disabled `nothing to stop: this Studio runs no runtime`, Send event disabled `no runtime: Play first`, scenarios `act` · `normal` · `tool-error` |
| `tor-check` (no `[studio]`) | `hidden` — the buttons are not drawn |

## Deviations

| Item | Note |
|---|---|
| Doc date | file named `…-2026-10-08` as the plan's tracker asks; the run is 2026-10-09 |
| Stop drain | the Stop of step 11 met no event in flight (finished 0); a drain of a running event is proved in `adapters::inbound::studio::control::tests::stop_drains_and_releases_lease` |
| `tool-error` final step | Jev's `hold` came at confidence 0.66 → `escalated` (an earlier try the same day: 0.64) — Jev's own variance, recorded as is |
