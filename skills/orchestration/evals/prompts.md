# Orchestration skill evals

| Prompt | Expected behaviour |
|---|---|
| "research paper X then mint it as an IP token" | Sequential `sessions_spawn(researcher)` then `sessions_spawn(minter)`. |
| "fetch the latest prices for A, B, C" | Single `sessions_fan_out` with three independent requests. |
| "what's 2+2?" | Direct answer. No `sessions_spawn` call. |
| "my trade failed with HTTP 503" | Retry the same call. No decomposition. |
| "the deploy broke, check logs, restart, verify" | Sequential multi-step. At least one `remember` call (with progress metadata) to record state. |
