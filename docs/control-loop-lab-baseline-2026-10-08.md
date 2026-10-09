# Control-loop lab — baseline real-Jev run (2026-10-08)

ST-03 evidence (TENGU_STUDIO_PLAN.md § 8) for `sandboxes/control-loop-lab`, run row by row against the matrix in `docs/control-loop-lab-2026-10-08.md`. **Verdict: A0–A15 all pass.** Every A1–A4 try hit (3 of 3 each). The run corrected nine details of the runbook (§ Corrections).

## Setup

| What | Value |
|---|---|
| Code | `feature/studio` at `02c13268b2961086315c9b2c53d45aaffebe1602` (sandbox + runbook of `52cad0cf2badb4ec9a11457e862775dc242575a6`) |
| Binary | `cargo build` (dev profile, default features) → `$CARGO_TARGET_DIR/debug/tengu`. The runbook's `cargo build --release` builds the same code with the same features; only optimisation differs |
| Host | macOS aarch64 (Darwin 25.6.0), cwd = the worktree root |
| Env (names only) | `TENGU_HOME=$HOME/tengu-lab/home` exported first · `OPENROUTER_API_KEY` exported in the same shell from the main checkout's `.env`, never printed · `RUST_LOG` not exported (the parent `.env` sets it) |
| Window | 2026-10-08 19:16:28Z (A0) → 19:21:04Z (A14) |
| Jev | requested `~typesafe/jev-latest`; every call returned build `typesafe/jev-1.13-20260917` |
| Spend | 32 Jev calls · $0.000799134 · 19 027 in / 1 319 out tokens · latency 267–679 ms (median 344, mean 377) |
| Isolation | `~/.tengu/logs/decisions.jsonl` sha256 `7deaacb257f43d3dc3e622f6d08adddad72e24f7cc812f7722a98ec25e0039d5` (151 lines) before and after · `control-loop-lab` appears in none of `~/.tengu/logs/{decisions,egress}.jsonl` or `tengu.log` · no `run-control-loop-lab.json` and no lab map under `~/.tengu` · key: `rg -l -F --hidden --no-ignore "$OPENROUTER_API_KEY"` over the worktree, `~/tengu-lab` and the scratch evidence finds no file (exit 1). `rg 'sk-or'` finds only the existing placeholder fixtures (`sk-or-v1-0123456789abcdef`, `sk-or-...`, `sk-or-1`) |

## Matrix result

