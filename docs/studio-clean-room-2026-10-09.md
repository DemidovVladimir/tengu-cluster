# ST-90 clean-room acceptance (2026-10-09)

> `TENGU_STUDIO_PLAN.md` ST-90. A fresh clone of origin/main at #44 followed `docs/control-loop-lab-2026-10-08.md` and `docs/studio-2026-10-08.md` as written, then ran the repository checks. Run by a clean-room agent; recorded by the coordinator.

**Verdict: PASS.** A fresh clone follows both quick starts, and A0–A15, G1–G8, S1/S2/S4 and the Studio Play/Stop flow behave as documented. Every repository check is green. The 8 doc fixes it found are applied (§ Doc corrections).

| Item | Value |
|---|---|
| Clone | `https://github.com/DemidovVladimir/tengu-cluster.git` @ `f7ac9a5636e2493e3839e896cba9951a2d41aefb` (#44), into `$D/repo`; `git status` still clean at the end |
| `$D` | `<session scratch dir>/clean-room (not kept)` |
| Build | `CARGO_TARGET_DIR=$D/target CARGO_BUILD_JOBS=4 nice -n 10 cargo build --release --features studio`: exit 0 in 3m15s. One warning, from before #40: dead `claude_cli_env` at `src/adapters/outbound/egress.rs:349` |
| Env | `TENGU_HOME=$HOME/tengu-lab/home` exported first. The key was exported from the main `.env` and never printed. Helper scripts: `$D/env.sh`, `runtime.sh`, `studio.sh`, `checks.sh` |
| Isolation | `~/.tengu/logs/decisions.jsonl` sha256 `7deaacb257f43d3dc3e622f6d08adddad72e24f7cc812f7722a98ec25e0039d5` before and after. No file under `~/.tengu` is newer than the session. Main checkout untouched |
| Leftovers before start | The lab home already held a run from 10:01–10:06 (Studio stop, 12 decision lines, 2 trace files). It was moved aside (now `~/tengu-lab/home-validation-2026-10-09`, the Studio validation run) so the counts below start from zero. Nothing was deleted |
| Jev | Model `typesafe/jev-1.13-20260917`. 39 calls cost $0.001054704. Latency 258–458 ms |
| Secrets | Key value: 0 matches in `$D/out`, the lab home and `$W`. The token was in one stdout line, which I masked |

## Step 1: control-loop lab (`docs/control-loop-lab-2026-10-08.md`)

| # | Outcome |
|---|---|
| Quick start | `decide act`: write_marker → hold. `run` for 25 s gave 2 ticks, then SIGINT exit 0. `doctor --live`: `=> live` |
| A0 | exit 0. Shows `lab: engine=openrouter model=anthropic/claude-haiku-4-5 …`, `network: open`, `proxy: none (direct)` |
| A1 | 3/3 hold (`stopped`) with `history=[{t:1,hold}]`. `out/` was **not empty**, because the quick start had already written `marker.txt`; its mtime stayed the same |
| A2 | 3/3 write_marker executed (`ok:true`, `File 'out/marker.txt' written (12 bytes)`), then hold. Marker contains `scenario=act` |
| A3 | 3/3 read_probe executed (`ok:false`, `Cannot read file 'in/absent.txt'…`), then hold. exit 0 |
| A4 | 3/3 `escalated` hold at confidence 0.94 / 0.91 / 0.94. `history=[]`. Map sha `1a97d5793e4abfd1a53fb717c34b1a9fbfa1dc49b7ef6dd4ec165813cac5a1ef`. Audit line has `trigger map:…`, `act_at 1.0`, `t 0` |
| A5 | `dry_run` write_marker (no `ok` field), then hold. Marker unchanged. Map sha `ed5ed6a09388130bcfa59f5b95d8a8ca8b75f1e462742b168b11e413c61e682d` |
| A6 | 24 lines = quick start 4 (decide 2 + 2 ticks) + A1–A4 tries 18 + A5 2. Every line has `decision_id`, `model`, `latency_ms`, `usage`. The 7 tool lines carry `call_id=demo:decide-demo-<uuid>:1` |
| A7 | Leases taken, both feeds started, `tengu run started`. A tick every 20 s (`tick:<ms>`). The probe warns `fatal` every 30 s |
| A8 | At 68 s: `done 4`, `=> live`. At 93 s: `done 5`, `=> live`. The probe stays `ok` after 90 s because it is optional |
| A9 | Heartbeat: `running`, A7's holder, `completed 5`, `tick live`, `probe down`/`fatal`, `heartbeat_secs 2` |
| A10 | exit 1 with ``…held by `<A7 holder>` for 27 s more…``. Heartbeat unchanged |
| A11 | `stopping reason=SIGINT`, `loop totals … 6/6`, `lease_released=true`, exit 0. Heartbeat `stopped`/`SIGINT`. `doctor --live` shows `FAIL heartbeat stopped`, loop/feed rows `ok`, `=> NOT live`, exit 1 |
| A12 | New holder and newer `started_at_ms`; counters reset (3); `t` starts again at 1. Run 1 and run 2 never share a `runtime_id` or `run_id` |
| A13 | `out/` holds only `marker.txt`. `no-egress`. Logs dir has no `risk.jsonl`. `$W` contains only `.tengu in out`, and `in/` is empty |
| A14 / A15 | Same as A11 (exit 0) / cleanup in Step 4 |
| G1 | 16 nodes, 17 edges. `config_hash 6b52fc14d7212ed029da0f2bc2eb0f2a8e12e14ddd04c7465f1bc91be65dea2f`. The whole JSON equals the fixture |
| G2 | `act_at` 0.8→1.0 and `dry_run` false→true. A map with `act_at 0.5` exits 1 with "may only raise it" |
| G3–G7 | 17 recordings (14 `decide` with null `runtime_id`, 3 `run`). `--after 8` returns seqs 9–12. `../x` is refused with exit 1. The A2/A3/A4 event sequences match the doc exactly |
| G8 | Runtime events run starting → running, then per tick fired / queued / tick_sent / started … completed; probe `feed.failed` is `fatal` with `retrying:false`; stop gives `runtime.stopped` with `lease_released:true`. The order differs from the doc in places (fix 5) |

## Step 2: Studio (`docs/studio-2026-10-08.md`)

| Step | Outcome |
|---|---|
| `--bind 0.0.0.0` | exit 1, `refused — … loopback only` |
| `studio --port 0` | Prints `Studio: http://127.0.0.1:50469/#t=<64 hex>`. Logs go to stderr and `tengu.log` |
| meta · graph · control · health | `control_enabled:true`, `read_only:false`. Graph 16/17, same hash. Control `idle` (play ok; stop and event say why not; scenarios act/normal/tool-error). Health `live:false` |
| Guards | No token: 401. Foreign `Host`: 421. Foreign `Origin`: 403. POST with token only: 403. POST to graph with all proofs: 405. Play without proofs: 403 |
| Play | 200. Holder `Vladimirs-MacBook-Pro-2.local:90754:4a3ff420-4f82-4184-ae3a-0015177f5fb0`, run `749ce715-382c-42bd-97ad-eccc93545d30`. `doctor --live`: `=> live` |
| Second runner | `tengu run` exits 1 and names the Studio holder |
| Ticks · events | 2 ticks completed. `act` returned 202 (session `studio-act-26153417-9720-4a7a-a646-cb2b42c3c8cf`, write_marker ok). `tool-error` returned 202 (session `studio-tool-error-d247a989-a3d4-42e2-8b8b-f76ca5370da2`, read_probe failed, then hold escalated at 0.64 < 0.8, the same Jev variance seen before). `nope` returned 422; a second Play returned 409 |
| Runs · replay | 53 events. Two fetches were byte-identical, and the page without `view` is byte-identical to `tengu trace show`. Board: closed, loop demo 4/4, model is the Jev build |
| Stop | 200 with `reason studio stop`, drain 0/0/0, `lease_released:true`. Control `stopped`; heartbeat `stopped`/`studio stop` |
| SIGINT Studio | exit 0. `pgrep -fl 'tengu (run\|studio)'` is empty. Studio's own run `421ff11d-0440-4879-bf36-7251bf7095dc` (`kind=studio`, 16 events) ends with `studio.stopped` |
| Not checked | The browser page visuals (S3, S5–S7). I checked their API equivalents with curl only |

## Step 3: repository checks (no key exported, `TENGU_HOME=$D/test-home`)

| Check | Result |
|---|---|
| `cargo fmt --all --check` | exit 0 |
| `cargo clippy --all-features --all-targets` | exit 0 with 102 warnings (71 bin + 30 test-only + 1 `tests/bridge_conformance.rs`). **None are in the #40–#44 SOE / source / ranking / studio / trace files.** All 8 changed files that do have warnings have them on lines from before #40 (blame: `51c16256e5a900ccafa30c2c618bdb34da4ee32a`, `9cc0939ff6f213b250f091fc5d94dc08676848de`, `131af134b57191850c249008a50b2ec05a59ed53`, `e83cb7b5aa1e2b1e809de6150745b43c227a6aec`) |
| `cargo test --workspace` | 13 suites: 2130 passed, 0 failed, 101 ignored (unittests 2067). 4 compiler warnings, from before #40 |
| `cargo test --workspace --features studio` | 13 suites: 2176 passed, 0 failed, 101 ignored (unittests 2113, 55 `studio::` lines) |
| layering · scope · tutorial_map · code_map · language_policy · lineage_cli | 3 · 2 · 4 · 2 · 3 · 8 passed, 0 failed |

## Step 4: cleanup

I ran the runbook block with `logs/trace` kept: removed `marker.txt`, `observations.db*`, `logs/{decisions.jsonl,tengu.log,maps}` and `state`. 19 trace files remain in `~/tengu-lab/home/logs/trace/control-loop-lab/`. Copies of the logs, state and decisions are in `$D/out/` (`decisions-lab.jsonl`, `lab-copy/`).

## Doc corrections — all 8 applied 2026-10-09 (docs sweep B; #4 was already fixed by the sweep)

| # | File:line | Problem | Fix |
|---|---|---|---|
| 1 | `docs/control-loop-lab-2026-10-08.md:54` (A1) | "`$W/out` empty" contradicts the quick start (`:42`), which writes `out/marker.txt` | "`out/marker.txt` not written (absent on a fresh lab; mtime unchanged after the quick start)" |
| 2 | `docs/control-loop-lab-2026-10-08.md:59` (A6) | "= Jev calls so far" leaves out the quick start and the 3 tries | "= every Jev call since cleanup: quick start (decide 2 + 1 per tick) + per try A1 1, A2 2, A3 2, A4 1, A5 2" (seen: 24) |
| 3 | `docs/control-loop-lab-2026-10-08.md:33` | `cargo build --release`, but S1–S7 (`:87`) need the studio feature | `cargo build --release --features studio   # A0–A15 / G1–G8 need only the default build` |
| 4 | `docs/control-loop-lab-2026-10-08.md:92` (S2) | "`-X POST` ⇒ 405": the CSRF guard answers first, so it is 403 | "`-X POST` ⇒ 403 (no `Origin` / `Sec-Fetch-Site`); with all three proofs ⇒ 405" |
| 5 | `docs/control-loop-lab-2026-10-08.md:83` (G8) | The strict per-tick order does not hold: on the first tick `loop.started` (seq 9) came before `feed.tick_sent` (11), probe events interleave, and the first tick's `observation.read` is `stale` | Add "(tasks run concurrently: `loop.started` may come before `feed.tick_sent` and probe events interleave; the first tick's `observation.read` is `stale` because there is no row yet)" |
| 6 | `docs/studio-2026-10-08.md:12` | The binary path ignores `CARGO_TARGET_DIR`, and the `--port` default is not stated | Add "(`$CARGO_TARGET_DIR/release/tengu` when set)" and "`--port` 0 or omitted = any free port" |
| 7 | `docs/studio-2026-10-08.md:64` (points to `docs/studio-acceptance-2026-10-08.md:20-23`) | The curl helper uses `$BASE` and `$TOK` but never shows how to get them | Inline the helper with `URL=$(sed -n 's/^Studio: //p' studio.out); BASE=${URL%%/#t=*}; TOK=${URL##*#t=}`, plus `G` / `P` |
| 8 | `docs/studio-2026-10-08.md:59` | The Play 409 row lists only "lease held elsewhere" | "409 lease held elsewhere · already running here (Stop first)" |

