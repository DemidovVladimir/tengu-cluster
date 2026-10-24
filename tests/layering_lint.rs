//! Hexagonal layering lint — `docs/archive/2026-09/hexagonal-plan-2026-09-23.md`.
//!
//! Every `crate::<layer>` reference in a layer's non-test code must point at
//! a layer it may depend on:
//!
//! | Layer | May import |
//! |---|---|
//! | `domain` | `domain` (and no IO crates) |
//! | `ports` | `domain`, `config` |
//! | `config` | `domain` |
//! | `application` | `domain`, `ports`, `config`, `application` (IO crates only in the shrink-only `APPLICATION_IO`) |
//! | `adapters/outbound` | all but `adapters::inbound`, `bootstrap` (`FORBIDDEN`) |
//! | `bootstrap` | all but `adapters::inbound` (`FORBIDDEN`) |
//! | `adapters/inbound`, `main.rs`, `lib.rs` | everything |
//!
//! | Subtree (`FORBIDDEN`) | Must not import |
//! |---|---|
//! | `domain/soe` | `domain::xm`, `domain::backtest::{engine, spec}`, `config::risk` |
//! | `domain/xm` · `domain/backtest` · `domain/source` | `domain::soe` |
//! | `application/backtest` | `application::soe` |
//! | `application/soe` | `application::backtest`, `domain::xm`, `domain::backtest::engine` |
//!
//! Core never imports a vertical (`VERTICALS`: the trading desk, xlab, the
//! Solana family, lineage / evidence, SOE and the source layer): every other
//! module of `domain`, `ports`, `config`, `application` and
//! `adapters/outbound` is core, and only the `COMPOSITION` files (module
//! lists, the `Config` aggregate and its loader, the tool catalog),
//! `bootstrap/`, `adapters/inbound/`, `main.rs` and `lib.rs` wire verticals in. A
//! vertical may use core and other verticals.
//!
//! A `use` statement counts leaf by leaf: grouped and multi-line imports
//! (`use crate::domain::{soe, xm::risk};`) are expanded (`file_paths`);
//! the core check also resolves `super::` paths (`resolve_super`).
//!
//! Test code is inline `#[cfg(test)] mod … { … }` blocks and the files the
//! crate builds only in tests (`tests/support/test_files.rs`: a `mod x;`
//! behind `#[cfg(test)]`, `#[path]` or not, and every module below one).
//!
//! `EXCEPTIONS` lists known violations still being unwound. It may only
//! shrink; the rewrite is done when it is empty.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

#[path = "support/test_files.rs"]
mod support;
use support::test_files;

const LAYERS: &[&str] = &[
    "domain",
    "ports",
    "config",
    "application",
    "adapters",
    "bootstrap",
];

/// (layer dir under `src/`, crate-root modules it may reference).
const RULES: &[(&str, &[&str])] = &[
    ("domain", &["domain"]),
    ("ports", &["domain", "ports", "config"]),
    ("config", &["domain", "config"]),
    ("application", &["domain", "ports", "config", "application"]),
];

/// (layer dir under `src/`, path prefixes it must not reference) — for
/// layers that may use most of the crate.
const FORBIDDEN: &[(&str, &[&str])] = &[
    (
        "adapters/outbound",
        &["crate::adapters::inbound", "crate::bootstrap"],
    ),
    // The composition root builds adapters; inbound adapters call it, never
    // the other way round.
    ("bootstrap", &["crate::adapters::inbound"]),
    // The Software Opportunity Engine and the trading stack stay apart
    // (`docs/soe-2026-10-08.md` § 13 generation isolation): the opportunity
    // domain never reads the trading domain, the backtest engine or its
    // specs, or the risk config; neither reads it back. The source layer is
    // cross-domain: imports go soe → source only (brief D1).
    (
        "domain/soe",
        &[
            "crate::domain::xm",
            "crate::domain::backtest::engine",
            "crate::domain::backtest::spec",
            "crate::config::risk",
        ],
    ),
    ("domain/xm", &["crate::domain::soe"]),
    ("domain/backtest", &["crate::domain::soe"]),
    ("domain/source", &["crate::domain::soe"]),
    ("application/backtest", &["crate::application::soe"]),
    (
        "application/soe",
        &[
            "crate::application::backtest",
            "crate::domain::xm",
            "crate::domain::backtest::engine",
        ],
    ),
];

