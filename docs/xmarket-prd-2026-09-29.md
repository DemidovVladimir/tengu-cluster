> Operator PRD, stored verbatim on 2026-09-29. Working sandbox name: `xmarket` (`sandboxes/xmarket/config.toml`).
> Gap tracker (what Tengu must add): [`xmarket-tracker-2026-09-29.md`](xmarket-tracker-2026-09-29.md) — rules in its **§ 0 Start here**. How to execute: [`xmarket-build-plan-2026-09-30.md`](xmarket-build-plan-2026-09-30.md) (kickoff prompt at its end).
> The operator addendum below (2026-09-30) is not part of the v0.4 text; it extends it and wins where the two differ.

## Operator addendum — decisions and rules (2026-09-30)

| Topic | Rule |
|---|---|
| Build mandate | Build the full scope in this PRD, including the parts the feasibility study found no edge for; tested, self-validated and fixed until it runs smoothly; docs adjusted; a separate sandbox (`xmarket-weekend`) for the weekend investigation run. Execution plan: [`xmarket-build-plan-2026-09-30.md`](xmarket-build-plan-2026-09-30.md) |
| Engine compatibility | Every tool — existing and new — works 100 % under `engine = "openrouter"`, `"local"` and `"claude_code"` (subscription, through `tengu mcp-bridge`, exactly as in-process) — **no exceptions**. A tool is done only when its schema lint, bridge conformance case and live engine-matrix smoke pass |
| Trading mode | Paper first. Live trading starts only in the live pilot (M3b), after the M3 edge check says "go"; paper keeps running alongside |
| Budget | $100: paper starting cash, the `[risk]` total-exposure cap, and later the deposit of a dedicated Hyperliquid sub-account whose API wallet can trade but not withdraw |
| Risk | A deterministic `[risk]` gate inside every order tool, which neither Jev nor an LLM can change: total exposure ≤ $100, ≤ $25 per order, ≤ $50 per position, no leverage, halt at a $10 daily or $25 total loss (defaults, editable in TOML); a kill switch only the operator can reset; exit rules (take-profit / stop-loss / max holding time); live entries carry exchange-side TP / SL |
| Continuous process | `tengu run --sandbox xmarket` runs 24/7 in Docker on the operator's Hetzner or Hostinger VPS from the first milestone (restart policy, health check) |
| Network | `[egress] network = "open"` for now, switchable to Tor later: every new transport goes through `egress.rs` |
| Jurisdiction | The operator is in Kazakhstan. No legal or regulatory gates in the plan; the only effect is technical — some venue hosts are unreachable from Kazakh connections, so the process runs on a VPS abroad |
| Engines | Jev (`~typesafe/jev-latest`) decides; the architect (slow path) runs on OpenRouter or on a hardened `claude_code` agent (built-in tools off, `--strict-mcp-config`); no shell in the trading sandbox |
| Spend | One OpenRouter key for Jev and LLM calls with a $40 / day limit |
| Data identity | `SEC_USER_AGENT` is set in the repo `.env` (verified 2026-09-30) |
| Feasibility (2026-09-30) | Verdict **re-scope** ([`xmarket-feasibility-2026-09-30.md`](xmarket-feasibility-2026-09-30.md)): the cross-venue convergence idea fails after costs; two Hyperliquid-only rules (weekend overshoot fade, overnight follow-through after post-market earnings) show a weak in-sample edge to paper-test; at $100 no system profile breaks even. **Operator decision 2026-09-30: keep the full plan**, with the report attached as a warning; a holdout test of the two rules and a weekend order-book recording (2026-10-02 → 10-05) run first. Holdout result: the weekend fade passes on 53 new names (+49.8 bps net per trade, same weekends); the post-earnings rule is not confirmed |
| Build order | E0 engine parity → M0 thin paper slice (+ the weekend sandbox) → M1 universe + history → M2 continuous market observation → M3 edge check (go / no-go) → M3b live pilot → M4 information layer → M5 event ↔ asset + Jev + slow path → M6 opportunities + full paper execution → M7 evaluation → M8 extensions |

