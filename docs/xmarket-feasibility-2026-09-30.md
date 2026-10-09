# xmarket — feasibility study (2026-09-30)

Does the PRD's opportunity exist, and what is it worth? Answered before building, from public data only. Inputs: 22 Hyperliquid xyz stock perps (hourly bars since 2026-03-06, 5-minute for 17 days, 1-minute for 3.6 days, 30 days of funding, order books), Robinhood Chain pools (1,199 real swaps via GeckoTerminal), the underlying stocks (Robinhood public history — Yahoo rate-limited this IP), EDGAR 8-K / 6-K since March. Method: one collector, four analysts, a critic that re-computed every load-bearing number from the raw data (all held; two were corrected downward). Plan and backlog: [`xmarket-tracker-2026-09-29.md`](xmarket-tracker-2026-09-29.md); PRD: [`xmarket-prd-2026-09-29.md`](xmarket-prd-2026-09-29.md).

## Verdict: re-scope

The PRD's central idea — trading price gaps between venues — fails. Two narrower Hyperliquid-only patterns show a weak, in-sample edge worth a forward paper test.

| Opportunity | Verdict | Edge after costs | How often | Confidence |
|---|---|---|---|---|
| Hyperliquid ↔ Robinhood token convergence | no-go | $100 round trip: p50 −19.9 bps, p75 −12.1 bps; 0 of 1,199 swaps positive at $25 — pools are already arbitraged to within their fees | ~0 | medium |
| Hyperliquid ↔ underlying convergence | no-go | \|gap\| p90 is 10–11 bps, below costs; HL sits a median +3.6 bps from the stock and gaps halve in 0.5–2 min | gaps > 30 bps in 0.2–0.6 % of bars | high |
| Hyperliquid's lead at the open | no-go | ≈ 0: the open already prices HL's closed-market move (slope 1.03, R² 0.92 at 09:00 ET; R² 0.996 at 09:30 ET) | ~92 ticker-opens / week | high |
| Generic 8-K reaction | no-go | +7 bps (CI −58 to +74); HL prices a filing within 1–2 h | ~7 filings / week | medium |
| **W — weekend overshoot fade** (HL only): at Sun 18:00 ET fade the move since Fri 20:00 ET, exit Mon 09:00 ET | paper-test | +46 bps net per trade in-sample (CI ≈ +7 to +85, weekends resampled); 21 of 30 weekends positive, 22 of 22 names positive | 1 window / week, ~12 names with a signal ≥ 50 bps | low |
| **E — overnight follow-through after post-market earnings** (HL only): 1–4 h after an Item 2.02 release, follow HL's move, exit before 09:30 ET | paper-test | +110 bps net per trade in-sample (CI +43 to +174, nights resampled); +67 bps without the 5 best trades; placebo nights −11 bps | ~15 per earnings season in these 22 names | low |
| Stale Robinhood pool after HL jumps | measure only | unknown — tokens lag HL for ~1 h after events, but only the 0.05 % pools could clear their ~15 bps break-even | ~1 / week | low |

W and E were chosen after looking at the data (~27 and ~100 variants tried) in one 7-month rally regime, so they are hypotheses, not proven edges. Costs used: HL xyz taker fee 0.9 bps (growth mode; xyz:MSTR 9 bps — unconfirmed by a fill); HL round trip 2–5 bps at $100–$1k; 16 of 21 Robinhood tokens cost 30–220+ bps per round trip.

## What it is worth

Both rules on the same capital, with the in-sample edge cut by half:

| Capital | Gross / day | Net with floor system ($0.26/day) | Net with lean system ($2.24/day) | Net with typical system ($19.28/day) |
|---|---|---|---|---|
| $100 | $0.06 | −$0.21 | −$2.18 | −$19.23 |
| $1k | $0.58 | +$0.32 | −$1.66 | −$18.70 |
| $10k | $5.81 | +$5.55 | +$3.57 | −$13.47 |
| $50k | $19.70–29.05 | +$19.44–28.79 | +$17.45–26.81 | +$0.41–9.77 |