/// Vertical modules (`crate::` paths, whole segments): core code must not
/// name one (`core_never_uses_a_vertical`).
const VERTICALS: &[&str] = &[
    "crate::domain::backtest",
    "crate::domain::book",
    "crate::domain::calendar",
    "crate::domain::evidence",
    "crate::domain::evidence_coverage",
    "crate::domain::hl",
    "crate::domain::lineage",
    "crate::domain::lp",
    "crate::domain::market",
    "crate::domain::marketdata",
    "crate::domain::marketdata_decode",
    "crate::domain::marketdata_stats",
    "crate::domain::sec",
    "crate::domain::soe",
    "crate::domain::solana",
    "crate::domain::solana_tx",
    "crate::domain::solana_write",
    "crate::domain::source",
    "crate::domain::xm",
    "crate::ports::book",
    "crate::ports::evidence",
    "crate::ports::lineage",
    "crate::ports::market_data",
    "crate::ports::paper",
    "crate::ports::soe",
    "crate::ports::solana_signer",
    "crate::ports::solana_writes",
    "crate::ports::source_store",
    "crate::application::backtest",
    "crate::application::evidence",
    "crate::application::lineage",
    "crate::application::paper",
    "crate::application::ranking",
    "crate::application::soe",
    "crate::application::sources",
    "crate::config::backtest",
    "crate::config::lineage",
    "crate::config::risk",
    "crate::config::soe",
    "crate::config::solana",
    "crate::config::sources",
    "crate::config::strategy_ranking",
    "crate::config::xmarket",
    "crate::adapters::outbound::backfill",
    "crate::adapters::outbound::decision_cache",
    "crate::adapters::outbound::evidence",
    "crate::adapters::outbound::hyperliquid",
    "crate::adapters::outbound::lineage",
    "crate::adapters::outbound::market_data",
    "crate::adapters::outbound::paper_store",
    "crate::adapters::outbound::soe",
    "crate::adapters::outbound::solana",
    "crate::adapters::outbound::sources",
    "crate::adapters::outbound::tools::crypto",
    "crate::adapters::outbound::tools::hyperliquid",
    "crate::adapters::outbound::tools::soe",
    "crate::adapters::outbound::tools::solana",
    "crate::adapters::outbound::tools::sources",
    "crate::adapters::outbound::tools::xlab",
    "crate::adapters::outbound::tools::xm",
];

/// Core files (relative to `src/`) that wire verticals in; `bootstrap/`,
/// `adapters/inbound/`, `main.rs` and `lib.rs` are composition as a whole.
const COMPOSITION: &[&str] = &[
    "domain/mod.rs",
    "ports/mod.rs",
    "config/mod.rs",
    "config/load.rs",
    "config/sections.rs",
    "application/mod.rs",
    "adapters/mod.rs",
    "adapters/outbound/mod.rs",
    "adapters/outbound/tools/mod.rs",
];

/// Crates that do IO: `domain` must not name them, `application` only in
/// [`APPLICATION_IO`].
const IO_CRATES: &[&str] = &[
    "reqwest::",
    "tokio::fs",
    "tokio::net",
    "tokio::process",
    "std::fs",
    "std::net",
    "std::process",
    "rusqlite::",
    "tokio_postgres::",
];

/// Known violations: (file relative to `src/`, forbidden path prefix).
/// Remove entries as they are fixed; never add without a plan entry.
const EXCEPTIONS: &[(&str, &str)] = &[];

/// `application` files (relative to `src/`) whose non-test code still names
/// an IO crate. IO belongs in an outbound adapter behind a port: a file leaves
/// this list when its IO moves out; nothing joins (shrink-only,
/// `application_io_list_only_shrinks`).
const APPLICATION_IO: &[&str] = &[
    "application/backtest/gate.rs",
    "application/backtest/mod.rs",
    "application/backtest/run_dir.rs",
    "application/decision_loop/mod.rs",
    "application/orchestrator/shared_files.rs",
    "application/ranking/mod.rs",
    "application/ranking/store.rs",
    "application/runtime/health.rs",
    "application/skills/lifecycle/audit.rs",
    "application/skills/lifecycle/evolve.rs",
    "application/skills/lifecycle/fixtures.rs",
    "application/skills/lifecycle/metric_kinds/description_trigger.rs",
    "application/skills/lifecycle/metric_kinds/llm_judge.rs",
    "application/skills/lifecycle/metrics.rs",
    "application/skills/lifecycle/scanner.rs",
    "application/skills/lifecycle/scratch_worktree.rs",
    "application/skills/lifecycle/storage.rs",
    "application/skills/registry.rs",
    "application/tools/registry.rs",
];

