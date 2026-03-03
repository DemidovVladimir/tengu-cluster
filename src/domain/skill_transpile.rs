//! Detection and analysis of foreign (non-Rust) runtime dependencies in skills.
//!
//! Skills that depend on Node.js, Python, or other external runtimes break
//! tengu's compile-time, self-contained model. This module detects such
//! dependencies so the application layer can warn users, auto-prefer Rust
//! variants, or scaffold transpilation projects.

use std::fmt;

/// A non-Rust runtime detected in a skill.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) enum ForeignRuntime {
    NodeJs,
    Python,
    Deno,
    Ruby,
}

impl fmt::Display for ForeignRuntime {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NodeJs => write!(f, "Node.js/TypeScript"),
            Self::Python => write!(f, "Python"),
            Self::Deno => write!(f, "Deno/TypeScript"),
            Self::Ruby => write!(f, "Ruby"),
        }
    }
}

/// A fenced code block written in a foreign language found in skill markdown.
#[derive(Debug, Clone)]
pub(crate) struct ForeignCodeBlock {
    pub language: String,
    pub line_number: usize,
}

/// Full report of foreign dependencies detected in a single skill.
#[derive(Debug, Clone)]
pub(crate) struct TranspileReport {
    pub skill_name: String,
    pub runtimes: Vec<ForeignRuntime>,
    pub code_blocks: Vec<ForeignCodeBlock>,
    /// Name of an existing Rust-equivalent skill, if found.
    pub rust_variant: Option<String>,
}

impl TranspileReport {
    pub fn has_foreign_deps(&self) -> bool {
        !self.runtimes.is_empty()
    }
}

/// Map a fenced code block language tag to a foreign runtime (if any).
fn language_to_runtime(lang: &str) -> Option<ForeignRuntime> {
    match lang {
        "javascript" | "js" | "typescript" | "ts" | "jsx" | "tsx" => Some(ForeignRuntime::NodeJs),
        "python" | "py" => Some(ForeignRuntime::Python),
        "ruby" | "rb" => Some(ForeignRuntime::Ruby),
        _ => None,
    }
}

/// Detect foreign-language fenced code blocks in skill markdown content.
///
/// Scans for `` ```language `` fences and checks if the language tag maps to
/// a non-Rust runtime. Returns all detected blocks.
pub(crate) fn detect_foreign_code_blocks(content: &str) -> Vec<ForeignCodeBlock> {
    let mut blocks = Vec::new();
    let mut in_fence = false;

    for (idx, line) in content.lines().enumerate() {
        let trimmed = line.trim();

        if in_fence {
            if trimmed.starts_with("```") {
                in_fence = false;
            }
            continue;
        }

        if let Some(rest) = trimmed.strip_prefix("```") {
            in_fence = true;
            let lang = rest.split_whitespace().next().unwrap_or("").to_lowercase();
            if !lang.is_empty() {
                if language_to_runtime(&lang).is_some() {
                    blocks.push(ForeignCodeBlock {
                        language: lang,
                        line_number: idx + 1,
                    });
                }
            }
        }
    }

    blocks
}

/// Detect a foreign runtime in a skill's execution template.
///
/// Checks if the template invokes a non-Rust binary (node, python, etc.).
pub(crate) fn detect_foreign_runtime_in_template(template: &str) -> Option<ForeignRuntime> {
    for token in template.split_whitespace() {
        // Handle paths like /usr/bin/node
        let binary = token.rsplit('/').next().unwrap_or(token);
        match binary {
            "node" | "npx" | "npm" | "yarn" | "bun" | "ts-node" | "tsx" => {
                return Some(ForeignRuntime::NodeJs);
            }
            "python" | "python3" | "pip" | "pip3" | "pipx" => {
                return Some(ForeignRuntime::Python);
            }
            "deno" => return Some(ForeignRuntime::Deno),
            "ruby" | "gem" | "bundle" | "bundler" => return Some(ForeignRuntime::Ruby),
            _ => {}
        }
    }
    None
}

/// Collect unique runtimes from detected code blocks.
pub(crate) fn runtimes_from_blocks(blocks: &[ForeignCodeBlock]) -> Vec<ForeignRuntime> {
    let mut seen = std::collections::HashSet::new();
    let mut runtimes = Vec::new();
    for block in blocks {
        if let Some(rt) = language_to_runtime(&block.language) {
            if seen.insert(rt.clone()) {
                runtimes.push(rt);
            }
        }
    }
    runtimes
}

/// Check if a Rust-equivalent skill name exists among all known skill names.
///
/// Detects variants where `_rust` or `-rust` appears anywhere in the name.
/// For example, `aura_orchestrator` matches `aura_rust_orchestrator` because
/// removing `_rust` from the candidate yields the original name.
pub(crate) fn find_rust_variant<'a>(
    skill_name: &str,
    all_names: &'a [String],
) -> Option<&'a str> {
    let normalized = skill_name.replace('-', "_");

    for name in all_names {
        let n = name.replace('-', "_");
        if n == normalized {
            continue; // skip self
        }
        // Check if removing "_rust" from the candidate yields our name.
        // Handles: aura_rust_orchestrator → aura_orchestrator
        //          aura_orchestrator_rust → aura_orchestrator
        let without_rust = n.replace("_rust", "");
        if without_rust == normalized {
            return Some(name);
        }
    }

    None
}

