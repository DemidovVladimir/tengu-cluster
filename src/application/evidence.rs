//! Evidence use cases (`tengu evidence`, `docs/lineage-2026-10-06.md` § 3,
//! roadmap Phase 0) over the ports of `ports/evidence.rs`; the math is in
//! `domain/{evidence,evidence_coverage}.rs`, `domain/xm/{grade,regrade}.rs`.
//!
//! | Use case | Rule |
//! |---|---|
//! | [`snapshot`] | a plan record (no `captured_at`) → every source checked (exists, kind as declared, no symlink) before anything is created → the vault created (refused when it exists) → items copied, each copy hashed = its source → `MANIFEST.json` (every file `{path, sha256, bytes}`, sorted, pretty JSON + `\n`) → `chmod a-w` → the captured record (`captured_at`, `manifest_sha256` = sha256 of the manifest bytes, per item `sha256` (a file's, a dir's [`tree_hash`]), `files`, `bytes`) |
//! | [`verify`] | re-hash the vault: the manifest bytes vs `manifest_sha256`; every manifest file; every item recomputed from the fresh hashes vs the record — `MATCH` · `MISMATCH` · `ABSENT`, and `EXTRA` for a vault file the manifest does not list |
//! | [`coverage`] | `domain::evidence_coverage` over the `ok` / `partial` instants of one schema in recorder day files (+ `market.db` bars as the second source) |
//! | [`grade`] | `domain::xm::grade::grade_account` per account of a ledger (all by default) |
//! | [`regrade`] | `domain::xm::regrade` over rows read around each instant: `mkt_ctx/1` at the anchor, entry, exit and every funding hour; `hl_book/1` (with data) at entry and exit; `market.db` funding + 1 m bars; the universe = the given ids, else every `hl_book/1` key recorded between entry − 1 h and exit + 1 h |

use std::collections::{BTreeMap, BTreeSet};

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

use crate::domain::book::L2Book;
use crate::domain::canonical::sha256_hex;
use crate::domain::evidence::{tree_hash, EvidenceRecord, ItemKind, ManifestEntry};
use crate::domain::evidence_coverage::{self, key_instrument, Bar, Bars, Coverage, Stream, Window};
use crate::domain::observation::ObsStatus;
use crate::domain::xm::grade::{grade_account, AccountGrade};
use crate::domain::xm::regrade::{
    self, BarClose, BookSample, CtxSample, FundingRate, Instants, LegData, Limits, Regrade,
    RegradeRule,
};
use crate::ports::evidence::{BackfillSource, LedgerSource, RecordedHistory, Vault};

const HOUR_MS: i64 = 3_600_000;
const CTX_SCHEMA: &str = "mkt_ctx/1";
const BOOK_SCHEMA: &str = "hl_book/1";

// ── snapshot ───────────────────────────────────────────────────────

/// `MANIFEST.json` bytes: entries sorted by path, pretty JSON + newline.
pub(crate) fn manifest_bytes(entries: &[ManifestEntry]) -> Result<Vec<u8>> {
    let mut sorted = entries.to_vec();
    sorted.sort_by(|a, b| a.path.as_bytes().cmp(b.path.as_bytes()));
    let mut bytes = serde_json::to_vec_pretty(&sorted)?;
    bytes.push(b'\n');
    Ok(bytes)
}

fn manifest_sha256(bytes: &[u8]) -> Result<String> {
    Ok(sha256_hex(
        std::str::from_utf8(bytes).context("MANIFEST.json is not UTF-8")?,
    ))
}

