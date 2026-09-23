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
const FORBIDDEN: &[(&str, &[&str])] = &[(
    "adapters/outbound",
    &["crate::adapters::inbound", "crate::bootstrap"],
)];

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

/// Non-comment lines before the first `#[cfg(test)]` test *module* (a
/// `#[cfg(test)]` on a single item mid-file does not end the scan).
fn code_lines(src: &str) -> impl Iterator<Item = (usize, &str)> {
    let lines: Vec<&str> = src.lines().collect();
    let end = lines
        .windows(2)
        .position(|w| {
            let next = w[1].trim_start();
            w[0].trim() == "#[cfg(test)]" && (next.starts_with("mod ") || next.contains(" mod "))
        })
        .unwrap_or(lines.len());
    lines
        .into_iter()
        .enumerate()
        .take(end)
        .filter(|(_, l)| !l.trim_start().starts_with("//"))
        .map(|(i, l)| (i + 1, l))
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
            for (n, line) in code_lines(&text) {
                for path in crate_paths(line) {
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
            for (n, line) in code_lines(&text) {
                for path in crate_paths(line) {
                    if forbidden.iter().any(|p| path.starts_with(p)) {
                        found.push(format!("src/{rel}:{n}: `{dir}` must not use `{path}`"));
                    }
                }
            }
        }
    }
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
                .any(|(_, l)| crate_paths(l).iter().any(|p| p.starts_with(prefix)));
            !used
        })
        .collect();
    assert!(
        stale.is_empty(),
        "remove stale EXCEPTIONS entries: {stale:?}"
    );
}