# Product Requirements Document
## Tengu Event-Driven Cross-Market Trading Intelligence System

**Version:** 0.4  
**Platform:** Tengu Cluster  
**Decision/Execution Engine:** TypeSafe JEV  
**Status:** Research / MVP Definition

---

# 1. Product Vision

Build an event-driven financial intelligence and trading system on top of Tengu Cluster that continuously observes:

- financial news;
- company and regulatory announcements;
- X/Twitter and other real-time social information;
- Robinhood Chain;
- Hyperliquid/HIP-3;
- crypto markets;
- underlying/reference markets;
- relevant on-chain activity.

The system should not be designed around a fixed asset such as NVIDIA.

Instead, it should continuously discover:

**WHAT happened**

→ **WHICH entities/assets are affected**

→ **WHERE those assets or related exposures can be traded**

→ **HOW the relevant markets are reacting**

→ **WHETHER a meaningful trading opportunity exists**

→ **WHAT bounded action JEV should take**

The fundamental objective is to identify temporary differences in how markets absorb new information.

---

# 2. Core Hypothesis

Financial information does not propagate through every market simultaneously.

Different markets have different:

- participants;
- liquidity;
- trading hours;
- leverage;
- funding;
- oracle mechanisms;
- geographic exposure;
- information latency;
- risk appetite.

Therefore an important event may create temporary differences in price discovery.

The system should detect and analyze these differences dynamically across whatever assets are affected by the event.

---

# 3. Event-First Architecture

The system should support two complementary discovery paths.

## Market-First Discovery

The system continuously monitors known markets and detects unusual behavior such as:

- price divergence;
- volume spike;
- funding anomaly;
- open-interest change;
- order-book imbalance;
- liquidation activity.

It then searches for information that may explain the movement.

Conceptually:

**MARKET ANOMALY**

→ **WHAT IS MOVING?**

→ **WHY?**

→ **RELATED ASSETS**

→ **OPPORTUNITY**

---

## Information-First Discovery

The system continuously monitors information sources.

When potentially significant information appears:

**NEWS / X / PRIMARY SOURCE**

→ **WHAT HAPPENED?**

→ **WHO IS AFFECTED?**

→ **WHICH ASSETS ARE AFFECTED?**

→ **WHERE ARE THEY TRADED?**

→ **HOW ARE MARKETS REACTING?**

→ **IS THERE AN OPPORTUNITY?**

Both paths should operate simultaneously.

---

# 4. Dynamic Asset Discovery

The trading universe must not be permanently restricted to a predetermined ticker list.

The system should maintain knowledge of currently accessible instruments across supported venues.

When an event identifies an asset, the system should determine whether a relevant representation exists on:

- Robinhood Chain;
- Hyperliquid;
- HIP-3;
- crypto venues;
- underlying/reference markets;
- other supported venues added later.

For example:

**News mentions TSLA**

→ discover TSLA representations.

**News mentions Coinbase**

→ discover COIN plus potentially related crypto exposures.

**News mentions Ethereum**

→ discover ETH markets and relevant derivatives.

**News mentions Anthropic**

→ determine whether direct or synthetic Anthropic exposure exists.

The system should therefore build the tradable universe dynamically from current venue metadata.

---

# 5. Asset Relationship Graph

Direct ticker matching is insufficient.

The system should maintain an evolving relationship graph between:

**entities**

**assets**

**companies**

**tokens**

**commodities**

**indices**

**sectors**

**suppliers**

**customers**

**competitors**

**markets**

**derivatives**

Example:

**Anthropic**

may connect to:

- Anthropic private-market instruments;
- Amazon;
- Google;
- NVIDIA;
- AI-related assets.

But these relationships have different strengths.

The system must distinguish between:

**direct exposure**

and