/// Build a complete transpile report for a skill.
pub(crate) fn analyze_skill(
    skill_name: &str,
    content: &str,
    execution_template: Option<&str>,
    all_skill_names: &[String],
) -> TranspileReport {
    let code_blocks = detect_foreign_code_blocks(content);
    let mut runtimes = runtimes_from_blocks(&code_blocks);

    // Also check execution template for foreign binaries.
    if let Some(tmpl) = execution_template {
        if let Some(rt) = detect_foreign_runtime_in_template(tmpl) {
            if !runtimes.contains(&rt) {
                runtimes.push(rt);
            }
        }
    }

    let rust_variant = find_rust_variant(skill_name, all_skill_names).map(|s| s.to_string());

    TranspileReport {
        skill_name: skill_name.to_string(),
        runtimes,
        code_blocks,
        rust_variant,
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detect_javascript_code_blocks() {
        let content = r#"
# My Skill

Some text.

```javascript
const x = require("foo");
```

More text.

```rust
fn main() {}
```

```typescript
import { bar } from "baz";
```
"#;
        let blocks = detect_foreign_code_blocks(content);
        assert_eq!(blocks.len(), 2);
        assert_eq!(blocks[0].language, "javascript");
        assert_eq!(blocks[1].language, "typescript");
    }

    #[test]
    fn detect_python_code_blocks() {
        let content = r#"
```python
import requests
```
"#;
        let blocks = detect_foreign_code_blocks(content);
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].language, "python");
    }

    #[test]
    fn no_foreign_blocks_in_pure_rust_skill() {
        let content = r#"
```rust
fn main() {}
```

```bash
echo hello
```

```graphql
query { foo }
```
"#;
        let blocks = detect_foreign_code_blocks(content);
        assert!(blocks.is_empty());
    }

    #[test]
    fn detect_node_in_execution_template() {
        assert_eq!(
            detect_foreign_runtime_in_template("npx ts-node script.ts"),
            Some(ForeignRuntime::NodeJs)
        );
        assert_eq!(
            detect_foreign_runtime_in_template("node -e 'console.log(1)'"),
            Some(ForeignRuntime::NodeJs)
        );
    }

    #[test]
    fn detect_python_in_execution_template() {
        assert_eq!(
            detect_foreign_runtime_in_template("python3 -c 'print(1)'"),
            Some(ForeignRuntime::Python)
        );
    }

    #[test]
    fn no_foreign_runtime_in_bash_template() {
        assert_eq!(
            detect_foreign_runtime_in_template("curl -s https://example.com"),
            None
        );
        assert_eq!(
            detect_foreign_runtime_in_template("rg '{{pattern}}' {{path}}"),
            None
        );
    }

    #[test]
    fn find_rust_variant_match() {
        let names = vec![
            "aura_orchestrator".to_string(),
            "aura_rust_orchestrator".to_string(),
        ];
        let result = find_rust_variant("aura_orchestrator", &names);
        assert_eq!(result, Some("aura_rust_orchestrator"));
    }

    #[test]
    fn find_rust_variant_with_hyphens() {
        let names = vec![
            "aura-orchestrator".to_string(),
            "aura-rust-orchestrator".to_string(),
        ];
        let result = find_rust_variant("aura-orchestrator", &names);
        assert_eq!(result, Some("aura-rust-orchestrator"));
    }

    #[test]
    fn find_rust_variant_no_match() {
        let names = vec!["beach_science".to_string()];
        let result = find_rust_variant("beach_science", &names);
        assert_eq!(result, None);
    }

    #[test]
    fn analyze_skill_with_js_detects_foreign_deps() {
        let content = r#"
# Aura Orchestrator

```javascript
import { createWalletClient } from "viem";
```

```rust
fn main() {}
```
"#;
        let all_names = vec![
            "aura_orchestrator".to_string(),
            "aura_rust_orchestrator".to_string(),
        ];
        let report = analyze_skill("aura_orchestrator", content, None, &all_names);
        assert!(report.has_foreign_deps());
        assert_eq!(report.runtimes, vec![ForeignRuntime::NodeJs]);
        assert_eq!(report.code_blocks.len(), 1);
        assert_eq!(
            report.rust_variant.as_deref(),
            Some("aura_rust_orchestrator")
        );
    }

    #[test]
    fn analyze_clean_skill_no_foreign_deps() {
        let content = r#"
```bash
curl -s https://example.com
```
"#;
        let report = analyze_skill("beach_science", content, None, &["beach_science".into()]);
        assert!(!report.has_foreign_deps());
        assert!(report.rust_variant.is_none());
    }

    #[test]
    fn runtimes_deduplication() {
        let blocks = vec![
            ForeignCodeBlock {
                language: "javascript".into(),
                line_number: 1,
            },
            ForeignCodeBlock {
                language: "typescript".into(),
                line_number: 10,
            },
            ForeignCodeBlock {
                language: "js".into(),
                line_number: 20,
            },
        ];
        let runtimes = runtimes_from_blocks(&blocks);
        // All map to NodeJs, so only one entry.
        assert_eq!(runtimes.len(), 1);
        assert_eq!(runtimes[0], ForeignRuntime::NodeJs);
    }
}