| # | Result | Observed |
|---|---|---|
| A0 | ✅ | exit 0 · `lab: engine=openrouter model=anthropic/claude-haiku-4-5 endpoint=https://openrouter.ai/api transport=http-json context=1000000 output_cap=16384 streaming=false` · `network: open` · `proxy: none (direct)` · `llm api: direct` |
| A1 | ✅ 3/3 | `[{"action":"hold","outcome":"stopped"}]`, `history [{"action":"hold","t":1}]`, `$W/out` empty |
| A2 | ✅ 3/3 | `write_marker` executed (`ok: true`, `File 'out/marker.txt' written (12 bytes)`) → `hold`; `marker.txt` = `scenario=act` |
| A3 | ✅ 3/3 | `read_probe` executed, `ok: false`, `Cannot read file 'in/absent.txt': No such file or directory (os error 2)` → `hold`; exit 0 |
| A4 | ✅ 3/3 | `escalated`, `action hold`, confidence 0.95 / 0.94 / 0.94 · `history []` · map `1a97d5793e4abfd1a53fb717c34b1a9fbfa1dc49b7ef6dd4ec165813cac5a1ef` · audit `trigger` = `map:` + that sha256, `act_at 1.0`, `t 0` |
| A5 | ✅ 1/1 | `dry_run` `write_marker` → `hold` · `history[0].result "dry_run"`, no `ok`, no `call_id` · `marker.txt` mtime and size unchanged · map `ed5ed6a09388130bcfa59f5b95d8a8ca8b75f1e462742b168b11e413c61e682d` |
| A6 | ✅ | 20 lines after A1–A5 = 3·1 + 3·2 + 3·2 + 3·1 + 2 · tool lines `call_id demo:decide-demo-<uuid>:1` |
| A7 | ✅ | leases taken → `feed started` probe, tick → `tengu run started loops=["demo"] feeds=["probe", "tick"]` → `tick:<slot ms>` step every 20 s → probe `feed calls failed … class="fatal"` every 30 s |
| A8 | ✅ | 19:18:59Z: up 75 s, `done 4`, tick `items 4` · 19:19:30Z: up 106 s, `done 6`, `items 6` · both `=> live`, exit 0 |
| A9 | ✅ | `state running` · `completed 6` · tick `live` · probe `down`, `fatal` · `heartbeat_secs 2` |
| A10 | ✅ | refusal names the A7 holder, `for 26 s more` · exit 1 · heartbeat unchanged |
| A11 | ✅ | SIGINT 19:19:52.759Z → `stopping` +1 ms → `stopped … aborted=0 … lease_released=true` +3 ms · exit 0 · heartbeat `stopped`, `SIGINT` · doctor `FAIL heartbeat stopped 6 s ago: SIGINT`, `=> NOT live`, exit 1 |
| A12 | ✅ | new holder, later `started_at_ms`, counters from 0; tick audit times and `t` split cleanly (§ Runtime ids) |
| A13 | ✅ | `$W/out` = `marker.txt` · `$W` = `.tengu/ in/ out/` · `no-egress` (no `egress.jsonl` at all) · `$TENGU_HOME/logs` = `decisions.jsonl maps tengu.log` (no `risk.jsonl`) |
| A14 | ✅ | as A11: `accepted=4 completed=4 failed=0 dropped=0`, `lease_released=true`, exit 0 · doctor `FAIL heartbeat stopped 0 s ago: SIGINT`, exit 1 |
| A15 | ✅ | guarded block ran (`TENGU_HOME` matched); `$W/.tengu/observations.db` was left behind → the block now removes it too |
| extra | ✅ | `decide` (normal) while run 1 was up: exit 0, `hold`, session `decide-demo-2e1fa740-8de8-4c61-8d99-a67a2ca45e28`; runtime counters untouched; 609 input tokens (it saw `world.tick`) |

## Jev hit rate

| Scenario | Tries | Expected | Hits | Confidence of the pick | Calls | Latency ms | Cost USD |
|---|---:|---|---:|---|---:|---|---:|
| normal (`decide`) | 3 (+1 during run 1) | hold | 4/4 | 1.0 ×4 | 4 | 319–657 | 0.000092610 |
| act | 3 | write_marker → hold | 3/3 | 0.92 / 0.94 / 0.92 → 0.99 / 1.0 / 1.0 | 6 | 280–428 | 0.000143010 |
| tool-error | 3 | read_probe → hold | 3/3 | 0.98 / 0.99 / 0.98 → 0.91 / 0.92 / 0.92 | 6 | 307–679 | 0.000142884 |
| uncertain (map, `act_at 1.0`) | 3 | escalated | 3/3 | hold 0.95 / 0.94 / 0.94 | 3 | 303–380 | 0.000067284 |
| act-dry (map) | 1 | dry_run → hold | 1/1 | 0.92 → 0.95 | 2 | 386–433 | 0.000046914 |
| tick (`tengu run`) | 11 | hold | 11/11 | 1.0 ×11 | 11 | 267–455 | 0.000306432 |

## Audit excerpts (`$TENGU_HOME/logs/decisions.jsonl`, all 32 lines)

Every line: `sandbox control-loop-lab`, `loop demo`, `model typesafe/jev-1.13-20260917`. `call_id` is set only on the 6 tool steps (`demo:<session_id>:1`).