**indirect relationship.**

---

# 6. Impact Discovery

When new information arrives, the intelligence layer should identify potentially affected entities.

Example:

> Company X announces unexpected restrictions on semiconductor exports.

Potential direct entities:

- affected semiconductor companies.

Potential secondary entities:

- suppliers;
- customers;
- competitors;
- semiconductor ETFs;
- related commodities.

Potential macro relationships:

- currencies;
- indices;
- geographic markets.

The system should generate an initial candidate impact set rather than immediately trade all related assets.

---

# 7. Impact Confidence

Each relationship between an event and an asset should carry a confidence level.

Conceptually:

**Direct**

The event explicitly concerns the asset/company.

**Strong**

The economic relationship is well established.

**Possible**

A reasonable relationship exists but requires confirmation.

**Speculative**

Relationship is weak or uncertain.

Automated trading should initially focus primarily on direct and strongly supported relationships.

Speculative relationships should normally be used for research rather than execution.

---

# 8. Example — Direct Asset

News:

> Apple unexpectedly lowers revenue guidance.

Entity extraction:

**Apple**

Asset discovery:

**AAPL**

Venue discovery:

**underlying AAPL**

**Robinhood AAPL Stock Token**

**Hyperliquid/HIP-3 AAPL market, if available**

The system then compares market reactions.

---

# 9. Example — Crypto Event

News:

> Major regulatory decision affecting Ethereum staking.

Entity:

**Ethereum**

Affected asset:

**ETH**

Possible related assets:

- staking protocols;
- liquid-staking tokens;
- relevant DeFi protocols.

The system should first examine ETH.

Secondary assets should require stronger evidence.

---

# 10. Example — Cross-Asset Event

News:

> Unexpected geopolitical event significantly affects oil supply.

Affected economic asset:

**oil**

Potential related markets:

- oil instruments;
- energy companies;
- energy ETFs;
- currencies of major exporters;
- relevant crypto prediction/perpetual markets if available.

The system should dynamically discover which supported venues actually provide executable exposure.

---

# 11. Example — Company Relationship

News:

> Anthropic announces a major infrastructure agreement.

Direct:

**Anthropic**

Then relationship discovery might identify:

**cloud provider**

**chip supplier**

**strategic investor**

But the system must not assume all related companies should move.

Instead:

**event relationship**

+

**market reaction**

must confirm relevance.

---

# 12. Market Confirmation

The system should ask:

> Is the market actually reacting to this relationship?

Suppose news appears involving Anthropic.

Potential related assets:

- AMZN;
- GOOG;
- NVDA.

Market behavior:

AMZN: +0.1%

GOOG: +0.0%

NVDA: +2.3%

NVDA volume: strongly elevated.

NVDA Hyperliquid OI: increasing.

The system now has evidence that the market currently associates the event more strongly with NVDA.

This is more useful than relying purely on semantic reasoning.

---

# 13. Dynamic Event Pipeline

The central workflow should be:

**RAW INFORMATION**

↓

**EVENT EXTRACTION**

↓

**ENTITY DISCOVERY**

↓

**ASSET DISCOVERY**

↓

**RELATIONSHIP ANALYSIS**

↓

**VENUE DISCOVERY**

↓

**MARKET REACTION**

↓

**CROSS-MARKET COMPARISON**

↓

**OPPORTUNITY CANDIDATE**

↓

**JEV DECISION**

↓

**RISK GATE**

↓

**TENGU EXECUTION**

This replaces any NVDA-centric workflow.

---

# 14. Reverse Discovery

The reverse process is equally important.

Suppose Hyperliquid suddenly shows:

**Asset X +7%**

**OI +25%**

**funding rapidly increasing**

**volume 8× normal**

while the reference market moves only 2%.

The system should ask:

> Why?

It should search:

- recent news;
- X;
- company announcements;
- regulatory sources;
- related assets.

It may discover an event before the system's normal news feed has classified it.

