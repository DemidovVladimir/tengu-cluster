# Studio trace evidence — ST-12 / Gate 2 (2026-10-08)

`TENGU_STUDIO_PLAN.md` ST-12 + Gate 2: the control-loop lab run for real, then `tengu trace show` reconstructs every run. Gate 2 is **waived by the operator's 2026-10-08 instruction**, not reviewed. Event contract: `docs/runtime-2026-09-30.md` § Trace. Runbook rows G6–G8: `docs/control-loop-lab-2026-10-08.md`.

| Item | Value |
|---|---|
| When | 2026-10-08 20:36:16Z → 20:37:49Z |
| Build | `cargo build --release` of the ST-12 tree on `feature/studio` (parent `9b956fa4fb2ea748d0c32a429bdc964f29d6701c`), default features |
| Home | `TENGU_HOME=$HOME/tengu-lab/home` (exported first); `~/.tengu/logs/decisions.jsonl` sha256 equal before and after |
| Jev | 12 real calls, model `typesafe/jev-1.13-20260917`, latency 249–529 ms, cost $0.00030891 |
| `config_hash` | `a68882899f1aacfe0dde95eb5c6c1a9490750886c5af79b11255437044b4b40f` on every event (= the ST-10 graph) |

Paths below: `$HOME` replaces the home dir. Ids are in full.

## The five recordings (`tengu trace runs`)

| Run | `run_id` | `kind` | `runtime_id` | Events | First → last |
|---|---|---|---|---|---|
| decide act | `71650e67-fb10-4aac-8a5a-a1c837c1cee8` | decide | null | 12 | `run.opened` → `trigger.completed` |
| decide tool-error | `7343fce8-0543-414a-bcbb-c38eb9ff512d` | decide | null | 12 | `run.opened` → `trigger.completed` |
| decide `uncertain.map.json` | `ca7326e9-8304-42cb-b4e5-125f9755d54a` | decide | null | 6 | `run.opened` → `trigger.completed` |
| `tengu run` #1 (62 s, SIGINT) | `d3987899-1b21-45b1-af40-87c3335ee767` | run | `Vladimirs-MacBook-Pro-2.local:27798:bfd39ea7-b4cb-4260-9618-3e5a0a88f482` | 57 | `run.opened` → `runtime.stopped` |
| `tengu run` #2 (restart, 26 s, SIGINT) | `47c724e2-b8cf-4d23-a771-4a51be54c056` | run | `Vladimirs-MacBook-Pro-2.local:28618:9860a615-b02d-44c4-bd82-1b0ce7399a51` | 29 | `run.opened` → `runtime.stopped` |

## Decide `act` — the whole run

`echo '{"scenario":"act"}' | tengu decide --sandbox control-loop-lab --loop demo --event -` → exit 0, outcomes `executed write_marker`, `stopped hold`; session `decide-demo-e6c5a282-95f4-4ee5-801f-f29708b126b4`. Parent = the parent's `seq` (`event_id` = `<run_id>:<seq>`).

| seq | kind | status | node | parent | call id | ms |
|---|---|---|---|---|---|---|
| 1 | `run.opened` | ok | `trigger:decide` | - | - | - |
| 2 | `trigger.decide` | running | `trigger:decide` | - | - | - |
| 3 | `observation.read` | missing | `world:demo/tick` | 2 | - | - |
| 4 | `jev.completed` | ok | `jev:demo` | 2 | - | 529 |
| 5 | `action.selected` | running | `action:demo/write_marker` | 4 | `demo:decide-demo-e6c5a282-95f4-4ee5-801f-f29708b126b4:1` | - |
| 6 | `tool.started` | running | `tool:lab/write_file` | 5 | `demo:decide-demo-e6c5a282-95f4-4ee5-801f-f29708b126b4:1` | - |
| 7 | `tool.completed` | ok | `tool:lab/write_file` | 6 | `demo:decide-demo-e6c5a282-95f4-4ee5-801f-f29708b126b4:1` | 0 |
| 8 | `action.completed` | ok | `action:demo/write_marker` | 5 | `demo:decide-demo-e6c5a282-95f4-4ee5-801f-f29708b126b4:1` | - |
| 9 | `observation.read` | missing | `world:demo/tick` | 2 | - | - |
| 10 | `jev.completed` | ok | `jev:demo` | 2 | - | 262 |
| 11 | `action.selected` | ok | `action:demo/hold` | 10 | - | - |
| 12 | `trigger.completed` | ok | `trigger:decide` | 2 | - | 793 |

