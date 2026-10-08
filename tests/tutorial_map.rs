//! Tutorial map — keeps the visual feature manual `docs/tutorial/` tied to
//! the code (`docs/tutorial/AUTHORING.md` § Sync rule).
//!
//! | Check | Fails when |
//! |---|---|
//! | pages agree | a slug is in `assets/nav.js` but not in `sources.toml` (or the reverse), or its `<slug>.html` is missing |
//! | sources exist | a `sources` path is gone (a directory prefix ends in `/`) — a file moved or was deleted |
//! | src covered | a `src/**/*.rs` file is explained by no page and is not listed under `[glue]` |
//! | pages well-formed | a page misses the shared assets or the `Checked against the code on` footer, or links a local `.html` that does not exist |
//!
//! Which pages to update after a code change: every `[pages.<slug>]` whose
//! `sources` contains the changed file or one of its parent directories.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

use regex::Regex;

const FOOTER: &str = "Checked against the code on";

fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn tutorial() -> PathBuf {
    root().join("docs/tutorial")
}

/// `sources.toml`: `[glue] sources` + one `[pages.<slug>] sources` per page.
struct Map {
    glue: Vec<String>,
    pages: BTreeMap<String, Vec<String>>,
}

fn load_map() -> Map {
    let path = tutorial().join("sources.toml");
    let text = fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    let v: toml::Value = toml::from_str(&text).unwrap_or_else(|e| panic!("sources.toml: {e}"));
    let list = |t: &toml::Value, at: &str| -> Vec<String> {
        t.get("sources")
            .and_then(|s| s.as_array())
            .unwrap_or_else(|| panic!("sources.toml {at}: `sources` array required"))
            .iter()
            .map(|x| {
                x.as_str()
                    .unwrap_or_else(|| panic!("sources.toml {at}: sources must be strings"))
                    .to_string()
            })
            .collect()
    };
    let glue = v.get("glue").map(|g| list(g, "[glue]")).unwrap_or_default();
    let pages = v
        .get("pages")
        .and_then(|p| p.as_table())
        .expect("sources.toml: [pages.<slug>] blocks required")
        .iter()
        .map(|(slug, t)| (slug.clone(), list(t, &format!("[pages.{slug}]"))))
        .collect();
    Map { glue, pages }
}

fn nav_slugs() -> Vec<String> {
    let text = fs::read_to_string(tutorial().join("assets/nav.js")).expect("assets/nav.js");
    Regex::new(r#"slug:\s*"([a-z0-9-]+)""#)
        .unwrap()
        .captures_iter(&text)
        .map(|c| c[1].to_string())
        .collect()
}

fn rust_files(dir: &Path, out: &mut Vec<String>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            rust_files(&path, out);
        } else if path.extension().is_some_and(|e| e == "rs") {
            let rel = path.strip_prefix(root()).unwrap();
            out.push(rel.to_string_lossy().replace('\\', "/"));
        }
    }
}

fn covers(source: &str, file: &str) -> bool {
    if source.ends_with('/') {
        file.starts_with(source)
    } else {
        file == source
    }
}

#[test]
fn nav_and_sources_list_the_same_pages() {
    let map = load_map();
    let nav: Vec<String> = nav_slugs();
    let nav_set: BTreeSet<&String> = nav.iter().collect();
    assert_eq!(nav.len(), nav_set.len(), "assets/nav.js lists a slug twice");
    let map_set: BTreeSet<&String> = map.pages.keys().collect();
    let only_nav: Vec<_> = nav_set.difference(&map_set).collect();
    let only_map: Vec<_> = map_set.difference(&nav_set).collect();
    assert!(
        only_nav.is_empty() && only_map.is_empty(),
        "pages in assets/nav.js but not sources.toml: {only_nav:?}; in sources.toml but not nav.js: {only_map:?}"
    );
    let missing: Vec<_> = nav
        .iter()
        .filter(|s| !tutorial().join(format!("{s}.html")).is_file())
        .collect();
    assert!(
        missing.is_empty(),
        "no docs/tutorial/<slug>.html for: {missing:?}"
    );
    assert!(
        tutorial().join("index.html").is_file(),
        "docs/tutorial/index.html missing"
    );
}

#[test]
fn every_source_path_exists() {
    let map = load_map();
    let mut dead = Vec::new();
    let all = map
        .pages
        .iter()
        .flat_map(|(slug, s)| s.iter().map(move |p| (slug.as_str(), p)))
        .chain(map.glue.iter().map(|p| ("glue", p)));
    for (slug, p) in all {
        let path = root().join(p.trim_end_matches('/'));
        let ok = if p.ends_with('/') {
            path.is_dir()
        } else {
            path.is_file()
        };
        if !ok {
            dead.push(format!("[{slug}] {p}"));
        }
    }
    assert!(
        dead.is_empty(),
        "docs/tutorial/sources.toml names paths that do not exist (moved or deleted? update the path and re-check the page):\n{}",
        dead.join("\n")
    );
}

#[test]
fn every_source_file_is_explained_by_a_page() {
    let map = load_map();
    let mut files = Vec::new();
    rust_files(&root().join("src"), &mut files);
    files.sort();
    let unmapped: Vec<&String> = files
        .iter()
        .filter(|f| {
            !map.pages.values().flatten().any(|s| covers(s, f))
                && !map.glue.iter().any(|s| covers(s, f))
        })
        .collect();
    assert!(
        unmapped.is_empty(),
        "src files no tutorial page covers — add each to the [pages.<slug>] it belongs to (or [glue] if it only wires modules) in docs/tutorial/sources.toml, and explain it on that page:\n{}",
        unmapped
            .iter()
            .map(|s| s.as_str())
            .collect::<Vec<_>>()
            .join("\n")
    );
}

#[test]
fn pages_are_well_formed() {
    let href = Regex::new(r#"href="([a-z0-9-]+)\.html(?:#[A-Za-z0-9._~-]*)?""#).unwrap();
    let mut pages = nav_slugs();
    pages.push("index".to_string());
    let mut problems = Vec::new();
    for slug in &pages {
        let path = tutorial().join(format!("{slug}.html"));
        let Ok(html) = fs::read_to_string(&path) else {
            continue; // reported by nav_and_sources_list_the_same_pages
        };
        for asset in ["assets/site.css", "assets/nav.js", "assets/site.js"] {
            if !html.contains(asset) {
                problems.push(format!("{slug}.html: does not load {asset}"));
            }
        }
        if !html.contains("<title>") {
            problems.push(format!("{slug}.html: no <title>"));
        }
        if slug != "index" && !html.contains(FOOTER) {
            problems.push(format!("{slug}.html: no \"{FOOTER} <date>\" footer"));
        }
        for c in href.captures_iter(&html) {
            if !tutorial().join(format!("{}.html", &c[1])).is_file() {
                problems.push(format!(
                    "{slug}.html: links {}.html, which does not exist",
                    &c[1]
                ));
            }
        }
    }
    assert!(problems.is_empty(), "{}", problems.join("\n"));
}
