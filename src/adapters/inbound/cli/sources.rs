//! `tengu sources …` — the source layer (O2) of a `[sources]` sandbox
//! (`sandboxes/soe`): the registry, the operator's fetches and imports, the
//! as-of evidence packet, retention purges and the runtime kill switch. No
//! LLM; agents never fetch (the `source_evidence` tool is read-only).
//!
//! | Subcommand | Does |
//! |---|---|
//! | `list` | every registry row: kind, enabled (TOML), runtime switch, class / trust, retention, license; then every cursor and the newest fetch per query — full ids |
//! | `fetch --source <id> [--from] [--to] [--ciks <list>]` | `sec_edgar`: the CIKs of `--ciks`, else the row's `entities`, from `--from` (required); `ted_search`: the row's query, one publication day at a time (`--from` omitted = the day after the cursor); a disabled, unreviewed or switched-off row is refused before any request; prints the run table; exit 1 when any row failed |
//! | `import --source <id> --file <json> --observed-at <t>` | `ted_search` only: a saved search reply read as if fetched at `t` (offline: fixtures, evaluation sets) — the fetch's gate and records, no coverage; parsed now, so captured mode sees it from now, knowable mode from each notice's publication |
//! | `asof --at <t> [--mode captured\|knowable] [--source] [--entity] [--event] [--published-from <t>] [--published-to <t>] [--text]` | the evidence packet `source_asof/1` (`application::sources::evidence_as_of`): canonical JSON, or with `--text` its fenced text |
//! | `purge [--source <id>]` | each row's retention (`raw_retention_days`, `record_retention_days`; `0` = forever) applied now; prints each tombstone |
//! | `terms --source <id> --file <saved page>` | the terms page the operator reviewed (critic U9): refused unless its sha256 is the row's `terms_sha256`; kept whole as a `terms` snapshot, never purged; `list` shows whether it is stored |
//! | `disable --source <id> --reason <text>` · `enable --source <id> --reason <text>` | the runtime kill switch (critic U10): a row appended to `sources.db` — off refuses every fetch and import of the source at once, whatever the TOML says; `enable` lifts only that (a row with `enabled = false` stays off) |
//!
//! | Rule | Value |
//! |---|---|
//! | Times | epoch ms, RFC 3339 or `YYYY-MM-DD` (`domain::marketdata::parse_time`) |
//! | Store | `<TENGU_HOME>/state/<sources.state>/sources.db`; `list`, `asof` and `purge` read without creating it |
//! | Network | `fetch` only, through the installed `[egress]` policy (`--sandbox` installs the sandbox's); audit lines attributed `agent = cli`, `session = sources-fetch:<start ms>`, `call_id = <source>:<entity>` (SEC: `<source>:sec:cik:<cik10>`, TED: `<source>:query:<sha256>`) |

use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Result};
use clap::{Subcommand, ValueEnum};

use super::history::sections;
use crate::adapters::outbound::backfill::{text_table, BackfillReport, Retry};
use crate::adapters::outbound::clock::SystemClock;
use crate::adapters::outbound::egress::{audited_as, CallScope};
use crate::adapters::outbound::sources::sec::{sec_source_fetch, source_sec_client, SecFetch};
use crate::adapters::outbound::sources::ted::{
    source_ted_client, ted_import, ted_source_fetch, TedFetch,
};
use crate::adapters::outbound::sources::{
    existing_source_store, fetch_gate, open_source_store, sources_state_dir,
};
use crate::application::sources::{evidence_as_of, purge_request, AsOfRequest};
use crate::config::sections::SandboxSections;
use crate::config::sources::{sources_db, SourceEntry, SourceKind, SourcesConfig};
use crate::config::Config;
use crate::domain::canonical::canonical_json;
use crate::domain::marketdata::{fmt_time, parse_time};
use crate::domain::observation::now_ms;
use crate::domain::source::ted::{query_key, TED_MAX_LIMIT};
use crate::domain::source::AsOfMode;
use crate::ports::clock::Clock;
use crate::ports::source_store::{
    body_sha256, switched_off, Batch, Snapshot, SnapshotKind, SourceStore, SourceSwitch,
};

