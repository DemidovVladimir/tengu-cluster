//! Adapter for generating and writing transpile scaffold projects to disk.

use std::path::{Path, PathBuf};

use crate::domain::skill_transpile::TranspileReport;

/// Generated scaffold file contents for a skill that needs transpilation.
#[derive(Debug, Clone)]
pub(crate) struct ScaffoldContent {
    pub skill_name: String,
    pub cargo_toml: String,
    pub main_rs: String,
    pub report_md: String,
}

/// Build scaffold content for a skill that needs transpilation.
pub(crate) fn build_scaffold_content(report: &TranspileReport) -> ScaffoldContent {
    let cargo_toml = format!(
        r#"[package]
name = "{name}"
version = "0.1.0"
edition = "2021"

[dependencies]
reqwest = {{ version = "0.12", features = ["json", "blocking"] }}
serde = {{ version = "1", features = ["derive"] }}
serde_json = "1"
anyhow = "1"
"#,
        name = report.skill_name.replace('-', "_"),
    );

    let main_rs = format!(
        r#"//! Transpiled skill binary for: {name}
//!
//! This is an auto-generated scaffold. Fill in the implementation
//! to replace the foreign-runtime version of this skill.
//!
//! Detected foreign runtimes: {runtimes}
//! Foreign code blocks found: {block_count}
//!
//! Build: cargo build --release
//! The compiled binary can then be referenced in a SKILL.md execution template.

use anyhow::Result;

fn main() -> Result<()> {{
    let args: Vec<String> = std::env::args().skip(1).collect();
    eprintln!("[{name}] called with {{}} args", args.len());

    // TODO: Implement the skill logic here.
    // Use reqwest for HTTP calls, serde_json for JSON processing.
    //
    // Example:
    //   let client = reqwest::blocking::Client::new();
    //   let resp = client.get("https://api.example.com/data")
    //       .header("Authorization", format!("Bearer {{}}", api_key))
    //       .send()?;
    //   println!("{{}}", resp.text()?);

    Ok(())
}}
"#,
        name = report.skill_name,
        runtimes = report
            .runtimes
            .iter()
            .map(|r| r.to_string())
            .collect::<Vec<_>>()
            .join(", "),
        block_count = report.code_blocks.len(),
    );

    let mut report_md = format!(
        "# Transpile Report: {}\n\n\
         ## Detected Foreign Dependencies\n\n",
        report.skill_name,
    );

    for rt in &report.runtimes {
        report_md.push_str(&format!("- **{}**\n", rt));
    }

    if !report.code_blocks.is_empty() {
        report_md.push_str("\n## Foreign Code Blocks Found\n\n");
        for block in &report.code_blocks {
            report_md.push_str(&format!(
                "- `{}` block at line {}\n",
                block.language, block.line_number
            ));
        }
    }

    report_md.push_str(
        "\n## Next Steps\n\n\
         1. Review `src/main.rs` and implement the equivalent logic in Rust\n\
         2. Add any additional dependencies to `Cargo.toml`\n\
         3. Build with `cargo build --release`\n\
         4. Create a SKILL.md that calls the compiled binary\n",
    );

    ScaffoldContent {
        skill_name: report.skill_name.clone(),
        cargo_toml,
        main_rs,
        report_md,
    }
}

/// Write a scaffold project to disk at `output_dir/<skill_name>/`.
///
/// Creates `Cargo.toml`, `src/main.rs`, and `REPORT.md`.
pub(crate) fn write_scaffold(
    content: &ScaffoldContent,
    output_dir: &Path,
) -> std::io::Result<PathBuf> {
    let skill_dir = output_dir.join(&content.skill_name);
    let src_dir = skill_dir.join("src");
    std::fs::create_dir_all(&src_dir)?;

    std::fs::write(skill_dir.join("Cargo.toml"), &content.cargo_toml)?;
    std::fs::write(src_dir.join("main.rs"), &content.main_rs)?;
    std::fs::write(skill_dir.join("REPORT.md"), &content.report_md)?;

    Ok(skill_dir)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::skill_transpile::{ForeignCodeBlock, ForeignRuntime};

    #[test]
    fn scaffold_content_is_generated() {
        let report = TranspileReport {
            skill_name: "test_skill".into(),
            runtimes: vec![ForeignRuntime::NodeJs],
            code_blocks: vec![ForeignCodeBlock {
                language: "javascript".into(),
                line_number: 5,
            }],
            rust_variant: None,
        };
        let content = build_scaffold_content(&report);
        assert_eq!(content.skill_name, "test_skill");
        assert!(content.cargo_toml.contains("test_skill"));
        assert!(content.cargo_toml.contains("reqwest"));
        assert!(content.main_rs.contains("test_skill"));
        assert!(content.main_rs.contains("Node.js/TypeScript"));
        assert!(content.report_md.contains("javascript"));
        assert!(content.report_md.contains("line 5"));
    }

    #[test]
    fn scaffold_writes_files_to_disk() {
        let dir = tempfile::tempdir().unwrap();
        let report = TranspileReport {
            skill_name: "test_skill".into(),
            runtimes: vec![ForeignRuntime::NodeJs],
            code_blocks: vec![ForeignCodeBlock {
                language: "javascript".into(),
                line_number: 5,
            }],
            rust_variant: None,
        };
        let content = build_scaffold_content(&report);
        let path = write_scaffold(&content, dir.path()).unwrap();
        assert!(path.join("Cargo.toml").exists());
        assert!(path.join("src/main.rs").exists());
        assert!(path.join("REPORT.md").exists());
    }
}
