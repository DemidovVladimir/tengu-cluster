//! Language policy — where non-Rust source may live (`CLAUDE.md` /
//! `AGENTS.md` "Rust only" box and its one exception, the Studio web page).
//!
//! | Extension | Allowed under |
//! |---|---|
//! | `.js` `.mjs` `.cjs` `.css` `.html` | `docs/` (the static docs site) · `web/studio/` (the Studio frontend) |
//! | `.htm` | the same + `tests/fixtures/` (captured pages are data, never authored) |
//! | `.ts` `.tsx` `.jsx` `.mts` `.cts` | nowhere (they need a build step) |
//! | `.py` | nowhere |
//! | `.sh` | `deploy/` · `tests/fixtures/` |
//!
//! Checks tracked files (`git ls-files`), so a new file fails once it is
//! staged. `web/studio/` also loads nothing remote: no `http://` /
//! `https://` / protocol-relative URL in any of its files (no CDN, no
//! telemetry).

use std::path::{Path, PathBuf};
use std::process::Command;

fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

/// Every tracked path, repo-relative, `/`-separated.
fn tracked() -> Vec<String> {
    let out = Command::new("git")
        .args(["ls-files", "-z"])
        .current_dir(root())
        .output()
        .expect("language_policy: `git ls-files` must run (a git checkout is required)");
    assert!(
        out.status.success(),
        "git ls-files failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout)
        .expect("git ls-files: non-UTF-8 path")
        .split('\0')
        .filter(|p| !p.is_empty())
        .map(str::to_string)
        .collect()
}

/// The dirs a file with extension `ext` may live under; `None` = not a
/// policed extension; `Some(&[])` = nowhere.
fn allowed(ext: &str) -> Option<&'static [&'static str]> {
    const WEB: &[&str] = &["docs/", "web/studio/"];
    const HTM: &[&str] = &["docs/", "web/studio/", "tests/fixtures/"];
    const SH: &[&str] = &["deploy/", "tests/fixtures/"];
    match ext {
        "js" | "mjs" | "cjs" | "css" | "html" => Some(WEB),
        "htm" => Some(HTM),
        "ts" | "tsx" | "jsx" | "mts" | "cts" | "py" => Some(&[]),
        "sh" => Some(SH),
        _ => None,
    }
}

/// `path` breaks the policy: the reason.
fn violation(path: &str) -> Option<String> {
    let ext = Path::new(path).extension()?.to_str()?.to_ascii_lowercase();
    let dirs = allowed(&ext)?;
    if dirs.iter().any(|d| path.starts_with(d)) {
        return None;
    }
    Some(if dirs.is_empty() {
        format!("{path}: `.{ext}` is not allowed anywhere in this repo")
    } else {
        format!("{path}: `.{ext}` may live only under {}", dirs.join(", "))
    })
}

#[test]
fn non_rust_only_in_allowed_dirs() {
    let bad: Vec<String> = tracked().iter().filter_map(|p| violation(p)).collect();
    assert!(
        bad.is_empty(),
        "language policy (CLAUDE.md / AGENTS.md \"Rust only\"):\n- {}",
        bad.join("\n- ")
    );
}

/// The rule table itself: what passes, what fails.
#[test]
fn policy_table() {
    for ok in [
        "docs/tutorial/assets/site.js",
        "docs/code-map.html",
        "web/studio/index.html",
        "web/studio/studio.js",
        "web/studio/studio.css",
        "tests/fixtures/sec/0000320193-26-000006-index.htm",
        "tests/fixtures/fake_mcp_server.sh",
        "deploy/install.sh",
        "src/main.rs",
        "Makefile",
    ] {
        assert_eq!(violation(ok), None, "{ok}");
    }
    for bad in [
        "src/probe.js",
        "web/other/app.js",
        "web/studio-x/app.js",
        "tests/fixtures/gen.py",
        "scripts/x.py",
        "web/studio/app.ts",
        "docs/x.tsx",
        "tests/fixtures/page.html",
        "scripts/run.sh",
        "web/studio/start.sh",
    ] {
        assert!(violation(bad).is_some(), "{bad}");
    }
}

/// `web/studio/` works offline: no remote URL in any of its files.
#[test]
fn studio_web_is_local_only() {
    let mut bad = Vec::new();
    for path in tracked().iter().filter(|p| p.starts_with("web/studio/")) {
        let text = std::fs::read_to_string(root().join(path)).unwrap_or_default();
        for (i, line) in text.lines().enumerate() {
            let l = line.to_ascii_lowercase();
            if l.contains("http://")
                || l.contains("https://")
                || l.contains("\"//")
                || l.contains("'//")
            {
                bad.push(format!("{path}:{}: {}", i + 1, line.trim()));
            }
        }
    }
    assert!(
        bad.is_empty(),
        "web/studio/ must load nothing remote (no CDN, no telemetry):\n- {}",
        bad.join("\n- ")
    );
}