#[derive(Subcommand)]
pub(super) enum SourcesAction {
    /// Every [sources.registry.*] row (kind, enabled, runtime switch, class /
    /// trust, retention, license), then cursors and the newest fetch per query.
    List,
    /// Fetch a row into sources.db (operator only): SEC EDGAR filings of
    /// CIKs, or EU TED notices one publication day at a time. Refused for a
    /// disabled, unreviewed or switched-off row before any request.
    Fetch {
        /// The registry row id (sec_edgar, ted_search).
        #[arg(long)]
        source: String,
        /// Start, inclusive: epoch ms, RFC 3339 or a UTC date. TED: omitted =
        /// the day after the cursor.
        #[arg(long)]
        from: Option<String>,
        /// End, exclusive; default (and at most) now.
        #[arg(long)]
        to: Option<String>,
        /// sec_edgar: comma-separated CIKs (10 digits, or sec:cik:<10
        /// digits>); default the row's `entities`.
        #[arg(long)]
        ciks: Option<String>,
    },
    /// Read a saved TED search reply into sources.db as if fetched at
    /// --observed-at (offline; fixtures, evaluation sets). No coverage.
    Import {
        #[arg(long)]
        source: String,
        #[arg(long)]
        file: PathBuf,
        /// When the reply was read: epoch ms, RFC 3339 or a UTC date.
        #[arg(long)]
        observed_at: String,
    },
    /// The evidence packet source_asof/1 at --at: canonical JSON (or --text).
    Asof {
        /// Epoch ms, RFC 3339 or a UTC date.
        #[arg(long)]
        at: String,
        #[arg(long, value_enum, default_value_t = ModeArg::Captured)]
        mode: ModeArg,
        #[arg(long)]
        source: Option<String>,
        /// An entity key in full (sec:cik:0000320193, ted:buyer:CZE:60457856).
        #[arg(long)]
        entity: Option<String>,
        /// An event key in full (sec:filing:<accession>, ted:procedure:<id>).
        #[arg(long)]
        event: Option<String>,
        /// Keep records published at or after this.
        #[arg(long)]
        published_from: Option<String>,
        /// Keep records published before this (exclusive).
        #[arg(long)]
        published_to: Option<String>,
        /// The packet's text (free text fenced) instead of JSON.
        #[arg(long)]
        text: bool,
    },
    /// Apply each row's retention now (raw_retention_days,
    /// record_retention_days; 0 = forever); prints each tombstone.
    Purge {
        /// One row; default every row.
        #[arg(long)]
        source: Option<String>,
    },
    /// Store the terms page the operator reviewed — its sha256 must be the
    /// row's terms_sha256 — in sources.db, whole and never purged.
    Terms {
        #[arg(long)]
        source: String,
        /// The page as saved and reviewed (its sha256 is terms_sha256).
        #[arg(long)]
        file: PathBuf,
    },
    /// Runtime kill switch: refuse every fetch and import of a source now,
    /// whatever the TOML says (an appended row in sources.db).
    Disable {
        #[arg(long)]
        source: String,
        #[arg(long)]
        reason: String,
    },
    /// Lift a runtime `disable` (a row with enabled = false stays off).
    Enable {
        #[arg(long)]
        source: String,
        #[arg(long)]
        reason: String,
    },
}

/// `--mode`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub(super) enum ModeArg {
    /// What this system had read and parsed by t.
    Captured,
    /// What could have been known at t.
    Knowable,
}

impl From<ModeArg> for AsOfMode {
    fn from(m: ModeArg) -> Self {
        match m {
            ModeArg::Captured => AsOfMode::Captured,
            ModeArg::Knowable => AsOfMode::Knowable,
        }
    }
}

pub(super) async fn run_sources(config: &Config, action: SourcesAction) -> Result<()> {
    let sections = sections(config);
    let now = now_ms();
    match action {
        SourcesAction::List => print!("{}", list(&sections).await?),
        SourcesAction::Fetch {
            source,
            from,
            to,
            ciks,
        } => {
            let from = from.as_deref().map(time).transpose()?;
            let to = to.as_deref().map(time).transpose()?.unwrap_or(i64::MAX);
            let report = fetch(&sections, &source, from, to, ciks.as_deref(), now).await?;
            finish(report)?;
        }
        SourcesAction::Import {
            source,
            file,
            observed_at,
        } => {
            let report =
                import(&sections, &source, &file, time(&observed_at)?, &SystemClock).await?;
            finish(report)?;
        }
        SourcesAction::Asof {
            at,
            mode,
            source,
            entity,
            event,
            published_from,
            published_to,
            text,
        } => {
            let req = AsOfRequest {
                at_ms: time(&at)?,
                mode: mode.into(),
                source,
                entity,
                event_key: event,
                published_from_ms: published_from.as_deref().map(time).transpose()?,
                published_to_ms: published_to.as_deref().map(time).transpose()?,
            };
            println!("{}", asof(&sections, &req, text).await?);
        }
        SourcesAction::Purge { source } => {
            print!("{}", purge(&sections, source.as_deref(), now).await?)
        }
        SourcesAction::Terms { source, file } => {
            println!("{}", terms(&sections, &source, &file, now).await?)
        }
        SourcesAction::Disable { source, reason } => {
            println!("{}", switch(&sections, &source, false, &reason, now).await?)
        }
        SourcesAction::Enable { source, reason } => {
            println!("{}", switch(&sections, &source, true, &reason, now).await?)
        }
    }
    Ok(())
}

