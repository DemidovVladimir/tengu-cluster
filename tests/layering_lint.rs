//! Hexagonal layering lint — `docs/hexagonal-plan-2026-09-23.md`.
//!
//! Every `crate::<layer>` reference in a layer's non-test code must point at
//! a layer it may depend on:
//!
//! | Layer | May import |
//! |---|---|
//! | `domain` | `domain` (and no IO crates) |
//! | `ports` | `domain`, `config` |
//! | `config` | `domain` |
//! | `application` | `domain`, `ports`, `config`, `application` |
//! | `adapters/outbound` | all but `adapters::inbound`, `bootstrap` (`FORBIDDEN`) |
//! | `bootstrap` | all but `adapters::inbound` (`FORBIDDEN`) |
//! | `adapters/inbound`, `main.rs` | everything |
//!
//! | Subtree (`FORBIDDEN`) | Must not import |
//! |---|---|
//! | `domain/soe` | `domain::xm`, `domain::backtest::{engine, spec}`, `config::risk` |
//! | `domain/xm` · `domain/backtest` · `domain/source` | `domain::soe` |
//! | `application/backtest` | `application::soe` |
//! | `application/soe` | `application::backtest`, `domain::xm`, `domain::backtest::engine` |
//!
//! A `use` statement counts leaf by leaf: grouped and multi-line imports
//! (`use crate::domain::{soe, xm::risk};`) are expanded (`file_paths`).
//!
//! `EXCEPTIONS` lists known violations still being unwound. It may only
//! shrink; the rewrite is done when it is empty.

use std::fs;
use std::path::{Path, PathBuf};

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

/// Crates that do IO; `domain` must not name them.
const DOMAIN_IO_CRATES: &[&str] = &[
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

fn violations() -> Vec<String> {
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut found = Vec::new();
    for (layer, allowed) in RULES {
        let mut files = Vec::new();
        rust_files(&src.join(layer), &mut files);
        for file in files {
            let rel = file
                .strip_prefix(&src)
                .unwrap()
                .to_string_lossy()
                .replace('\\', "/");
            let text = fs::read_to_string(&file).unwrap();
            for (n, path) in file_paths(&text) {
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
                    .any(|(f, p)| *f == rel && path.starts_with(p))
                {
                    continue;
                }
                found.push(format!("src/{rel}:{n}: `{layer}` must not use `{path}`"));
            }
            if *layer == "domain" {
                for (n, line) in code_lines(&text) {
                    for io in DOMAIN_IO_CRATES {
                        if line.contains(io) {
                            found.push(format!("src/{rel}:{n}: `domain` must not do IO (`{io}`)"));
                        }
                    }
                }
            }
        }
    }
    for (dir, forbidden) in FORBIDDEN {
        let mut files = Vec::new();
        rust_files(&src.join(dir), &mut files);
        for file in files {
            let rel = file
                .strip_prefix(&src)
                .unwrap()
                .to_string_lossy()
                .replace('\\', "/");
            let text = fs::read_to_string(&file).unwrap();
            for (n, path) in file_paths(&text) {
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