/// The module table's `snapshot`; returns the captured record and the
/// manifest entries.
pub(crate) fn snapshot(
    plan: &EvidenceRecord,
    vault: &dyn Vault,
    captured_at: &str,
) -> Result<(EvidenceRecord, Vec<ManifestEntry>)> {
    let errs = plan.validation_errors();
    if !errs.is_empty() {
        bail!("record `{}` is invalid: {}", plan.id, errs.join("; "));
    }
    if plan.is_captured()
        || plan.manifest_sha256.is_some()
        || plan
            .items
            .iter()
            .any(|i| i.sha256.is_some() || i.files.is_some() || i.bytes.is_some())
    {
        bail!(
            "record `{}` is already captured — a snapshot is taken once",
            plan.id
        );
    }
    if vault.exists() {
        bail!(
            "vault {} exists — refused (a vault is created once)",
            vault.root_display()
        );
    }
    let mut problems = Vec::new();
    for item in &plan.items {
        match vault.inspect_source(&item.source) {
            Ok(info) if info.kind != item.kind => problems.push(format!(
                "item `{}`: source {} is a {:?}, the record says {:?}",
                item.path, item.source, info.kind, item.kind
            )),
            Ok(_) => {}
            Err(e) => problems.push(format!("item `{}`: {e:#}", item.path)),
        }
    }
    if !problems.is_empty() {
        bail!("nothing copied: {}", problems.join("; "));
    }
    vault.create()?;
    let mut record = plan.clone();
    let mut manifest = Vec::new();
    for item in &mut record.items {
        let entries = vault
            .copy_in(&item.source, item.kind, &item.path)
            .with_context(|| format!("item `{}`", item.path))?;
        item.files = Some(entries.len() as u64);
        item.bytes = Some(entries.iter().map(|e| e.bytes).sum());
        item.sha256 = Some(match item.kind {
            ItemKind::File => entries
                .first()
                .map(|e| e.sha256.clone())
                .context("a file item copied nothing")?,
            ItemKind::Dir => tree_hash(&item.path, &entries),
        });
        manifest.extend(entries);
    }
    let listed: BTreeSet<String> = vault.list()?.into_iter().collect();
    let copied: BTreeSet<String> = manifest.iter().map(|e| e.path.clone()).collect();
    if listed != copied {
        bail!(
            "the vault holds other files than the copies: extra {:?}, absent {:?}",
            listed.difference(&copied).collect::<Vec<_>>(),
            copied.difference(&listed).collect::<Vec<_>>()
        );
    }
    let bytes = manifest_bytes(&manifest)?;
    vault.write_manifest(&bytes)?;
    vault.seal()?;
    record.captured_at = Some(captured_at.to_string());
    record.manifest_sha256 = Some(manifest_sha256(&bytes)?);
    let errs = record.validation_errors();
    if !errs.is_empty() {
        bail!("captured record invalid: {}", errs.join("; "));
    }
    manifest.sort_by(|a, b| a.path.as_bytes().cmp(b.path.as_bytes()));
    Ok((record, manifest))
}