Therefore:

**markets can discover news**

just as

**news can discover markets.**

---

# 15. Information Sources

The system should continuously consume:

### Primary sources

- companies;
- regulators;
- exchanges;
- protocols;
- government sources.

### Professional news

- Reuters;
- Bloomberg;
- Financial Times;
- comparable services.

### Fast information

- specialist reporters;
- market accounts;
- crypto researchers;
- on-chain researchers.

### Social information

- X/Twitter;
- Coin Bureau;
- analysts;
- traders;
- industry commentators.

No single source should define reality.

---

# 16. Event Classification

Every relevant event should be classified.

Possible categories include:

- earnings;
- guidance;
- acquisition;
- partnership;
- investment;
- financing;
- product launch;
- regulation;
- court decision;
- security breach;
- hack;
- exchange listing;
- delisting;
- token unlock;
- macroeconomic data;
- central-bank decision;
- geopolitical event;
- commodity disruption;
- liquidation event;
- rumor;
- correction;
- other.

This taxonomy should remain extensible.

---

# 17. Information State

The system should distinguish:

- official fact;
- confirmed reporting;
- credible report;
- claim;
- rumor;
- opinion;
- analysis;
- duplicate;
- repost;
- correction.

An event should be allowed to evolve.

Example:

**RUMOR**

→ **CREDIBLE REPORT**

→ **MULTIPLE CONFIRMATIONS**

→ **OFFICIAL**

The system should update its confidence rather than create unrelated events for every new article.

---

# 18. Novelty and Deduplication

Information should only have high impact if it is genuinely new.

Fifty accounts repeating the same article do not constitute fifty signals.

The system should maintain a canonical event and attach subsequent information to it.

Independent confirmations may increase confidence.

Reposts should not.

---

# 19. Market Universe

The system should maintain a continuously updated catalog of available instruments.

For each instrument it should know:

- venue;
- symbol;
- underlying entity/asset;
- instrument type;
- oracle/reference;
- trading availability;
- liquidity;
- leverage where applicable;
- economic relationship to other instruments.

This catalog forms the bridge between information intelligence and market execution.

---

# 20. Market Data

For relevant instruments the system should obtain, where available:

- executable bid;
- executable ask;
- last trade;
- order book;
- volume;
- liquidity;
- oracle;
- reference price;
- mark price;
- funding;
- predicted funding;
- open interest;
- volatility;
- market status.

The system must distinguish:

**reference**

**oracle**

**last price**

**mark**

**actual executable price.**

---

# 21. Opportunity Types

The architecture should not assume that every opportunity is the same.

Initial research should include several opportunity families.

## Cross-Market Convergence

Same or equivalent exposure trades differently across venues.

Example:

Robinhood TSLA cheap

vs

Hyperliquid TSLA expensive.

---

## Information Latency

One market reacts materially faster than another after new information.

---

## Overreaction

A leveraged market reacts significantly more strongly than reference markets.

Potential evidence:

- extreme funding;
- rapid OI increase;
- one-sided order book;
- liquidation activity.

---

## Underreaction

A market has not yet incorporated confirmed information reflected elsewhere.

---

## Related-Asset Reaction

Information concerning one entity causes measurable repricing of another economically connected asset.

This strategy requires substantially stronger validation because the relationship is less direct.

---

# 22. JEV's Role

JEV should make bounded decisions based on normalized observations.

It should not be restricted to NVDA or any fixed asset.

The currently relevant asset set should be part of the observation context.

Possible decisions include:

- ignore event;
- monitor event;
- investigate entity;
- discover affected assets;
- inspect market;
- request confirmation;
- identify candidate opportunity;
- reject opportunity;
- escalate;
- paper-enter;
- hold;
- reduce;
- close.

JEV chooses among permitted actions.

It does not invent arbitrary actions.

---

# 23. Dynamic JEV Context

A decision cycle might contain:

**EVENT**

