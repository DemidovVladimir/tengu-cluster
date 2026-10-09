# Control-loop lab — runbook + acceptance matrix (2026-10-08)

`sandboxes/control-loop-lab`: the safe reference run of tick → Jev → tool → audit → health (TENGU_STUDIO_PLAN.md ST-01 / ST-02). Existing CLI and tools only — no new tool, no Studio. Guard test: `config::decision_loop::tests::control_loop_lab_has_no_dangerous_surface`. Live proof: ST-03 `docs/control-loop-lab-baseline-2026-10-08.md` — A0–A15 passed on real Jev (`typesafe/jev-1.13-20260917`, 32 calls, $0.000799134); the rows below carry what that run corrected. ST-10 – ST-12: `tengu studio graph` (the workflow graph) and `tengu trace` (each `run` / `decide` is a recording of every runtime, feed, loop, Jev, action and tool step; audit lines carry `runtime_id` / `run_id`) — § Graph + trace below.

## The sandbox

| Piece | TOML | Value |
|---|---|---|
| loop | `[decision_loops.demo]` | Jev `~typesafe/jev-latest` · `act_at = 0.8` · `dry_run = false` · `escalate = false` · `max_steps = 3` · `history = 4` · `world = { tick = "feed/1:tick" }` (60 s) |
| actions | `hold` · `write_marker` · `read_probe` | terminal · `write_file` `out/marker.txt` = `scenario=<event.scenario>` (bound from the event, no question) · `read_file` `in/absent.txt` (never exists ⇒ fails; `read_only`) |
| scenario → choice | `goal` | `normal` → hold · `act` → write_marker, hold · `tool-error` → read_probe, hold · `uncertain` → no clear action |
| feeds (`tengu run`) | `[feeds.tick]` · `[feeds.probe]` | tick: event `{scenario = "normal", source = "tick"}` every 20 s + at start, required, stale 60 s · probe: `read_file in/absent.txt` every 30 s, optional ⇒ `down` (fatal) |
| agent | `[agents.lab]` | private (no `description`, not default) · `tools = ["read_file", "write_file", "list_directory"]` · scopes = fs roots inside `~/tengu-lab/control-loop-lab` only |
| runtime | `[runtime]` | beat 2 s · stale 10 s · grace 10 s · 1 event in flight · 4 queued |
| egress | `[egress]` | `open` (Jev only; no tool host) |
| events | `scenarios/{normal,act,tool-error}.json` | `{"scenario": …}` |
| maps | `scenarios/uncertain.map.json` · `scenarios/act-dry.map.json` | `act_at 1.0` + dry-run on · dry-run on (`ExecutionMap::apply` narrows only) |
| never | — | `[risk]` · signer · `[generation]` · `run_command` · network tool · write outside `out/marker.txt` |

## Setup (both paths)

| Env var (names only) | Need | Why |
|---|---|---|
| `TENGU_HOME` | **required** = `$HOME/tengu-lab/home`, exported **first** | isolates `logs/decisions.jsonl`, `logs/maps/`, `state/` from `~/.tengu` (the weekend run). tengu loads the nearest `.env` up from the cwd (dotenvy walks parent dirs): a worktree under `.claude/worktrees/` reads the main checkout's `.env`, which sets `TENGU_HOME=~/.tengu`. An exported value wins (dotenv never overrides) |
| `OPENROUTER_API_KEY` | required | Jev decisions + the `lab` engine build (`doctor`). Export it; a parent `.env` may also supply it (see `TENGU_HOME`) — never print it |
| `OPENROUTER_BASE_URL` | optional | default `https://openrouter.ai/api` |
| `RUST_LOG` | optional | `tengu run` logs to stderr + `$TENGU_HOME/logs/tengu.log`; `decide` / `doctor` / `studio` / `trace` log to **stderr** (since ST-11; stdout = the JSON / report alone) — `tengu=info` is always added, so `RUST_LOG` cannot silence them |

```bash
cd <repo or worktree root>               # --sandbox is cwd-relative
export TENGU_HOME="$HOME/tengu-lab/home"   # never ~/.tengu
mkdir -p "$HOME/tengu-lab/control-loop-lab/in" "$HOME/tengu-lab/control-loop-lab/out" "$TENGU_HOME"
cargo build --release                      # default features suffice; `cargo build` (debug, ST-03) behaves the same
T=target/release/tengu                     # $CARGO_TARGET_DIR/release/tengu when that is set
S=sandboxes/control-loop-lab; W="$HOME/tengu-lab/control-loop-lab"
J() { sed -n '/^{/,$p'; }                  # only for a binary before ST-11 (logs on stdout): $T decide … | J | jq …
```

## Quick start (3 commands)

