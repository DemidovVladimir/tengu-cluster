//! Scope enforcement lint.
//!
//! Asserts that every `Tool::execute` body under `src/adapters/outbound/{tools,mcp_client}/`
//! begins with a `ctx.scope.check_*()` call. This prevents new tools from
//! shipping without explicit scope gating.
//!
//! Also: no new module-wide `#![allow(dead_code)]` under `src/`
//! ([`MODULE_DEAD_CODE_ALLOWED`], shrink-only).

use std::fs;
use std::path::Path;

/// The only files allowed a module-wide `#![allow(dead_code)]` (it hides a
/// whole module from rustc). Shrink-only: a file leaves when its consumer
/// ships, nothing joins — put an item-level allow with its reason instead.
/// `domain/soe/portfolio.rs`: `PublicBrief` waits for its O7 sanitizer.
const MODULE_DEAD_CODE_ALLOWED: &[&str] = &["src/domain/soe/portfolio.rs"];

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

    // Sanity: we should find at least egress.rs, bootstrap/, runner.rs
    assert!(
        files.len() >= 3,
        "Expected at least 3 .rs files in src/adapters/, found {}",
        files.len()
    );

    // Verify every file is readable
    for file in &files {
        let content = fs::read_to_string(file)
            .unwrap_or_else(|e| panic!("Failed to read {}: {}", file.display(), e));
        assert!(!content.is_empty(), "File {} is empty", file.display());
    }
}

#[test]
fn every_tool_execute_checks_scope() {
    use regex::Regex;

    // Every `Tool` impl: the tool catalog + MCP proxy tools.
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/adapters/outbound");
    let mut files = collect_rs_files(&root.join("tools"));
    files.extend(collect_rs_files(&root.join("mcp_client")));
    assert!(
        files.len() >= 10,
        "scope lint found only {} tool files under {} — did the tools move?",
        files.len(),
        root.display()
    );

    // Pattern: find `fn execute(` inside an impl block, then check the body
    // contains `scope.check_` within the next ~30 lines. A `// scope: pure-compute`
    // annotation on the first 30 lines exempts tools that have no external side
    // effects (no fs/net/shell/wallet access) — e.g. `abi_encode`, `hex_to_uint256`.
    //
    // 30 lines is the window because some tools do necessary arg parsing (URL
    // extraction, host derivation, chunk sizing) before their scope call, and
    // rustfmt reformatting can legitimately push the scope call further down.
    // If a tool needs more than 30 lines before its first scope check, the
    // tool is doing too much before gating — tighten its logic or add an
    // explicit `// scope: pure-compute` if no gate is required.
    let execute_re = Regex::new(r"fn execute\s*\(").unwrap();
    let scope_check_re = Regex::new(r"scope\.check_").unwrap();
    let pure_compute_re = Regex::new(r"//\s*scope:\s*pure-compute").unwrap();

    let mut violations = Vec::new();

    for file in &files {
        let content = fs::read_to_string(file).unwrap();
        for (i, line) in content.lines().enumerate() {
            if execute_re.is_match(line) {
                // Look at the next 30 lines for a scope check.
                let window: String = content
                    .lines()
                    .skip(i)
                    .take(30)
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

#[test]
fn module_wide_dead_code_allows_only_shrink() {
    use regex::Regex;

    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let inner_allow = Regex::new(r"^\s*#!\[(allow|cfg_attr)\(.*\bdead_code\b").unwrap();
    let mut found: Vec<String> = collect_rs_files(&root.join("src"))
        .into_iter()
        .filter(|f| {
            fs::read_to_string(f)
                .unwrap()
                .lines()
                .any(|l| inner_allow.is_match(l))
        })
        .map(|f| {
            f.strip_prefix(root)
                .unwrap()
                .to_string_lossy()
                .replace('\\', "/")
        })
        .collect();
    found.sort();
    assert_eq!(
        found, MODULE_DEAD_CODE_ALLOWED,
        "module-wide `#![allow(dead_code)]` changed: delete what rustc flags (or use an \
         item-level allow with its reason); when one is removed, drop it from \
         MODULE_DEAD_CODE_ALLOWED"
    );
}