| session_id | t | decision_id | pick (conf) | outcome | ok | ms | cost |
|---|---:|---|---|---|---|---:|---:|
| decide-demo-8be58ce7-b40e-48f7-8445-b26417a21cc2 | 1 | gen-dec-1791487005-CHowWIz8hKrJRzVYfmyw | hold (1.0) | stopped | — | 476 | 0.000022344 |
| decide-demo-ff2422db-2dfd-4ab0-8443-0e8eecc8c8ec | 1 | gen-dec-1791487005-GTFBhwL52HRXEwCP9K1H | hold (1.0) | stopped | — | 319 | 0.000022344 |
| decide-demo-2bd1581f-19c2-4d57-b74c-2d604920c8e6 | 1 | gen-dec-1791487006-FMrHjPDF67siv17F2T98 | hold (1.0) | stopped | — | 657 | 0.000022344 |
| decide-demo-d7f4f119-7a57-4882-bb7e-b78f4f9f2ba4 | 1 | gen-dec-1791487018-lXUXn1COTaCnvdkv0mC5 | write_marker (0.92) | executed | true | 428 | 0.000022344 |
| decide-demo-d7f4f119-7a57-4882-bb7e-b78f4f9f2ba4 | 2 | gen-dec-1791487018-6UJ9TmuIjR2G3lomBCzD | hold (0.99) | stopped | — | 280 | 0.000025326 |
| decide-demo-df9de90c-b00c-4248-9de3-9848a1cc21b8 | 1 | gen-dec-1791487019-vyshmuBY2aStSOMqMma8 | write_marker (0.94) | executed | true | 315 | 0.000022344 |
| decide-demo-df9de90c-b00c-4248-9de3-9848a1cc21b8 | 2 | gen-dec-1791487019-lIiGlEb7zuUYf16YTlVI | hold (1.0) | stopped | — | 367 | 0.000025326 |
| decide-demo-1fe407c7-b345-4c22-9f28-ac9148617652 | 1 | gen-dec-1791487019-8sbT0OjKJk17lwOa26jF | write_marker (0.92) | executed | true | 355 | 0.000022344 |
| decide-demo-1fe407c7-b345-4c22-9f28-ac9148617652 | 2 | gen-dec-1791487020-K5DhpeJHHUnxKe5uOUZR | hold (1.0) | stopped | — | 318 | 0.000025326 |
| decide-demo-3e9eb7b0-208d-438f-b047-4dba88c7e083 | 1 | gen-dec-1791487025-d0xVIZjseiUkiC2hidAD | read_probe (0.98) | executed | false | 435 | 0.000022386 |
| decide-demo-3e9eb7b0-208d-438f-b047-4dba88c7e083 | 2 | gen-dec-1791487025-KA3pRmOZACcGruOZv7eo | hold (0.91) | stopped | — | 336 | 0.000025242 |
| decide-demo-9e4050a6-3f9e-41aa-bbb6-6bc7cfe8ba87 | 1 | gen-dec-1791487025-F40FGGrZUSI98e7mdjut | read_probe (0.99) | executed | false | 344 | 0.000022386 |
| decide-demo-9e4050a6-3f9e-41aa-bbb6-6bc7cfe8ba87 | 2 | gen-dec-1791487026-Y9k2gBcKHdjq1r1c7UZq | hold (0.92) | stopped | — | 307 | 0.000025242 |
| decide-demo-c033f3ef-b5cf-4620-bb8d-a86a5eefd3cf | 1 | gen-dec-1791487026-BQCNijenY8c7I2rksrXo | read_probe (0.98) | executed | false | 679 | 0.000022386 |
| decide-demo-c033f3ef-b5cf-4620-bb8d-a86a5eefd3cf | 2 | gen-dec-1791487027-os55wR9UsJ0vA9zY6NPV | hold (0.92) | stopped | — | 368 | 0.000025242 |
| decide-demo-04b4b679-6c88-4011-a2f6-18d793d87cfc | 0 | gen-dec-1791487033-UR2HaTbgTe6YCQELsGSW | hold (0.95) | escalated | — | 305 | 0.000022428 |
| decide-demo-c26946fe-412f-4546-a726-a396d24ea76b | 0 | gen-dec-1791487033-qZAqcXufM7RQlGF7JTyS | hold (0.94) | escalated | — | 303 | 0.000022428 |
| decide-demo-7e0bec10-d6de-45d6-b8b5-a65b8efa1799 | 0 | gen-dec-1791487034-yjvN7PYWTkh7Pmxe9Sfv | hold (0.94) | escalated | — | 380 | 0.000022428 |
| decide-demo-0549801e-c85d-4b2a-bb61-3c57212c8321 | 1 | gen-dec-1791487040-Lh1VBhq4OQoOYs4VtJeo | write_marker (0.92) | dry_run | — | 433 | 0.000022344 |
| decide-demo-0549801e-c85d-4b2a-bb61-3c57212c8321 | 2 | gen-dec-1791487041-gD2GqOFfKalxL8cMtf0s | hold (0.95) | stopped | — | 386 | 0.000024570 |
| tick:1791487064049 | 1 | gen-dec-1791487064-tBIuODAXue7uT736RrIf | hold (1.0) | stopped | — | 341 | 0.000023478 |
| tick:1791487080000 | 2 | gen-dec-1791487080-xZV6dutZ2cRCI1dCiMNb | hold (1.0) | stopped | — | 358 | 0.000027342 |
| tick:1791487100000 | 3 | gen-dec-1791487100-0VAtfhV2YBJiRRLJav44 | hold (1.0) | stopped | — | 313 | 0.000028014 |
| tick:1791487120000 | 4 | gen-dec-1791487120-5e1NSPMEvoch0b99H944 | hold (1.0) | stopped | — | 310 | 0.000028686 |
| tick:1791487140000 | 5 | gen-dec-1791487140-SefHJOdJxAgZEKtQXNeZ | hold (1.0) | stopped | — | 455 | 0.000029358 |
| tick:1791487160000 | 6 | gen-dec-1791487160-zZZJoJwOJ9rkJCLGepfD | hold (1.0) | stopped | — | 338 | 0.000029358 |
| decide-demo-2e1fa740-8de8-4c61-8d99-a67a2ca45e28 | 1 | gen-dec-1791487185-HTngOaqkQYjAcksNYKc0 | hold (1.0) | stopped | — | 589 | 0.000025578 |
| tick:1791487180000 | 7 | gen-dec-1791487180-stLXQ0XqYhrk7QBqmatx | hold (1.0) | stopped | — | 270 | 0.000029358 |
| tick:1791487205025 | 1 | gen-dec-1791487205-VNqwJged3Dxq9MUbJKCY | hold (1.0) | stopped | — | 398 | 0.000026796 |
| tick:1791487220000 | 2 | gen-dec-1791487220-li2ETaBKnjIxRB6htQcV | hold (1.0) | stopped | — | 267 | 0.000027342 |
| tick:1791487240000 | 3 | gen-dec-1791487240-jQO6OLtCQEFt5cQ6I7xK | hold (1.0) | stopped | — | 302 | 0.000028014 |
| tick:1791487260000 | 4 | gen-dec-1791487260-p7nFwmBDEXJEHvAI2cmH | hold (1.0) | stopped | — | 321 | 0.000028686 |

