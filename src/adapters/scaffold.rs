//! Workspace scaffold — creates directories and seed files before agents start.

use std::path::{Path, PathBuf};
use tengu_core::config::ScaffoldConfig;
use tracing::info;

/// Expand `~` to the user's home directory.
fn expand_tilde(path: &str) -> PathBuf {
    if path.starts_with('~') {
        if let Some(home) = dirs_next::home_dir() {
            return home.join(&path[2..]); // skip "~/"
        }
    }
    PathBuf::from(path)
}

/// Apply scaffold: create root directory, subdirectories, and seed files.
///
/// Files are only created if they don't already exist (never overwrites).
/// Returns the expanded root path.
pub(crate) fn apply_scaffold(scaffold: &ScaffoldConfig) -> anyhow::Result<PathBuf> {
    let root = expand_tilde(&scaffold.root);

    // Create root.
    std::fs::create_dir_all(&root)?;
    info!(path = %root.display(), "Scaffold: workspace root ensured");

    // Create subdirectories.
    for dir in &scaffold.directories {
        let full = root.join(dir);
        std::fs::create_dir_all(&full)?;
    }
    if !scaffold.directories.is_empty() {
        info!(count = scaffold.directories.len(), "Scaffold: directories created");
    }

    // Seed files (skip if already exists).
    let mut created = 0usize;
    for file in &scaffold.files {
        let full = root.join(&file.path);
        if full.exists() {
            continue;
        }
        // Ensure parent directory exists.
        if let Some(parent) = full.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&full, &file.content)?;
        created += 1;
    }
    if created > 0 {
        info!(created, total = scaffold.files.len(), "Scaffold: seed files written");
    }

    Ok(root)
}

/// Run scaffold if configured, log result. Called before agents start.
pub(crate) fn maybe_apply_scaffold(config: &tengu_core::config::Config) {
    if let Some(ref scaffold) = config.scaffold {
        match apply_scaffold(scaffold) {
            Ok(root) => {
                println!("  Workspace scaffolded at {}", root.display());
            }
            Err(e) => {
                eprintln!("  WARNING: Scaffold failed: {}", e);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tengu_core::config::ScaffoldFile;

    #[test]
    fn scaffold_creates_dirs_and_files() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("test-project");

        let scaffold = ScaffoldConfig {
            root: root.to_string_lossy().to_string(),
            directories: vec!["src".into(), "docs".into(), "public/assets".into()],
            files: vec![
                ScaffoldFile {
                    path: "README.md".into(),
                    content: "# Test".into(),
                },
                ScaffoldFile {
                    path: "src/index.js".into(),
                    content: "console.log('hello');".into(),
                },
            ],
            project: None,
        };

        let result = apply_scaffold(&scaffold).unwrap();
        assert_eq!(result, root);
        assert!(root.join("src").is_dir());
        assert!(root.join("docs").is_dir());
        assert!(root.join("public/assets").is_dir());
        assert!(root.join("README.md").exists());
        assert_eq!(
            std::fs::read_to_string(root.join("src/index.js")).unwrap(),
            "console.log('hello');"
        );
    }

    #[test]
    fn scaffold_does_not_overwrite_existing_files() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("existing");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("keep.txt"), "original").unwrap();

        let scaffold = ScaffoldConfig {
            root: root.to_string_lossy().to_string(),
            directories: vec![],
            files: vec![ScaffoldFile {
                path: "keep.txt".into(),
                content: "overwritten".into(),
            }],
            project: None,
        };

        apply_scaffold(&scaffold).unwrap();
        assert_eq!(
            std::fs::read_to_string(root.join("keep.txt")).unwrap(),
            "original"
        );
    }
}