| Event | Payload excerpt |
|---|---|
| 4 `jev.completed` | `decision_id` `gen-dec-1791491777-jRTDHH7q6gLFhkBPI3MC` · `legal` `{"hold":{},"read_probe":{"probe":["absent"]},"write_marker":{"scenario":["act"]}}` · `questions` `["next_action"]` · `next_action` `{"choice":"write_marker","confidence":0.89,"probabilities":{"hold":0.07,"read_probe":0.0,"write_marker":0.93}}` · `usage.cost` 0.000022344 · `artifact` `$HOME/tengu-lab/home/logs/decisions.jsonl` key = the `decision_id` |
| 7 `tool.completed` | `{"line1":"File 'out/marker.txt' written (12 bytes)","text_bytes":40,"tool":"write_file"}` |
| 8 `action.completed` | `args` `{"content":"scenario=act","path":"out/marker.txt"}` · `ok` true · `outcome` `{"action":"write_marker","outcome":"executed"}` · `confidence` 0.89 |

## Decide `tool-error` and the `uncertain` map

| Run | Events (seq kind status node ← parent) | Excerpt |
|---|---|---|
| tool-error `7343fce8-0543-414a-bcbb-c38eb9ff512d` | same shape as act; 5 `action.selected` running `action:demo/read_probe` ← 4 · 6 `tool.started` `tool:lab/read_file` ← 5 · **7 `tool.failed`** ← 6 · **8 `action.completed` failed** ← 5 · 11 `action.selected` ok `action:demo/hold` · 12 `trigger.completed` | 7: `{"error":"Cannot read file 'in/absent.txt': No such file or directory (os error 2)","tool":"read_file"}` · 8: `ok` false, `outcome` `executed` (a tool failure is not a loop failure) |
| uncertain `ca7326e9-8304-42cb-b4e5-125f9755d54a` | 2 `trigger.map` running `trigger:map/1a97d5793e4abfd1a53fb717c34b1a9fbfa1dc49b7ef6dd4ec165813cac5a1ef` · 3 `observation.read` missing · 4 `jev.completed` ← 2 · **5 `action.escalated` escalated `gate:demo/act_at`** ← 4 · 6 `trigger.completed` | 4: `next_action` `hold` confidence 0.92 (`decision_id` `gen-dec-1791491778-A5QiPbr5wsRyT46JHv92`) · 5: `{"act_at":1.0,"action":"hold","confidence":0.92,"escalate":false,"escalator":false,"outcome":{"action":"hold","confidence":0.92,"outcome":"escalated"}}` |

## `tengu run` #1 — ticks, probe failures, SIGINT

Ticks every 20 s (+ one at start), probe every 30 s. One tick and one probe slot, in file order (the run repeats them; 5 ticks, 3 probes):

| seq | kind | status | node | parent | session |
|---|---|---|---|---|---|
| 1–3 | `run.opened` · `runtime.starting` (pending) · `runtime.running` | | `runtime:control-loop-lab` | - | - |
| 4 | `feed.fired` | running | `feed:probe` | - | `probe:1791491779196` |
| 5 | `feed.fired` | running | `feed:tick` | - | `tick:1791491779196` |
| 6 | `loop.queued` | pending | `loop:demo` | 5 | `tick:1791491779196` |
| 7 · 8 | `tool.started` · `tool.failed` | running · failed | `tool:lab/read_file` | 4 · 7 | `probe:1791491779196` |
| 9 | `loop.started` | running | `loop:demo` | 6 | `tick:1791491779196` |
| 10 | `feed.failed` | failed | `feed:probe` | 4 | `probe:1791491779196` |
| 11 | `observation.read` | missing | `world:demo/tick` | 9 | `tick:1791491779196` |
| 12 | `feed.tick_sent` | ok | `feed:tick` | 5 | `tick:1791491779196` |
| 13 | `jev.completed` | ok | `jev:demo` | 9 | `tick:1791491779196` |
| 14 | `action.selected` | ok | `action:demo/hold` | 13 | `tick:1791491779196` |
| 15 | `loop.completed` | ok | `loop:demo` | 9 | `tick:1791491779196` |
| 16–55 | 4 more ticks (`observation.read` **ok** from the 2nd: the `feed/1:tick` row exists) + 2 probe slots | | | | `tick:1791491780000` … `tick:1791491840000` |
| 56 | `runtime.stopping` | pending | `runtime:control-loop-lab` | - | - |
| 57 | `runtime.stopped` | ok | `runtime:control-loop-lab` | 56 | - |

