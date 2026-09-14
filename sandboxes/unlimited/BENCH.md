# unlimited — model bench recipe

Same question, N models, through the real subagent loop (`tengu run-agent`), no TUI.

| Step | Command |
|---|---|
| 1. Bench dir | `mkdir -p /tmp/bench/agents && ln -s $PWD/sandboxes /tmp/bench/sandboxes && ln -s $PWD/skills /tmp/bench/skills` |
| 2. One spec per model | `/tmp/bench/agents/ds-flash.toml` (below); repeat with another `model` slug |
| 3. One IPC input per model | `/tmp/bench/in-flash.json` (below) |
| 4. Run | `cd /tmp/bench && TENGU_AGENT_IPC=1 RUST_LOG=tengu=info tengu run-agent < in-flash.json > out-flash.json 2> err-flash.log` |
| 5. Compare | `out-*.json` → `status`, `summary`, `metrics[]` (tokens + latency per LLM call); `err-*.log` → `metrics kind="subagent"` lines |

```toml
# /tmp/bench/agents/ds-flash.toml
name = "ds-flash"
description = "DeepSeek flash bench agent."
engine = "openrouter"
model = "deepseek/deepseek-v4-flash"
tools = ["http_request"]
skills = []
max_turns = 8
timeout_secs = 180
```

```json
{"goal":"What is the current BTC price in USD? Call http_request on a public API, then answer in ONE sentence with price, source URL, and which LLM model you are.",
 "agent_name":"ds-flash","model":"deepseek/deepseek-v4-flash","tools":["http_request"],"skills":[],
 "max_turns":8,"session_id":"bench-unlimited","step_id":"btc-flash","sandbox_config":"unlimited"}
```

`OPENROUTER_API_KEY` must be in the environment (`.env` is only auto-loaded from the repo cwd).

## 2026-09-13 result (BTC price, CoinGecko)

| Model | LLM calls | Tokens in/out (sum) | Wall | Answer quality |
|---|---|---|---|---|
| `deepseek/deepseek-v4-flash` | 5 | 7 029 / 913 | 24 s | Correct price + URL; needed 4 tool rounds (CoinGecko 403 without `User-Agent`); claimed to be "Claude" |
| `deepseek/deepseek-v4-pro` | 3 | 3 671 / 506 | 31 s | Correct price + URL; added `User-Agent` after one 403; cleanest run |
| `deepseek/deepseek-r1-0528` | 3 | 2 998 / 1 657 | 86 s | Correct price + URL; reasoning tokens dominate; claimed to be "gpt-4" |

Takeaways: all three drive `http_request` + `compress_and_store` correctly; none knows its own model (the `run-agent` system prompt does not state it, so put the model name in `identity.instructions` if you want it echoed); pro is the best cost/latency/rounds trade-off for tool use.
