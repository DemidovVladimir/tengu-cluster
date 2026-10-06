//! `tengu lineage …` — the lineage registry's checks and views
//! (`docs/lineage-2026-10-06.md` § 5). No sandbox config, no secrets: it
//! reads the registry (`--registry`, default `lineage` under the cwd), the
//! repo around it (`sandboxes/`, `skills/`, `repo:` files) and
//! `<TENGU_HOME>/state` read-only; only `seal` writes (one row appended to
//! `locks.toml`). stdout carries the view; logs go to stderr.
//!
//! | Command | Prints | Exit 1 when |
//! |---|---|---|
//! | `verify [--pins] [--evidence]` | the findings table (severity, code, record, message) + counts | any Error finding, or the registry does not load (`load_error` per file) |
//! | `show <id>` | the record file as written (text) · its fields (json); an episode's quadrant | no such record |
//! | `trace <id>` | family → variants → experiments → windows / results / evidence → verdict → episodes → incidents (`▶` = the start) | no such record |
//! | `family <id> [--state xlab]…` | the variant tree + search accounting (run dirs of the states) | no such family |
//! | `attempts [--state xlab]…` | every run dir → its variants or `UNREGISTERED`; holdout reads; unreadable dirs | — |
//! | `report <family> --forward <experiment> [--state xlab]…` | the 21 Rule-W acceptance answers (handoff § 57) with their source fields | no such record |
//! | `capabilities [--generation ID]` | every capability (class, version, permission, lifecycle, bindings, contract, generations) | no such generation |
//! | `generation <ID>` | the generation, its frozen digest (manifest + listed capability records) against its lock, every pin OK / DRIFT / UNRESOLVED | no such generation |
//! | `seal <variant:ID \| experiment:ID>` | appends `[[sealed]] record, sha256 (the file's digest), sealed_at (now)` to `locks.toml` | already sealed, not `preregistered = true`, its outcome already known, or the registry has errors on it |
//!
//! `--format text|json` on every view.

use std::io::Write as _;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Context, Result};
use clap::{Args, Subcommand, ValueEnum};
use serde::Serialize;
use serde_json::json;

use crate::adapters::outbound::lineage::{result_sources, FsResolver, RepoProbe, RunDirs};
use crate::application::lineage::attempts::scan;
use crate::application::lineage::verify::{pin_status, verify, VerifyOpts};
use crate::config::lineage::{load_registry, record_path, repo_root, LOCKS_FILE};
use crate::config::paths::resolve_tengu_home;
use crate::domain::lineage::acceptance::acceptance;
use crate::domain::lineage::locks::sealed_row;
use crate::domain::lineage::query::{
    attempt_rows, enum_name, family_report, resolve_id, trace, Attempts,
};
use crate::domain::lineage::registry::label;
use crate::domain::lineage::value::{parse_seal_record, RecordKind, Time, TimeOrder};
use crate::domain::lineage::{has_errors, Finding, Registry, Severity};
use crate::ports::lineage::ResultSource;

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub(super) enum Format {
    Text,
    Json,
}

#[derive(Args)]
pub(crate) struct LineageArgs {
    /// The registry dir (default `lineage` under the cwd).
    #[arg(long, global = true)]
    registry: Option<PathBuf>,
    /// `text` (default) or `json`.
    #[arg(long, global = true, value_enum, default_value_t = Format::Text)]
    format: Format,
    #[command(subcommand)]
    action: LineageAction,
}