Escalated line (A4 try 1), verbatim:

```json
{"act_at":1.0,"answers":{"next_action":{"choice":"hold","confidence":0.95,"probabilities":{"hold":0.97,"read_probe":0.03,"write_marker":0.0},"type":"choice"}},"args":null,"call_id":null,"decision_id":"gen-dec-1791487033-UR2HaTbgTe6YCQELsGSW","latency_ms":305,"loop":"demo","model":"typesafe/jev-1.13-20260917","obs":null,"ok":null,"output":null,"result":{"action":"hold","confidence":0.95,"outcome":"escalated"},"sandbox":"control-loop-lab","session_id":"decide-demo-04b4b679-6c88-4011-a2f6-18d793d87cfc","t":0,"trigger":"map:1a97d5793e4abfd1a53fb717c34b1a9fbfa1dc49b7ef6dd4ec165813cac5a1ef","ts":1791487033,"ts_ms":1791487033526,"usage":{"cost":0.000022428,"input_tokens":534,"output_tokens":41}}
```

## Health before / during / after

| When | `doctor --live` | Heartbeat line | Loop / feeds |
|---|---|---|---|
| before (19:17:39Z) | exit 1 · `=> NOT live` | `FAIL heartbeat missing: no run-control-loop-lab.json — is \`tengu run --sandbox control-loop-lab\` running?` | — (no `state/` dir yet) |
| run 1, 19:18:59Z | exit 0 · `=> live` | `ok heartbeat running · up 75 s · last beat 1 s ago (limit 10 s) · pid 74815 · holder Vladimirs-MacBook-Pro-2.local:74815:83519fb5-3839-45a3-9724-78322e317268` | `done 4` · probe `down`, `no item in 75 s since start (stale after 90 s)`, `last error fatal: Cannot read file 'in/absent.txt': …` · tick `live · items 4 · last item 19 s ago` |
| run 1, 19:19:30Z | exit 0 · `=> live` | `ok heartbeat running · up 106 s · last beat 0 s ago` | `done 6` · probe still `ok` at 106 s > 90 s (optional) · tick `items 6` |
| after run 1 | exit 1 · `=> NOT live` | `FAIL heartbeat stopped 6 s ago: SIGINT · pid 74815 · holder Vladimirs-MacBook-Pro-2.local:74815:83519fb5-3839-45a3-9724-78322e317268` | rows frozen at last values: `ok loop demo … done 7`, `ok feed tick … live · items 7` |
| run 2, 19:20:57Z | exit 0 · `=> live` | `ok heartbeat running · up 52 s · last beat 0 s ago · pid 76105 · holder Vladimirs-MacBook-Pro-2.local:76105:f9ecbbab-7cea-4648-8025-cd26aefb5801` | `done 3` · tick `items 3` |
| after run 2 | exit 1 · `=> NOT live` | `FAIL heartbeat stopped 0 s ago: SIGINT · pid 76105 · holder Vladimirs-MacBook-Pro-2.local:76105:f9ecbbab-7cea-4648-8025-cd26aefb5801` | — |

