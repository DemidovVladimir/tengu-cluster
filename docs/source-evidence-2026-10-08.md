# Source evidence — the cross-domain source layer (O2, 2026-10-08)

Facts read from approved public sources become **source records** with full provenance; a pure **as-of view** answers "what did the sources say at t?" without reading anything later. First sources: SEC EDGAR (company filings) and the EU TED Search API (public procurement notices). Consumers: the Software Opportunity Engine (`domain/soe/`, built separately) and later xmarket `info-*` — imports go soe → source, never back. Visual walkthrough: [`tutorial/source-evidence.html`](tutorial/source-evidence.html).

| Rule | Applied as |
|---|---|
| Facts, not guesses | `Inference` has no path into a `Fact`; it never counts toward confidence |
| Lossless | ids, hashes, CIKs (leading zeros) and amounts as written (`NativeAmount`: decimal text + ISO 4217); no float, no conversion here |
| Append-only | a correction, an edit or a withdrawal is a new record; nothing is rewritten |
| No lookahead | two clocks (§ 4); split-world checks prove nothing after t moves a packet at t |
| Operator fetches, agents read | `tengu sources fetch` / `import` only; the `source_evidence` tool has no fetch argument |
| Rights first | a row ships `enabled = false`; enabling needs reviewed terms + both retention periods (load error otherwise) |
| Source text is data | every free text reaches an LLM inside one fence, after one system note |
| Loss is visible | gaps, failed fetches, unparsed reads, withdrawals and purges show in the packet |

## 1. Registry — `[sources]` (`src/config/sources.rs`)

| Field | Rule (a violation fails `Config::load`; `deny_unknown_fields` everywhere) |
|---|---|
| `state` | one dir name: store `<TENGU_HOME>/state/<state>/sources.db`; the dir outside every `fs_roots` and agent `workspace` |
| row id | `[a-z0-9_]+` = the records' `source_id` |
| `kind` · `class` · `trust` · `revision` | `sec_edgar` \| `ted_search` · six classes, the class must allow the trust · `immutable` \| `in_place` (the knowable clock) |
| `enabled` · `hosts` · `auth` · `rate_limit` | `false` = listed, never fetched · each inside `[egress] allow_hosts`, none in `deny_hosts` · `none` \| `user_agent_env:<VAR>` \| `api_key_env:<VAR>` · names a `[rate_limits.<name>]` |
| `store_raw` · `jurisdiction` · `language` | keep raw bodies · `law_regulator`: an ISO 3166 code or `EU` |
| `license` · `terms_url` · `terms_sha256` · `terms_reviewed_at` | required when enabled: reuse terms, https URL, sha256 (64 hex) of the page the operator reviewed, `YYYY-MM-DD` |
| `raw_retention_days` · `record_retention_days` | required when enabled; `0` = forever |
| per kind · class | `sec_edgar`: `user_agent_env` auth, `forms` ⊆ the kept SEC forms, `entities` = `sec:cik:<10 digits>`; `ted_search`: `query` with `{from}` and `{to}` (each a day, `YYYYMMDD`); `registry_marketplace`: `listing_max_age_days` |

| `sandboxes/soe` row | Class · trust · revision | Hosts · auth · budget | Retention (proposed) |
|---|---|---|---|
| `sec_edgar` | `company_primary` · `primary` · `immutable` | `www.sec.gov`, `data.sec.gov` · `$SEC_USER_AGENT` · `[rate_limits.sec]` 300 / min, burst 5 | raw + records forever |
| `ted_search` | `law_regulator` (`EU`) · `primary` · `immutable` | `api.ted.europa.eu` · anonymous · `[rate_limits.ted]` 60 / min, burst 2 (assumed) | raw 90 days, records forever |

`sandboxes/soe`: `network = "open"`, `allow_hosts` = the three hosts, no `[generation]`, one agent `soe_reader` (`claude_code`, built-ins off, `tools = ["source_evidence"]`); `config::sources::tests::soe_sandbox_is_a_closed_world` pins it.

## 2. Record — `source_record/1` (`src/domain/source/record.rs`)

| Field | Value |
|---|---|
| `record_id` | `<source_id>:<native_id>:<content_hash>`, each part whole; the same content read twice = the same record |
| `content_hash` | sha256 of the canonical JSON of what the source said (ids, entities, url, publication and validity times, jurisdiction, language, currency, fact, origin, supersedes) — never fetch / parse metadata |
| `event_key` · `entities` | `sec:filing:<accession>`, `ted:procedure:<id>` (`ted:notice:<publication number>` without a procedure) · `sec:cik:<10 digits>`, `ted:buyer:<country>:<id>` |
| clocks | `published_ms` (the source made it public) · `observed_ms` (fetch of the newest raw snapshot it rests on) · `parsed_ms` (≥ observed) · `valid_from_ms` / `valid_until_ms?` |
| provenance | `source_class`, `trust`, `url`, `snapshots[]` (sha256), `parser_version` (`sec-submissions/1`, `ted-search/1`), `access_method`, `license_or_terms`, `terms_sha256`, `jurisdiction`, `language`, `currency?` |
| links | `supersedes` (the record a correction replaces) · `origin` (the source a syndicated copy came from) |
| `fact` | `sec_filing` · `ted_notice` · `unparsed` (parse failed) · `withdrawn` (`gone` \| `moved`) |
| `parse` | `ok` · `partial` (typed fact + errors) · `error` (`unparsed`) — never a dropped record |

