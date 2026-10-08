# lineage/ — the experiment, variant, Experience, capability, generation and ranking-contract registry

One record = one TOML file; adding a record needs no Rust. Design: `docs/lineage-2026-10-06.md`.

| Dir / file | Record | Key fields |
|---|---|---|
| `families/<id>.toml` | hypothesis family | `hypothesis`, `role`, `status`, `origin*`, `preceded_by`, `controls`, `[prior_search]` |
| `variants/<id>.toml` | one variant of a family | `family`, `parent` (`ROOT`), `[[changed]]`, `preregistered`, `[spec]` |
| `experiments/<id>.toml` | hypothesis → config → evidence → result → verdict | `kind`, `[[windows]]`, `[[results]]` (`extract`), `[verdict]`, `validity` |
| `episodes/<id>.toml` | Experience episode | `[context]`, `[[information]]`, `[[alternatives]]`, `[decision]`, `[quality]` |
| `incidents/<id>.toml` | operational / data incident | `class`, `strategy_impact`, `[[data_impact]]` |
| `capabilities/<id>.toml` | decision-relevant capability | `class`, `version`, `permission`, `lifecycle`, `contract`, `bindings` |
| `generations/<id>.toml` | generation manifest (W1 …) | `status`, `sandboxes`, `[[capabilities]]`, `[[pins]]` |
| `evidence/<id>.toml` | evidence snapshot (`tengu evidence snapshot`) | `vault`, `[[items]]` + sha256 |
| `rankings/<id>.toml` | strategy-ranking contract (`domain/lineage/ranking.rs`) | `sandbox`, `strategies`, `tz`, `cutoff`, `days`, `from`, `cohort`, `on_missing`, `[freshness]`, `[eligibility]`, `[rating]` |
| `locks.toml` | `[[frozen]]` generations, `[[sealed]]` preregistrations | append-only |

| Rule | Value |
|---|---|
| Id | `^[A-Za-z0-9][A-Za-z0-9._-]{0,79}$`, the file stem, unique across all kinds |
| Missing fact | `"UNKNOWN"` — never a guess |
| Time | quoted: `"2026-10-01T18:28:09Z"` (UTC) · `"2026-09-30"` · `"UNKNOWN"` |
| Locator | `repo:` · `run:<state>/<run id>` · `vault:<snapshot>/<path>` · `state:` · `git:<40 hex>` · `record:<kind>/<id>` · `url:https://…` |
| Ids and hashes | always in full |

## Add a record

1. Copy a record of the same kind (examples: `tests/fixtures/lineage/registry/`), set `id` = the new file stem.
2. A preregistration: `preregistered = true`, then `tengu lineage seal variant:<id>` (or `experiment:<id>`, `ranking:<id>`) **before** its outcome. A ranking contract unsealed = Warn `ranking_unsealed` (a draft for the operator's review); the publisher runs only a sealed one.
3. Freezing a generation: set `status = "FROZEN"` + `frozen_at`, then append a `[[frozen]]` row with the digest `tengu lineage generation <id>` prints.

## Bind a sandbox to a generation

`[generation] id = "W1"`, `registry = "../../lineage"` in `sandboxes/<name>/config.toml`; the generation lists `<name>` in `sandboxes`. Every load then refuses a tool or strategy kind outside its capabilities, and a FROZEN generation whose lock or `config:` / `spec:` pins changed.

## Run a ranking contract

`[strategy_ranking] registry = "../../lineage"`, `contracts = ["<id>"]` in `sandboxes/<name>/config.toml` (the contract's `sandbox` = `<name>`). `tengu ranking run --sandbox <name>` (or the `strategy_ranking` tool, a feed) refuses the contract until `tengu lineage seal ranking:<id>` and after any later edit (`contract_changed`): a changed policy is a new id (`….v2`). Retention keeps every run the registry cites, bound or not. Doc: `docs/strategy-ranking-automation-2026-10-08.md`.

## Verify

```sh
tengu lineage verify                     # records, references, time, holdout, forward, locks
tengu lineage verify --pins --evidence   # + every pin and binding, every file hash and extracted result
tengu lineage report rule_w --forward <forward experiment>   # the 21 Rule-W answers
```

Exit 1 on any Error finding. Views: `show`, `trace`, `family`, `attempts`, `capabilities`, `generation` (`--format json`).
