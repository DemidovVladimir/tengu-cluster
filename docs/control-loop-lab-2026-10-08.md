# Control-loop lab — runbook + acceptance matrix (2026-10-08)

`sandboxes/control-loop-lab`: the safe reference run of tick → Jev → tool → audit → health (TENGU_STUDIO_PLAN.md ST-01 / ST-02). Existing CLI and tools only — no new tool, no Studio. Guard test: `config::decision_loop::tests::control_loop_lab_has_no_dangerous_surface`. Live proof: ST-03 (`docs/control-loop-lab-baseline-*.md`).

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
| `TENGU_HOME` | **required** = `$HOME/tengu-lab/home` | isolates `logs/decisions.jsonl`, `logs/maps/`, `state/` from `~/.tengu` (the weekend run). An exported value wins over a checkout's `.env` (dotenv never overrides) |
| `OPENROUTER_API_KEY` | required | Jev decisions + the `lab` engine build (`doctor`). A worktree has no `.env` |
| `OPENROUTER_BASE_URL` | optional | default `https://openrouter.ai/api` |
| `RUST_LOG` | optional | `tengu run` logs `tengu=info` to stderr + `$TENGU_HOME/logs/tengu.log` |

```bash
cd <repo or worktree root>               # --sandbox is cwd-relative
export TENGU_HOME="$HOME/tengu-lab/home"   # never ~/.tengu
mkdir -p "$HOME/tengu-lab/control-loop-lab/in" "$HOME/tengu-lab/control-loop-lab/out" "$TENGU_HOME"
cargo build --release                      # default features suffice
T=target/release/tengu                     # $CARGO_TARGET_DIR/release/tengu when that is set
S=sandboxes/control-loop-lab; W="$HOME/tengu-lab/control-loop-lab"
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
| A4 | uncertain | `$T decide --sandbox control-loop-lab --map $S/scenarios/uncertain.map.json` | `outcomes = [{"outcome":"escalated","action":<any>,"confidence":<below 1.0>}]` · `history = []` · `map.sha256` = 64 hex · `map.path = $TENGU_HOME/logs/maps/<sha256>.json` · its audit line has `"trigger":"map:<sha256>"`, `"act_at":1.0` |
| A5 | dry-run | `$T decide --sandbox control-loop-lab --map $S/scenarios/act-dry.map.json` | `outcomes[0] = {"outcome":"dry_run","action":"write_marker"}` · `history[0].result = "dry_run"`, no `ok` · `$W/out/marker.txt` unchanged |
| A6 | audit | `rg -c '"sandbox":"control-loop-lab"' $TENGU_HOME/logs/decisions.jsonl` | = Jev calls so far (one per step; per try: A1 1, A2 2, A3 2, A4 1, A5 2) · each line: `decision_id`, `model` (the build Jev returned), `latency_ms`, `usage` · A2 / A3 tool lines: `call_id = demo:decide-demo-<uuid>:1` |
| A7 | runtime start | terminal A: `$T run --sandbox control-loop-lab` | stderr: `runtime leases taken` (`leases=["runtime:control-loop-lab"]`, `holder=<host>:<pid>:<uuid>`) · `feed started` × 2 (`probe`, `tick`) · `tengu run started` (`loops=["demo"]`) · every 20 s `decision loop step` with `session_id=tick:<slot ms>` · every 30 s warn `feed calls failed (not retried; next slot tries again)` `feed=probe` `class=fatal` |
| A8 | tick + health | ≥ 65 s after A7, terminal B: `$T doctor --sandbox control-loop-lab --live`; again 25 s later | exit 0 · `ok heartbeat running · … · holder <A7 holder>` · `ok loop demo in_flight 0 · queue 0 · done N · …` (N ≥ 3, grows between calls) · `ok feed tick (required) live · items N …` · `ok feed probe (optional) down · items 0 · … · last error fatal: …` · `=> live` |
| A9 | heartbeat | `cat $TENGU_HOME/state/run-control-loop-lab.json` | `"state":"running"` · `holder` = A7's · `loops.demo.completed` ≥ 3 · `feeds.tick.state` `live` · `feeds.probe.state` `down`, `last_error_class` `fatal` · `heartbeat_secs` 2 |
| A10 | second runner | terminal B: `$T run --sandbox control-loop-lab; echo exit=$?` | ``sandbox `control-loop-lab` is already running: lease `runtime:control-loop-lab` in <$TENGU_HOME/state/runtime.db> is held by `<A7 holder>` for <n> s more. …`` · `exit=1` |
| A11 | graceful stop | Ctrl-C in A; then B: `$T doctor --sandbox control-loop-lab --live; echo exit=$?` | A: `tengu run stopping reason=SIGINT failed=false` · `loop totals decision_loop=demo …` · `tengu run stopped … aborted=0 … lease_released=true` · exit 0 · heartbeat `"state":"stopped"`, `"stop_reason":"SIGINT"` · B: `FAIL heartbeat stopped <n> ago: SIGINT …` · `=> NOT live` · `exit=1` |
| A12 | restart | terminal A: `$T run --sandbox control-loop-lab` again | new `holder` (uuid ≠ A7's) · newer `started_at_ms` · loop counters from 0 · run-1 `tick:` audit lines have `ts_ms` ≤ run-1 stop, run-2 lines ≥ run-2 `started_at_ms` (the separation proof until ST-11 adds `runtime_id`) |
| A13 | nothing dangerous | `ls -a $W/out` · `test ! -s $TENGU_HOME/logs/egress.jsonl && echo no-egress` · `ls $TENGU_HOME/logs` | only `marker.txt` · `no-egress` (no tool network call; LLM API calls are not egress-audited) · no `risk.jsonl` · `$W` holds only `in/` (empty), `out/`, `.tengu/` (observation store) |
| A14 | stop | Ctrl-C in A | as A11 |
| A15 | cleanup | see Cleanup | lab paths only |

`tengu decide` builds its own loop outside the runtime lease: it may run while A7 is up (separate history, own `decide-demo-<uuid>` session).

## Troubleshooting

| Symptom | Cause | Fix |
|---|---|---|
| `Failed to load sandbox 'control-loop-lab' from sandboxes/control-loop-lab/config.toml` | cwd is not the repo / worktree root | `cd` there (`--sandbox` is cwd-relative) |
| `lab: backend init error: OPENROUTER_API_KEY is required` · `OPENROUTER_API_KEY is required for the decision model` | key not exported in this terminal | export it (name only in notes and transcripts) |
| lines land in `~/.tengu/logs/decisions.jsonl` | `TENGU_HOME` not exported in that terminal | stop; export; never clean `~/.tengu` |
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
  rm -r "$TENGU_HOME/logs" "$TENGU_HOME/state"        # after ST-03 copied its excerpts
else echo "TENGU_HOME is not the lab home: nothing removed"; fi
```

Never `tengu prune` (it deletes `<TENGU_HOME>/logs`), never `~/.tengu`, never another sandbox's workspace, never a broader recursive target. Keep the ST-03 baseline doc.