Major unexpected announcement involving Company X.

**DIRECT ASSET**

XYZ

**RELATED ASSETS**

ABC — strong relationship

DEF — moderate relationship

GHI — weak relationship

**AVAILABLE VENUES**

XYZ Robinhood

XYZ Hyperliquid

ABC Hyperliquid

XYZ underlying reference

**MARKET REACTION**

XYZ reference +3.1%

XYZ Robinhood +2.4%

XYZ Hyperliquid +5.2%

Funding strongly positive.

OI +18%.

JEV can now make a bounded decision using the current context.

---

# 24. JEV Does Not Discover Financial Truth Alone

JEV's decisions should be informed by:

- deterministic calculations;
- entity/event intelligence;
- venue discovery;
- historical relationships;
- market confirmation;
- source reliability.

JEV should not independently invent that:

> Company X is related to Company Y, therefore buy Y.

Uncertain relationships should be investigated or escalated.

---

# 25. Deterministic Layer

Normal software should calculate:

- basis;
- spread;
- price changes;
- volume changes;
- OI changes;
- funding;
- volatility;
- transaction costs;
- expected slippage;
- exposure;
- leverage;
- P&L;
- risk limits;
- data freshness.

JEV should reason over these results rather than reproduce financial arithmetic.

---

# 26. Architect Role

A higher-capability architect/research model should handle situations requiring deeper reasoning.

Examples:

- unfamiliar company;
- unknown token;
- new relationship;
- complex geopolitical event;
- new financial instrument;
- conflicting information;
- unexpected market behavior.

The architect may research and return structured context.

It should not directly execute unrestricted financial transactions.

---

# 27. Fast Path and Slow Path

## Fast Path

JEV handles known patterns:

**observe**

→ **classify**

→ **monitor**

→ **candidate**

→ **risk**

→ **paper execution**

Fast and bounded.

---

## Slow Path

Unknown situation:

**new event**

→ **relationship unclear**

→ **JEV escalates**

→ **architect researches**

→ **structured result**

→ **JEV resumes**

This allows the system to respond to assets we never explicitly anticipated when designing it.

---

# 28. Risk Engine

Risk policy remains independent from both JEV and the architect.

It must control:

- permitted instruments;
- permitted venues;
- position limits;
- leverage;
- total exposure;
- asset exposure;
- loss limits;
- minimum expected edge;
- slippage;
- liquidity;
- hedge availability;
- data quality.

JEV cannot override risk policy.

---

# 29. Unknown Assets

An important safety rule:

**newly discovered does not mean immediately tradable.**

An unknown asset may enter states such as:

**DISCOVERED**

→ **IDENTIFIED**

→ **MAPPED**

→ **OBSERVED**

→ **VALIDATED**

→ **PAPER-TRADABLE**

→ eventually **LIVE-APPROVED**

This allows the universe to expand dynamically without granting immediate execution permission to arbitrary instruments.

---

# 30. JEV Execution

Once an opportunity has passed required validation:

**JEV selects bounded action**

↓

**Risk validates**

↓

**Tengu exposes permitted tool operation**

↓

**execution occurs**

↓

**result becomes new observation**

↓

**JEV decides next action**

This follows the existing Tengu/JEV Typesafe execution model.

---

# 31. Paper Trading

The first system must operate in paper/dry-run mode.

Simulation must include:

- actual bid/ask;
- liquidity;
- slippage;
- fees;
- funding;
- network costs;
- latency;
- partial fills;
- failed fills.

The system should record every JEV decision whether or not a trade occurs.

---

# 32. Decision Audit

For every decision the system should retain enough information to answer:

- What event triggered investigation?
- Which entities were identified?
- Which assets were considered?
- Why were they considered related?
- Which venues were available?
- What were market conditions?
- Which actions were available to JEV?
- What did JEV choose?
- With what confidence?
- What did risk decide?
- What happened afterward?

This creates an auditable decision history.

---

