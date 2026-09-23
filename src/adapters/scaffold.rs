//! Workspace scaffold — creates directories and seed files before agents start.

use crate::config::ScaffoldConfig;
use std::path::PathBuf;
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
        info!(
            count = scaffold.directories.len(),
            "Scaffold: directories created"
        );
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
        info!(
            created,
            total = scaffold.files.len(),
            "Scaffold: seed files written"
        );
    }

    Ok(root)
}

/// Run scaffold if configured, log result. Called before agents start.
pub(crate) fn maybe_apply_scaffold(config: &crate::config::Config) {
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