## 3. Store — `sources.db` (`src/adapters/outbound/sources/store.rs`, port `src/ports/source_store.rs`)

| Table | Writes |
|---|---|
| `snapshots` (sha256 → kind `response` \| `terms`, url, status, size, body when `store_raw`) | insert or ignore; a raw purge drops the body, keeps hash + size |
| `records` + `record_entities` | insert or ignore only; deleted only by record retention |
| `coverage` (one row per fetch: span, complete, error class) · `cursors` | appended · the one mutable row, moved only inside a committed batch |
| `purges` (tombstones) · `switches` (runtime kill switch) | appended |

One `commit` = snapshots + records + coverage + cursor in one transaction: every record validated, every named snapshot known, every body hashing to its sha256 — or nothing. Mode 0600 for the file, its WAL and WAL index (created 0600 before SQLite opens it), `user_version` 2. Kill switch: the last appended `switches` row per source wins — append order, never the clock. A read at t seeds `min(published, observed) ≤ t`, then adds every version of each item and every record of each event (the knowable clock needs an item's first read; a correction may lack the entity).

## 4. As-of view — two clocks (`src/domain/source/asof.rs`)

| Record | Captured (what we had read by t) | Knowable (what was public by t) |
|---|---|---|
| first read of an `immutable` item, or a reparse of its bytes | `max(published, observed, parsed)` | `published` |
| an edit (new bytes, same id), any `in_place` item | same | `max(published, observed)` |
| a withdrawal | same | `max(published, observed)`, never back-dated |

| Step at t | Rule |
|---|---|
| current version | per item the newest read, then the higher parser version; older ones `superseded` by `revision` |
| corrections | `supersedes` removes its target only on the same event and when both are visible; else an issue |
| class rules (PRD §5.1) | `law_regulator` needs a code jurisdiction, `registry_marketplace` a fresh listing — a miss is an issue, never a drop; `customer_demand` under 2 distinct events is `single_signal` (one post is not demand) |
| states | in force · `pending` (before `valid_from_ms`) · `expired` (from `valid_until_ms`) · `withdrawn` · `unparsed` |
| confidence per event | `confirmed` (≥ 1 primary) · `corroborated` (≥ 2 origins) · `single` · `trigger_only` · `none`; a copy counts as its origin |
| conflicts | same event, kind and stage: SEC `cik` / `form` / `items`; TED `value` (as a number), `deadline_ms`, `cpv`, same notice type and lot set |

## 5. Evidence packet — `source_asof/1` (`src/domain/source/packet.rs`)

| Part | Holds |
|---|---|
| facts · pending · expired · withdrawn · unparsed | current records at t, filtered by source / entity / event / publication window `[from, to)` |
| superseded · events · conflicts · issues · demand | as § 4 |
| freshness | per source and query: last complete fetch, age, covered span, gaps, failed fetches, parse failures, purges — all at or before t |
| citations | url, content hash, snapshots, parser, terms of every record named |
| inferences | its own list; never a fact, never counted |

`render_text`: typed tokens printed whole outside the fence; titles, buyer names, reasons inside `<source-text record=… field=…>`; nested tags and copies of the note removed until none is left. `page(limit)` keeps the newest records and counts the rest as `omitted`.

## 6. Sources (`src/adapters/outbound/sources/{sec,ted}.rs`, parsers `src/domain/source/{sec_records,ted}.rs`)

| | SEC EDGAR (`sec_edgar`) | EU TED (`ted_search`) |
|---|---|---|
| Request | `SecClient` (shared with `tengu history events`): submissions + older pages per CIK, one index page per filing without a stored `ok` record | `POST /v3/notices/search`, anonymous: one Europe/Paris publication day, 21 allow-listed fields, pages of 250, `scope = ALL` |
| Record | one per kept filing; native id = accession | one per notice; native id = publication number |
| Published at | index page `Accepted` (New York → UTC); unread index ⇒ `partial`, a time never before the acceptance | end of the publication date in its own offset (TED gives a date only) |
| Corrections | an amendment is its own accession and event | a change notice `supersedes` the publication it names, else the newest earlier notice of the procedure with the same type and lot |
| Coverage · cursor key | `cik:<cik10>` · newest accession (moves only when the CIK read is complete) | `query:<sha256 of the template>` · newest day read whole |
| On failure | what was read is committed, coverage incomplete, the cursor stays; next CIK | same; later days are not read; `--from` omitted resumes after the cursor |

Hosts, audit (`sec_edgar` / `ted_search`) and attribution: [`egress-2026-09-16.md`](egress-2026-09-16.md) § Source hosts.

## 7. Operator CLI — `tengu sources` (`src/adapters/inbound/cli/sources.rs`)

| Subcommand | Does |
|---|---|
| `list` | rows (enabled, runtime switch, class / trust, retention, terms), cursors, newest fetch per query |
| `fetch --source <id> [--from] [--to] [--ciks]` | gate (kind, enabled, reviewed terms, not switched off) → fetch → commit; exit 1 on a failed row |
| `import --source ted_search --file <json> --observed-at <t>` | a saved TED reply read as if fetched at t; no coverage; captured mode sees it from the import |
| `asof --at <t> [--mode] [--source] [--entity] [--event] [--published-from] [--published-to] [--text]` | the packet as canonical JSON, or its fenced text |
| `terms --source <id> --file <page>` | stores the reviewed terms page only when its sha256 = `terms_sha256`; never purged |
| `purge [--source <id>]` | applies each row's retention now; prints each tombstone |
| `disable` · `enable --source <id> --reason <text>` | the runtime kill switch: off refuses every fetch and import at once; `enable` never lifts a TOML `enabled = false` |

## 8. Agent tool — `source_evidence` (`src/adapters/outbound/tools/sources/`)

| Aspect | Value |
|---|---|
| Args (strict) | `at` (default now, never after now) · `mode` `captured` \| `knowable` · `source` · `entity` · `event_key` · `from` · `to` · `limit` (1–50, default 10) |
| Row | `source_asof/1:<source\|all>:<event\|entity\|all>:<at ms>`, ttl 0 (never cached); status `ok` · `partial` (failed fetch, gap, unparsed / partial record) · `absent` (no record) |
| Text | line 1 + features + ≤ 8 error lines, then the packet's fenced text; ≤ 6 000 bytes (fewer records, the rest `omitted`) — never the `data` JSON |
| Refusals | no `[sources]` (`sources_state_missing`), an unknown key / source, a bad time or limit — before any read; no `sources.db` yet ⇒ an empty packet, nothing created |
| Gate | opt-in (`WORKSPACE_TOOLS`); scope `fs_roots` = the workspace (observation store); no host, no env |
| Parity | bridge conformance `source_evidence:asof` + `:no_sources`; engine-matrix set `sources` (offline local leg; live legs per engine); capability `lineage/capabilities/intel.source_evidence.toml` (CANDIDATE, READ_ONLY, no generation) |

## 9. Rights, retention, PII

| Concern | Rule |
|---|---|
| Terms | no fetch or import without `license` + `terms_sha256` (`SourceEntry::fetch_stamp`); every record carries both |
| Retention | `raw_retention_days` drops raw bodies (hash + size stay; terms pages never); `record_retention_days` deletes records — a later replay of an earlier t misses them; every purge that removed something leaves a tombstone, shown in the packet from its time on |
| Kill switch | `tengu sources disable` appends a `switches` row; the TOML stays the ceiling |
| Contact data | TED asks for no contact field (person, email, phone, address) and the decoder reads only its allow-list; buyers are organisations |
| Credentials | SEC's User-Agent is a request header only — never in a snapshot, record, report or audit line; TED is anonymous |
| Reach | `sources.db` (+ `-wal`, `-shm`) 0600, its dir outside every fs root and workspace; agents read it only through `source_evidence` |

## 10. Tests

| Test | Guards |
|---|---|
| `cargo test --bin tengu domain::source::checks` | split-world no-lookahead: three worlds, every move after t (delete, next ms, flood, coverage, reparse, purge), both clocks |
| `cargo test --bin tengu domain::source::eval` | 12 dated replay cases (`tests/fixtures/sources/eval/cases.json`, regenerate with `TENGU_REGEN_SOURCE_EVAL=1`): byte-identical packets; copies never confirm |
| `cargo test --bin tengu sources::` | store append-only + reads = whole-store packet; SEC / TED fetch, resume, partial rules; User-Agent never stored; CLI import → asof deterministic |
| `cargo test --bin tengu config::sources` | load rules; the example block; `soe_sandbox_is_a_closed_world` |
| `cargo test --test bridge_conformance` · `--test engine_matrix offline_local_sources` | same text + store rows in-process and through the bridge; a 16k local model gets each read whole |

## 11. Open

| Item | State |
|---|---|
| Operator: terms wording + hashes, retention, `[rate_limits.ted]`, SEC class (`company_primary` vs `law_regulator`), TED query scope | proposed in `sandboxes/soe`; rows stay disabled until signed |
| Tor | whether SEC / TED accept Tor exits is unverified ⇒ `network = "open"` |
| Withdrawals | no fetcher re-checks stored items, so none produces `withdrawn` yet |
| TED change notice read before its target | no `supersedes` link; days read whole are skipped — needs a relink pass or TED's notice identifier |
| Registry listing freshness | taken from the source's newest complete fetch (any query) |
| Text bound | fixed 6 000 bytes (≈ 5 SEC / TED records per call) on every engine |
| Live engine-matrix legs | `openrouter_*_sources`, `claude_code_sources`, `local_sources` not run yet |
| O2 exit | 12 source-level cases ship; opportunity-level cases wait for O0 |
| Scheduled fetch · more sources | none (`[feeds] kind = "poll"` reserved) · Companies House (needs a key), GitHub Events later |
