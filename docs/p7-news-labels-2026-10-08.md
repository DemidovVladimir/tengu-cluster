# Phase 7 — news / information labels (2026-10-08)

`TENGU_ROADMAP.md` Phase 7, gate G7: do weekend moves with real-world information behave unlike thin-market noise? Built point-in-time labels for every rule W candidate and tested the label policies against rule W.

## Built

| Piece | Where |
|---|---|
| SEC EDGAR filings | `tengu history events --sandbox xlab-w2 --instruments @xyz_stocks --from 2026-03-01` (`domain/sec.rs`, `backfill/sec.rs`): 63 of 75 names have a CIK, 844 filings (8-K 547 · 6-K 153 · 10-Q 104 · 10-K 17 · …), `market.db` `events` + `event_coverage` |
| Publication time | the filing index page's "Accepted" time (New York, DST-aware). The submissions JSON `acceptanceDateTime` is wrong by the New York offset for some filers (AAPL, AMZN, META, BB, BABA) and right for others (TSLA, NVDA, MSFT, …) — pinned in a test with full accession numbers |
| Labels | `weekend_window` knob `labels = { skip, lookback_mins = 240, new_listing_days = 14, forms }` (`domain/backtest/labels.rs`); absent ⇒ W1's spec hashes unchanged (test pins all 11) |
| Rule | window = [anchor − 240 min (Fri 16:00 ET close), decision]: NEWS = a filing / split published in it (≤ the decision) · UNCERTAIN = no source covers the whole window, or listed < 14 days (1d bars backfilled for listing age) · else NOISE; NEWS > UNCERTAIN > NOISE |
| Integrity | `checks.rs`: an event published after t, moved, deleted or flooded changes nothing decided at or before t; one published at exactly t counts |
| Label | `info_label` on candidates; kept out of the Jev gate event |

## Result (xlab-w2, cost model v2, decisions 2026-03-01 → 2026-10-03, development data)

Labels over 1,500 candidates: NEWS 62 · UNCERTAIN 273 · NOISE 1,165. Every in-window filing is a Friday after-close filing (none on Saturday or Sunday).

| Spec | n | mean net bps | 95 % CI | Sharpe |
|---|---|---|---|---|
| rule W, all names | 1,500 | +45.84 | [+11.0, +77.1] | 3.50 |
| skip NEWS | 1,438 | +45.60 | [+10.7, +77.3] | 3.42 |
| NOISE only | 1,165 | +50.59 | [+15.9, +79.9] | 3.86 |
| rule W top 4 | 116 | +142.65 | [+46.1, +243.3] | 3.69 |
| top 4, skip NEWS | 116 | +140.75 | [+45.1, +241.4] | 3.66 |
| top 4, NOISE only | 116 | +134.74 | [+62.6, +205.8] | 4.60 |

Verdict: **INCONCLUSIVE**. NEWS candidates fade like the rest, which fits HL pricing a filing within 1–2 h (`ext.feasibility.generic_8k.2026-09-30`). NOISE-only drops the 12 non-SEC names and new listings: lower variance, no higher mean. Records: `lineage/experiments/xlab.weekend_fade{,_top4}.news_labels.2026-10-08.toml`, variants `weekend_fade{,.top4}.{skip_news,noise_only}`.

## G7

| Check | Result |
|---|---|
| Provenance and timing kept | PASS: accession numbers in full, index-page acceptance times, fetch time stored apart |
| No future leakage | PASS: publication ≤ decision, enforced by the checks |
| Development / holdout separated | PASS: development only; forward weekends are the holdout |
| Variants registered | PASS: 4 variants + 2 experiments (defaults set before any run) |
| Economics with costs | PASS: cost model v2 (P9) |
| Enters W2? | **No**: no value beyond the baseline. Kept as Experience |

Unknowns: headlines / wires (no point-in-time source); xyz ticker ↔ SEC company checked by ticker (7 names by company name); the 12 non-US names have no event source.
