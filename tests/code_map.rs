//! Code map freshness — `docs/code-map.md` + `docs/code-map.html`.
//!
//! Builds the file-level knowledge graph from `src/` (every `.rs` file with
//! its layer, size and `//!` summary; `uses` edges from `crate::` paths;
//! `implements` edges from `impl Trait for`; env vars each file names —
//! edges from non-test code only: no inline `#[cfg(test)]` block and no file
//! built only in tests, `tests/support/test_files.rs`) and checks that:
//!
//! - the `GENERATED` block in `docs/code-map.html` equals it, and
//! - the `GENERATED` block of `docs/code-map.md` § 7 equals its table: every
//!   source file by layer, its line count and the first paragraph of its
//!   `//!` doc (edit the module doc, never the table).
//!
//! Regenerate both blocks after changing code:
//! `TENGU_REGEN_CODE_MAP=1 cargo test --test code_map`.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

use regex::Regex;
use serde_json::{json, Value};

#[path = "support/test_files.rs"]
mod support;
use support::test_files;

const BEGIN: &str = "/* GENERATED:BEGIN — tests/code_map.rs, do not edit by hand */";
const END: &str = "/* GENERATED:END */";
const MD_BEGIN: &str = "<!-- GENERATED:BEGIN — tests/code_map.rs, do not edit by hand -->";
const MD_END: &str = "<!-- GENERATED:END -->";

/// § 7's sections: `layer()` → heading, in page order.
const SECTIONS: [(&str, &str); 8] = [
    ("entry", "Entry"),
    ("domain", "domain — data + pure policy"),
    ("ports", "ports — traits"),
    ("config", "config — TOML schema"),
    ("application", "application — use cases"),
    ("bootstrap", "bootstrap — composition root"),
    ("outbound", "adapters/outbound — driven adapters"),
    ("inbound", "adapters/inbound — driving adapters"),
];

fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

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

fn non_test(src: &str) -> String {
    code_lines(src)
        .into_iter()
        .map(|(_, l)| l)
        .collect::<Vec<_>>()
        .join("\n")
}

fn layer(rel: &str) -> &'static str {
    match rel.split('/').collect::<Vec<_>>().as_slice() {
        ["domain", ..] => "domain",
        ["ports", ..] => "ports",
        ["config", ..] => "config",
        ["application", ..] => "application",
        ["bootstrap", ..] => "bootstrap",
        ["adapters", "inbound", ..] => "inbound",
        ["adapters", "outbound", ..] => "outbound",
        _ => "entry",
    }
}

/// Resolve `crate::a::b::c` to the source file of the longest module prefix.
fn resolve(path: &str, files: &BTreeSet<String>) -> Option<String> {
    let segs: Vec<&str> = path.trim_start_matches("crate::").split("::").collect();
    for n in (1..=segs.len()).rev() {
        let base = segs[..n].join("/");
        for cand in [format!("{base}.rs"), format!("{base}/mod.rs")] {
            if files.contains(&cand) {
                return Some(cand);
            }
        }
    }
    None
}