## Jev decision ids (all `typesafe/jev-1.13-20260917`)

| Step · run_id | decision_id(s) |
|---|---|
| QS decide `43f7e3cf-1b53-459f-836d-658011b92f54` | `gen-dec-1791534229-z4gjh7FSunlf23ZxZNzz` `gen-dec-1791534229-wRWdlaSt3qpV5dAVWcHy` |
| QS run `cf48afef-34ad-405a-8186-e2e8e381940d` (holder `Vladimirs-MacBook-Pro-2.local:85443:324db35e-d0d6-4825-9b0e-0583f007f922`) | `gen-dec-1791534234-WK78RO5GAMP5bBNHzAZo` `gen-dec-1791534240-hQJH9aWkmSaDo8KcKDp4` |
| A1 `22cbd6eb-90b1-4b46-8607-22bac78932da` · `09c590b7-fae6-43e2-9cb7-53d5aea0e006` · `9ccee845-1f50-4eaf-a578-8cf57640c2e2` | `gen-dec-1791534272-rHZl1rYtvIX2FTFjJIBo` · `gen-dec-1791534273-BS6fgPVGNuYysdALh9Q6` · `gen-dec-1791534273-CU7Y0ESLOEXv2ZpDdZea` |
| A2 `4c51edb4-03fd-4583-b810-f8ff709a9092` · `c9027ddc-ee8c-42b5-9dd7-adde1d63bb52` · `4e61193b-ba4a-43c2-8c0c-0ae34d588052` | `gen-dec-1791534278-ul2OPDEjWJqZpJqLLDA0` `gen-dec-1791534279-nzB90y8IFX8vTQqQDBh3` · `gen-dec-1791534279-MmNmNgmnpUP0leie2xnL` `gen-dec-1791534279-FYmtI6ZoxcDwJe9xvlp7` · `gen-dec-1791534280-1wiXXxYIUm8eoa00eaP7` `gen-dec-1791534280-3EfWV2h5aboGMMY0mlnI` |
| A3 `2074536c-3e34-4a6f-b226-cc3e904a99ae` · `75647ad8-a213-4389-bcb1-9a1d2c321773` · `eae7a79a-2966-42c4-a672-8b9b34e79138` | `gen-dec-1791534285-MzvHE3GqV1NKRKJKrLP3` `gen-dec-1791534285-YivvEW57dks0h5xh25H0` · `gen-dec-1791534286-5vb72fVHFhhifZM5K7AK` `gen-dec-1791534286-7ycLaBAmEEYec0Xp9hNI` · `gen-dec-1791534286-7Hu3kHiz10XzcNWu1Adx` `gen-dec-1791534287-KZOS6IAN2cM9Wk2seUU4` |
| A4 `783e263f-b8ac-4bbe-afe8-284b45437ce9` · `49c1d144-e471-4c71-b04b-ff698ad5d2f7` · `29921098-5641-40fe-ae38-83b346863910` | `gen-dec-1791534287-xMbjx7fTOb4HUjss1o0a` · `gen-dec-1791534288-bOznhA6gCxHVCrKJ0A8m` · `gen-dec-1791534288-OHBglyJm8vr8xlDYD34X` |
| A5 `f6dae06f-bf96-417c-8e54-7aa6320f1f49` | `gen-dec-1791534293-rpXYelAx2KeeQ2X4z639` `gen-dec-1791534294-n5JhmtGhXEPplNxfEVBF` |
| A7 `e9b2668d-633f-41b7-a1d8-592569f96458` (holder `Vladimirs-MacBook-Pro-2.local:86824:aa573847-7cad-43f0-93ed-246999813b5b`) | `gen-dec-1791534307-EUH0miRVRNRC1tQEtrBT` `gen-dec-1791534320-AKq1L5N4sqzzuIDEP4lf` `gen-dec-1791534340-GY5WC2WVWU4VQES7W74P` `gen-dec-1791534360-6uQT7ZBUu22OyULLroA6` `gen-dec-1791534380-hae8kBho8ji3bSMwafz7` `gen-dec-1791534400-O1vm24c34hGj3I8n6PP6` |
| A12 `9c0fcd4a-0601-4ffb-bac7-89d39b2c80e9` (holder `Vladimirs-MacBook-Pro-2.local:89300:db737680-dcca-455c-881d-67bb72f21d18`) | `gen-dec-1791534400-6uWScOmVvwCB0NVmIb5i` `gen-dec-1791534420-DDfCHS0DhYowPFbn0Fyk` `gen-dec-1791534440-k4iJ0pl6MVmy3DcnOhto` |
| Studio Play `749ce715-382c-42bd-97ad-eccc93545d30` (holder `Vladimirs-MacBook-Pro-2.local:90754:4a3ff420-4f82-4184-ae3a-0015177f5fb0`) | ticks `gen-dec-1791534501-7GWgqVptS6UIOdveN2be` `gen-dec-1791534520-rf7YgSmKk93MH59tAyW6` · act `gen-dec-1791534521-YoNy3Df6G4jsrTZegY4r` `gen-dec-1791534521-5CStYGoJEDGFiPfyCqpq` · tool-error `gen-dec-1791534522-Yz5FBRuY0V9KPF12Xv6i` `gen-dec-1791534522-g5kW6sYLmjkd6tjyDYjB` (escalated) |