fn rust_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            rust_files(&path, out);
        } else if path.extension().is_some_and(|e| e == "rs") {
            out.push(path);
        }
    }
}

/// Line numbers + text of non-comment code, skipping `#[cfg(test)] mod … { … }`
/// blocks wherever they sit in the file (brace-matched).
fn code_lines(src: &str) -> Vec<(usize, &str)> {
    let lines: Vec<&str> = src.lines().collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        let next = lines.get(i + 1).map(|l| l.trim_start()).unwrap_or("");
        if lines[i].trim() == "#[cfg(test)]" && (next.starts_with("mod ") || next.contains(" mod "))
        {
            let mut depth = 0i32;
            let mut j = i + 1;
            while j < lines.len() {
                depth += lines[j].matches('{').count() as i32;
                depth -= lines[j].matches('}').count() as i32;
                j += 1;
                if depth <= 0
                    && (lines[j - 1].contains('}') || lines[j - 1].trim_end().ends_with(';'))
                {
                    break;
                }
            }
            i = j;
            continue;
        }
        if !lines[i].trim_start().starts_with("//") {
            out.push((i + 1, lines[i]));
        }
        i += 1;
    }
    out
}

/// Every `.rs` file under `src/` by its `src/`-relative path.
fn src_texts(src: &Path) -> BTreeMap<String, String> {
    let mut paths = Vec::new();
    rust_files(src, &mut paths);
    paths
        .iter()
        .map(|p| {
            let rel = p
                .strip_prefix(src)
                .unwrap()
                .to_string_lossy()
                .replace('\\', "/");
            (rel, fs::read_to_string(p).unwrap())
        })
        .collect()
}

/// Every `crate::<layer>...` path in `line`, as the full path text.
fn crate_paths(line: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest = line;
    while let Some(i) = rest.find("crate::") {
        let tail = &rest[i..];
        let end = tail
            .find(|c: char| !(c.is_alphanumeric() || c == '_' || c == ':'))
            .unwrap_or(tail.len());
        let path = tail[..end].trim_end_matches(':').to_string();
        if LAYERS
            .iter()
            .any(|l| path == format!("crate::{l}") || path.starts_with(&format!("crate::{l}::")))
        {
            out.push(path);
        }
        rest = &tail[end.max(7)..];
    }
    out
}

/// `tree` (a `use` tree after `prefix`) expanded into full paths:
/// `domain::{xm, soe::{a, b as c}, self}` → `domain::xm`, `domain::soe::a`,
/// `domain::soe::b`, `domain`.
fn expand_use(prefix: &str, tree: &str, out: &mut Vec<String>) {
    let tree = tree.trim();
    if tree.is_empty() {
        return;
    }
    let Some(open) = tree.find('{') else {
        let path = tree.split(" as ").next().unwrap_or(tree).trim();
        let full = match path {
            "self" => prefix.trim_end_matches("::").to_string(),
            _ => format!("{prefix}{path}"),
        };
        out.push(full);
        return;
    };
    let head = &tree[..open];
    let inner = tree[open + 1..].trim_end().trim_end_matches('}');
    let mut depth = 0i32;
    let mut start = 0;
    for (i, c) in inner.char_indices() {
        match c {
            '{' => depth += 1,
            '}' => depth -= 1,
            ',' if depth == 0 => {
                expand_use(&format!("{prefix}{head}"), &inner[start..i], out);
                start = i + 1;
            }
            _ => {}
        }
    }
    expand_use(&format!("{prefix}{head}"), &inner[start..], out);
}

/// Every `crate::<layer>…` path of a file's non-test code with its line:
/// inline paths (`crate_paths`) and each `use crate::…;` statement expanded
/// (grouped and multi-line imports included).
fn file_paths(text: &str) -> Vec<(usize, String)> {
    let mut out = Vec::new();
    let mut stmt: Option<(usize, String)> = None;
    for (n, line) in code_lines(text) {
        out.extend(crate_paths(line).into_iter().map(|p| (n, p)));
        let t = line.trim();
        let t = t
            .strip_prefix("pub(crate) ")
            .or_else(|| t.strip_prefix("pub "))
            .unwrap_or(t);
        if stmt.is_none() && t.starts_with("use crate::") {
            stmt = Some((n, String::new()));
        }
        if let Some((_, s)) = stmt.as_mut() {
            s.push_str(t);
            s.push(' ');
        }
        if t.ends_with(';') {
            if let Some((at, s)) = stmt.take() {
                let body = s.trim().trim_end_matches(';');
                let body = body.strip_prefix("use ").unwrap_or(body);
                let mut paths = Vec::new();
                expand_use("", body, &mut paths);
                out.extend(
                    paths
                        .into_iter()
                        .filter(|p| crate_paths(p).first() == Some(p))
                        .map(|p| (at, p)),
                );
            }
        }
    }
    out
}

