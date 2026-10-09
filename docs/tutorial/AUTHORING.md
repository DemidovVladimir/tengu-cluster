# Tengu field manual — authoring + sync rules

Static site, one HTML page per feature, for human users. No build step: open
`index.html` or serve the folder from any static host. JavaScript lives only
here and in `web/studio/` (the Studio page — the one exception in the
`CLAUDE.md` "Rust only" box; `tests/language_policy.rs`).

| File | Role |
|---|---|
| `index.html` | landing: doctrine animation + feature cards (from `assets/nav.js`) |
| `<slug>.html` | one feature page |
| `assets/site.css` | tokens (light + dark), layout, every component |
| `assets/site.js` | rail, pager, theme, stage player, tabs, terminal, bars |
| `assets/nav.js` | page list + chapters = reading order |
| `sources.toml` | page → the source paths it explains (the sync map) |
| `decision-loop.html` | **the exemplar** — copy its structure |

## Sync rule (code ↔ pages)

| When | Do |
|---|---|
| You change code under a path listed in `sources.toml` | Re-read the changed code, update every page whose `sources` match, bump its footer date (`Checked against the code on YYYY-MM-DD.`) — same commit |
| You add / move / delete a source file | Update `sources` paths, or `[glue] sources` for a file that only wires modules (`cargo test --test tutorial_map` fails on a dead path, an unmapped `src/` file, a page missing from `nav.js` or `sources.toml`, a page without the shared assets, a `<title>` or the footer, or a link to a missing page) |
| You add a feature | New `<slug>.html` + one `nav.js` entry + one `[pages.<slug>]` block |
| Behaviour unchanged (refactor, rename inside a file) | Page stays; still check names / paths the page shows |

Claude Code also gets a reminder from the `PostToolUse` hook in
`.claude/settings.json` (matcher `Edit|Write|MultiEdit`): after every edit of a
`src/**/*.rs` file it names that file and asks for the pages, the footer date and
`cargo test --test tutorial_map`. It is a reminder only; the test is the gate.

## Page anatomy (in order)

| Block | Markup | Words |
|---|---|---|
| Hero | `header.hero`: `.eyebrow` (chapter kanji `.stamp` + chapter), `h1`, `p.lede`, `ul.facts` (3–4 real defaults / numbers), optional `ul.legend` | lede ≤ 40 |
| The picture | `figure.stage[data-stage]`: SVG + `ol.captions` | 5–10 captions, ≤ 30 words each |
| 2–4 focused visuals | widgets, tabs, grids, cards, bars — one idea each | ≤ 2 sentences per intro |
| Outcomes / states | `ul.states` | one line each |
| Guarantees | `ul.checks` — only what code enforces | one line each |
| Try it | `.term-wrap > pre.term[data-type]` with `span.ln` (+ `.cmd`) | real commands only |
| Where it lives | `.table-wrap > table` file → owns | — |
| Footer | `nav#pager` + `p.stamp-foot` | — |

## Components

| Component | Markup |
|---|---|
| Stage step | any SVG element with `data-at="3"`, `"1,4"`, `"2-5"`, `"6-"`, `"*"`: highlighted on those steps, dimmed otherwise |
| Node | `<g class="n heart" data-at="2"><rect …/><text …/></g>` roles: `heart` LLM · `brain` memory · `hands` tools · `guard` safety · `jev` typed decisions · `data` stores/logs · none = user / event |
| Edge | `<path class="e" id="p-x" d="…" data-at="2"/>` (+ `dash`, `flow` = animated dashes, `bad`) |
| Moving token | `<circle class="tok hands" r="6" data-path="p-x" data-at="2" data-delay="300" data-dur="1500"/>` (`data-once`, `data-reverse`) |
| Step badge | `<g class="pop badge" data-at="4"><rect …/><text …/></g>` |
| Page hook | `figure.addEventListener("stage:step", e => e.detail.step)` |
| Tabs | `.tabs > .tablist > button[aria-controls]` + `.tabpanel#id` (`hidden` on all but first) |
| Bars | `.bars > .bar > .lbl + .track > .fill[style="--w:83%"] + .val` |
| In-view CSS | `[data-inview]` gets `.in-view` once visible |
| Chips | `.chip.heart` … `.chip.ok/.warn/.bad`; kanji inside `<span class="k">` |

SVG: `viewBox` width 760–900, text ≥ 11 px at 1:1, node titles ≤ 18 chars,
smalls ≤ 24 chars. Arrowheads come from `site.js`. Page-only CSS/JS goes in a
`<style>` / `<script>` in that page.

## Writing rules

| Rule | Why |
|---|---|
| Facts come from the code (read the `.rs`), never from older docs | docs drift; code is the truth |
| Every number on a page is a real default or limit in code | readers act on them |
| Example values are labelled "example" | never pass invented data as real |
| Never shorten an address, mint, hash, key or id (no `…` inside one) | operator rule |
| Plain words, short sentences, active voice | page is a tutorial |
| No secrets, no private wallet / account values beyond what sandboxes already commit | public site |
| Link to sibling pages by slug (`backtest.html`) | the rail and pager do the rest |

Check a page: `open docs/tutorial/<slug>.html` (light, dark, ~400 px wide).
Deploy: copy `docs/tutorial/` as-is to any static host; pages load fonts from
Google Fonts and nothing else.