fn time(s: &str) -> Result<i64> {
    parse_time(s).map_err(|e| anyhow!(e))
}

/// Print the run table; exit 1 (an error) when any row failed.
fn finish(report: BackfillReport) -> Result<()> {
    print!("{}", report.render());
    match report.error_count() {
        0 => Ok(()),
        n => bail!("{n} error(s) — see the table above; what was read stays (a re-run resumes)"),
    }
}

fn registry(sections: &SandboxSections) -> Result<&SourcesConfig> {
    sections.sources.as_deref().ok_or_else(|| {
        anyhow!(
            "sources_state_missing: no [sources] section — `tengu sources` needs a sandbox \
             with one (sandboxes/soe: --sandbox soe)"
        )
    })
}

fn row<'a>(registry: &'a SourcesConfig, id: &str) -> Result<&'a SourceEntry> {
    registry.registry.get(id).ok_or_else(|| {
        anyhow!(
            "no [sources.registry.{id}] row (rows: {})",
            registry
                .registry
                .keys()
                .map(String::as_str)
                .collect::<Vec<_>>()
                .join(", ")
        )
    })
}

async fn list(sections: &SandboxSections) -> Result<String> {
    let registry = registry(sections)?;
    let store = existing_source_store(sections)?;
    let switches = match &store {
        Some(s) => s.switches(None).await?,
        None => Vec::new(),
    };
    let days = |d: Option<u32>| match d {
        Some(0) => "forever".to_string(),
        Some(n) => format!("{n}d"),
        None => "-".to_string(),
    };
    let mut rows: Vec<Vec<String>> = Vec::new();
    for (id, e) in &registry.registry {
        let terms = match (&e.terms_sha256, &store) {
            (None, _) => "no reviewed terms".to_string(),
            (Some(sha), Some(s)) if terms_page_stored(s.as_ref(), sha).await? => {
                format!("{} (page stored)", e.license.clone().unwrap_or_default())
            }
            (Some(_), _) => format!(
                "{} (page not stored: tengu sources terms)",
                e.license.clone().unwrap_or_default()
            ),
        };
        rows.push(vec![
            id.clone(),
            e.kind.as_str().to_string(),
            e.enabled.to_string(),
            match switched_off(&switches, id) {
                Some(s) => format!("off since {} ({})", fmt_time(s.at_ms), s.reason),
                None => "on".to_string(),
            },
            format!("{}/{}", e.class.as_str(), e.trust.as_str()),
            format!(
                "raw {} · records {}",
                days(e.raw_retention_days),
                days(e.record_retention_days)
            ),
            terms,
        ]);
    }
    let mut out = text_table(
        &[
            "source",
            "kind",
            "enabled",
            "runtime",
            "class/trust",
            "retention",
            "terms",
        ],
        &rows,
    );
    let Some(store) = store else {
        out.push_str(&format!(
            "\nnothing stored yet: no {}\n",
            sources_db(sources_state_dir(sections)?).display()
        ));
        return Ok(out);
    };
    let cursors: Vec<Vec<String>> = store
        .cursors(None)
        .await?
        .into_iter()
        .map(|c| vec![c.source_id, c.query_key, c.value, fmt_time(c.updated_ms)])
        .collect();
    if !cursors.is_empty() {
        out.push('\n');
        out.push_str(&text_table(
            &["source", "query", "cursor", "updated"],
            &cursors,
        ));
    }
    let mut newest: std::collections::BTreeMap<(String, String), crate::domain::source::Coverage> =
        std::collections::BTreeMap::new();
    for c in store.coverage(None).await? {
        let key = (c.source_id.clone(), c.query_key.clone());
        if newest
            .get(&key)
            .map_or(true, |n| c.fetched_ms >= n.fetched_ms)
        {
            newest.insert(key, c);
        }
    }
    let fetches: Vec<Vec<String>> = newest
        .into_values()
        .map(|c| {
            vec![
                c.source_id,
                c.query_key,
                fmt_time(c.fetched_ms),
                format!("{} → {}", fmt_time(c.from_ms), fmt_time(c.to_ms)),
                if c.complete {
                    "complete".to_string()
                } else {
                    format!("incomplete ({})", c.error_class.unwrap_or_default())
                },
            ]
        })
        .collect();
    if !fetches.is_empty() {
        out.push('\n');
        out.push_str(&text_table(
            &["source", "query", "last fetch", "span", "status"],
            &fetches,
        ));
    }
    Ok(out)
}

