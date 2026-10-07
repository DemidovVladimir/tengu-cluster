---
name: execution-map
description: "Hand a decision loop (Jev) an execution map: which steps of a sandbox loop to run, in what order, on which values. Use when an architect agent wants Jev to execute a chain of tools (LP, hedge, swap, any loop action) without calling the tools itself."
editable_by_learner: false
---

# Execution map

You plan; Jev executes. An execution map is JSON you hand to one `[decision_loops.<loop>]` loop of the sandbox config. Jev then runs that loop's steps in your order, with your values, and stops on anything wrong. The map can only **narrow** the loop: the tools, arguments, wallets and `mode` are the sandbox's, never yours.

## Shape

```json
{
  "loop": "lp_exec",
  "goal": "Open a small test position: 0.5 SOL + 40 USDC.",
  "sequence": ["snapshot", "plan_swap", "swap?", "refresh?", "open"],
  "event": {"target_sol": 0.5, "target_usdc": 40},
  "caps": {"plan_swap": {"target_sol": 1.0}},
  "max_steps": 6
}
```

| Field | Required | Rule |
|---|---|---|
| `loop` | yes | a `[decision_loops.<name>]` of the sandbox |
| `event` | no (`{}`) | the values the loop binds (`{ event = "/target_sol" }` slots); a JSON object ≤ 16 KiB |
| `goal` | no | appended to the loop's goal as `Task (architect): …`, ≤ 2 000 chars — say what you want and when to stop |
| `sequence` | no | step order; `"step?"` = skip when it cannot run (e.g. no swap route). Replaces the loop's own order |
| `actions` | no | run only these actions (terminal actions such as `hold` always stay) |
| `caps` | no | `{action: {slot: max}}` — only tighter than the loop's caps |
| `max_steps` | no | only lower; must exceed the sequence length |
| `act_at` | no | Jev's confidence floor, only higher (≤ 1) |
| `dry_run` | no | `true` logs writes without running them; you cannot turn it off |

Any other key, a looser cap, an unknown action or a broken sequence refuses the whole map with every reason listed. Nothing runs.

## Run it

```bash
tengu decide --sandbox <sandbox> --map - <<'EOF'
{"loop": "lp_exec", "event": {"target_sol": 0.5, "target_usdc": 40}}
EOF
```

## Read the result

| Key | Meaning |
|---|---|
| `outcomes[]` | per step: `executed`, `dry_run`, `stopped` (a terminal action), `escalated` (Jev below its confidence floor — nothing more ran), `rejected`, `refused` (a risk gate) |
| `history[]` | per step: `args` actually sent, `ok`, the reduced `result` |
| `map.sha256` | the map's identity — every audit line of the run carries `trigger = "map:<sha256>"`; the map is kept under `<TENGU_HOME>/logs/maps/` |

How a chain behaves:

- A step whose values are missing (no route, a blocked plan, a failed read) is never offered. An optional step is skipped. A required step stops the chain.
- A failed or refused step stops the chain.
- `escalated` means Jev judged the next step unsafe or unclear. Read `history`, fix the map (smaller amounts, a clearer goal) and send a new one. Never retry the same map in a loop.

Always show every address, mint, signature and hash in full.
