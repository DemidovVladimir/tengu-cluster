//! Scope enforcement lint.
//!
//! Asserts that every `Tool::execute` body under `src/adapters/plugins/`
//! begins with a `ctx.scope.check_*()` call. This prevents new tools from
//! shipping without explicit scope gating.

use std::fs;
use std::path::Path;

/// Collect all `.rs` files under a directory, recursively.
fn collect_rs_files(dir: &Path) -> Vec<std::path::PathBuf> {
    let mut files = Vec::new();
    if let Ok(entries) = fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                files.extend(collect_rs_files(&path));
            } else if path.extension().and_then(|e| e.to_str()) == Some("rs") {
                files.push(path);
            }
        }
    }
    files
}

#[test]
fn can_read_all_adapter_sources() {
    let adapters_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/adapters");
    let files = collect_rs_files(&adapters_dir);

    // Sanity: we should find at least ports.rs, config.rs, types.rs
    assert!(
        files.len() >= 3,
        "Expected at least 3 .rs files in src/adapters/, found {}",
        files.len()
    );

    // Verify every file is readable
    for file in &files {
        let content = fs::read_to_string(file)
            .unwrap_or_else(|e| panic!("Failed to read {}: {}", file.display(), e));
        assert!(
            !content.is_empty(),
            "File {} is empty",
            file.display()
        );
    }
}

#[test]
fn every_tool_execute_checks_scope() {
    use regex::Regex;

    let plugins_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/adapters/plugins");
    let files = collect_rs_files(&plugins_dir);

    // Pattern: find `fn execute(` inside an impl block, then check the body
    // contains `scope.check_` within the next ~10 lines. A `// scope: pure-compute`
    // annotation on the first 10 lines exempts tools that have no external side
    // effects (no fs/net/shell/wallet access) — e.g. `abi_encode`, `hex_to_uint256`.
    let execute_re = Regex::new(r"fn execute\s*\(").unwrap();
    let scope_check_re = Regex::new(r"scope\.check_").unwrap();
    let pure_compute_re = Regex::new(r"//\s*scope:\s*pure-compute").unwrap();

    let mut violations = Vec::new();

    for file in &files {
        let content = fs::read_to_string(file).unwrap();
        for (i, line) in content.lines().enumerate() {
            if execute_re.is_match(line) {
                // Look at the next 10 lines for a scope check.
                let window: String = content
                    .lines()
                    .skip(i)
                    .take(10)
                    .collect::<Vec<_>>()
                    .join("\n");
                if !scope_check_re.is_match(&window) && !pure_compute_re.is_match(&window) {
                    violations.push(format!(
                        "{}:{} — execute() without scope.check_*() or `// scope: pure-compute`",
                        file.display(),
                        i + 1
                    ));
                }
            }
        }
    }

    assert!(
        violations.is_empty(),
        "Tools missing scope enforcement:\n{}",
        violations.join("\n")
    );
}
