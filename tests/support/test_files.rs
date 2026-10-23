//! The `src/` files the crate builds only in tests — shared by
//! `tests/layering_lint.rs` and `tests/code_map.rs` (`#[path]`), which treat
//! them as test code like an inline `#[cfg(test)] mod … { … }` block.

use std::collections::{BTreeMap, BTreeSet};

/// `(behind #[cfg(test)], name, #[path])` of every out-of-line `mod name;`.
fn mod_decls(text: &str) -> Vec<(bool, String, Option<String>)> {
    let mut out = Vec::new();
    let mut attrs: Vec<&str> = Vec::new();
    for line in text.lines() {
        let t = line.trim();
        if t.starts_with("#[") {
            attrs.push(t);
            continue;
        }
        let decl = t
            .strip_prefix("pub(crate) ")
            .or_else(|| t.strip_prefix("pub "))
            .unwrap_or(t);
        if let Some(name) = decl.strip_prefix("mod ").and_then(|r| r.strip_suffix(';')) {
            let path = attrs.iter().find_map(|a| {
                a.strip_prefix("#[path = \"")
                    .and_then(|r| r.strip_suffix("\"]"))
            });
            out.push((
                attrs.contains(&"#[cfg(test)]"),
                name.trim().to_string(),
                path.map(str::to_string),
            ));
        }
        attrs.clear();
    }
    out
}

/// The file `mod name;` in `parent` loads (paths relative to `src/`).
fn mod_file(
    parent: &str,
    name: &str,
    path: Option<&str>,
    files: &BTreeMap<String, String>,
) -> Option<String> {
    let join = |a: &str, b: &str| {
        if a.is_empty() {
            b.to_string()
        } else {
            format!("{a}/{b}")
        }
    };
    let (dir, stem) = parent.rsplit_once('/').unwrap_or(("", parent));
    if let Some(p) = path {
        return Some(join(dir, p));
    }
    let base = match stem {
        "mod.rs" | "main.rs" | "lib.rs" => dir.to_string(),
        s => join(dir, s.trim_end_matches(".rs")),
    };
    let module = join(&base, name);
    [format!("{module}.rs"), join(&module, "mod.rs")]
        .into_iter()
        .find(|c| files.contains_key(c))
}

/// Files (keys of `files`: `src/`-relative path → text) the crate builds
/// only in tests: a `mod x;` behind `#[cfg(test)]`, `#[path]` or not, and
/// every module declared below one.
pub fn test_files(files: &BTreeMap<String, String>) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    loop {
        let before = out.len();
        for (file, text) in files {
            let in_test = out.contains(file);
            for (cfg_test, name, path) in mod_decls(text) {
                if cfg_test || in_test {
                    if let Some(child) = mod_file(file, &name, path.as_deref(), files) {
                        out.insert(child);
                    }
                }
            }
        }
        if out.len() == before {
            return out;
        }
    }
}