## Runtime ids (holder = `runtime_id` until ST-11)

| Run | holder | pid | started_at_ms | stopped ts_ms | accepted / completed / failed | tick audit `ts_ms` | `t` |
|---|---|---:|---|---|---|---|---|
| 1 | `Vladimirs-MacBook-Pro-2.local:74815:83519fb5-3839-45a3-9724-78322e317268` | 74815 | 1791487064041 | 1791487192761 | 7 / 7 / 0 | 1791487064391 … 1791487180275 (≤ stop) | 1–7 |
| 2 | `Vladimirs-MacBook-Pro-2.local:76105:f9ecbbab-7cea-4648-8025-cd26aefb5801` | 76105 | 1791487205019 | 1791487264591 | 4 / 4 / 0 | 1791487205424 … 1791487260325 (≥ start) | 1–4 |

## Transcript (sanitized: `$HOME` for the home path, env values never shown)

```text
$ echo '{"scenario":"act"}' | $T decide --sandbox control-loop-lab --loop demo --event - | J | jq -c .     # A2 try 1
{"audit":"$HOME/tengu-lab/home/logs/decisions.jsonl","history":[{"action":"write_marker","args":{"content":"scenario=act","path":"out/marker.txt"},"ok":true,"result":"File 'out/marker.txt' written (12 bytes)","t":1},{"action":"hold","t":2}],"outcomes":[{"action":"write_marker","outcome":"executed"},{"action":"hold","outcome":"stopped"}],"session_id":"decide-demo-d7f4f119-7a57-4882-bb7e-b78f4f9f2ba4"}

$ $T decide --sandbox control-loop-lab --loop demo --event $S/scenarios/tool-error.json | J | jq -c .   # A3 try 1
{"audit":"$HOME/tengu-lab/home/logs/decisions.jsonl","history":[{"action":"read_probe","args":{"path":"in/absent.txt"},"ok":false,"result":"Cannot read file 'in/absent.txt': No such file or directory (os error 2)","t":1},{"action":"hold","t":2}],"outcomes":[{"action":"read_probe","outcome":"executed"},{"action":"hold","outcome":"stopped"}],"session_id":"decide-demo-3e9eb7b0-208d-438f-b047-4dba88c7e083"}

$ $T decide --sandbox control-loop-lab --map $S/scenarios/uncertain.map.json | J | jq -c .            # A4 try 1
{"audit":"$HOME/tengu-lab/home/logs/decisions.jsonl","history":[],"map":{"path":"$HOME/tengu-lab/home/logs/maps/1a97d5793e4abfd1a53fb717c34b1a9fbfa1dc49b7ef6dd4ec165813cac5a1ef.json","sha256":"1a97d5793e4abfd1a53fb717c34b1a9fbfa1dc49b7ef6dd4ec165813cac5a1ef"},"outcomes":[{"action":"hold","confidence":0.95,"outcome":"escalated"}],"session_id":"decide-demo-04b4b679-6c88-4011-a2f6-18d793d87cfc"}

$ $T decide --sandbox control-loop-lab --map $S/scenarios/act-dry.map.json | J | jq -c .              # A5
{"audit":"$HOME/tengu-lab/home/logs/decisions.jsonl","history":[{"action":"write_marker","args":{"content":"scenario=act","path":"out/marker.txt"},"result":"dry_run","t":1},{"action":"hold","t":2}],"map":{"path":"$HOME/tengu-lab/home/logs/maps/ed5ed6a09388130bcfa59f5b95d8a8ca8b75f1e462742b168b11e413c61e682d.json","sha256":"ed5ed6a09388130bcfa59f5b95d8a8ca8b75f1e462742b168b11e413c61e682d"},"outcomes":[{"action":"write_marker","outcome":"dry_run"},{"action":"hold","outcome":"stopped"}],"session_id":"decide-demo-0549801e-c85d-4b2a-bb61-3c57212c8321"}

$ $T run --sandbox control-loop-lab          # A7 (stderr, run 1; metrics lines omitted)
19:17:44.044015Z INFO runtime leases taken sandbox=control-loop-lab leases=["runtime:control-loop-lab"] holder=Vladimirs-MacBook-Pro-2.local:74815:83519fb5-3839-45a3-9724-78322e317268 state_dir=$HOME/tengu-lab/home/state
19:17:44.048971Z INFO feed started feed=probe kind=tool every_secs=Some(30) windows=0 at=[] tz=UTC required=false
19:17:44.049069Z INFO feed started feed=tick kind=tick every_secs=Some(20) windows=0 at=[] tz=UTC required=true
19:17:44.049119Z INFO webhooks off (built without --features webhooks)
19:17:44.049240Z INFO tengu run started sandbox=control-loop-lab holder=Vladimirs-MacBook-Pro-2.local:74815:83519fb5-3839-45a3-9724-78322e317268 state_dir=$HOME/tengu-lab/home/state loops=["demo"] feeds=["probe", "tick"]
19:17:44.049709Z WARN feed calls failed (not retried; next slot tries again) feed=probe slot_ms=1791487064049 failed=1 calls=1 class="fatal" error=Cannot read file 'in/absent.txt': No such file or directory (os error 2)
19:17:44.392207Z INFO decision loop step decision_loop=demo session_id=tick:1791487064049 step=0 outcome=Stopped { action: "hold" }
19:18:00.363568Z INFO decision loop step decision_loop=demo session_id=tick:1791487080000 step=0 outcome=Stopped { action: "hold" }
… every 20 s: tick hold · every 30 s: probe fatal …

$ $T run --sandbox control-loop-lab; echo exit=$?          # A10
Error: sandbox `control-loop-lab` is already running: lease `runtime:control-loop-lab` in $HOME/tengu-lab/home/state/runtime.db is held by `Vladimirs-MacBook-Pro-2.local:74815:83519fb5-3839-45a3-9724-78322e317268` for 26 s more. Stop that `tengu run` / `tengu webhooks` first (SIGTERM drains it); if it crashed, retry once the lease expires.
exit=1

$ kill -INT 74815                              # A11 (= Ctrl-C)
19:19:52.760423Z INFO tengu run stopping reason=SIGINT failed=false
19:19:52.763193Z INFO loop totals decision_loop=demo accepted=7 completed=7 failed=0 dropped=0
19:19:52.763218Z INFO tengu run stopped finished=0 dropped=0 aborted=0 aborted_tasks=[] lease_released=true
(process exit 0)
```