/// `fetch` (module table): the row's gate first, then its fetcher.
async fn fetch(
    sections: &SandboxSections,
    id: &str,
    from: Option<i64>,
    to: i64,
    ciks: Option<&str>,
    now: i64,
) -> Result<BackfillReport> {
    let registry = registry(sections)?;
    let entry = row(registry, id)?;
    // The TOML gate first: a refused row creates nothing.
    entry.fetch_stamp(id).map_err(|e| anyhow!(e))?;
    let store = open_source_store(sections)?;
    fetch_gate(store.as_ref(), id, entry, entry.kind)
        .await
        .map_err(|e| anyhow!(e))?;
    let clock = SystemClock;
    let retry = Retry::BACKFILL;
    let session = format!("sources-fetch:{now}");
    let scope = |call_id: String| CallScope {
        agent: "cli".to_string(),
        session: session.clone(),
        call_id,
    };
    match entry.kind {
        SourceKind::SecEdgar => {
            let from = from.ok_or_else(|| anyhow!("sec_edgar needs --from"))?;
            let ciks = sec_ciks(entry, ciks)?;
            let client = source_sec_client(sections, id, entry)?;
            let f = SecFetch {
                client: &client,
                store: store.as_ref(),
                clock: &clock,
                retry: &retry,
            };
            let mut report = BackfillReport::default();
            for cik in &ciks {
                let one = audited_as(
                    scope(format!("{id}:sec:cik:{cik}")),
                    sec_source_fetch(&f, id, entry, std::slice::from_ref(cik), from, to),
                )
                .await;
                report.rows.extend(one.rows);
                report.errors.extend(one.errors);
                report.notes.extend(one.notes);
            }
            Ok(report)
        }
        SourceKind::TedSearch => {
            if ciks.is_some() {
                bail!("--ciks is for a sec_edgar row; `{id}` is ted_search");
            }
            let client = source_ted_client(sections, id, entry)?;
            let f = TedFetch {
                client: &client,
                store: store.as_ref(),
                clock: &clock,
                retry: &retry,
                limit: TED_MAX_LIMIT,
            };
            let key = query_key(entry.query.as_deref().unwrap_or_default());
            Ok(audited_as(
                scope(format!("{id}:{key}")),
                ted_source_fetch(&f, id, entry, from, to),
            )
            .await)
        }
    }
}

/// `--ciks`, else the row's `entities` (each 10 digits, in order, no repeat).
fn sec_ciks(entry: &SourceEntry, ciks: Option<&str>) -> Result<Vec<String>> {
    let items: Vec<String> = match ciks {
        Some(list) => list
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(|s| s.strip_prefix("sec:cik:").unwrap_or(s).to_string())
            .collect(),
        None => entry
            .entities
            .iter()
            .filter_map(|e| e.strip_prefix("sec:cik:").map(String::from))
            .collect(),
    };
    let mut out: Vec<String> = Vec::new();
    for c in items {
        if !(c.len() == 10 && c.bytes().all(|b| b.is_ascii_digit())) {
            bail!("CIK `{c}` is not 10 digits (zero-padded, e.g. 0000320193)");
        }
        if !out.contains(&c) {
            out.push(c);
        }
    }
    if out.is_empty() {
        bail!("no CIK: give --ciks or set the row's `entities` (sec:cik:<10 digits>)");
    }
    Ok(out)
}

/// `import` (module table); `clock` stamps the parse (captured mode sees an
/// import from then, knowable mode from each notice's publication).
async fn import(
    sections: &SandboxSections,
    id: &str,
    file: &Path,
    observed_ms: i64,
    clock: &dyn Clock,
) -> Result<BackfillReport> {
    let registry = registry(sections)?;
    let entry = row(registry, id)?;
    if entry.kind != SourceKind::TedSearch {
        bail!(
            "import reads saved TED search replies; `{id}` is a `{}` row (a SEC filing's time needs its index page: fetch it)",
            entry.kind.as_str()
        );
    }
    let store = open_source_store(sections)?;
    Ok(ted_import(store.as_ref(), clock, id, entry, file, observed_ms).await)
}