#[derive(Subcommand)]
pub(super) enum LineageAction {
    /// Check the registry: references, time, holdout, forward, locks; with
    /// --pins every pin and binding; with --evidence every file and result.
    Verify {
        #[arg(long)]
        pins: bool,
        #[arg(long)]
        evidence: bool,
    },
    /// One record.
    Show { id: String },
    /// Family → variants → experiments → evidence → verdict → episodes → incidents.
    Trace { id: String },
    /// A family's variant tree and search accounting.
    Family {
        id: String,
        /// State dirs whose run dirs count (repeatable).
        #[arg(long = "state", default_value = "xlab")]
        states: Vec<String>,
    },
    /// Every run dir → its variant, or UNREGISTERED.
    Attempts {
        #[arg(long = "state", default_value = "xlab")]
        states: Vec<String>,
    },
    /// The 21 Rule-W acceptance answers (handoff § 57).
    Report {
        family: String,
        #[arg(long)]
        forward: String,
        #[arg(long = "state", default_value = "xlab")]
        states: Vec<String>,
    },
    /// The capability registry (one generation's with --generation).
    Capabilities {
        #[arg(long)]
        generation: Option<String>,
    },
    /// A generation, its lock and its pins (OK / DRIFT / UNRESOLVED).
    Generation { id: String },
    /// Seal a preregistration: variant:<id> or experiment:<id>.
    Seal { record: String },
}

fn print_json<T: Serialize>(v: &T) -> Result<()> {
    println!("{}", serde_json::to_string_pretty(v)?);
    Ok(())
}

fn load(dir: &Path) -> Result<Registry> {
    load_registry(dir).map_err(|errs| {
        anyhow!(
            "the registry {} does not load ({} problem(s)):\n{}",
            dir.display(),
            errs.len(),
            errs.iter()
                .map(|e| format!("- {e}"))
                .collect::<Vec<_>>()
                .join("\n")
        )
    })
}

fn attempts(states: &[String]) -> Attempts {
    let source = RunDirs {
        tengu_home: resolve_tengu_home(),
    };
    scan(&source, states)
}

/// `tengu lineage` (module table).
pub(super) fn run_lineage(args: LineageArgs) -> Result<()> {
    let dir = args.registry.unwrap_or_else(|| PathBuf::from("lineage"));
    let format = args.format;
    match args.action {
        LineageAction::Verify { pins, evidence } => run_verify(&dir, pins, evidence, format),
        LineageAction::Show { id } => show(&dir, &load(&dir)?, &id, format),
        LineageAction::Trace { id } => {
            let reg = load(&dir)?;
            let t = trace(&reg, &id).map_err(|e| anyhow!(e))?;
            if format == Format::Json {
                return print_json(&t);
            }
            for l in &t.lines {
                println!(
                    "{}{}{} {}: {}",
                    if l.start { "▶ " } else { "  " },
                    "  ".repeat(l.depth),
                    l.kind,
                    l.id,
                    l.text
                );
            }
            Ok(())
        }
        LineageAction::Family { id, states } => {
            let reg = load(&dir)?;
            let r = family_report(&reg, &id, &attempts(&states)).map_err(|e| anyhow!(e))?;
            if format == Format::Json {
                return print_json(&r);
            }
            println!(
                "family {} — {} [{} · {}: {}]",
                r.family, r.title, r.role, r.status, r.status_reason
            );
            println!("hypothesis: {}", r.hypothesis);
            println!("variants:");
            for v in &r.variants {
                println!(
                    "  {}{} [{}{}] runs {} · holdout reads {} · experiments {}{}",
                    "  ".repeat(v.depth),
                    v.id,
                    v.status,
                    if v.preregistered {
                        ", preregistered"
                    } else {
                        ""
                    },
                    v.runs,
                    v.holdout_reads,
                    if v.experiments.is_empty() {
                        "none".to_string()
                    } else {
                        v.experiments.join(", ")
                    },
                    if v.changed.is_empty() {
                        String::new()
                    } else {
                        format!(" · changed {}", v.changed.join(", "))
                    }
                );
                println!("  {}  spec: {}", "  ".repeat(v.depth), v.spec);
            }
            let s = &r.search;
            println!("search accounting:");
            println!("  registered variants: {}", s.registered_variants);
            println!("  prior search: {}", s.prior_search);
            println!(
                "  run dirs ({}): {} distinct spec hash(es), {} run(s), {} holdout read(s)",
                if s.states.is_empty() {
                    "none scanned".to_string()
                } else {
                    s.states.join(", ")
                },
                s.distinct_spec_hashes,
                s.runs,
                s.holdout_reads
            );
            for u in &s.unregistered {
                println!(
                    "  UNREGISTERED {} (strategy {}): runs {} · holdout reads {}",
                    u.spec_sha256,
                    u.strategy,
                    u.runs.join(", "),
                    u.holdout_reads
                );
            }
            println!("  total tried: {}", s.total_tried);
            Ok(())
        }
        LineageAction::Attempts { states } => {
            let reg = load(&dir)?;
            let a = attempts(&states);
            let rows = attempt_rows(&reg, &a);
            if format == Format::Json {
                return print_json(
                    &json!({"rows": rows, "holdout_reads": a.holdout_reads, "problems": a.problems}),
                );
            }
            for r in &rows {
                println!(
                    "{}  {}{}  {}  {}  {}",
                    r.run.state,
                    r.run.run_id,
                    if r.run.kept { " (keep)" } else { "" },
                    r.run.strategy,
                    r.run.spec_sha256,
                    if r.variants.is_empty() {
                        "UNREGISTERED".to_string()
                    } else {
                        r.variants.join(", ")
                    }
                );
            }
            println!(
                "{} run dir(s), {} unregistered, {} holdout read(s) in {}",
                rows.len(),
                rows.iter().filter(|r| r.variants.is_empty()).count(),
                a.holdout_reads.len(),
                states.join(", ")
            );
            for p in &a.problems {
                eprintln!("problem: {p}");
            }
            Ok(())
        }
        LineageAction::Report {
            family,
            forward,
            states,
        } => {
            let reg = load(&dir)?;
            let answers =
                acceptance(&reg, &family, &forward, &attempts(&states)).map_err(|e| anyhow!(e))?;
            if format == Format::Json {
                return print_json(&answers);
            }
            for a in &answers {
                println!("{:>2}. {}", a.n, a.question);
                for l in &a.lines {
                    println!("    {l}");
                }
                if a.is_unknown() {
                    let why = a.unknown_reason.as_deref().unwrap_or_default();
                    println!("    (UNKNOWN: {why})");
                }
                if !a.sources.is_empty() {
                    println!("    sources: {}", a.sources.join(", "));
                }
            }
            Ok(())
        }
        LineageAction::Capabilities { generation } => {
            capabilities(&load(&dir)?, generation.as_deref(), format)
        }
        LineageAction::Generation { id } => generation(&dir, &load(&dir)?, &id, format),
        LineageAction::Seal { record } => seal(&dir, &record),
    }
}