| System profile | What runs | Cost / day (month) | Break-even capital |
|---|---|---|---|
| floor | market data + Jev only, no LLM, no X | $0.26 ($8) | ≈ $0.46k |
| lean | + news layer (LLM extraction, a few escalations), no X | $2.24 ($68) | ≈ $3.9k |
| typical | + X feed, more escalations | $19.28 ($587) | ≈ $33k |
| heavy | everything at its caps | $53.77 ($1,637) | ≈ $93k |

At $100 nothing breaks even: that phase is a validation spend (~$8–68 / month plus build time). The news / X / LLM layer pays for itself only above ~$30k of capital. Weekend-fade capacity is roughly $30–80k per weekend (thin names trade $8–27k per hour at the Sunday entry); earnings-rule capacity ≈ $10k per trade.

## Operator decision (2026-09-30)

**Keep the full plan**; this report stays attached as a warning. Both cheap checks run before building:

| Check | State |
|---|---|
| Holdout test: the weekend fade (W) and post-earnings rule (E), unchanged, on every other xyz single-stock perp over the same months | done — **W passes, E not confirmed** (§ Holdout result) |
| Weekend order books: every listed xyz market's 20-level book + contexts + Robinhood quotes every 5 min, Fri 2026-10-02 19:30 ET → Mon 2026-10-05 10:00 ET | scheduled — `~/.tengu/state/xmarket/research/weekend-2026-10-02/` (`sampler.sh`, `sampler.log`, `sampler.pid`); the Mac must stay on and online |

## Holdout result (2026-09-30)

The same rules, unchanged, on the 53 other xyz single-stock perps with data (42 US-listed incl. ADRs, 11 foreign-listed; indices, ETFs, FX and commodities excluded; the 5 delisted single-stock markets return no candles). The rule code first reproduced the in-sample numbers exactly. Net = gross − 3.8 bps round trip. Pass thresholds were fixed before the run: W ≥ +15 bps, E ≥ +40 bps average net per trade.

| Rule | n | Mean net | 95% CI | Positive | Verdict |
|---|---|---|---|---|---|
| W — all names | 950 trades, 53 names, 30 weekends | +49.8 bps | +9.6 to +86.0 | 24 of 30 weekends, 62 % of trades | **pass** |
| W — top 4 per weekend | 120 trades | +118.1 bps | +16.3 to +216.6 | 22 of 30 weekends | **pass** |
| E — enter t0 + 4 h | 37 events, 25 names | +35.5 bps | −33.7 to +108.9 | 20 of 37 | weak |
| E — enter t0 + 2 h | 37 events | +72.9 bps | −6.7 to +149.2 | 21 of 37 | pass on the mean only; CI includes 0 |

| Reading | Detail |
|---|---|
| W holds on new names | The per-stock reversal also works market-neutral in the holdout: +55.4 bps per trade (CI +22.0 to +85.9, t 4.21 across weekends). US-listed names: +53.3 (CI +20.4 to +82.5); foreign-listed names, whose home market opens inside the hold: +35.1, CI spanning 0 |
| …but on the same weekends | New names, not new time: holdout and in-sample weekend results correlate 0.47. Forward weekends are still needed, and this weekend's recording adds the first executable prices |
| Thin names | 8.7 % of entry bars had no trades (stale entry price); entry bars with ≥ 50 trades: +72.3 bps (n 363). The 3.8 bps cost is optimistic for the thinnest names |
| E is not confirmed | Dropping the 5 best events leaves −21.8 bps (t0 + 4 h). ARM's 6-K earnings night (accession 0001973239-26-000113, 2026-07-29), left out by the protocol because 6-K rows carry no item codes, would have lost 1,580 bps and turns E (t0 + 4 h) into a fail. Both E variants miss the M3 condition that the CI excludes 0 |
| Pooled in-sample + holdout | W all names: +48.3 bps (n 1,525, CI +14.8 to +80.7); W top 4: +114.4 (n 239). E looks better pooled (+75.4, n 73) only because of the in-sample half |
| Economics | Unchanged: at $100 the weekend fade is worth cents a day; its value depends on capital and on whether the recorded weekend books show fillable prices |