/// The generated graph: `{ nodes, edges }` over `src/` files and env vars.
fn build_graph() -> Value {
    let src = root().join("src");
    let mut paths = Vec::new();
    rust_files(&src, &mut paths);
    let rel = |p: &Path| {
        p.strip_prefix(&src)
            .unwrap()
            .to_string_lossy()
            .replace('\\', "/")
    };
    let files: BTreeSet<String> = paths.iter().map(|p| rel(p)).collect();
    let texts: BTreeMap<String, String> = paths
        .iter()
        .map(|p| (rel(p), fs::read_to_string(p).unwrap()))
        .collect();
    let tests = test_files(&texts);

    let crate_path = Regex::new(r"crate(?:::[A-Za-z_][A-Za-z0-9_]*)+").unwrap();
    let trait_def = Regex::new(r"(?m)^\s*pub(?:\([a-z]+\))?\s+trait\s+([A-Za-z0-9_]+)").unwrap();
    let impl_for = Regex::new(
        r"impl(?:<[^>]*>)?\s+(?:[A-Za-z_][A-Za-z0-9_]*::)*([A-Z][A-Za-z0-9_]*)(?:<[^>]*>)?\s+for\s",
    )
    .unwrap();
    let env_var =
        Regex::new(r#""((?:TENGU|OPENROUTER|TELEGRAM|PRIVY|ANTHROPIC)_[A-Z0-9_]+)""#).unwrap();

    let mut trait_home: BTreeMap<String, String> = BTreeMap::new();
    for (file, text) in &texts {
        for c in trait_def.captures_iter(text) {
            trait_home.insert(c[1].to_string(), file.clone());
        }
    }

    let mut nodes = Vec::new();
    let mut edges: BTreeSet<(String, String, &'static str)> = BTreeSet::new();
    let mut envs: BTreeSet<String> = BTreeSet::new();
    for (file, text) in &texts {
        let doc = text
            .lines()
            .find_map(|l| l.strip_prefix("//! ").or_else(|| l.strip_prefix("//!")))
            .unwrap_or("")
            .trim()
            .to_string();
        nodes.push(json!({
            "id": format!("src/{file}"),
            "kind": "file",
            "layer": layer(file),
            "lines": text.lines().count(),
            "doc": doc,
        }));
        let code = if tests.contains(file) {
            String::new()
        } else {
            non_test(text)
        };
        for m in crate_path.find_iter(&code) {
            if let Some(target) = resolve(m.as_str(), &files) {
                if &target != file {
                    edges.insert((format!("src/{file}"), format!("src/{target}"), "uses"));
                }
            }
        }
        for c in impl_for.captures_iter(&code) {
            if let Some(home) = trait_home.get(&c[1]) {
                if home != file {
                    edges.insert((format!("src/{file}"), format!("src/{home}"), "implements"));
                }
            }
        }
        for c in env_var.captures_iter(&code) {
            envs.insert(c[1].to_string());
            edges.insert((format!("src/{file}"), format!("env:{}", &c[1]), "env"));
        }
    }
    for env in &envs {
        nodes.push(json!({ "id": format!("env:{env}"), "kind": "env", "layer": "env" }));
    }
    let edges: Vec<Value> = edges
        .into_iter()
        .map(|(from, to, kind)| json!({ "from": from, "to": to, "kind": kind }))
        .collect();
    json!({ "nodes": nodes, "edges": edges })
}

fn generated_block() -> String {
    let graph = serde_json::to_string(&build_graph()).unwrap();
    format!("{BEGIN}\nconst GENERATED = {graph};\n{END}")
}

#[test]
fn html_graph_matches_source() {
    let path = root().join("docs/code-map.html");
    let html = fs::read_to_string(&path).expect("docs/code-map.html");
    let (start, end) = (
        html.find(BEGIN).expect("GENERATED:BEGIN marker"),
        html.find(END).expect("GENERATED:END marker") + END.len(),
    );
    let fresh = generated_block();
    if html[start..end] == fresh {
        return;
    }
    if std::env::var_os("TENGU_REGEN_CODE_MAP").is_some() {
        fs::write(
            &path,
            format!("{}{}{}", &html[..start], fresh, &html[end..]),
        )
        .unwrap();
        return;
    }
    panic!(
        "docs/code-map.html is stale — regenerate with \
         `TENGU_REGEN_CODE_MAP=1 cargo test --test code_map`"
    );
}

/// A module's `//!` summary: its first paragraph on one line — up to a
/// blank doc line, a table, a list, a heading or a code fence; `|` escaped
/// for the table cell.
fn summary(text: &str) -> String {
    let mut out: Vec<&str> = Vec::new();
    for line in text.lines() {
        let Some(rest) = line.trim_start().strip_prefix("//!") else {
            // Before the doc: blank lines, inner attributes, `// path` comments.
            let lead = line.trim();
            if out.is_empty()
                && (lead.is_empty() || lead.starts_with("#!") || lead.starts_with("//"))
            {
                continue;
            }
            break;
        };
        let rest = rest.trim();
        let ends = rest.is_empty()
            || ["|", "- ", "* ", "#", "```"]
                .iter()
                .any(|p| rest.starts_with(p));
        if ends {
            if out.is_empty() {
                continue;
            }
            break;
        }
        out.push(rest);
    }
    out.join(" ").replace('|', "\\|")
}

/// § 7 of `docs/code-map.md`, between its markers (module doc).
fn markdown_block() -> String {
    let src = root().join("src");
    let mut paths = Vec::new();
    rust_files(&src, &mut paths);
    let mut rows: BTreeMap<&str, Vec<String>> = BTreeMap::new();
    let mut files: Vec<(String, String)> = paths
        .iter()
        .map(|p| {
            let rel = p
                .strip_prefix(&src)
                .unwrap()
                .to_string_lossy()
                .replace('\\', "/");
            (rel, fs::read_to_string(p).unwrap())
        })
        .collect();
    files.sort();
    for (rel, text) in &files {
        rows.entry(layer(rel)).or_default().push(format!(
            "| `src/{rel}` | {} | {} |",
            text.lines().count(),
            summary(text)
        ));
    }
    let mut out = vec![MD_BEGIN.to_string()];
    for (key, heading) in SECTIONS {
        let rows = rows.remove(key).unwrap_or_default();
        out.push(format!("\n### {heading} ({} files)\n", rows.len()));
        out.push("| File | Lines | What it is |".into());
        out.push("|---|---:|---|".into());
        out.extend(rows);
    }
    assert!(rows.is_empty(), "a layer with no § 7 section: {rows:?}");
    out.push(String::new());
    out.push(MD_END.to_string());
    out.join("\n")
}

#[test]
fn markdown_table_matches_source() {
    let path = root().join("docs/code-map.md");
    let md = fs::read_to_string(&path).expect("docs/code-map.md");
    let (start, end) = (
        md.find(MD_BEGIN).expect("GENERATED:BEGIN marker in § 7"),
        md.find(MD_END).expect("GENERATED:END marker in § 7") + MD_END.len(),
    );
    let fresh = markdown_block();
    if md[start..end] == fresh {
        return;
    }
    if std::env::var_os("TENGU_REGEN_CODE_MAP").is_some() {
        fs::write(&path, format!("{}{}{}", &md[..start], fresh, &md[end..])).unwrap();
        return;
    }
    panic!(
        "docs/code-map.md § 7 is stale — regenerate with \
         `TENGU_REGEN_CODE_MAP=1 cargo test --test code_map`"
    );
}