## ST-10 / ST-11 smoke (2026-10-08 19:56–19:57Z, debug build of `feature/studio`, same lab home)

| Check | Result |
|---|---|
| G1 `studio graph` | `config_hash` `a68882899f1aacfe0dde95eb5c6c1a9490750886c5af79b11255437044b4b40f` · 16 nodes · 17 edges (= golden) · stderr 4 log lines, stdout JSON only |
| G2 `--map uncertain.map.json` | `map.sha256` `1a97d5793e4abfd1a53fb717c34b1a9fbfa1dc49b7ef6dd4ec165813cac5a1ef` · `act_at` 0.8 → 1.0 · `dry_run` false → true · `{"loop":"demo","act_at":0.5}` → exit 1 `execution map.act_at = 0.5 must be in [0.8, 1] — a map may only raise it` |
| A2 `decide` (real Jev) | stdout parsed by `jq` with no filter · `write_marker` executed, `hold` · model `typesafe/jev-1.13-20260917` · `run_id` `0c6f502e-c8db-48f4-bf4f-9843c0836fc1` on both audit lines, `runtime_id` null · `trace` = `$HOME/tengu-lab/home/logs/trace/control-loop-lab/0c6f502e-c8db-48f4-bf4f-9843c0836fc1.jsonl` |
| A7 `run` 26 s + SIGINT | holder `Vladimirs-MacBook-Pro-2.local:96248:d657d420-589f-40a7-8f35-0891e6b16e79` · log `trace recording run_id="648f7ea2-4a6f-4c6d-a920-8cbe6922b3da"` · `tengu run started … run_id="648f7ea2-4a6f-4c6d-a920-8cbe6922b3da"` · 2 tick lines (`tick:1791489426965`, `tick:1791489440000`) carry that holder + run id · `doctor --live` exit 0 `=> live` (report on stdout, 4 log lines on stderr) · stopped `lease_released=true`, exit 0 |
| G3 `trace runs` | `0c6f502e-c8db-48f4-bf4f-9843c0836fc1` `kind` `decide` · `648f7ea2-4a6f-4c6d-a920-8cbe6922b3da` `kind` `run`, `runtime_id` = the holder · 1 event each (`run.opened`; ST-12 adds the rest) |
| G4 refusal | `trace show --run ../../etc` → exit 1, "not a run id (a lowercase UUID …)" · `trace runs` without `--sandbox` → exit 1, "--sandbox <name> is required" |