# 33. Learning From Decisions

The system should eventually evaluate:

### Event intelligence

Which events actually move markets?

### Relationships

Which entity relationships produce repeatable secondary reactions?

### Sources

Which sources provide genuine information advantage?

### Markets

Which venues tend to lead price discovery?

### JEV

Which decisions are well calibrated?

### Strategies

Which opportunity types survive costs?

The purpose of historical data is therefore broader than ordinary backtesting.

It allows the system's assumptions themselves to be tested.

---

# 34. Research Questions

The project should answer:

### Discovery

Can news reliably identify affected tradable assets automatically?

Can market anomalies reliably identify relevant news?

### Cross-market

Which venues typically lead price discovery?

How large and frequent are executable dislocations?

### Relationships

Do secondary assets react consistently enough to trade?

Which relationships are stable?

### Information

Does X provide meaningful lead time?

Which sources are useful for which asset classes?

### JEV

Does JEV improve opportunity selection over deterministic rules?

Are JEV confidence levels calibrated?

When should JEV escalate rather than decide?

### Execution

Does theoretical edge survive realistic latency and execution costs?

---

# 35. MVP Stages

## Stage 1 — Dynamic Market Discovery

Discover available instruments across supported venues.

Build relationships between equivalent instruments.

No trading.

---

## Stage 2 — Market Observation

Continuously monitor available markets.

Detect unusual price, funding, OI, volume and liquidity behavior.

---

## Stage 3 — Information Observation

Monitor:

- news;
- primary sources;
- X/social.

Extract events and entities.

---

## Stage 4 — Event ↔ Asset Discovery

Map events to potential affected assets.

Discover where those assets can be observed/traded.

Measure actual market reactions.

---

## Stage 5 — JEV Classification

Allow JEV to make bounded decisions about:

- relevance;
- investigation;
- confirmation;
- candidate opportunities;
- escalation.

---

## Stage 6 — Opportunity Research

Study:

- convergence;
- information latency;
- overreaction;
- underreaction;
- related-asset reactions.

---

## Stage 7 — Paper Execution

Allow JEV to control bounded paper-trading actions through Tengu under deterministic risk constraints.

---

## Stage 8 — Evaluation

Compare:

**market-only**

vs

**market + information**

vs

**market + JEV**

vs

**market + information + JEV**

and determine which components actually add economic value.

---

# 36. Central System Model

The system is therefore not:

**NVDA**

→ **Robinhood**

→ **Hyperliquid**

Instead it is:

**WORLD EVENTS + MARKET EVENTS**

↓

**TENGU INFORMATION TOOLS**

↓

**EVENT / ANOMALY DISCOVERY**

↓

**ENTITY DISCOVERY**

↓

**DYNAMIC ASSET DISCOVERY**

↓

**VENUE DISCOVERY**

↓

**MARKET CONFIRMATION**

↓

**CROSS-MARKET ANALYSIS**

↓

**JEV TYPESAFE DECISION**

↓

**DETERMINISTIC RISK**

↓

**TENGU EXECUTION**

↓

**OBSERVE RESULT**

↓

**JEV**

with:

**UNKNOWN / COMPLEX**

↓

**ARCHITECT**

↓

**STRUCTURED CONTEXT**

↓

**JEV**

---

# 37. Product Principle

The system should not be designed to know in advance what tomorrow's important asset will be.

That is precisely what the information layer is intended to discover.

A significant event could concern:

**NVDA today**

**TSLA tomorrow**

**ETH next week**

**oil after a geopolitical event**

**an asset that was not part of the original universe at all.**

Tengu should discover and understand the event.

The asset graph should identify relevant economic exposures.

Market data should determine whether those exposures are actually reacting.

Deterministic software should calculate the financial state.

JEV should choose the next bounded action.

Risk policy should determine whether that action is permitted.

Tengu tools should execute it.

This creates a system designed around **events and opportunities**, rather than around predetermined tickers.