```bash
$T decide --sandbox control-loop-lab --loop demo --event $S/scenarios/act.json   # Jev writes out/marker.txt
$T run --sandbox control-loop-lab                                               # terminal A; Ctrl-C = graceful stop
$T doctor --sandbox control-loop-lab --live                                     # terminal B, same env
```

## CLI-only path = acceptance matrix

Jev answers vary per call: A1–A4 pass when the expected result wins ≥ 2 of 3 tries — record every try. Run in order.

| # | Scenario | Command | Expected |
|---|---|---|---|
| A0 | config + engines | `$T doctor --sandbox control-loop-lab` | exit 0 · `lab: engine=openrouter model=anthropic/claude-haiku-4-5 …` · `network: open` · `proxy: none (direct)` |
| A1 | normal | `$T decide --sandbox control-loop-lab --loop demo --event $S/scenarios/normal.json` | exit 0 · `outcomes = [{"outcome":"stopped","action":"hold"}]` · `history = [{"t":1,"action":"hold"}]` · `session_id = decide-demo-<uuid>` · `audit = $TENGU_HOME/logs/decisions.jsonl` · `$W/out` empty |
| A2 | act | `echo '{"scenario":"act"}' \| $T decide --sandbox control-loop-lab --loop demo --event -` | `outcomes = [{"outcome":"executed","action":"write_marker"},{"outcome":"stopped","action":"hold"}]` · `history[0] = {"t":1,"action":"write_marker","args":{"content":"scenario=act","path":"out/marker.txt"},"ok":true,"result":"File 'out/marker.txt' written (12 bytes)"}` · `cat $W/out/marker.txt` → `scenario=act` |
| A3 | tool-error | `$T decide --sandbox control-loop-lab --loop demo --event $S/scenarios/tool-error.json` | exit 0 (a tool failure is not a loop failure) · `outcomes = [{"outcome":"executed","action":"read_probe"},{"outcome":"stopped","action":"hold"}]` · `history[0]`: `"args":{"path":"in/absent.txt"}`, `"ok":false`, `result` starts `Cannot read file 'in/absent.txt'` |
| A4 | uncertain | `$T decide --sandbox control-loop-lab --map $S/scenarios/uncertain.map.json` | `outcomes = [{"outcome":"escalated","action":<any>,"confidence":<below 1.0>}]` · `history = []` · `map.sha256` = 64 hex · `map.path = $TENGU_HOME/logs/maps/<sha256>.json` (`1a97d5793e4abfd1a53fb717c34b1a9fbfa1dc49b7ef6dd4ec165813cac5a1ef` for this file) · its audit line has `"trigger":"map:<sha256>"`, `"act_at":1.0`, `"t":0` (an escalated step is not a history step). ST-03: Jev held at 0.94–0.95 — the map's `act_at` makes it escalate |
| A5 | dry-run | `$T decide --sandbox control-loop-lab --map $S/scenarios/act-dry.map.json` | `outcomes[0] = {"outcome":"dry_run","action":"write_marker"}` · `history[0].result = "dry_run"`, no `ok` · `$W/out/marker.txt` unchanged |
| A6 | audit | `rg -c '"sandbox":"control-loop-lab"' $TENGU_HOME/logs/decisions.jsonl` | = Jev calls so far (one per step; per try: A1 1, A2 2, A3 2, A4 1, A5 2) · each line: `decision_id`, `model` (the build Jev returned), `latency_ms`, `usage` · A2 / A3 tool lines: `call_id = demo:decide-demo-<uuid>:1` |
| A7 | runtime start | terminal A: `$T run --sandbox control-loop-lab` | stderr: `runtime leases taken` (`leases=["runtime:control-loop-lab"]`, `holder=<host>:<pid>:<uuid>`) · `feed started` × 2 (`probe`, `tick`) · `tengu run started` (`loops=["demo"]`) · every 20 s `decision loop step` with `session_id=tick:<slot ms>` · every 30 s warn `feed calls failed (not retried; next slot tries again)` `feed=probe` `class=fatal` |
| A8 | tick + health | ≥ 65 s after A7, terminal B: `$T doctor --sandbox control-loop-lab --live`; again 25 s later | exit 0 · `ok heartbeat running · … · holder <A7 holder>` · `ok loop demo in_flight 0 · queue 0 · done N · …` (N ≥ 3, grows between calls) · feeds in name order: `ok feed probe (optional) down · items 0 · no item in <n> s since start (stale after 90 s) · … · last error fatal: …` (stays `ok` past 90 s: optional) · `ok feed tick (required) live · items N …` · `=> live` |
| A9 | heartbeat | `cat $TENGU_HOME/state/run-control-loop-lab.json` | `"state":"running"` · `holder` = A7's · `loops.demo.completed` ≥ 3 · `feeds.tick.state` `live` · `feeds.probe.state` `down`, `last_error_class` `fatal` · `heartbeat_secs` 2 |
| A10 | second runner | terminal B: `$T run --sandbox control-loop-lab; echo exit=$?` | ``sandbox `control-loop-lab` is already running: lease `runtime:control-loop-lab` in <$TENGU_HOME/state/runtime.db> is held by `<A7 holder>` for <n> s more. …`` · `exit=1` · heartbeat unchanged (its startup INFO lines still append to `tengu.log`; the refusal is stderr only) |
| A11 | graceful stop | Ctrl-C in A; then B: `$T doctor --sandbox control-loop-lab --live; echo exit=$?` | A: `tengu run stopping reason=SIGINT failed=false` · `loop totals decision_loop=demo …` · `tengu run stopped … aborted=0 … lease_released=true` · exit 0 · heartbeat `"state":"stopped"`, `"stop_reason":"SIGINT"` · B: `FAIL heartbeat stopped <n> s ago: SIGINT · pid … · holder <A7 holder>` · the loop / feed rows keep their last values (`ok`, tick `live`) — only the heartbeat fails · `=> NOT live` · `exit=1` |
| A12 | restart | terminal A: `$T run --sandbox control-loop-lab` again | new `holder` (uuid ≠ A7's) · newer `started_at_ms` · loop counters from 0 · run-1 `tick:` audit lines have `ts_ms` ≤ run-1 stop, run-2 lines ≥ run-2 `started_at_ms`, and their `t` restarts at 1 (a tick line's `t` is the loop's step count in that process, not per event) · since ST-11 every line carries `runtime_id` (the holder) + `run_id` (its trace file): run 1 and run 2 never share either |
| A13 | nothing dangerous | `ls -a $W/out` · `test ! -s $TENGU_HOME/logs/egress.jsonl && echo no-egress` · `ls $TENGU_HOME/logs` | only `marker.txt` · `no-egress` (no tool network call; LLM API calls are not egress-audited) · no `risk.jsonl` · `$W` holds only `in/` (empty), `out/`, `.tengu/` (observation store) |
| A14 | stop | Ctrl-C in A | as A11 |
| A15 | cleanup | see Cleanup | lab paths only |

`tengu decide` builds its own loop outside the runtime lease: it may run while A7 is up (separate history, own `decide-demo-<uuid>` session, runtime counters untouched). It reads the same workspace observation store, so while A7 keeps `feed/1:tick` ≤ 60 s old its Jev state carries `world.tick` (ST-03: 609 input tokens vs 532 without); with no runtime the row is missing.

## Graph + trace (ST-10 – ST-12)

| # | Command | Expected |
|---|---|---|
| G1 | `$T studio graph --sandbox control-loop-lab \| jq -c '{sandbox, config_hash, n: (.nodes\|length), e: (.edges\|length)}'` | `config_hash` = 64 hex (`Config::source_sha256`) · 16 nodes, 17 edges = `tests/fixtures/studio/graph-control-loop-lab.json` |
| G2 | `$T studio graph --sandbox control-loop-lab --map $S/scenarios/uncertain.map.json` | `map.sha256` = A4's · `loop:demo` `map_changes` `act_at` 0.8 → 1.0, `dry_run` false → true · a widening map (`act_at 0.5`) exits 1 naming the reason |
| G3 | after A2 + A7: `$T trace runs --sandbox control-loop-lab` | one line per recording: `kind` `decide` (`runtime_id` null) · `run` (`runtime_id` = A7's holder); first event `run.opened` |
| G4 | `$T trace show --sandbox control-loop-lab --run <run_id> [--after <seq>] [--follow]` | the run's events in `seq` order (`event_id` = `<run_id>:<seq>`); `--follow` tails until Ctrl-C · a bad id (`../x`) is refused |
| G5 | `rg '"session_id":"tick:' $TENGU_HOME/logs/decisions.jsonl \| jq -c '{runtime_id, run_id}'` | every A7 tick line: A7's holder + its trace `run_id`; `decide` lines: `run_id` = the output's `run_id`, no `runtime_id` |
| G6 | after A2: `$T trace show --sandbox control-loop-lab --run <A2 run_id> \| jq -r '[.seq, .kind, .status, .node_id] \| @tsv'` | `run.opened` · `trigger.decide` (running) · `observation.read` (`world:demo/tick`) · `jev.completed` (`jev:demo`) · `action.selected` (running, `action:demo/write_marker`) · `tool.started` → `tool.completed` (`tool:lab/write_file`) · `action.completed` (ok) · `observation.read` · `jev.completed` · `action.selected` (ok, `action:demo/hold`) · `trigger.completed` |
| G7 | A3 / A4 runs, same command | A3: `tool.failed` (`tool:lab/read_file`, `payload.error` `Cannot read file 'in/absent.txt' …`) → `action.completed` **failed** · A4: `trigger.map` (`trigger:map/<sha256>`) → `jev.completed` → `action.escalated` (`gate:demo/act_at`, `confidence` < `act_at` 1.0) |
| G8 | after A7 (≥ 1 tick): the `run` recording | `runtime.starting` → `runtime.running` · per tick `feed.fired` (`feed:tick`) → `loop.queued` → `feed.tick_sent` → `loop.started` → step events → `loop.completed` (`payload.stats`) · per probe `feed.fired` → `tool.failed` → `feed.failed` (`fatal`, `retrying: false`) · Ctrl-C: `runtime.stopping` → `runtime.stopped` (`lease_released: true`) · A12 = a new `run_id` and `runtime_id` |

Every event's fields, parents and the Studio visual it drives: `docs/runtime-2026-09-30.md` § Trace. Trace files: `$TENGU_HOME/logs/trace/control-loop-lab/<run_id>.jsonl` (removed by the cleanup below). Recorded run: `docs/studio-trace-evidence-2026-10-08.md`.

## Studio server (ST-20, build with `--features studio`)

| # | Command | Expected |
|---|---|---|
| S1 | `$T studio --sandbox control-loop-lab` (any time; Ctrl-C stops it) | stdout `Studio: http://127.0.0.1:<port>/#t=<64 hex>`; open it: header sandbox + `config_hash`, health `not live` before A7 |
| S2 | `curl -s -H "X-Studio-Token: <token>" http://127.0.0.1:<port>/api/v1/graph` | = G1 (16 nodes, 17 edges) · no token ⇒ 401 · `-H 'Host: evil.example'` ⇒ 421 · `-H 'Origin: http://evil.example'` ⇒ 403 · `-X POST` ⇒ 405 |
| S3 | A7 running, page open | live panel `attached: run <run_id> · runtime <A7 holder>`, events arrive per tick; health `live` (= `doctor --live`); A12 ⇒ `restarted: run <new run_id>` |
| S4 | `$T studio --sandbox control-loop-lab --bind 0.0.0.0` | refused (`loopback only`), exit 1 |

Recorded: `docs/studio-trace-evidence-2026-10-08.md` § ST-20.

## Troubleshooting

| Symptom | Cause | Fix |
|---|---|---|
| `Failed to load sandbox 'control-loop-lab' from sandboxes/control-loop-lab/config.toml` | cwd is not the repo / worktree root | `cd` there (`--sandbox` is cwd-relative) |
| `lab: backend init error: OPENROUTER_API_KEY is required` · `OPENROUTER_API_KEY is required for the decision model` | key not exported in this terminal | export it (name only in notes and transcripts) |
| lines land in `~/.tengu/logs/decisions.jsonl` | `TENGU_HOME` not exported in that terminal (a parent `.env` may set `~/.tengu`) | stop; export; never clean `~/.tengu` |
| `jq: parse error` on `decide` output | a binary before ST-11: stdout starts with `tengu=info` log lines | `… \| sed -n '/^{/,$p' \| jq …` (`J` above) |
| first log lines show `proxy="socks5h://127.0.0.1:9050"` | base defaults (no `$TENGU_HOME/config.toml`) install Tor, then `--sandbox` installs `proxy="direct"` before any call | nothing — the doctor's `Egress` block is the truth |
| A1–A4 picks another action | Jev answers vary call to call | 2-of-3 rule; record every try |
| A4 not `escalated` | Jev returned confidence exactly 1.0 | the map keeps dry-run on: nothing written; record it, rerun |
| A3 `"ok":true` | `$W/in/absent.txt` exists | `rm "$W/in/absent.txt"` |
| A10 refused with nothing running | a crashed run's lease | retry once it expires (≤ 30 s) |
| `FAIL heartbeat missing` | no `tengu run` under this `TENGU_HOME` | start A7 in the same env |
| decide error after 429 / 5xx | Jev: 1 retry, breaker after 3 failures for 30 s | wait 30 s, rerun |
| Ctrl-C slow | drain ≤ `shutdown_grace_secs` (10 s) | wait; a 2nd Ctrl-C exits 130 without a clean stop |

## Cleanup (lab paths only)

```bash
if [ "$TENGU_HOME" = "$HOME/tengu-lab/home" ]; then
  rm -f "$HOME/tengu-lab/control-loop-lab/out/marker.txt"
  rm -f "$HOME/tengu-lab/control-loop-lab/.tengu/observations.db"*   # loop/1 + feed/1 rows of the last run
  rm -r "$TENGU_HOME/logs" "$TENGU_HOME/state"        # after ST-03 copied its excerpts
else echo "TENGU_HOME is not the lab home: nothing removed"; fi
```

Never `tengu prune` (it deletes `<TENGU_HOME>/logs`), never `~/.tengu`, never another sandbox's workspace, never a broader recursive target. Keep the ST-03 baseline doc.