## Corrections (the run vs the ST-02 runbook)

| # | Finding | Evidence | Change |
|---|---|---|---|
| 1 | "A worktree has no `.env`" is wrong: dotenvy walks up from the cwd, so a worktree under `.claude/worktrees/` loads the main checkout's `.env` (`TENGU_HOME=~/.tengu`, `RUST_LOG`, keys) | `dotenvy-0.15.7/src/find.rs` · `adapters/inbound/cli/mod.rs:368` | runbook setup rows + troubleshooting row, `config.toml` quickstart comment: export `TENGU_HOME` first |
| 2 | `decide` / `doctor` print `tengu=info` log lines on stdout before their output; `RUST_LOG=warn` does not silence them (`tengu=info` always added) | `cli/mod.rs:524-531` (default subscriber → stdout) · `RUST_LOG=warn … doctor` still prints INFO | runbook `J()` helper (`sed -n '/^{/,$p'`) + troubleshooting row. **Fixed in ST-11**: `cli/mod.rs::stdout_is_data` sends `decide` / `doctor` / `studio` logs to stderr (test `data_commands_log_to_stderr`; § ST-10 / ST-11 smoke: `decide` stdout parses with `jq` directly) |
| 3 | The first log lines show a Tor policy (`socks5h://127.0.0.1:9050`) from the base defaults, then `--sandbox` installs `direct` | every A0–A14 log head | troubleshooting row (harmless) |
| 4 | An escalated step's audit line has `t 0`; Jev held "uncertain" at 0.94–0.95 — only the map's `act_at 1.0` escalates it | A4 lines | matrix A4 |
| 5 | doctor lists feeds in name order (probe first); an optional feed stays `ok` past its stale limit | A8 | matrix A8 |
| 6 | After a stop, the loop / feed rows keep their last values; only the heartbeat row fails | A11 doctor | matrix A11 |
| 7 | A tick line's `t` is the loop's step count in that process: 1–7 in run 1, restarts at 1 in run 2 | § Runtime ids | matrix A12 |
| 8 | `decide` during a run reads `world.tick` from the shared observation store (609 vs 532 input tokens); "missing under decide" holds only with no runtime up | extra row | runbook note under the matrix, `config.toml` `world` comment |
| 9 | Cleanup left `$W/.tengu/observations.db` (`loop/1:demo`, `feed/1:tick` rows of the last run) | `find ~/tengu-lab` after A15 | cleanup block removes it |

Unchanged and confirmed: the sandbox TOML values, every scenario file, the 2-of-3 rule, A6 per-try counts, the lease refusal, graceful drain, no egress, no `risk.jsonl`. A15 removed the lab state. Raw copies were kept only in the session scratchpad and were not committed. This doc is the record.