/// `crate::` path of the module a `src/`-relative file defines
/// (`config/builder/mod.rs` → `crate::config::builder`).
fn module_of(rel: &str) -> String {
    let m = rel.trim_end_matches(".rs").trim_end_matches("/mod");
    format!("crate::{}", m.replace('/', "::"))
}

fn is_vertical(path: &str) -> bool {
    VERTICALS
        .iter()
        .any(|v| path == *v || path.starts_with(&format!("{v}::")))
}

/// `text` with `super::` (and `super::super::`) spelled from the crate root,
/// so `file_paths` sees relative imports too.
fn resolve_super(rel: &str, text: &str) -> String {
    let parent = |m: &str| m.rsplit_once("::").map(|(p, _)| p.to_string());
    let module = module_of(rel);
    let Some(up) = parent(&module) else {
        return text.to_string();
    };
    let mut out = text.to_string();
    if let Some(up2) = parent(&up) {
        out = out.replace("super::super::", &format!("{up2}::"));
    }
    out.replace("super::", &format!("{up}::"))
}

/// Core files that name a vertical (module doc: core never imports one).
fn core_vertical_violations() -> Vec<String> {
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let texts = src_texts(&src);
    let tests = test_files(&texts);
    let mut found = Vec::new();
    for (rel, text) in &texts {
        let composition = rel == "main.rs"
            || rel == "lib.rs"
            || rel.starts_with("bootstrap/")
            || rel.starts_with("adapters/inbound/")
            || COMPOSITION.contains(&rel.as_str());
        if composition || tests.contains(rel) || is_vertical(&module_of(rel)) {
            continue;
        }
        for (n, path) in file_paths(&resolve_super(rel, text)) {
            if is_vertical(&path)
                && !EXCEPTIONS
                    .iter()
                    .any(|(f, p)| f == rel && path.starts_with(p))
            {
                found.push(format!(
                    "src/{rel}:{n}: core must not use the vertical `{path}`"
                ));
            }
        }
    }
    found.sort();
    found.dedup();
    found
}

fn violations() -> Vec<String> {
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let texts = src_texts(&src);
    let tests = test_files(&texts);
    let in_dir = |dir: &str| {
        let prefix = format!("{dir}/");
        texts
            .iter()
            .filter(|(rel, _)| rel.starts_with(&prefix) && !tests.contains(*rel))
            .collect::<Vec<_>>()
    };
    let mut found = Vec::new();
    for (layer, allowed) in RULES {
        for (rel, text) in in_dir(layer) {
            for (n, path) in file_paths(text) {
                let target = path
                    .trim_start_matches("crate::")
                    .split("::")
                    .next()
                    .unwrap();
                if allowed.contains(&target) {
                    continue;
                }
                if EXCEPTIONS
                    .iter()
                    .any(|(f, p)| f == rel && path.starts_with(p))
                {
                    continue;
                }
                found.push(format!("src/{rel}:{n}: `{layer}` must not use `{path}`"));
            }
            let checks_io = *layer == "domain"
                || (*layer == "application" && !APPLICATION_IO.contains(&rel.as_str()));
            if checks_io {
                for (n, line) in code_lines(text) {
                    for io in IO_CRATES {
                        if line.contains(io) {
                            found.push(format!(
                                "src/{rel}:{n}: `{layer}` must not do IO (`{io}`): add a port"
                            ));
                        }
                    }
                }
            }
        }
    }
    for (dir, forbidden) in FORBIDDEN {
        for (rel, text) in in_dir(dir) {
            for (n, path) in file_paths(text) {
                if forbidden.iter().any(|p| path.starts_with(p)) {
                    found.push(format!("src/{rel}:{n}: `{dir}` must not use `{path}`"));
                }
            }
        }
    }
    found.sort();
    found.dedup();
    found
}

#[test]
fn layers_only_depend_inward() {
    let found = violations();
    assert!(
        found.is_empty(),
        "layering violations ({}):\n{}",
        found.len(),
        found.join("\n")
    );
}