| Event | Payload excerpt |
|---|---|
| 2 `runtime.starting` | `holder` = the `runtime_id` · `leases` `["runtime:control-loop-lab"]` · `pid` 27798 · `artifact` `$HOME/tengu-lab/home/state/run-control-loop-lab.json` |
| 10 `feed.failed` | `{"class":"fatal","error":"Cannot read file 'in/absent.txt': No such file or directory (os error 2)","failed":1,"ok":0,"retrying":false}` |
| `loop.completed` ×5 | `stats.completed` 1 → 2 → 3 → 4 → 5 (`accepted` the same, `failed` 0), `duration_ms` 250–366 |
| 56 · 57 | `reason` `SIGINT` · `grace_secs` 10 · `drain` `{"finished":0,"dropped":0,"aborted":0}` · `aborted_tasks` [] · `lease_released` true |
| health (same run) | `doctor --live` at 62 s: exit 0, `=> live`, `loop demo … done 5`, `feed probe (optional) down … fatal`, `feed tick (required) live · items 5`; heartbeat after: `stopped`, `SIGINT`, holder = the `runtime_id` |

## Restart (`tengu run` #2) and live = replay

| Check | Result |
|---|---|
| New ids | run #2 `run_id` `47c724e2-b8cf-4d23-a771-4a51be54c056` ≠ #1 · `runtime_id` pid 28618 / uuid `9860a615-b02d-44c4-bd82-1b0ce7399a51` ≠ #1 · each heartbeat's `holder` = its run's `runtime_id` |
| Evidence not mixed | `decisions.jsonl`: 5 tick lines carry run #1's ids, 2 carry run #2's, each decide's lines (2 · 2 · 1) its own `run_id` and no `runtime_id`; every trace file has one `run_id`, one `runtime_id`, one `config_hash` |
| Run #2 events | 29: `runtime.*` ×4, `feed.fired` ×4, `loop.queued / started / completed` ×2, `tool.failed` ×2, `feed.failed` ×2, `runtime.stopped` (`lease_released` true) · its first `observation.read` is **ok** (run #1's `feed/1:tick` row is ≤ 60 s old in the workspace store — the store persists, the trace does not) |
| Live follow = replay | `trace show --run 47c724e2-b8cf-4d23-a771-4a51be54c056 --after 0 --follow`, started 4 s into run #2: 29 lines, byte-identical (`jq -c`) to the replay after the stop |

## Reconstruction checks (all 5 runs)

| Check | Result |
|---|---|
| Order | `seq` = 1 … n with no gap; `event_id` = `<run_id>:<seq>` on every event |
| Tree | every `parent_event_id` names an earlier event of the same run (90 of 116 events have a parent; the 26 roots: `run.opened`, `runtime.starting / running / stopping`, `feed.fired`, the `trigger.decide / map` root) |
| Stable replay | `trace show` twice = same bytes; `--after 5` = the replay's tail from seq 6 |
| Close | each decide ends `trigger.completed`, each run `runtime.stopped` |
| No secret | `rg -F` of the `OPENROUTER_API_KEY` value (never printed): 0 matches in `$HOME/tengu-lab/home/logs/trace` (5 files), 0 anywhere under the lab home |
| Nothing dangerous | `$HOME/tengu-lab/control-loop-lab/out` = `marker.txt` only · no `egress.jsonl`, no `risk.jsonl` · `~/.tengu` unchanged |

## Findings

| # | Finding | Effect |
|---|---|---|
| 1 | The first tick of a fresh runtime reads `world.tick` **missing**: `feed.tick_sent` (and the health row) follow the submit | true to the code; later ticks read `ok` |
| 2 | A decide run's `run.opened` names `trigger:decide` also for a map run; the root `trigger.map` names `trigger:map/<sha256>` | cosmetic; Studio keys on `trigger.*`. Fixed after this run (review of ST-10..ST-12): a decide's `run.opened` names no node (`trace_store::tests::run_opened_node_by_kind`); the tables above show the build that ran |
| 3 | `tool.*` `duration_ms` 0 for `read_file` / `write_file` (sub-millisecond); `runtime.stopped` drain 0 ms (SIGINT while idle) | real values |

