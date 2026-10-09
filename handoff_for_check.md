# Handoff for check — 2026-10-08 → 2026-10-09 session

> For the operator's architecture agent. Everything below is verifiable from git, GitHub and the commands at the end. Ids and hashes are full.

## 1. What was asked → what was delivered

| Ask (operator, 2026-10-08) | Delivered | Where |
|---|---|---|
| Deep-compare `docs/software-opportunity-{prd,roadmap,compatibility}-2026-10-04.md` + `docs/strategy-ranking-automation-2026-10-08.md` with the code; implement what is lacking (Rust, project rules) | Strategy ranking SR-1…SR-8; Software Opportunity Engine O0–O4 (stops at Operator Review #2) | PRs #40, #41, #43 |
| Update every doc so the code is the source of truth (tutorial + agent markdown) | Two doc sweeps (A: README, architecture, context, research, memory/skills, 22 tutorial pages; B: CLAUDE.md/AGENTS.md, handoff, hub, code map, runtime/tool docs, remaining tutorial pages, feature docs) | PRs #42, #45 |
| Implement `TENGU_STUDIO_PLAN.md` with workflows; validate `control-loop-lab` via Studio | ST-00…ST-40 (+ ST-90 clean room); live validation with real Jev + screenshots | PR #44, `docs/studio-evidence/`, PR #45 |
| PR + merge everything into main, no review wait; no stale branches / PRs left alone | 7 PRs squash-merged, 0 open PRs; this session's branches deleted (tips kept as `archive/*` tags) | § 4 |
| A handoff with all commits, PRs and branches | this file | repo root |

## 2. Merge model

| Rule | Applied |
|---|---|
| Work isolation | every lane in its own git worktree under `.claude/worktrees/` with its own `CARGO_TARGET_DIR` (`~/.cache/tengu-xm.noindex/agents/<lane>`); worktrees + target dirs removed after merge |
| Merge | one PR per feature, CI `rust-quality` green (fmt, check --all-features, clippy, `cargo test --workspace`, + `cargo test --features studio --bin tengu studio` added in #44), squash-merged |
| Reviews | every lane ended with an adversarial review agent (fixes committed in the same branch); integration merges re-ran the full suites |
| Weekend freeze | **the local main checkout was NOT pulled** (it is at `10bdbb53a27390f6a67fd73826740befe05ababe` + a 1-line local runbook edit) — see § 6 |

## 3. Pull requests (all squash-merged into `main`; 0 open PRs)

| PR | Title | Squash commit on main | Merged (UTC) |
|---|---|---|---|
| #39 | xlab-w2: keep every backtest run dir | `10bdbb53a27390f6a67fd73826740befe05ababe` | 2026-10-08T17:09:12Z |
| #40 | Strategy ranking: sealed contracts, deterministic ranker, publisher, xlab-w2 feeds | `117b2595b0864066502d156aca42c7180494d67a` | 2026-10-08T20:48:47Z |
| #41 | SOE O0–O2: opportunity domain, deterministic economics + gates, source evidence layer (SEC, TED) | `bc9bc12a038654ac4941c714abcfa777c99af631` | 2026-10-08T21:07:29Z |
| #42 | Docs sweep A | `e84e886c6d3d4dc985e408991fe66d1773140c01` | 2026-10-09T01:54:09Z |
| #43 | SOE O3/O4: weekly portfolio cycle, replay + grading, Review #2 packet, soe tools, feed job, SOE-G0 | `7cae230f61eddcc014b94521564b15a7b3ac817f` | 2026-10-09T07:24:11Z |
| #44 | Tengu Studio + control-loop-lab (TENGU_STUDIO_PLAN ST-00…ST-40) | `f7ac9a5636e2493e3839e896cba9951a2d41aefb` | 2026-10-09T08:18:13Z |
| #45 | Docs sweep B + ST-90 clean room + this handoff | see `git log origin/main -1` after merge | 2026-10-09 |

### 3.1 Branch commits per PR (full ids; branch tips kept as `archive/<branch>` tags)

## PR #39 — xlab-w2: keep every backtest run dir

| Field | Value |
|---|---|
| URL | https://github.com/DemidovVladimir/tengu-cluster/pull/39 |
| Branch | `fix/xlab-w2-retention` — deleted after merge; tip kept as tag `archive/fix/xlab-w2-retention` |
| Squash commit on main | `10bdbb53a27390f6a67fd73826740befe05ababe` |
| Merged at (UTC) | 2026-10-08T17:09:12Z |

Branch commits (1):

- `7f508389dc56907eacf5857faeac4bf2ccd4a6d2` xlab-w2: keep every backtest run dir (keep_runs = 0)

## PR #40 — Strategy ranking: sealed contracts, deterministic ranker, publisher, xlab-w2 feeds

| Field | Value |
|---|---|
| URL | https://github.com/DemidovVladimir/tengu-cluster/pull/40 |
| Branch | `feature/strategy-ranking` — deleted after merge; tip kept as tag `archive/feature/strategy-ranking` |
| Squash commit on main | `117b2595b0864066502d156aca42c7180494d67a` |
| Merged at (UTC) | 2026-10-08T20:48:47Z |

Branch commits (9):

- `caacdc0be446ce9d7628beacbc31ef0992855c1f` Strategy ranking SR-1/SR-2 prerequisites: ranking contracts, report cohort identity, [strategy_ranking]
- `42584f85ca739b7ee6a0df408726e05488d5deec` Strategy ranking SR-2/SR-3: pure selection + ranker (domain/backtest/ranking.rs)
- `5a4682e8f0c7fce262f94895e2fe9b134f71e4ae` Strategy ranking SR-5: coordinator (application/ranking) + `tengu ranking run|show`
- `7c7a7cdc5df01f1b117abbc8b0c3a30dcff5b78d` config/xmarket.rs: the strategy-rankings row names its writer (application/ranking/) and files
- `6573e24f44da1eb35ff2f7930348cbd884fa1239` Strategy ranking SR-4 + SR-7: `strategy_ranking` tool on every engine + binary acceptance
- `6a99fe212062036833b8a4bb28e3a6457573262d` Strategy ranking SR-6: xlab-w2 runs the daily + weekend rankings (unsealed, unbound)
- `e84f438ef9a545a242c1576c4bcaa5e1491b02fe` docs: strategy-ranking automation doc, as built (SR-8)
- `dc466c170521e68b716392db8e89db429f68264a` Strategy ranking SR-8: tutorial page, agent guides and operator docs as built
- `88b86dde45227dade1d67e2f60c5716041c7747a` Strategy ranking review: as-of standing, lost-lease writes, published-only show

## PR #41 — SOE O0–O2: opportunity domain, deterministic economics + gates, source evidence layer (SEC, TED)

| Field | Value |
|---|---|
| URL | https://github.com/DemidovVladimir/tengu-cluster/pull/41 |
| Branch | `feature/soe-o0-o2` — deleted after merge; tip kept as tag `archive/feature/soe-o0-o2` |
| Squash commit on main | `bc9bc12a038654ac4941c714abcfa777c99af631` |
| Merged at (UTC) | 2026-10-08T21:07:29Z |

Branch commits (17):

- `740e1e97fda76df3d41d8167c184a7d89bcfd29c` O2 record model: domain::source pure source records + provenance
- `872de9dbb2b69fc3fbb229aa52d1ed23e3f3de4a` SOE O0: public contract, sources and threat model doc
- `ef4a6a70aaf660a4e359cf9aec41a1045b93fcba` SOE O1: domain/soe value types — exact money, FX, bps, Est, schema tags
- `4d2827c8502483a690d94c4c24c91f16d4637560` SOE O1: versioned records + private profile / record-dir loader
- `a798640a1bf06fa3dd9c1b5d1c4f0797b9d047c7` O2 sources: [sources] registry + soe sandbox, as-of view, evidence packet, split-world checks
- `6c0fe6d3dfccc100abefed83cf074849fe1c6203` SOE O1: deterministic economics + PRD § 7.2 hard gates
- `8172f4ee713cefa8d1b158131b6ff94332444114` O2 sources: append-only sources.db store + SEC EDGAR source keyed by CIK
- `32ba0f1825b4ba5520de70092b28c70bfcb3ec7d` SOE O1 W5 + O0 W8: ranking primitives, capability fit, HOLD week + dated eval set
- `e3aaf7dc72229b5cf7b4dfebe8eb9e7a919c8f6a` SOE O1 W7 + W9: offline `tengu soe init|check|portfolio|sensitivity|eval`
- `faa8d238f044d7888e21065cea409aab42549dfb` O2 sources: EU TED Search source + `tengu sources` CLI with a runtime kill switch
- `5209f6470ee1a82bf35caab7228b0b727e9bcb8d` O2 sources: read-only `source_evidence` tool + 12 dated replay cases (O2 exit)
- `982fd19c24f7ca4bc79c03c949594d5e425c41cd` O2 sources docs: source-evidence doc, tutorial picture end to end, tool + config rows
- `48e2454e7c8521b9d9f620027d1ad692cb616760` review-C: sources.db WAL/SHM owner-only; kill switch by append order
- `206e22f345a256fe8414b6c646bf563a44b3e876` SOE O1 review: void experiments, payback scan bound, atomic init
- `9e199b77e88e1ce5c2767525e4939fffe1beed26` Merge branch 'feature/soe-sources' into feature/soe-o0-o2
- `4e3b2a47e7bce2ff770ed06ea241130f3ce06501` soe init: the four money values are REQUIRED placeholders, never in public source
- `e0951c001170ed2da1eac5eab0153e1c31647026` Merge remote-tracking branch 'origin/main' into feature/soe-o0-o2

## PR #42 — Docs sweep A: docs and tutorial pages match the code (README, architecture, context, research, memory/skills, 22 tutorial pages)

| Field | Value |
|---|---|
| URL | https://github.com/DemidovVladimir/tengu-cluster/pull/42 |
| Branch | `docs/sweep-2026-10-08` — deleted after merge; tip kept as tag `archive/docs/sweep-2026-10-08` |
| Squash commit on main | `e84e886c6d3d4dc985e408991fe66d1773140c01` |
| Merged at (UTC) | 2026-10-09T01:54:09Z |

Branch commits (7):

- `7fcbacd93d6bfd84c525c5e4a62f0d7299fdc927` docs sweep: readme — catalog count 47, lineage/ranking/sources/soe commands + sandboxes, config sections, module map
- `31c0929d0e5e0ba7dbcfd7a028021a0c866acefb` docs sweep: architecture — lineage/evidence, ranking, sources, SOE subsystems; executor abort + leaf join, retry backoff, memory port
- `efde4875f770ef69c093dbab074ef493efd8003e` docs sweep: context — mechanism count, compress_and_store paths + line refs, budget floor, comparison rows for lineage/ranking/sources
- `1ff4df8b2c3bdc906c61f3088ec29305caa77ad1` docs sweep: research — P0–P11 status, registry counts, xlab-w2 + SEC events, evidence verify/evaluate flags, no-pull note for weekend #2
- `d09f905b7015b8fbacae70c0d08fe45414c69897` docs sweep: memory-skills — skill gating/tiers/editability, learning-agent caveat, conformance + engine-matrix counts, engine recipe paths
- `70efdffad9fc7bed6a5bd72c006359f3d3f75b5d` docs sweep: tutorial-1 — beyond-chat commands, soe egress, generation tool filter, offline commands skip the vault
- `999cb7a8f2bacf2292da2ca1e2f59eb69f584180` docs sweep: tutorial-2 — every top-level command, soe + xlab-w2 sandboxes, lineage/backtest/channel fixes

## PR #43 — SOE O3/O4: weekly portfolio cycle, replay + grading, Review #2 packet, soe tools, feed job, SOE-G0

| Field | Value |
|---|---|
| URL | https://github.com/DemidovVladimir/tengu-cluster/pull/43 |
| Branch | `feature/soe-o3-infra` — deleted after merge; tip kept as tag `archive/feature/soe-o3-infra` |
| Squash commit on main | `7cae230f61eddcc014b94521564b15a7b3ac817f` |
| Merged at (UTC) | 2026-10-09T07:24:11Z |

Branch commits (29):

- `740e1e97fda76df3d41d8167c184a7d89bcfd29c` O2 record model: domain::source pure source records + provenance
- `872de9dbb2b69fc3fbb229aa52d1ed23e3f3de4a` SOE O0: public contract, sources and threat model doc
- `ef4a6a70aaf660a4e359cf9aec41a1045b93fcba` SOE O1: domain/soe value types — exact money, FX, bps, Est, schema tags
- `4d2827c8502483a690d94c4c24c91f16d4637560` SOE O1: versioned records + private profile / record-dir loader
- `a798640a1bf06fa3dd9c1b5d1c4f0797b9d047c7` O2 sources: [sources] registry + soe sandbox, as-of view, evidence packet, split-world checks
- `6c0fe6d3dfccc100abefed83cf074849fe1c6203` SOE O1: deterministic economics + PRD § 7.2 hard gates
- `8172f4ee713cefa8d1b158131b6ff94332444114` O2 sources: append-only sources.db store + SEC EDGAR source keyed by CIK
- `32ba0f1825b4ba5520de70092b28c70bfcb3ec7d` SOE O1 W5 + O0 W8: ranking primitives, capability fit, HOLD week + dated eval set
- `e3aaf7dc72229b5cf7b4dfebe8eb9e7a919c8f6a` SOE O1 W7 + W9: offline `tengu soe init|check|portfolio|sensitivity|eval`
- `faa8d238f044d7888e21065cea409aab42549dfb` O2 sources: EU TED Search source + `tengu sources` CLI with a runtime kill switch
- `2e6b78fb5e93d63a1cdf49844cfb876f4d81397c` Merge branch 'feature/soe-sources' into feature/soe-o3
- `86beb09e19db5843135e2a7ed96568acb98c09a8` SOE O3/O4 pure domain: observe, proposals, challenges, allocation, memo, forecast log, review packet, ops
- `f32f54fbe55102b1b0caa2e07294c3592c8d6c34` SOE O3 ports: stage runner + cycle store
- `3853f14bbdf7d33d5802bbd55d012ee1e96dd92c` SOE O3 weekly cycle: run_cycle over the ports + synthetic cycle cases
- `8617286770cbd001d592f202521f8312c1c75346` SOE O4: replay, operator grades, forecast resolution, Operator Review #2 packet
- `d65b30cb4615542ba818a973c9aa432c11ac8f66` SOE O3 infra: [soe] section, cycle adapters, feed kind `job` + soe_cycle job, SOE state lease
- `a841dbf6ee1470a6e03ff28b537f6a5634b0bb9e` Merge branch 'feature/soe-o3-infra' into feature/soe-o3
- `35a29bd4b3798a761f008651fd6c7723ddbf0f0d` Merge branch 'feature/soe-o3' into feature/soe-o3-infra
- `ed34247824f4c63a881893744ec5e8195069dcfe` Merge branch 'feature/soe-o3' into feature/soe-o3-infra
- `cc660b53d5009a44a5f6389d5e00ec1adeebc978` SOE O4 CLI: tengu soe cycle|replay|grade|resolve|review|verify|show
- `14b2dbac5d67ec72cb503a9179d0485d8db1e405` SOE O3 stage tools, sandbox cycle, skills, SOE-G0 + § 13 guards
- `2c221e8093eed294a3ea7dd59aab1fd45013b6c1` Merge branch 'feature/soe-o3' into feature/soe-o3-infra
- `dc22415ebcfbed988e776945759e7302c3c85957` SOE O3/O4 docs: the operator's path to Review #2, gates, invariants → tests
- `35598f0b18e085d2ab687ad93d3645910d5f6401` SOE O3/O4 review fixes: holdout count first, unknown tokens, log resume, cycle lease, cache key
- `c6bf1b467eab9e7faee5c6abd1d07f710e0bca36` SOE O3/O4 review: freeze manifest and stage-cache recordings whole or absent
- `a547fc64fc7d0f2f09e354dfdb77001514311818` Merge origin/main (strategy ranking #40, reviewed SOE O0–O2 #41, docs sweep #42) into SOE O3/O4
- `cd366e64122879501970ade16325b0222a2ba040` SOE-G0 lists intel.source_evidence: the soe sandbox's reader loads again
- `13100f3c0de9c619930ce1bbac30f9f24894a96a` SOE docs: init's template leaves the four money values REQUIRED (no PRD figures)
- `8fac057ab6b6f10b02e10a2d420e36b96ec14a06` SOE doc: O2 row and § 15 step 1 — main merged in, SOE-G0 lists source_evidence

## PR #44 — Tengu Studio + control-loop-lab: workflow graph, execution trace, local browser UI, replay, Play/Stop (TENGU_STUDIO_PLAN ST-00…ST-40)

| Field | Value |
|---|---|
| URL | https://github.com/DemidovVladimir/tengu-cluster/pull/44 |
| Branch | `feature/studio` — deleted after merge; tip kept as tag `archive/feature/studio` |
| Squash commit on main | `f7ac9a5636e2493e3839e896cba9951a2d41aefb` |
| Merged at (UTC) | 2026-10-09T08:18:13Z |

Branch commits (23):

- `52cad0cf2badb4ec9a11457e862775dc242575a6` Studio ST-00..ST-02: Phase 0 audit, control-loop-lab sandbox, runbook
- `02c13268b2961086315c9b2c53d45aaffebe1602` Studio tracker: ST-00, ST-01, ST-02 done (52cad0cf2badb4ec9a11457e862775dc242575a6)
- `7062b4c6bbe302b78f76d13b7c33b5af5aa6896a` Studio ST-03: baseline real-Jev run of control-loop-lab; Gate 1 waived
- `285826452d8a160c542274ceeeda47695fa66072` Studio tracker: ST-03 done, Gate 1 waived (7062b4c6bbe302b78f76d13b7c33b5af5aa6896a)
- `dfec6877cabf2d9e1e533ca0ff714a3a4bd1f506` Studio ST-10 + ST-11: workflow graph read model, execution trace store
- `9b956fa4fb2ea748d0c32a429bdc964f29d6701c` Studio tracker: ST-10, ST-11 done (dfec6877cabf2d9e1e533ca0ff714a3a4bd1f506)
- `d6438a4294a74d1be4b3aa65d5722b7d29c3d5eb` Studio ST-12: instrument the lab path end to end; Gate 2 evidence
- `5a01d657b0a80e4234065f1918a6119cf96f7475` Studio tracker: ST-12 done, Gate 2 waived (d6438a4294a74d1be4b3aa65d5722b7d29c3d5eb)
- `c9eeabc1c94e28d0e9819bd94e5126cf095b7c40` Studio review ST-10..ST-12: scrub keys + graph URLs, one pass/fail rule, close stopped feed runs
- `5fdd82c110a9c7f26707fe8f18c5855804c77044` Studio tracker: ST-10..ST-12 review fixes (c9eeabc1c94e28d0e9819bd94e5126cf095b7c40)
- `1d8223fd4ed2c0384c20db27bee45625fb71c636` Studio ST-20 (1/2): Studio-web exception to Rust only + tests/language_policy.rs
- `fe5115bc31c0bb381ccb4ea80998cba459ccbf2a` Studio ST-20 (2/2): tengu studio — local read-only server, API + SSE
- `77bf750a4a53e3113a9bb53cf37aae46c6a93af6` Studio tracker: ST-20 done (fe5115bc31c0bb381ccb4ea80998cba459ccbf2a)
- `a0ecdb5490975eeb61865047186f73dbb18a0d9c` Studio ST-21 + ST-22: graph, inspector, timeline, runs + replay
- `3e6cf7898da8de8651798f8d9df60db4d5471da0` Studio tracker: ST-21, ST-22 done, Gate 3 waived (a0ecdb5490975eeb61865047186f73dbb18a0d9c)
- `b1d51fc05801a1bb29e654b21d94e3d55a45c26b` Studio ST-30 + ST-31: Play / Attach / graceful Stop through the tengu run start; control hardening
- `658589d05e0022c4b4ebb1efc0118874bd96c8d4` Studio tracker: ST-30, ST-31 done, Gate 4 waived (b1d51fc05801a1bb29e654b21d94e3d55a45c26b)
- `f56b1b45325c3a94a002569337f53512c41d335e` Studio ST-40 (editor design only) + operator docs for ST-20..ST-31
- `ffa850b2a10da71ba4287a307190fa57e005d978` Studio tracker: ST-40 done (design only), ST-90 docs landed (f56b1b45325c3a94a002569337f53512c41d335e)
- `7a69f79e51c46b54cfdaa0123da5b18c00610d7e` Studio review ST-20..ST-40: path-independent CSRF guard, served verdict tone, bounded control names, server logs like tengu run
- `9fee64f2a3cc22cb6f3a2117a27b28f5b6d1dcd0` Studio tracker: ST-20..ST-40 review (7a69f79e51c46b54cfdaa0123da5b18c00610d7e) — live guards, drain, kill -9
- `7217b209a9d09cea69d508b0cd5d2e1a4cb99586` Merge origin/main into feature/studio (#39–#43: keep_runs, strategy ranking, SOE O0–O4, docs sweep)
- `0ff7fcb6fa7d5d355709a70ae8a42ade93a013fe` config.example.toml: [studio] note names [soe] among hardened sandboxes

## PR #45 — Docs sweep B + ST-90 clean room + handoff

| Field | Value |
|---|---|
| Branch | `docs/sweep-b-2026-10-09` — deleted after merge; tip kept as tag `archive/docs/sweep-b-2026-10-09` |

Branch commits (the last one adds this file):

- `37abe0ce62083ea83578e3aa8c7b91078c14691e` docs: studio validation screenshots (run cbd8cd05-30fb-4177-aa99-0b978f0e83e5)
- `f23db47766f28035d7cea2e982ff8fb9299bcb6c` docs: agent guides (CLAUDE.md + AGENTS.md) match main #39-#44
- `ea1df6ea1897e60dae02d03e33d7751d988df64e` docs: handoff + docs hub for #39-#44
- `1541725793c5e4d724922321166092527e419645` docs: code map, configuration, tools, engines, bridge, skills match main
- `6719b8fdab3312a639d61fced786a79bb51009f1` docs: runtime, decision loops, observations, egress, webhooks, lping match main
- `0dacf40d832e43d5a16958c67e5152d545679713` docs(tutorial): tools, lineage, ranking, sources, soe, studio pages match main
- `eb2a2d401b3756f8c01589f0a7a7e1de03f59b1b` docs(tutorial): nav, index, big picture, sandboxes, ops, secrets, egress, metrics, observations match main
- `88460f661fa1b16f24fb9f2ac4a0a8694b834062` docs: README, architecture, comparison match main #39-#44
- `dac8d9c7689750d775e19ff1acf33f145b008435` docs: Studio, lab, ranking, lineage, xlab, SOE, source docs match main
- `5234bdd992f06988255e812ecc2929bbbc8ac729` Studio ST-90: clean-room acceptance passed; its 8 doc fixes applied
- this file: `handoff_for_check.md` (commit message "handoff_for_check.md: commits, PRs, branches, gates for the architecture check")

## 4. Branches and tags

### 4.1 Deleted (this session's branches, all merged)

| Branch | PR | Tip (kept as tag `archive/<branch>`) |
|---|---|---|
| `fix/xlab-w2-retention` | #39 | `7f508389dc56907eacf5857faeac4bf2ccd4a6d2` |
| `feature/strategy-ranking` | #40 | `88b86dde45227dade1d67e2f60c5716041c7747a` |
| `feature/soe-o0-o2` | #41 | `e0951c001170ed2da1eac5eab0153e1c31647026` |
| `docs/sweep-2026-10-08` | #42 | `999cb7a8f2bacf2292da2ca1e2f59eb69f584180` |
| `feature/soe-o3-infra` | #43 | `8fac057ab6b6f10b02e10a2d420e36b96ec14a06` |
| `feature/studio` | #44 | `0ff7fcb6fa7d5d355709a70ae8a42ade93a013fe` |
| `docs/sweep-b-2026-10-09` | #45 | its tip (tag `archive/docs/sweep-b-2026-10-09`) |
| local only, never pushed: `feature/soe-domain` (tip `206e22f345a256fe8414b6c646bf563a44b3e876`), `feature/soe-sources` (tip `48e2454e7c8521b9d9f620027d1ad692cb616760`), `feature/soe-o3` (tip `cc660b53d5009a44a5f6389d5e00ec1adeebc978`) | via #41 / #43 | contained in the archived tips `archive/feature/soe-o0-o2` and `archive/feature/soe-o3-infra` |
| `feature/remove-aura` + worktree `.claude/worktrees/jev-chains` + its 8.8 GB target dir | merged earlier as #30 (`9f0e98f96704eca960575d6c453c5dabb5487d65`; tip `8c169b924e5fba2cf8d30cd6c0bcecc28b9d18b5` had no unmerged diff) | — |

### 4.2 Archive tags pushed to origin (every branch tip existing on 2026-10-09)

| Tag | Commit |
|---|---|
| `archive/backup/docs-pipeline-html` | `a3b3d22b9b2084fdbe7bca55787388a44443ed6a` |
| `archive/backup/phase-0-baseline-and-config-scaffold` | `038d609aed106eb3ecac97e7500d9f25a7484193` |
| `archive/backup/refactor-hexagonal` | `d502565cc7a4af5f52b42d0723e1c3d8ee3c202e` |
| `archive/backup/stash-2026-03-22-event-bus-memory-switch` | `7e4855c13a86fc8ffbf910953c33a3794f96a9ad` |
| `archive/backup/stash-2026-04-20-phase-b-orchestration-collapse` | `6459966c7d0ef26986e52d7af5f4d09b569994d9` |
| `archive/docs/sweep-2026-10-08` | `999cb7a8f2bacf2292da2ca1e2f59eb69f584180` |
| `archive/evolve-skill-creator-2026-04-21T13-56-05Z` | `f16d2efdaaf0955cf4e426a0c38a7fc523d51e94` |
| `archive/feature/execution-map` | `07fca913da947786a06728c46b336fab5669e4bc` |
| `archive/feature/forward-evidence` | `c5e3b719cd3b4788ea90df28db2c0ccc4e489310` |
| `archive/feature/p6-candidate-table` | `0f0f0a2b154dceecf2facf72c76ef8267cb943e6` |
| `archive/feature/p6-decision-eval` | `e989fe011c9495cdeb9c9dbe7a585e751744d5ba` |
| `archive/feature/p7-news-labels` | `0c8eb49df908196bafff34489acbda19e33ac292` |
| `archive/feature/phase-b-orchestration-collapse` | `372a020b47a1ef0770a55625f79f7607004f7ef4` |
| `archive/feature/soe-o0-o2` | `e0951c001170ed2da1eac5eab0153e1c31647026` |
| `archive/feature/soe-o3-infra` | `8fac057ab6b6f10b02e10a2d420e36b96ec14a06` |
| `archive/feature/strategy-ranking` | `88b86dde45227dade1d67e2f60c5716041c7747a` |
| `archive/feature/studio` | `0ff7fcb6fa7d5d355709a70ae8a42ade93a013fe` |
| `archive/feature/w1-lineage` | `492ef59eaa1a0e4b1515ec8c4061cd4055c9978d` |
| `archive/fix/code-findings-and-tutorial` | `aa5240eee48f05a054b07158e63491dc59e1049f` |
| `archive/fix/code-findings-gaps-1` | `723fc48133852dc529b3c88f18542b070fdfa657` |
| `archive/fix/xlab-w2-retention` | `7f508389dc56907eacf5857faeac4bf2ccd4a6d2` |
| `archive/fwd/rule-w-2026-10-09` | `9101646f8f5ffe228bd3135cdb1f295db9825f5b` |
| `archive/weekend/xmarket-weekend-2026-10-02` | `e5daae83febea00e549ce9b8cac83bddce12a0a2` |
| `archive/xlab/weeknight-fade-test` | `868dabab6edb253125040a09c134e81c91b00ca4` |

Lineage check before tagging: all 12 full commit ids cited by `lineage/` + `docs/SESSION_HANDOFF.md` + `docs/w1-*.md` are on `main` or inside an existing tag (`w1-forward-2026-10-02`, `w1-forward-config-2026-10-02`, `w1-weeknight-specs-2026-10-03`), so no cited commit depends on a branch.

### 4.3 Still present — OPERATOR DECISION (older than this session)

Bulk deletion of these was **blocked by Claude Code's auto-mode classifier**, so they were left for the operator. Every tip is already preserved as `archive/<name>` (§ 4.2).

| Remote branches on origin (17) |
|---|
| `backup/docs-pipeline-html` · `backup/phase-0-baseline-and-config-scaffold` · `backup/refactor-hexagonal` · `backup/stash-2026-03-22-event-bus-memory-switch` · `backup/stash-2026-04-20-phase-b-orchestration-collapse` · `evolve-skill-creator-2026-04-21T13-56-05Z` · `feature/forward-evidence` (#36) · `feature/p6-candidate-table` (#34) · `feature/p6-decision-eval` (#33) · `feature/p7-news-labels` (#35) · `feature/phase-b-orchestration-collapse` (#5, closed unmerged) · `feature/w1-lineage` (#32) · `fix/code-findings-and-tutorial` (#31) · `fix/code-findings-gaps-1` (#38) · `fwd/rule-w-2026-10-09` (#37) · `weekend/xmarket-weekend-2026-10-02` (#25) · `xlab/weeknight-fade-test` (#26) |

Local branches in the main checkout (11): `feature/execution-map`, `feature/forward-evidence`, `feature/p6-candidate-table`, `feature/p6-decision-eval`, `feature/p7-news-labels`, `feature/w1-lineage`, `fix/code-findings-and-tutorial`, `fix/code-findings-gaps-1`, `fwd/rule-w-2026-10-09`, `weekend/xmarket-weekend-2026-10-02`, `xlab/weeknight-fade-test`.

To delete them (tips stay reachable through the `archive/*` tags):

```bash
cd ~/development/tengu-cluster
git push origin --delete backup/docs-pipeline-html backup/phase-0-baseline-and-config-scaffold backup/refactor-hexagonal backup/stash-2026-03-22-event-bus-memory-switch backup/stash-2026-04-20-phase-b-orchestration-collapse evolve-skill-creator-2026-04-21T13-56-05Z feature/forward-evidence feature/p6-candidate-table feature/p6-decision-eval feature/p7-news-labels feature/phase-b-orchestration-collapse feature/w1-lineage fix/code-findings-and-tutorial fix/code-findings-gaps-1 fwd/rule-w-2026-10-09 weekend/xmarket-weekend-2026-10-02 xlab/weeknight-fade-test
git branch -D feature/execution-map feature/forward-evidence feature/p6-candidate-table feature/p6-decision-eval feature/p7-news-labels feature/w1-lineage fix/code-findings-and-tutorial fix/code-findings-gaps-1 fwd/rule-w-2026-10-09 weekend/xmarket-weekend-2026-10-02 xlab/weeknight-fade-test
```

The `mol` remote (another repository) was not touched.

## 5. Local state (operator's Mac)

| Item | State |
|---|---|
| Main checkout `~/development/tengu-cluster` | **at `10bdbb53a27390f6a67fd73826740befe05ababe` (#39), NOT pulled** — see § 6. One tracked local edit: `docs/forward-evidence-runbook-2026-10-08.md` (+1 no-pull row). Untracked: `TENGU_STUDIO_PLAN.md`, `docs/strategy-ranking-automation-2026-10-08.md` (both tracked upstream now; local copies = the upstream text), `handoff_for_check.md` (this file), the 3 private `docs/software-opportunity-*-2026-10-04.md` (status sections updated locally, never committed — the repo is public), the 2 `docs/crypto-opportunity-*` research docs (untouched) |
| Worktrees under `.claude/worktrees/` | none left (all removed after merge) |
| Build dirs `~/.cache/tengu-xm.noindex/agents/*` | removed after each merge; `seed` refreshed 2026-10-08; `weekend/` (frozen binaries) untouched |
| `~/.tengu` | untouched by this work (`logs/decisions.jsonl` sha256 `7deaacb257f43d3dc3e622f6d08adddad72e24f7cc812f7722a98ec25e0039d5` before and after every lab run) |
| `~/tengu-lab/` | lab data: `home/` (clean-room traces), `home-validation-2026-10-09/` (traces behind `docs/studio-evidence/`), `control-loop-lab/` (lab workspace, empty `in/` `out/`) — registered in `docs/SESSION_HANDOFF.md` local-data table |

## 6. Weekend #2 freeze — do this after Monday's stop

The forward run (Fri 2026-10-09 19:30 ET → Mon 2026-10-12) runs the frozen binary `~/.cache/tengu-xm.noindex/weekend/tengu-acdef66` from the main checkout. Verified 2026-10-08: that binary refuses `origin/main`'s `lineage/rankings/` directory (`not a registry entry` → the W1 load of `xmarket-weekend` fails). So the main checkout must not be pulled before the stop.

After the stop (Mon ≥ 10:00 ET) and the Monday snapshot:

```bash
cd ~/development/tengu-cluster
git checkout -- docs/forward-evidence-runbook-2026-10-08.md
rm TENGU_STUDIO_PLAN.md docs/strategy-ranking-automation-2026-10-08.md handoff_for_check.md   # tracked upstream
git pull --ff-only
```

## 7. Verification evidence

| Check | Result | Where |
|---|---|---|
| CI `rust-quality` on every PR | green (fmt, `cargo check --all-features`, clippy, `cargo test --workspace`, + `cargo test --features studio --bin tengu studio` from #44) | GitHub checks of #39–#45 |
| Full suites on the final code | `cargo test --workspace` 2130 passed / 0 failed / 101 ignored; `--features studio` 2176 passed / 0 failed | integration merge of #44 + ST-90 clean room |
| Clean room (ST-90) | fresh clone of `f7ac9a5636e2493e3839e896cba9951a2d41aefb`: release build, lab A0–A15, G1–G8, Studio Play / events / Stop on real Jev — PASS | `docs/studio-clean-room-2026-10-09.md` |
| Studio live validation (coordinator) | real Jev (`typesafe/jev-1.13-20260917`), run `cbd8cd05-30fb-4177-aa99-0b978f0e83e5`, 117 events; guards 401 / 403 / 421; second runner refused; Stop drained, lease released; replay byte-identical; `tengu trace show` = API | `docs/studio-2026-10-08.md` § Live validation, `docs/studio-evidence/*.png` |
| Baselines | lab baseline 32 Jev calls ($0.000799134); Gate 2 trace evidence; ST-30 acceptance | `docs/control-loop-lab-baseline-2026-10-08.md`, `docs/studio-trace-evidence-2026-10-08.md`, `docs/studio-acceptance-2026-10-08.md` |
| Weekend freeze | `git diff --stat <base>...<branch> -- sandboxes/xmarket-weekend sandboxes/xlab lineage/generations/W1.toml lineage/locks.toml skills/xlab-research` empty for every PR; `tengu lineage verify --pins` 0 errors | review agents' reports |
| Privacy | no operator money figures in public source (`tengu soe init` leaves them `REQUIRED`); fixtures `synthetic = true`; TED fixtures scrubbed; key value found in no file | PR #41 / #43 notes |

## 8. Operator gates and open items

| Gate / item | What the operator does | Doc |
|---|---|---|
| G-SR1 — ranking contracts | review, then `tengu lineage seal ranking:rank.xlab-w2.daily.v1` and `ranking:rank.xlab-w2.weekend.v1` (the publisher refuses unsealed contracts; nothing ran on real state) | `docs/strategy-ranking-automation-2026-10-08.md` |
| G-O0 — SOE | `tengu soe init`, set the four `REQUIRED` money values, add capabilities, sign; approve SEC / TED terms + hashes and enable the rows | `docs/soe-2026-10-08.md` § 15 |
| G-R2 — SOE Operator Review #2 | after 4 weekly cycles: `tengu soe review`; nothing from O5–O8 was built | `docs/soe-2026-10-08.md` |
| SOE-G0 lock | the lock row in `lineage/locks.toml` is the operator's (after signing) | `docs/lineage-2026-10-06.md` |
| Operator Review #3 — Studio editor | ST-40 is design only (no save) | `docs/studio-editor-design-2026-10-08.md` |
| Live engine-matrix legs | `xlab_rank`, `sources`, `soe` live legs not run (need keys / claude login / the local PC); offline legs pass | `tests/engine_matrix.rs` |
| Studio gates 1–4 | recorded as waived by the operator's 2026-10-08 instruction (never as reviewed) | `TENGU_STUDIO_PLAN.md` § 8 |

## 9. Known limits (from the review agents; not fixed on purpose)

| Area | Limit |
|---|---|
| Ranking | variant status (verdict) is not as-of (schema has no status time); daily contract reads the whole window as development data |
| SOE | DILIGENCE costs no weekly budget (operator decision); a torn last forecast-log line fails closed until fixed by hand; `replay` / `grade` / `resolve` take no lease |
| Sources | `parsed_ms` is stamped before batch commit (live vs replay may differ within a fetch); raw snapshots are decoded UTF-8, not wire bytes |
| Studio / trace | `tengu webhooks` and planner / subagent steps record no trace; a `kill -9` leaves a run `open`; Play runs the config loaded at Studio start |
| Pre-existing warnings | unused imports `tools/solana/write_tokens.rs`, dead `claude_cli_env` without `claude_code`, older clippy findings (none in the new SOE / source / ranking / studio / trace files) |

## 10. How to check (commands)

```bash
git fetch origin --tags
git log --oneline 10bdbb53a27390f6a67fd73826740befe05ababe~1..origin/main          # #39 … #45
gh pr list --state open                                                            # expect: none
git ls-remote --tags origin 'refs/tags/archive/*' | wc -l                          # 25 (24 + docs/sweep-b)
# in a fresh clone of origin/main (NOT the main checkout before Monday):
cargo fmt --all --check && cargo test --workspace && cargo test --workspace --features studio
cargo test --test layering_lint --test scope_lint --test tutorial_map --test code_map --test language_policy --test lineage_cli
export TENGU_HOME=$HOME/tengu-lab/home   # never ~/.tengu
cargo build --release --features studio && target/release/tengu studio --sandbox control-loop-lab   # docs/studio-2026-10-08.md
target/release/tengu lineage verify --pins --registry lineage                     # 0 errors
```