#[test]
fn core_never_uses_a_vertical() {
    let found = core_vertical_violations();
    assert!(
        found.is_empty(),
        "core → vertical imports ({}): move the shared piece into core\n{}",
        found.len(),
        found.join("\n")
    );
}

/// A relative import is checked like a `crate::` one, and a vertical name
/// matches whole segments only (`marketdata` is not `market`).
#[test]
fn super_paths_resolve_and_verticals_match_by_segment() {
    let text = "use super::{xmarket::runtime_db, runtime};\nuse super::super::domain::lp::x;\n";
    let paths: Vec<String> = file_paths(&resolve_super("config/builder/compile.rs", text))
        .into_iter()
        .map(|(_, p)| p)
        .collect();
    for want in [
        "crate::config::builder::xmarket::runtime_db",
        "crate::config::builder::runtime",
        "crate::config::domain::lp::x",
    ] {
        assert!(paths.iter().any(|p| p == want), "{want} not in {paths:?}");
    }
    let paths: Vec<String> = file_paths(&resolve_super(
        "config/hardening.rs",
        "use super::xmarket::X;\n",
    ))
    .into_iter()
    .map(|(_, p)| p)
    .collect();
    assert!(
        paths.iter().all(|p| p == "crate::config::xmarket::X"),
        "{paths:?}"
    );
    assert!(is_vertical("crate::config::xmarket::X"));
    assert!(is_vertical("crate::domain::market"));
    assert!(!is_vertical("crate::domain::market_hours"));
    assert_eq!(module_of("config/builder/mod.rs"), "crate::config::builder");
}

#[test]
fn exceptions_are_still_needed() {
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let stale: Vec<_> = EXCEPTIONS
        .iter()
        .filter(|(file, prefix)| {
            let text = fs::read_to_string(src.join(file)).unwrap_or_default();
            let used = code_lines(&text)
                .into_iter()
                .any(|(_, l)| crate_paths(l).iter().any(|p| p.starts_with(prefix)));
            !used
        })
        .collect();
    assert!(
        stale.is_empty(),
        "remove stale EXCEPTIONS entries: {stale:?}"
    );
}

/// A grouped or multi-line `use` cannot hide a path from the rules: each
/// leaf is checked in full (`domain/soe` must not reach `domain::xm` even as
/// `use crate::domain::{soe, xm::risk};`).
#[test]
fn grouped_and_multi_line_imports_expand() {
    let text = "use crate::domain::{\n    xm::risk,\n    soe::{value::Minor, record as r},\n    self,\n};\n\
                pub(crate) use crate::{config::risk::RiskConfig, ports};\n\
                fn f() { crate::application::backtest::run(); }\n";
    let paths: Vec<String> = file_paths(text).into_iter().map(|(_, p)| p).collect();
    for want in [
        "crate::domain::xm::risk",
        "crate::domain::soe::value::Minor",
        "crate::domain::soe::record",
        "crate::domain",
        "crate::config::risk::RiskConfig",
        "crate::ports",
        "crate::application::backtest::run",
    ] {
        assert!(paths.iter().any(|p| p == want), "{want} not in {paths:?}");
    }
    assert!(
        paths.iter().all(|p| !p.contains(['{', '}', ' '])),
        "{paths:?}"
    );
}

/// Out-of-line test modules are test code: a `#[path]` sibling, a `tests.rs`
/// under a `mod.rs`, a `#[cfg(test)]` module file; prod files never are.
#[test]
fn cfg_test_module_files_are_test_code() {
    let tests = test_files(&src_texts(
        &Path::new(env!("CARGO_MANIFEST_DIR")).join("src"),
    ));
    for f in [
        "domain/schedule_tests.rs",
        "application/backtest/tests.rs",
        "domain/backtest/testkit.rs",
        "domain/source/eval.rs",
    ] {
        assert!(tests.contains(f), "{f} not in {tests:?}");
    }
    for f in [
        "domain/schedule.rs",
        "application/backtest/mod.rs",
        "main.rs",
    ] {
        assert!(!tests.contains(f), "{f} counted as test code");
    }
}

#[test]
fn application_io_list_only_shrinks() {
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let stale: Vec<_> = APPLICATION_IO
        .iter()
        .filter(|file| {
            let text = fs::read_to_string(src.join(file)).unwrap_or_default();
            !code_lines(&text)
                .into_iter()
                .any(|(_, l)| IO_CRATES.iter().any(|io| l.contains(io)))
        })
        .collect();
    assert!(
        stale.is_empty(),
        "these files no longer do IO — remove them from APPLICATION_IO: {stale:?}"
    );
}