async fn asof(sections: &SandboxSections, req: &AsOfRequest, text: bool) -> Result<String> {
    let registry = registry(sections)?;
    let store = existing_source_store(sections)?;
    let packet = evidence_as_of(store.as_deref(), registry, req).await?;
    Ok(if text {
        packet.render_text()
    } else {
        canonical_json(&serde_json::to_value(&packet)?)
    })
}

async fn purge(sections: &SandboxSections, only: Option<&str>, now: i64) -> Result<String> {
    let registry = registry(sections)?;
    if let Some(id) = only {
        row(registry, id)?;
    }
    let Some(store) = existing_source_store(sections)? else {
        return Ok("nothing stored yet: nothing to purge\n".into());
    };
    let mut out = String::new();
    let mut rows = Vec::new();
    for (id, entry) in &registry.registry {
        if only.is_some_and(|o| o != id) {
            continue;
        }
        let Some(req) = purge_request(id, entry, now) else {
            out.push_str(&format!(
                "{id}: kept forever (retention 0 or unset) — nothing to purge\n"
            ));
            continue;
        };
        let p = store.purge(&req).await?;
        let opt = |t: Option<i64>| t.map(fmt_time).unwrap_or_else(|| "-".into());
        rows.push(vec![
            p.source_id,
            fmt_time(p.purged_ms),
            opt(p.raw_before_ms),
            opt(p.records_before_ms),
            p.snapshots.to_string(),
            p.records.to_string(),
            p.reason,
        ]);
    }
    if !rows.is_empty() {
        out.push_str(&text_table(
            &[
                "source",
                "purged",
                "raw before",
                "records before",
                "bodies",
                "records",
                "reason",
            ],
            &rows,
        ));
        out.push_str("a purge that removed nothing leaves no tombstone\n");
    }
    Ok(out)
}

/// Whether the terms page `sha256` is stored (as a terms snapshot).
async fn terms_page_stored(store: &dyn SourceStore, sha256: &str) -> Result<bool> {
    Ok(store
        .snapshot(sha256)
        .await?
        .is_some_and(|s| s.kind == SnapshotKind::Terms && s.body.is_some()))
}

/// `terms` (module table, critic U9): the reviewed page's bytes, checked
/// against the row's `terms_sha256`, kept as a `terms` snapshot.
async fn terms(sections: &SandboxSections, id: &str, file: &Path, now: i64) -> Result<String> {
    let registry = registry(sections)?;
    let entry = row(registry, id)?;
    let Some(want) = entry.terms_sha256.as_deref() else {
        bail!(
            "source `{id}` has no terms_sha256: review the page, then set terms_sha256 = <sha256 of \
             the saved file> and terms_reviewed_at in its row"
        );
    };
    let url = entry
        .terms_url
        .as_deref()
        .ok_or_else(|| anyhow!("source `{id}` has no terms_url (the page's address)"))?;
    let body = std::fs::read(file).map_err(|e| anyhow!("{}: {e}", file.display()))?;
    let got = body_sha256(&body);
    if got != want {
        bail!(
            "{}: sha256 {got} is not the row's terms_sha256 {want} — another file, or the page \
             changed since it was reviewed",
            file.display()
        );
    }
    let store = open_source_store(sections)?;
    let report = store
        .commit(Batch {
            snapshots: vec![Snapshot::terms(id, url, now, &body)],
            ..Default::default()
        })
        .await?;
    Ok(if report.snapshots_new > 0 {
        format!(
            "source `{id}`: terms page {got} stored ({} bytes, {url}) — kept whole, never purged",
            body.len()
        )
    } else {
        format!("source `{id}`: terms page {got} already stored")
    })
}

/// `disable` / `enable` (module table).
async fn switch(
    sections: &SandboxSections,
    id: &str,
    enabled: bool,
    reason: &str,
    now: i64,
) -> Result<String> {
    let registry = registry(sections)?;
    let entry = row(registry, id)?;
    if reason.trim().is_empty() {
        bail!("--reason is required (it is kept with the switch)");
    }
    let store = open_source_store(sections)?;
    let off = switched_off(&store.switches(Some(id)).await?, id).cloned();
    if enabled && off.is_none() {
        return Ok(format!(
            "source `{id}` is not switched off at runtime: nothing to lift{}",
            toml_note(entry)
        ));
    }
    store
        .set_switch(&SourceSwitch {
            source_id: id.to_string(),
            enabled,
            at_ms: now,
            reason: reason.trim().to_string(),
        })
        .await?;
    Ok(if enabled {
        format!(
            "source `{id}`: the runtime switch is lifted at {}{}",
            fmt_time(now),
            toml_note(entry)
        )
    } else {
        format!(
            "source `{id}` switched off at {} ({}): every fetch and import is refused until \
             `tengu sources enable --source {id} --reason <text>`",
            fmt_time(now),
            reason.trim()
        )
    })
}