One trade was removed before computing outcomes: KIOXIA, weekend ending 2026-09-28 (trade.xyz halted and settled the market for a 3-for-1 split); kept split-adjusted, it would have added +609 bps.

## Proposed re-scope (not applied — the operator kept the full plan)

| Change | Detail |
|---|---|
| Before any build | 1. Re-run W and E on every other xyz single-stock perp over the same months — a failure stops that rule. 2. Sample HL order books every 5 min from Fri 2026-10-02 20:00 ET to Mon 09:30 ET — W dies if Sunday-entry spreads exceed ~20–30 bps on the names that carry it |
| M0 becomes an HL-only paper slice | Rules W and E, fixed in advance; an HL recorder (20-level l2Book every 60 s, 1m candles, funding, daily fee flags — HL keeps only 5,000 candles, so unrecorded weeks are lost); a shadow ledger at executable prices for every eligible name (the $100-capped ledger alone can never reach a verdict); US trading calendar + DST-safe ET exits; EDGAR scoped to the 22 CIKs, Item 2.02; Jev in shadow only |
| M3 go thresholds, fixed now | W: ≥ 12 weekends, mean net ≥ +15 bps at executable prices, t ≥ 1.5, not carried by ≤ 2 weekends. E: ≥ 25 events, mean net ≥ +40 bps, CI excludes 0 with nights resampled |
| Kept | The bridge rule (conformance shrinks to ~15–20 tools); M3b live pilot behind the M3 go — its first job is one $10 IOC to confirm the 0.9 bps fee |
| Parked until an edge exists | Robinhood Chain execution, CEX context, anomaly detectors, the news / X layer (M4), event ↔ asset graph, Jev classification and slow path (M5), pair orders (M6), RH sends (M8). Only the stale-pool quote recording continues, as a measurement |
| Spend | Floor profile (no LLM, no X) below ~$4k of capital; X only above ~$35k and after an M3 go |

## Limitations

- In-sample, one regime, small n (30 weekends, 36 earnings events); the 50 % haircut is judgement, not estimate. About half of W's profit is the shared market component — it behaves like one macro bet a week (sd ≈ 110–240 bps per weekend).
- No executable prices for weekends, overnight or earnings nights: all books and quotes come from one Wednesday pre-market window; those results use traded candle closes.
- Universe picked by today's volume (selection bias); tickers not used to form the rules are untested.
- Underlying from Robinhood historicals (no overnight ATS prints; hourly only since 2026-06-30), not the official opening auction.
- Fees are formula-derived, not confirmed by a fill; growth mode is a deployer switch that can raise them 10×. trade.xyz's off-hours oracle and mark rules (the likely cause of the weekend overshoot) were not read.
- Analysis scripts were throwaway (Rust-only repo) and lived in the session scratchpad; the numbers above are the record.

Status (2026-10-08): the off-hours oracle / mark rules were read and measured (P8, [`p8-hip3-oracle-2026-10-08.md`](p8-hip3-oracle-2026-10-08.md)); fees observed per name — 9.0 bps on `hyperliquid:xyz:BMNR`, `hyperliquid:xyz:MSTR`, `hyperliquid:xyz:PURRDAT`, `hyperliquid:xyz:STRC` (P9, [`p9-cost-liquidity-2026-10-08.md`](p9-cost-liquidity-2026-10-08.md)); rule W's forward paper test runs each weekend — 1 of the 12 its M3 go needs ([`w1-p0-weekend-2026-10-06.md`](w1-p0-weekend-2026-10-06.md)); xlab reproduces this study on history ([`xlab-2026-10-01.md`](xlab-2026-10-01.md) § 14).