fn run_verify(dir: &Path, pins: bool, evidence: bool, format: Format) -> Result<()> {
    let findings = match load_registry(dir) {
        Ok(reg) => {
            let probe = RepoProbe::new(dir);
            let resolver = FsResolver {
                repo: repo_root(dir),
                tengu_home: resolve_tengu_home(),
                evidence: reg.evidence.values().cloned().collect(),
                check_git: evidence,
            };
            let boxed = result_sources();
            let sources: Vec<&dyn ResultSource> = boxed.iter().map(|b| b.as_ref()).collect();
            verify(
                &reg,
                VerifyOpts { pins, evidence },
                &probe,
                &resolver,
                &sources,
            )
        }
        Err(errs) => errs
            .into_iter()
            .map(|e| Finding::error("load_error", dir.display().to_string(), e))
            .collect(),
    };
    let errors = findings
        .iter()
        .filter(|f| f.severity == Severity::Error)
        .count();
    let warns = findings
        .iter()
        .filter(|f| f.severity == Severity::Warn)
        .count();
    if format == Format::Json {
        print_json(&findings)?;
    } else {
        for f in &findings {
            println!(
                "{:<5}  {:<28}  {:<36}  {}",
                f.severity.to_string(),
                f.code,
                f.record,
                f.message
            );
        }
        println!(
            "lineage verify {}{}{}: {errors} error(s), {warns} warning(s)",
            dir.display(),
            if pins { " --pins" } else { "" },
            if evidence { " --evidence" } else { "" }
        );
    }
    std::io::stdout().flush().ok();
    if has_errors(&findings) {
        bail!("lineage verify: {errors} error(s)");
    }
    Ok(())
}