fn toml_note(entry: &SourceEntry) -> &'static str {
    if entry.enabled {
        ""
    } else {
        " (its row has enabled = false: the TOML keeps it off)"
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::domain::source::FENCE_NOTE;
    use crate::ports::clock::SimClock;

    const HASH: &str = "4a5e1e4baab89f3a32518a88c31bc87f618f76673e2cc77ab2127b7afdeda33b";

    /// A `[sources]` section with a disabled `sec_edgar` row and a
    /// `ted_search` row, enabled with synthetic terms when `ted_on`.
    fn sections(dir: &Path, ted_on: bool) -> SandboxSections {
        let ted_terms = if ted_on {
            format!(
                "enabled = true\nlicense = \"synthetic test terms\"\nterms_url = \"https://example.org/terms\"\n\
                 terms_sha256 = \"{HASH}\"\nterms_reviewed_at = \"2026-10-08\"\n\
                 raw_retention_days = 90\nrecord_retention_days = 0\n"
            )
        } else {
            "enabled = false\n".to_string()
        };
        let text = format!(
            r#"
            state = "soe"
            [registry.sec_edgar]
            kind = "sec_edgar"
            class = "company_primary"
            trust = "primary"
            revision = "immutable"
            enabled = false
            hosts = ["www.sec.gov", "data.sec.gov"]
            auth = "user_agent_env:TENGU_TEST_SEC_UA_NEVER_SET_9A2C"
            rate_limit = "sec"
            store_raw = true
            jurisdiction = "US"
            language = "en"
            [registry.ted_search]
            kind = "ted_search"
            class = "law_regulator"
            trust = "primary"
            revision = "immutable"
            hosts = ["api.ted.europa.eu"]
            auth = "none"
            rate_limit = "ted"
            store_raw = true
            jurisdiction = "EU"
            language = "en"
            query = "publication-date >= {{from}} AND publication-date <= {{to}}"
            {ted_terms}
            "#
        );
        let cfg: SourcesConfig = toml::from_str(&text).unwrap_or_else(|e| panic!("{e}\n{text}"));
        SandboxSections {
            sources: Some(Arc::new(cfg)),
            sources_state_dir: Some(dir.join("soe")),
            ..Default::default()
        }
    }

    fn t(s: &str) -> i64 {
        parse_time(s).unwrap()
    }

    fn fixture(name: &str) -> PathBuf {
        PathBuf::from(format!(
            "{}/tests/fixtures/ted/{name}",
            env!("CARGO_MANIFEST_DIR")
        ))
    }

    fn req(at: &str, mode: AsOfMode) -> AsOfRequest {
        AsOfRequest {
            at_ms: t(at),
            mode,
            source: None,
            entity: None,
            event_key: None,
            published_from_ms: None,
            published_to_ms: None,
        }
    }

    /// A disabled row, one without reviewed terms and a switched-off row are
    /// refused before any client is built (no `$SEC_USER_AGENT` is read, no
    /// request is sent — `api.ted.europa.eu` is never contacted here).
    #[tokio::test]
    async fn fetch_refuses_a_disabled_or_unreviewed_source() {
        let dir = tempfile::tempdir().unwrap();
        let now = t("2026-10-08T12:00:00Z");
        let off = sections(dir.path(), false);
        for (id, needle) in [("sec_edgar", "disabled"), ("ted_search", "disabled")] {
            let e = fetch(&off, id, Some(t("2026-10-01")), i64::MAX, None, now)
                .await
                .unwrap_err()
                .to_string();
            assert!(e.contains(needle), "{id}: {e}");
        }
        let mut unreviewed = sections(dir.path(), true);
        let mut cfg = (**unreviewed.sources.as_ref().unwrap()).clone();
        cfg.registry.get_mut("ted_search").unwrap().terms_sha256 = None;
        unreviewed.sources = Some(Arc::new(cfg));
        let e = fetch(
            &unreviewed,
            "ted_search",
            Some(t("2026-10-01")),
            i64::MAX,
            None,
            now,
        )
        .await
        .unwrap_err()
        .to_string();
        assert!(e.contains("no reviewed terms"), "{e}");
        let e = fetch(&off, "nope", None, i64::MAX, None, now)
            .await
            .unwrap_err()
            .to_string();
        assert!(
            e.contains("no [sources.registry.nope] row (rows: sec_edgar, ted_search)"),
            "{e}"
        );
        assert!(
            !dir.path().join("soe").exists(),
            "a refused fetch creates nothing"
        );
        // The kill switch: off, refused; listed; lifted.
        let on = sections(dir.path(), true);
        let said = switch(&on, "ted_search", false, "terms under review", now)
            .await
            .unwrap();
        assert!(
            said.contains("switched off at 2026-10-08T12:00:00Z (terms under review)"),
            "{said}"
        );
        let e = fetch(
            &on,
            "ted_search",
            Some(t("2026-10-01")),
            i64::MAX,
            None,
            now,
        )
        .await
        .unwrap_err()
        .to_string();
        assert!(
            e.contains("switched off at runtime since 2026-10-08T12:00:00Z (terms under review)"),
            "{e}"
        );
        let e = import(
            &on,
            "ted_search",
            &fixture("search_page_1.json"),
            t("2026-10-02"),
            &SimClock::at(now),
        )
        .await
        .unwrap();
        assert!(
            e.errors[0].contains("switched off at runtime"),
            "{:?}",
            e.errors
        );
        let table = list(&on).await.unwrap();
        assert!(
            table.contains("off since 2026-10-08T12:00:00Z (terms under review)"),
            "{table}"
        );
        let lifted = switch(&on, "ted_search", true, "reviewed", now + 1)
            .await
            .unwrap();
        assert!(lifted.contains("runtime switch is lifted"), "{lifted}");
        let again = switch(&on, "ted_search", true, "reviewed", now + 2)
            .await
            .unwrap();
        assert!(again.contains("nothing to lift"), "{again}");
        // The TOML stays the ceiling: a disabled row stays off after `enable`.
        switch(&off, "sec_edgar", false, "x", now + 3)
            .await
            .unwrap();
        let note = switch(&off, "sec_edgar", true, "y", now + 4).await.unwrap();
        assert!(note.contains("the TOML keeps it off"), "{note}");
        assert!(fetch(
            &off,
            "sec_edgar",
            Some(t("2026-10-01")),
            i64::MAX,
            None,
            now
        )
        .await
        .unwrap_err()
        .to_string()
        .contains("disabled"));
        assert!(switch(&on, "ted_search", false, " ", now).await.is_err());
        // --ciks only for SEC; SEC CIKs are 10 digits.
        let row = &on.sources.as_ref().unwrap().registry["sec_edgar"];
        assert_eq!(
            sec_ciks(row, Some("sec:cik:0000320193, 0000320193")).unwrap(),
            ["0000320193"]
        );
        assert!(sec_ciks(row, Some("320193"))
            .unwrap_err()
            .to_string()
            .contains("not 10 digits"));
        assert!(sec_ciks(row, None)
            .unwrap_err()
            .to_string()
            .contains("no CIK"));
    }

    /// Import the change-notice pair (parsed 2026-10-08), then ask the
    /// packet twice: the same bytes; captured sees the import from its
    /// parse, knowable each notice from its publication; the text fences
    /// the buyer name; a purge drops raw bodies only.
    #[tokio::test]
    async fn import_then_asof_is_deterministic() {
        let dir = tempfile::tempdir().unwrap();
        let s = sections(dir.path(), true);
        // Nothing stored: an empty packet, and no file created.
        let empty = asof(&s, &req("2026-10-08", AsOfMode::Captured), false)
            .await
            .unwrap();
        assert!(empty.contains("\"facts\":[]"), "{empty}");
        assert!(!dir.path().join("soe").join("sources.db").exists());
        assert!(list(&s).await.unwrap().contains("nothing stored yet"));

        let observed = t("2026-10-02T06:00:00Z");
        let clock = SimClock::at(t("2026-10-08T12:00:00Z"));
        let report = import(
            &s,
            "ted_search",
            &fixture("search_change_notice.json"),
            observed,
            &clock,
        )
        .await
        .unwrap();
        assert_eq!(report.error_count(), 0, "{}", report.render());
        assert_eq!(report.rows[0].rows, 2);
        let at = req("2026-10-09", AsOfMode::Captured);
        let a = asof(&s, &at, false).await.unwrap();
        let b = asof(&s, &at, false).await.unwrap();
        assert_eq!(a, b, "the same store, the same packet");
        let v: serde_json::Value = serde_json::from_str(&a).unwrap();
        assert_eq!(v["schema"], "source_asof/1");
        let facts = v["facts"].as_array().unwrap();
        assert_eq!(
            facts.len(),
            1,
            "the change notice corrects the original: {a}"
        );
        assert!(a.contains("674231-2026") && a.contains("657981-2026"));
        assert_eq!(v["superseded"][0]["by"], "correction");
        // Before the parse: captured has nothing (even after the stated
        // read); knowable has the original (published 2026-09-24) but not
        // the change (2026-10-01).
        for before in ["2026-10-03", "2026-09-30"] {
            let early = asof(&s, &req(before, AsOfMode::Captured), false)
                .await
                .unwrap();
            assert!(early.contains("\"facts\":[]"), "{before}: {early}");
        }
        let knowable = asof(&s, &req("2026-09-30", AsOfMode::Knowable), false)
            .await
            .unwrap();
        assert!(
            knowable.contains("657981-2026") && !knowable.contains("674231-2026"),
            "{knowable}"
        );
        // Text: ids whole, buyer name inside the fence.
        let text = asof(&s, &at, true).await.unwrap();
        assert!(text.contains(FENCE_NOTE), "{text}");
        assert!(
            text.contains("procedure=f08aa593-61f7-418e-90ec-043a09cc23b1"),
            "{text}"
        );
        assert!(
            text.contains(
                "field=\"buyer_name\">Středisko společných činností AV ČR, v. v. i.</source-text>"
            ),
            "{text}"
        );
        // The registry filter refuses an unknown source.
        let mut unknown = at.clone();
        unknown.source = Some("nope".into());
        assert!(asof(&s, &unknown, false).await.is_err());

        // Purge 100 days later: raw bodies (90 days) go, records stay.
        let later = observed + 100 * 86_400_000;
        let out = purge(&s, None, later).await.unwrap();
        assert!(out.contains("sec_edgar: kept forever"), "{out}");
        assert!(out.contains("ted_search"), "{out}");
        assert!(out.contains("raw_retention_days = 90"), "{out}");
        let after = asof(&s, &at, false).await.unwrap();
        let v2: serde_json::Value = serde_json::from_str(&after).unwrap();
        assert_eq!(v2["facts"], v["facts"], "records stay");
        let table = list(&s).await.unwrap();
        assert!(
            table.contains("ted_search") && table.contains("raw 90d · records forever"),
            "{table}"
        );
        // Import refuses a SEC row.
        let e = import(
            &s,
            "sec_edgar",
            &fixture("search_page_1.json"),
            observed,
            &clock,
        )
        .await
        .unwrap_err()
        .to_string();
        assert!(e.contains("fetch it"), "{e}");
    }

    /// Critic U9: the reviewed terms page is stored only when its bytes are
    /// the row's `terms_sha256`; kept whole through a raw purge; `list`
    /// says so.
    #[tokio::test]
    async fn terms_page_is_stored_only_when_it_matches() {
        let dir = tempfile::tempdir().unwrap();
        let page = b"<html>synthetic reuse terms, reviewed 2026-10-08</html>";
        let mut s = sections(dir.path(), true);
        let mut cfg = (**s.sources.as_ref().unwrap()).clone();
        cfg.registry.get_mut("ted_search").unwrap().terms_sha256 = Some(body_sha256(page));
        s.sources = Some(Arc::new(cfg));
        let good = dir.path().join("terms.html");
        let other = dir.path().join("other.html");
        std::fs::write(&good, page).unwrap();
        std::fs::write(&other, b"<html>changed</html>").unwrap();
        let now = t("2026-10-08T12:00:00Z");
        let e = terms(&s, "ted_search", &other, now)
            .await
            .unwrap_err()
            .to_string();
        assert!(
            e.contains(&format!(
                "is not the row's terms_sha256 {}",
                body_sha256(page)
            )),
            "{e}"
        );
        let e = terms(&s, "sec_edgar", &good, now)
            .await
            .unwrap_err()
            .to_string();
        assert!(e.contains("has no terms_sha256"), "{e}");
        let said = terms(&s, "ted_search", &good, now).await.unwrap();
        assert!(
            said.contains(&format!("terms page {} stored", body_sha256(page))),
            "{said}"
        );
        assert!(terms(&s, "ted_search", &good, now)
            .await
            .unwrap()
            .contains("already stored"));
        assert!(list(&s)
            .await
            .unwrap()
            .contains("synthetic test terms (page stored)"));
        // A raw purge long after keeps it.
        purge(&s, Some("ted_search"), now + 400 * 86_400_000)
            .await
            .unwrap();
        let store = open_source_store(&s).unwrap();
        let kept = store.snapshot(&body_sha256(page)).await.unwrap().unwrap();
        assert_eq!(
            (kept.kind, kept.body.as_deref()),
            (SnapshotKind::Terms, Some(&page[..]))
        );
        assert_eq!(
            (kept.request_key.as_str(), kept.url.as_str()),
            ("terms", "https://example.org/terms")
        );
    }
}