## Cleanup

The runbook's guarded block ran after the extracts (`TENGU_HOME` = the lab home): `out/marker.txt`, the workspace `observations.db*`, `$HOME/tengu-lab/home/{logs,state}` removed. Left: the empty lab dirs. Raw outputs stayed in the session scratchpad, not committed.

## ST-20 — `tengu studio` attached to the lab (2026-10-09)

Read-only server (`--features studio`) on a debug build of the ST-20 tree (parent `1d8223fd4ed2c0384c20db27bee45625fb71c636`), same lab home, 2026-10-09 01:24:35Z → 01:25:58Z. One Studio process for the whole session: before any runtime, `tengu run` #1 (45 s, SIGINT), `tengu run` #2 (25 s, SIGINT). Token: 64 hex, URL fragment only (not recorded here). Runbook rows S1–S4: `docs/control-loop-lab-2026-10-08.md`.

| Check | Result |
|---|---|
| Before a runtime | `/api/v1/meta`: `read_only: true`, `control_enabled: false`, `config_hash` `a68882899f1aacfe0dde95eb5c6c1a9490750886c5af79b11255437044b4b40f` · `/api/v1/graph` 16 nodes, 17 edges (= the ST-10 golden) · `/api/v1/health` `live: false`, heartbeat `missing` · `/api/v1/runs` empty · live stream `event: run` `waiting` |
| Guards (curl) | no token 401 · `Host: evil.example` 421 · `Origin: http://evil.example` 403 · `POST` 405 · `?token=` 200 · page headers: CSP `default-src 'self'` … `frame-ancestors 'none'`, `no-store`, `DENY`, `nosniff`, `no-referrer` · `--bind 0.0.0.0` refused, exit 1 |
| Attach (run #1) | live stream `attached`: run `77ed9381-60eb-4c8d-8bbd-c6f4d6594ba0`, runtime `Vladimirs-MacBook-Pro-2.local:53048:946975cb-d103-4b62-aed4-23e84fe775c9` · `/api/v1/health` at 45 s = `tengu doctor --live` at the same time: `live: true`, heartbeat ok, `loop demo` done 3, `feed tick` live, `feed probe` down (fatal, optional) — same 4 checks, same verdict |
| Stop / restart | after SIGINT: health `live: false`, heartbeat `stopped` (SIGINT) · run #2 → `event: run` `restarted`: run `ec4b4026-e2f8-4d32-b8ea-6acae2a9d021`, runtime `Vladimirs-MacBook-Pro-2.local:53617:003eeadf-60fe-4cbf-9785-4516e22dbeba`; `/api/v1/runs` `live_run_id` follows it |
| Live stream | 67 `trace` events = run #1 seq 1…38 then run #2 seq 1…29, no gap, no repeat; both runs end `runtime.stopped` (`lease_released: true`) |
| Replay = CLI | `/api/v1/runs/77ed9381-60eb-4c8d-8bbd-c6f4d6594ba0/events?limit=1000` = `tengu trace show` of the run, event for event (38, `more: false`) · `runs/…/stream` with `Last-Event-ID: 77ed9381-60eb-4c8d-8bbd-c6f4d6594ba0:5` starts at `:6` (33 events) |
| Jev | 5 real calls, `typesafe/jev-1.13-20260917`, 263–595 ms, cost $0.000136038; every `jev.completed` keeps `legal_actions` `hold, read_probe, write_marker` |
| Nothing dangerous | `~/.tengu/logs/decisions.jsonl` sha256 equal before and after · key value: 0 matches in the outputs and the lab home · no tengu process left · lab cleaned with the runbook block |

## ST-21 / ST-22 — the page over real runs (2026-10-09)

Debug build of the ST-21 / ST-22 tree with `--features studio` (parent `77bf750a4a53e3113a9bb53cf37aae46c6a93af6`), lab home, 2026-10-09 02:07:45Z → 02:17:23Z. One Studio process; the page rendered by headless Chrome (`--headless=new`, a temp profile, no extension, no sync, no proxy, loopback only; screenshots + `--dump-dom` kept in the session scratchpad, not committed). The Claude-in-Chrome extension was not connected, so no interactive session. Runbook rows S3, S5–S7: `docs/control-loop-lab-2026-10-08.md`.

| Run | `run_id` | Kind · events | Board (Rust) = what the page drew |
|---|---|---|---|
| decide act (A2) | `0dec1264-6d71-40d6-bf20-d7593b819d6e` | decide · 12 | Jev `write_marker` 0.86 then `hold` 1.0; `action:demo/write_marker`, `tool:lab/write_file`, `action:demo/hold`, `jev:demo` green, `world:demo/tick` amber (missing: no runtime); edges `jev:demo → action:demo/write_marker`, `action:demo/write_marker → tool:lab/write_file`, `jev:demo → action:demo/hold` lit green |
| decide `uncertain.map.json` (A4) | `996e416c-f7e1-408f-8d1a-c2e197b54b62` | decide · 6 | drawn on the kept map's graph (17 nodes, `trigger:map/1a97d5793e4abfd1a53fb717c34b1a9fbfa1dc49b7ef6dd4ec165813cac5a1ef` green, `act_at 1`, `write_marker` effect `logged`); `gate:demo/act_at` amber (`action.escalated`, `hold` 0.92 < 1.0), edge `jev:demo → action:demo/hold` amber; the inspector shows the narrowed loop (`act_at` 1, `dry_run` true, "narrowed by ExecutionMap::apply") — on a re-check of this run restored from its `/events` dump; the first capture showed the base section (the node was read before the map graph loaded: fixed, the page re-reads the selected node when the drawn graph changes) |
| `tengu run` #1 | `95ff067e-f446-410e-ab2c-7cd42af74cf9` | run · 73 (holder `Vladimirs-MacBook-Pro-2.local:80016:12536bb9-ab9e-45c4-aafb-0f71a39681fa`) | live at 99 s: state `live`, runtime `running`; `feed:tick`, `loop:demo`, `world:demo/tick`, `jev:demo`, `action:demo/hold` green, `feed:probe` + `tool:lab/read_file` red, edges `feed:probe → tool:lab/read_file` red, `jev:demo → action:demo/hold` green; grey: none (legal set `hold, read_probe, write_marker`) |
| `tengu run` #2 (restart) | `a0fd15c8-1779-4398-a691-ef4b4196d8eb` | run · 25 (holder `Vladimirs-MacBook-Pro-2.local:81184:757e93dd-b59f-4461-8370-a9c94ff0f449`) | its own board: `runtime_id` = its holder, loop counters from 0 (`completed` 2), `runtime.stopped` |

| Check | Result |
|---|---|
| Page = audit = health (run #1, at 99 s) | board `loop demo` `accepted 6 · completed 6 · failed 0 · queued 0` = heartbeat `loops.demo` (`accepted 6`, `completed 6`) = `/api/v1/health` and `tengu doctor --live` (`done 6`, `=> live`, exit 0) = 6 `decisions.jsonl` lines with this `run_id` = 6 `jev.completed` + 6 `loop.completed` in the timeline; `feed tick` `items 6` |
| Replay = live = CLI | `/events` in pages of 7 = one page of 1000 (73 events, same order, views included); without `view` = `tengu trace show` of the run, event for event (`jq -S`: the build that ran sorted the keys; after this run each page event keeps the trace line's field order, `{"schema_version":1,"event_id":…` — `reload_returns_identical_order`) |
| Reload | `/board` of run #1 read three times (before and after six page loads): byte-identical |
| Replay position | `board?upto=7` (the first `tool.started`): `tool:lab/read_file`, `feed:probe`, `feed:tick`, `loop:demo`, runtime amber, edge `feed:probe → tool:lab/read_file` amber + latest, counters `queued 1`; model `—` (no Jev call yet) |
| Runs not mixed | `/runs`: 4 runs, each `closed` with its own `runtime_id` (decides: none); each board's marks name only its own `event_id`s |
| Inspector | `/api/v1/nodes/loop:demo`: section `decision_loops.demo`, `timeout_secs` 20 (a default), evidence heartbeat + decision audit + `loop/1:demo` row; `feed:tick`: `feed/1:tick` in `~/tengu-lab/control-loop-lab/.tengu/observations.db` |
| Jev | 11 real calls, `typesafe/jev-1.13-20260917`, 265–756 ms, cost $0.000293286 |
| Phone | the layout reflows to one column at 500 px (headless Chrome's smallest window); every grid is `minmax(0, 1fr)`, ids wrap, never cut |
| Nothing dangerous | `~/.tengu/logs/decisions.jsonl` sha256 equal before and after · key value: 0 matches in the outputs and the lab home · no tengu or Chrome process left · lab cleaned with the runbook block |
