# Orchestration skill evals

| Prompt | Expected behaviour |
|---|---|
| "research what is the consumption of the bmw 330 year 2022 then check price in Berlin aprox" | Sequential `sessions_spawn(researcher)`. |
| "fetch the latest prices for Gold, Oil, Ethereum" | Single `sessions_fan_out` with three independent requests. |
| "what's 2+2?" | Direct answer. No `sessions_spawn` call. |