// ── verify ─────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub(crate) enum Verdict {
    Match,
    Mismatch,
    Absent,
    Extra,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct Verified {
    pub path: String,
    pub status: Verdict,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub actual: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct VerifyReport {
    pub record: String,
    pub vault: String,
    pub manifest: Verified,
    pub items: Vec<Verified>,
    pub files: usize,
    pub files_match: usize,
    /// Every file that is not `MATCH`.
    pub problems: Vec<Verified>,
}

impl VerifyReport {
    pub(crate) fn all_match(&self) -> bool {
        self.manifest.status == Verdict::Match
            && self.items.iter().all(|i| i.status == Verdict::Match)
            && self.problems.is_empty()
    }
}

fn verdict(path: &str, expected: Option<String>, actual: Option<String>) -> Verified {
    let status = match (&expected, &actual) {
        (Some(e), Some(a)) if e == a => Verdict::Match,
        (Some(_), Some(_)) => Verdict::Mismatch,
        (Some(_), None) => Verdict::Absent,
        (None, _) => Verdict::Extra,
    };
    Verified {
        path: path.to_string(),
        status,
        expected,
        actual,
    }
}

/// The module table's `verify`.
pub(crate) fn verify(record: &EvidenceRecord, vault: &dyn Vault) -> Result<VerifyReport> {
    if !record.is_captured() {
        bail!(
            "record `{}` is a plan (no captured_at) — snapshot it first",
            record.id
        );
    }
    if !vault.exists() {
        bail!("vault {} is ABSENT", vault.root_display());
    }
    let bytes = vault.read_manifest()?;
    let manifest_check = verdict(
        "MANIFEST.json",
        record.manifest_sha256.clone(),
        bytes.as_deref().map(manifest_sha256).transpose()?,
    );
    let listed: Vec<ManifestEntry> = match &bytes {
        Some(b) => serde_json::from_slice(b).context("MANIFEST.json does not parse")?,
        None => Vec::new(),
    };
    let on_disk: BTreeSet<String> = vault.list()?.into_iter().collect();
    let mut fresh: Vec<ManifestEntry> = Vec::new();
    let mut problems = Vec::new();
    let mut files_match = 0;
    for e in &listed {
        let actual = if on_disk.contains(&e.path) {
            Some(vault.hash(&e.path)?)
        } else {
            None
        };
        let v = verdict(
            &e.path,
            Some(format!("{} {}", e.sha256, e.bytes)),
            actual.as_ref().map(|a| format!("{} {}", a.sha256, a.bytes)),
        );
        if v.status == Verdict::Match {
            files_match += 1;
        } else {
            problems.push(v);
        }
        fresh.extend(actual);
    }
    let in_manifest: BTreeSet<&str> = listed.iter().map(|e| e.path.as_str()).collect();
    for path in &on_disk {
        if !in_manifest.contains(path.as_str()) {
            problems.push(verdict(path, None, Some(vault.hash(path)?.sha256)));
        }
    }
    let items = record
        .items
        .iter()
        .map(|item| {
            let mine: Vec<ManifestEntry> = fresh
                .iter()
                .filter(|e| match item.kind {
                    ItemKind::File => e.path == item.path,
                    ItemKind::Dir => e.path.starts_with(&format!("{}/", item.path)),
                })
                .cloned()
                .collect();
            let actual = (!mine.is_empty()).then(|| {
                let sha = match item.kind {
                    ItemKind::File => mine[0].sha256.clone(),
                    ItemKind::Dir => tree_hash(&item.path, &mine),
                };
                format!(
                    "{sha} {} {}",
                    mine.len(),
                    mine.iter().map(|e| e.bytes).sum::<u64>()
                )
            });
            let expected = Some(format!(
                "{} {} {}",
                item.sha256.as_deref().unwrap_or("UNKNOWN"),
                item.files.map_or("UNKNOWN".to_string(), |f| f.to_string()),
                item.bytes.map_or("UNKNOWN".to_string(), |b| b.to_string())
            ));
            verdict(&item.path, expected, actual)
        })
        .collect();
    Ok(VerifyReport {
        record: record.id.clone(),
        vault: vault.root_display(),
        manifest: manifest_check,
        items,
        files: listed.len(),
        files_match,
        problems,
    })
}

// ── coverage ───────────────────────────────────────────────────────

/// The module table's `coverage`. `instruments` narrows the keys;
/// `bars` = (source, interval name, interval ms).
pub(crate) fn coverage(
    history: &dyn RecordedHistory,
    schema: &str,
    window: &Window,
    instruments: Option<&BTreeSet<String>>,
    bars: Option<(&dyn BackfillSource, &str, i64)>,
) -> Result<Coverage> {
    let rows = history.instants(schema, window.from_ms, window.to_ms)?;
    let mut streams: BTreeMap<String, Stream> = BTreeMap::new();
    for (key, at, status) in rows {
        if instruments.is_some_and(|set| !set.contains(key_instrument(&key))) {
            continue;
        }
        let s = streams.entry(key.clone()).or_insert_with(|| Stream {
            key,
            covered_ms: Vec::new(),
            rows: 0,
        });
        s.rows += 1;
        if matches!(status, ObsStatus::Ok | ObsStatus::Partial) {
            s.covered_ms.push(at);
        }
    }
    let streams: Vec<Stream> = streams.into_values().collect();
    let second = match bars {
        Some((src, interval, bar_ms)) => {
            let mut b = Bars {
                bar_ms,
                source: format!("{} {interval}", src.describe()),
                by_instrument: BTreeMap::new(),
            };
            for s in streams.iter().filter(|s| !s.covered_ms.is_empty()) {
                let inst = key_instrument(&s.key).to_string();
                let got = src.bars(&inst, interval, window.from_ms - bar_ms, window.to_ms)?;
                b.by_instrument.insert(
                    inst,
                    got.into_iter()
                        .map(|x| Bar {
                            t_open_ms: x.t_open_ms,
                            trades: x.trades,
                        })
                        .collect(),
                );
            }
            Some(b)
        }
        None => None,
    };
    Ok(evidence_coverage::coverage(
        window,
        &streams,
        second.as_ref(),
    ))
}

// ── grade ──────────────────────────────────────────────────────────

/// The module table's `grade`: `accounts` empty = every account.
pub(crate) fn grade(ledger: &dyn LedgerSource, accounts: &[String]) -> Result<Vec<AccountGrade>> {
    let rows = ledger.read()?;
    let names: Vec<String> = if accounts.is_empty() {
        rows.accounts.iter().map(|a| a.account.clone()).collect()
    } else {
        accounts.to_vec()
    };
    names
        .iter()
        .map(|a| grade_account(&rows, a).map_err(anyhow::Error::msg))
        .collect()
}

// ── regrade ────────────────────────────────────────────────────────

fn merge(mut spans: Vec<(i64, i64)>) -> Vec<(i64, i64)> {
    spans.sort();
    let mut out: Vec<(i64, i64)> = Vec::new();
    for (a, b) in spans {
        match out.last_mut() {
            Some(last) if a <= last.1 => last.1 = last.1.max(b),
            _ => out.push((a, b)),
        }
    }
    out
}

/// The universe a regrade reads (module table).
pub(crate) fn regrade_universe(
    history: &dyn RecordedHistory,
    inst: &Instants,
) -> Result<Vec<String>> {
    let keys = history.keys(BOOK_SCHEMA, inst.entry_ms - HOUR_MS, inst.exit_ms + HOUR_MS)?;
    Ok(keys.iter().map(|k| key_instrument(k).to_string()).collect())
}

/// The module table's `regrade`.
pub(crate) fn regrade(
    history: &dyn RecordedHistory,
    market: Option<&dyn BackfillSource>,
    instruments: &[String],
    rule: &RegradeRule,
    inst: &Instants,
    lim: &Limits,
) -> Result<Regrade> {
    if !(inst.anchor_ms < inst.entry_ms
        && inst.entry_ms < inst.exit_ms
        && inst.anchor_ms < inst.signal_at()
        && inst.signal_at() < inst.exit_ms)
    {
        bail!("instants must satisfy anchor < entry < exit and anchor < signal < exit");
    }
    if !(rule.notional_usd.is_finite() && rule.notional_usd > 0.0) {
        bail!("notional_usd {} is not > 0", rule.notional_usd);
    }
    let mut ctx_spans = vec![
        (inst.anchor_ms - lim.anchor_max_age_ms, inst.anchor_ms + 1),
        (inst.entry_ms - lim.ctx_max_age_ms, inst.entry_ms + 1),
        (inst.signal_at() - lim.ctx_max_age_ms, inst.signal_at() + 1),
        (inst.exit_ms - lim.ctx_max_age_ms, inst.exit_ms + 1),
    ];
    let mut h = (inst.entry_ms.div_euclid(HOUR_MS) + 1) * HOUR_MS;
    while h <= inst.exit_ms {
        ctx_spans.push((h - lim.funding_max_age_ms, h + 1));
        h += HOUR_MS;
    }
    let ctx_spans = merge(ctx_spans);
    let book_spans = merge(vec![
        (
            inst.entry_ms - lim.book_max_age_ms,
            inst.entry_ms + lim.book_max_age_ms + 1,
        ),
        (
            inst.exit_ms - lim.book_max_age_ms,
            inst.exit_ms + lim.book_max_age_ms + 1,
        ),
    ]);
    let mut data = Vec::new();
    for id in instruments {
        let mut d = LegData {
            instrument: id.clone(),
            ..Default::default()
        };
        let ctx_key = format!("{CTX_SCHEMA}:{id}");
        for (a, b) in &ctx_spans {
            for r in history.rows(&ctx_key, *a, *b, false)? {
                d.ctx.push(CtxSample {
                    observed_at_ms: r.observed_at_ms,
                    status: r.status,
                    features: r.features,
                });
            }
        }
        let book_key = format!("{BOOK_SCHEMA}:{id}");
        for (a, b) in &book_spans {
            for r in history.rows(&book_key, *a, *b, true)? {
                let book = match r.data.as_ref().and_then(|v| v.get("book")) {
                    Some(v) => serde_json::from_value::<L2Book>(v.clone())
                        .map_err(|e| format!("book does not decode: {e}")),
                    None => Err(r
                        .errors
                        .clone()
                        .unwrap_or_else(|| "no book in the row's data".into())),
                };
                d.books.push(BookSample {
                    observed_at_ms: r.observed_at_ms,
                    status: r.status,
                    book,
                });
            }
        }
        if let Some(m) = market {
            d.funding = m
                .funding(id, inst.entry_ms, inst.exit_ms + HOUR_MS)?
                .into_iter()
                .map(|(t_ms, rate_1h)| FundingRate { t_ms, rate_1h })
                .collect();
            d.bars = m
                .bars(id, "1m", inst.entry_ms, inst.exit_ms + 1)?
                .into_iter()
                .map(|b| BarClose {
                    t_open_ms: b.t_open_ms,
                    close: b.close,
                })
                .collect();
        }
        d.ctx.sort_by_key(|s| s.observed_at_ms);
        d.ctx.dedup_by_key(|s| s.observed_at_ms);
        d.books.sort_by_key(|s| s.observed_at_ms);
        d.books.dedup_by_key(|s| s.observed_at_ms);
        data.push(d);
    }
    Ok(regrade::regrade(&data, rule, inst, lim))
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;
    use crate::adapters::outbound::evidence::vault::tests::unseal;
    use crate::adapters::outbound::evidence::vault::FsVault;
    use crate::domain::evidence::{EvidenceClass, EvidenceItem, Provenance};

    fn item(path: &str, source: &Path, kind: ItemKind) -> EvidenceItem {
        EvidenceItem {
            path: path.into(),
            source: source.to_string_lossy().into_owned(),
            kind,
            sha256: None,
            files: None,
            bytes: None,
            provenance: Provenance::LiveRecorded,
            class: EvidenceClass::ForwardPaper,
            role: "test".into(),
            note: None,
        }
    }

    #[test]
    fn snapshot_then_verify_then_tamper() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("src");
        std::fs::create_dir_all(src.join("hist/sub")).unwrap();
        std::fs::write(src.join("ledger.db"), b"ledger").unwrap();
        std::fs::write(src.join("hist/b.db"), b"bbb").unwrap();
        std::fs::write(src.join("hist/a.db"), b"aa").unwrap();
        std::fs::write(src.join("hist/sub/c.log"), b"c").unwrap();
        let plan = EvidenceRecord {
            id: "t".into(),
            title: "test".into(),
            notes: None,
            vault: "t".into(),
            captured_at: None,
            manifest_sha256: None,
            experiment: None,
            items: vec![
                item("s/ledger.db", &src.join("ledger.db"), ItemKind::File),
                item("s/history", &src.join("hist"), ItemKind::Dir),
            ],
        };
        let home = tmp.path().join("home");
        let vault = FsVault::new(&home, "t");
        let (rec, manifest) = snapshot(&plan, &vault, "2026-10-06T12:00:00Z").unwrap();
        assert!(rec.validation_errors().is_empty());
        assert_eq!(manifest.len(), 4);
        let dir = &rec.items[1];
        assert_eq!((dir.files, dir.bytes), (Some(3), Some(6)));
        // The tree hash = sha256 of the sorted `shasum -a 256` listing of the dir.
        let sha = |b: &[u8]| format!("{:x}", <sha2::Sha256 as sha2::Digest>::digest(b));
        let listing = format!(
            "{}  a.db\n{}  b.db\n{}  sub/c.log\n",
            sha(b"aa"),
            sha(b"bbb"),
            sha(b"c")
        );
        assert_eq!(
            dir.sha256.as_deref(),
            Some(sha(listing.as_bytes()).as_str())
        );
        assert_eq!(
            rec.items[0].sha256.as_deref(),
            Some(sha(b"ledger").as_str())
        );
        let root = home.join("state/evidence/t");
        let mbytes = std::fs::read(root.join("MANIFEST.json")).unwrap();
        assert_eq!(rec.manifest_sha256.as_deref(), Some(sha(&mbytes).as_str()));
        // Refusals: a captured record, an existing vault.
        assert!(snapshot(&rec, &FsVault::new(&home, "other"), "x").is_err());
        let again = snapshot(&plan, &vault, "x").unwrap_err().to_string();
        assert!(again.contains("exists"), "{again}");
        assert!(!home.join("state/evidence/other").exists());
        // Verify: all MATCH.
        let rep = verify(&rec, &vault).unwrap();
        assert!(rep.all_match(), "{rep:#?}");
        assert_eq!((rep.files, rep.files_match), (4, 4));
        // Tamper (after undoing the seal): MISMATCH, and an EXTRA file.
        unseal(&root);
        std::fs::write(root.join("s/history/b.db"), b"BBB").unwrap();
        std::fs::write(root.join("s/history/new.db"), b"n").unwrap();
        std::fs::remove_file(root.join("s/ledger.db")).unwrap();
        let rep = verify(&rec, &vault).unwrap();
        assert!(!rep.all_match());
        let status = |p: &str| rep.problems.iter().find(|v| v.path == p).map(|v| v.status);
        assert_eq!(status("s/history/b.db"), Some(Verdict::Mismatch));
        assert_eq!(status("s/history/new.db"), Some(Verdict::Extra));
        assert_eq!(status("s/ledger.db"), Some(Verdict::Absent));
        assert_eq!(rep.items[0].status, Verdict::Absent);
        assert_eq!(rep.items[1].status, Verdict::Mismatch);
        assert_eq!(rep.manifest.status, Verdict::Match);
    }

    #[test]
    fn a_missing_source_creates_no_vault() {
        let tmp = tempfile::tempdir().unwrap();
        let plan = EvidenceRecord {
            id: "t".into(),
            title: "test".into(),
            notes: None,
            vault: "t".into(),
            captured_at: None,
            manifest_sha256: None,
            experiment: None,
            items: vec![item("x", &tmp.path().join("nope"), ItemKind::File)],
        };
        let vault = FsVault::new(tmp.path(), "t");
        assert!(snapshot(&plan, &vault, "x").is_err());
        assert!(!vault.exists());
    }
}