fn show(dir: &Path, reg: &Registry, id: &str, format: Format) -> Result<()> {
    let (kind, id) = resolve_id(reg, id).map_err(|e| anyhow!(e))?;
    let quadrant = (kind == RecordKind::Episode)
        .then(|| reg.episodes.get(&id).map(|e| e.quadrant().name()))
        .flatten();
    if format == Format::Json {
        return print_json(&json!({
            "kind": kind,
            "id": id,
            "digest": reg.digests.get(&(kind, id.clone())),
            "quadrant": quadrant,
            "record": reg.record_json(kind, &id),
        }));
    }
    let path = record_path(dir, kind, &id);
    let text = std::fs::read_to_string(&path).with_context(|| path.display().to_string())?;
    println!("# {} — {}", label(kind, &id), path.display());
    if let Some(d) = reg.digests.get(&(kind, id.clone())) {
        println!("# digest {d}");
    }
    if let Some(q) = quadrant {
        println!("# quadrant {q}");
    }
    print!("{text}");
    Ok(())
}

fn capabilities(reg: &Registry, generation: Option<&str>, format: Format) -> Result<()> {
    let only = match generation {
        Some(g) => Some(
            reg.generations
                .get(g)
                .ok_or_else(|| anyhow!("no generation `{g}`"))?,
        ),
        None => None,
    };
    let rows: Vec<_> = reg
        .capabilities
        .values()
        .filter(|c| only.map_or(true, |g| g.capabilities.iter().any(|r| r.id == c.id)))
        .map(|c| {
            let gens: Vec<String> = reg
                .generations
                .values()
                .filter_map(|g| {
                    g.capabilities
                        .iter()
                        .find(|r| r.id == c.id)
                        .map(|r| format!("{} v{}", g.id, r.version))
                })
                .collect();
            json!({
                "id": c.id, "title": c.title, "class": c.class, "version": c.version,
                "permission": c.permission, "lifecycle": c.lifecycle,
                "contract": c.contract.to_string(),
                "bindings": c.bindings.iter().map(|b| b.to_string()).collect::<Vec<_>>(),
                "generations": gens,
            })
        })
        .collect();
    if format == Format::Json {
        return print_json(&rows);
    }
    for r in &rows {
        let s = |k: &str| r[k].as_str().unwrap_or("").to_string();
        let list = |k: &str| {
            r[k].as_array()
                .map(|a| {
                    a.iter()
                        .filter_map(|x| x.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                })
                .unwrap_or_default()
        };
        println!(
            "{} v{} [{} · {} · {}] {} — bindings {} · contract {} · generations {}",
            s("id"),
            r["version"],
            s("class"),
            s("permission"),
            s("lifecycle"),
            s("title"),
            if list("bindings").is_empty() {
                "none".to_string()
            } else {
                list("bindings")
            },
            s("contract"),
            if list("generations").is_empty() {
                "none".to_string()
            } else {
                list("generations")
            }
        );
    }
    println!(
        "{} capabilit{}{}",
        rows.len(),
        if rows.len() == 1 { "y" } else { "ies" },
        generation.map_or(String::new(), |g| format!(" of generation {g}"))
    );
    Ok(())
}

fn generation(dir: &Path, reg: &Registry, id: &str, format: Format) -> Result<()> {
    let g = reg
        .generations
        .get(id)
        .ok_or_else(|| anyhow!("no generation `{id}`"))?;
    let probe = RepoProbe::new(dir);
    let pins = pin_status(reg, id, &probe).map_err(|e| anyhow!(e))?;
    let digest = reg.digests.get(&(RecordKind::Generation, id.to_string()));
    let frozen = reg.frozen_digest(id);
    let lock = reg.locks.frozen.iter().rev().find(|f| f.generation == id);
    let lock_status = match (lock, &frozen) {
        (None, _) => "NO_LOCK",
        (Some(l), Some(d)) if &l.manifest_sha256 == d => "OK",
        _ => "CHANGED",
    };
    if format == Format::Json {
        return print_json(&json!({
            "generation": g, "digest": digest, "frozen_digest": frozen, "lock": lock,
            "lock_status": lock_status, "pins": pins,
        }));
    }
    println!(
        "generation {} — {} [{} · parent {} · frozen {}]",
        g.id,
        g.title,
        enum_name(&g.status),
        g.parent,
        g.frozen_at.map_or("UNKNOWN".to_string(), |t| t.to_string())
    );
    println!("sandboxes: {}", g.sandboxes.join(", "));
    println!(
        "capabilities: {}",
        g.capabilities
            .iter()
            .map(|c| format!("{} v{}", c.id, c.version))
            .collect::<Vec<_>>()
            .join(", ")
    );
    println!(
        "manifest digest {} · frozen digest (manifest + listed capabilities) {} · lock {lock_status}{}",
        digest.map_or("UNKNOWN", String::as_str),
        frozen.as_deref().unwrap_or("UNKNOWN"),
        lock.map_or(String::new(), |l| format!(
            " ({} at {})",
            l.manifest_sha256, l.frozen_at
        ))
    );
    for p in &pins {
        println!(
            "{:<10}  {:<15}  {}  pinned {}{}",
            p.status,
            enum_name(&p.role),
            p.target,
            p.pinned,
            match (&p.now, &p.reason) {
                (Some(now), _) if *now != p.pinned => format!(" now {now}"),
                (_, Some(r)) => format!(" — {r}"),
                _ => String::new(),
            }
        );
    }
    Ok(())
}

fn seal(dir: &Path, record: &str) -> Result<()> {
    let (kind, id) = parse_seal_record(record).map_err(|e| anyhow!(e))?;
    let reg = load(dir)?;
    if reg.locks.sealed.iter().any(|s| s.record == record) {
        bail!(
            "{record} is already sealed in {} — a seal is never redone",
            dir.join(LOCKS_FILE).display()
        );
    }
    let preregistered = match kind {
        RecordKind::Variant => reg.variants.get(&id).map(|v| v.preregistered),
        _ => reg.experiments.get(&id).map(|x| x.preregistered),
    }
    .ok_or_else(|| anyhow!("no {kind} `{id}`"))?;
    if !preregistered {
        bail!("{kind} `{id}` is not preregistered = true — set it before sealing");
    }
    let mine = label(kind, &id);
    let errors: Vec<Finding> = reg
        .validate()
        .into_iter()
        .filter(|f| f.severity == Severity::Error && f.record == mine && f.code != "seal_mismatch")
        .collect();
    if !errors.is_empty() {
        bail!(
            "{mine} has errors — fix them before sealing:\n{}",
            errors
                .iter()
                .map(|f| format!("- {} {}", f.code, f.message))
                .collect::<Vec<_>>()
                .join("\n")
        );
    }
    let now = Time::At(chrono::Utc::now().timestamp_millis() / 1000 * 1000);
    match reg.first_outcome(kind, &id) {
        None => {}
        Some(Time::Unknown) => bail!(
            "{mine}: its first outcome is UNKNOWN (a FORWARD window start or a ran_at not \
             given) — a seal cannot be shown to precede it; record the start first"
        ),
        Some(outcome) if outcome.order(&now) != TimeOrder::After => bail!(
            "{mine}: its first outcome ({outcome}) is already past — a seal now is not a \
             preregistration"
        ),
        Some(_) => {}
    }
    let digest = reg
        .digests
        .get(&(kind, id.clone()))
        .ok_or_else(|| anyhow!("{mine}: no digest"))?;
    let row = sealed_row(record, digest, &now);
    let locks = dir.join(LOCKS_FILE);
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&locks)
        .with_context(|| locks.display().to_string())?;
    f.write_all(row.as_bytes())
        .with_context(|| locks.display().to_string())?;
    print!("{row}");
    eprintln!("sealed {record} into {}", locks.display());
    Ok(())
}
