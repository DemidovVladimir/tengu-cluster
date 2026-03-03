//! Application service for skill transpilation — detecting foreign runtime
//! dependencies, auto-preferring Rust variants, and producing scaffold content
//! for manual transpilation.

use crate::domain::skill_transpile::{analyze_skill, TranspileReport};

/// Result of checking all loaded skills for foreign dependencies.
#[derive(Debug)]
pub(crate) struct TranspileScanResult {
    /// Skills with foreign dependencies that have a Rust equivalent available.
    pub auto_preferred: Vec<AutoPreferred>,
    /// Skills with foreign dependencies and no Rust equivalent — need scaffolding.
    pub needs_transpile: Vec<TranspileReport>,
    /// Skills that are clean (no foreign deps).
    pub clean_count: usize,
}

/// A foreign skill that was auto-disabled in favour of its Rust variant.
#[derive(Debug)]
pub(crate) struct AutoPreferred {
    pub foreign_skill: String,
    pub rust_variant: String,
    pub runtimes: Vec<String>,
}

/// Scan all loaded skills for foreign dependencies.
///
/// `skills` is a list of `(name, raw_markdown_content, optional_execution_template)`.
pub(crate) fn scan_skills_for_foreign_deps(
    skills: &[(String, String, Option<String>)],
) -> TranspileScanResult {
    let all_names: Vec<String> = skills.iter().map(|(n, _, _)| n.clone()).collect();

    let mut auto_preferred = Vec::new();
    let mut needs_transpile = Vec::new();
    let mut clean_count = 0;

    for (name, content, exec_template) in skills {
        let report = analyze_skill(
            name,
            content,
            exec_template.as_deref(),
            &all_names,
        );

        if !report.has_foreign_deps() {
            clean_count += 1;
            continue;
        }

        if let Some(ref rust_name) = report.rust_variant {
            auto_preferred.push(AutoPreferred {
                foreign_skill: name.clone(),
                rust_variant: rust_name.clone(),
                runtimes: report.runtimes.iter().map(|r| r.to_string()).collect(),
            });
        } else {
            needs_transpile.push(report);
        }
    }

    TranspileScanResult {
        auto_preferred,
        needs_transpile,
        clean_count,
    }
}

/// Format a user-facing summary of the transpilation scan results.
pub(crate) fn format_scan_summary(result: &TranspileScanResult) -> Option<String> {
    if result.auto_preferred.is_empty() && result.needs_transpile.is_empty() {
        return None;
    }

    let mut lines = Vec::new();
    lines.push("Skill transpile scan:".to_string());

    for ap in &result.auto_preferred {
        lines.push(format!(
            "  {} ({}) → auto-disabled, using Rust variant '{}'",
            ap.foreign_skill,
            ap.runtimes.join(", "),
            ap.rust_variant,
        ));
    }

    for report in &result.needs_transpile {
        let rts: Vec<String> = report.runtimes.iter().map(|r| r.to_string()).collect();
        lines.push(format!(
            "  {} — requires {} (no Rust variant found)",
            report.skill_name,
            rts.join(", "),
        ));
    }

    Some(lines.join("\n"))
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scan_detects_foreign_with_rust_variant() {
        let skills = vec![
            (
                "aura_orchestrator".into(),
                "```javascript\nconst x = 1;\n```".into(),
                None,
            ),
            (
                "aura_rust_orchestrator".into(),
                "```rust\nfn main() {}\n```".into(),
                None,
            ),
        ];
        let result = scan_skills_for_foreign_deps(&skills);
        assert_eq!(result.auto_preferred.len(), 1);
        assert_eq!(result.auto_preferred[0].foreign_skill, "aura_orchestrator");
        assert_eq!(
            result.auto_preferred[0].rust_variant,
            "aura_rust_orchestrator"
        );
        assert_eq!(result.clean_count, 1);
    }

    #[test]
    fn scan_detects_foreign_without_variant() {
        let skills = vec![(
            "node_skill".into(),
            "```javascript\nconsole.log(1);\n```".into(),
            Some("node script.js".into()),
        )];
        let result = scan_skills_for_foreign_deps(&skills);
        assert_eq!(result.needs_transpile.len(), 1);
        assert_eq!(result.needs_transpile[0].skill_name, "node_skill");
    }

    #[test]
    fn scan_clean_skills() {
        let skills = vec![
            ("beach_science".into(), "```bash\ncurl ...\n```".into(), None),
            ("my_tool".into(), "```rust\nfn x() {}\n```".into(), None),
        ];
        let result = scan_skills_for_foreign_deps(&skills);
        assert!(result.auto_preferred.is_empty());
        assert!(result.needs_transpile.is_empty());
        assert_eq!(result.clean_count, 2);
    }

    #[test]
    fn format_summary_none_when_clean() {
        let result = TranspileScanResult {
            auto_preferred: vec![],
            needs_transpile: vec![],
            clean_count: 3,
        };
        assert!(format_scan_summary(&result).is_none());
    }

    #[test]
    fn format_summary_shows_auto_preferred() {
        let result = TranspileScanResult {
            auto_preferred: vec![AutoPreferred {
                foreign_skill: "aura_orchestrator".into(),
                rust_variant: "aura_rust_orchestrator".into(),
                runtimes: vec!["Node.js/TypeScript".into()],
            }],
            needs_transpile: vec![],
            clean_count: 1,
        };
        let summary = format_scan_summary(&result).unwrap();
        assert!(summary.contains("auto-disabled"));
        assert!(summary.contains("aura_rust_orchestrator"));
    }

}
