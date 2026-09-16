//! `tengu prune` — wipe all cached/ephemeral state while preserving config,
//! secrets, skills, and project files.

use std::path::{Path, PathBuf};

/// A single item that can be pruned.
pub struct PruneTarget {
    pub path: PathBuf,
    pub label: String,
    pub exists: bool,
}

/// Inputs to [`plan_prune`].
///
/// `project_dirs` are `scaffold.project.directories` (pipeline artifacts that go
/// stale between runs) — pruned in soft mode when a workspace is known.
///
/// In `hard` mode the allow-list is bypassed: every entry directly inside each
/// workspace root is a target, so arbitrary agent-created dirs (e.g.
/// `image_payload/`) are removed too. This is safe because the workspace root is
/// the scaffold `root` — it holds only generated files; the sandbox config lives
/// under `sandboxes/<name>/config.toml`, never inside the workspace. The
/// workspace root dir itself is kept (only emptied), so it can be reused.
pub struct PruneOptions<'a> {
    pub tengu_home: &'a Path,
    pub workspaces: &'a [PathBuf],
    pub project_dirs: &'a [String],
    /// Hard reset: empty each workspace root entirely (every child, including
    /// `.tengu/`, arbitrary agent output dirs, and root runtime artifacts),
    /// instead of pruning only the known allow-list.
    pub hard: bool,
}

/// Scan `tengu_home` and workspace directories, returning every prunable target.
pub fn plan_prune(opts: &PruneOptions) -> Vec<PruneTarget> {
    let mut targets = Vec::new();

    let mut push = |path: PathBuf, label: String| {
        let exists = path.exists();
        targets.push(PruneTarget {
            path,
            label,
            exists,
        });
    };

    // Global state
    push(
        opts.tengu_home.join("state/flows"),
        "conversation flows".to_string(),
    );
    push(
        opts.tengu_home.join("memory"),
        "global memory vectors".to_string(),
    );
    push(opts.tengu_home.join("logs"), "log files".to_string());

    // Per-workspace state
    for ws in opts.workspaces {
        let ws_display = ws.display();

        if opts.hard {
            // Empty the workspace root entirely — it holds only generated files.
            // Enumerate direct children so each shows in the confirmation plan
            // (nothing here is the sandbox config, which lives in the repo).
            match std::fs::read_dir(ws) {
                Ok(entries) => {
                    let mut names: Vec<PathBuf> = entries.flatten().map(|e| e.path()).collect();
                    names.sort();
                    for p in names {
                        let name = p
                            .file_name()
                            .map(|n| n.to_string_lossy().into_owned())
                            .unwrap_or_default();
                        push(p, format!("generated {name} ({ws_display})"));
                    }
                }
                // Workspace root missing → nothing to prune for it.
                Err(_) => {}
            }
            continue;
        }

        // Soft mode: known allow-list only.
        push(
            ws.join("memory"),
            format!("workspace memory ({ws_display})"),
        );
        push(
            ws.join(".tengu-tasks"),
            format!("task outcomes ({ws_display})"),
        );
        push(
            ws.join(".tengu-attachments"),
            format!("attachments ({ws_display})"),
        );
        push(
            ws.join("storage"),
            format!("storage agent data ({ws_display})"),
        );

        // Scaffold project output directories — pipeline artifacts that go
        // stale between runs. Sort so a parent (e.g. `mint`) is seen before a
        // child (`mint/metadata`) and dominates it.
        let mut dirs: Vec<&String> = opts.project_dirs.iter().collect();
        dirs.sort();
        dirs.dedup();
        let mut added_dirs: Vec<String> = Vec::new();
        for dir in dirs {
            let dominated = added_dirs
                .iter()
                .any(|parent| dir.starts_with(&format!("{parent}/")));
            if dominated {
                continue;
            }
            let p = ws.join(dir);
            if p.exists() {
                push(p, format!("pipeline outputs {dir}/ ({ws_display})"));
                added_dirs.push(dir.clone());
            }
        }
    }

    targets
}

/// Remove every target that exists on disk.
/// Returns `(label, result)` for each attempted removal.
pub fn execute_prune(targets: &[PruneTarget]) -> Vec<(String, Result<(), String>)> {
    let mut results = Vec::new();
    for t in targets {
        if !t.exists {
            continue;
        }
        let outcome = if t.path.is_dir() {
            std::fs::remove_dir_all(&t.path)
        } else {
            std::fs::remove_file(&t.path)
        };
        results.push((t.label.clone(), outcome.map_err(|e| e.to_string())));
    }
    results
}

/// Human-readable plan summary.
pub fn format_prune_plan(targets: &[PruneTarget]) -> String {
    let mut out = String::from("Will remove:\n");
    for t in targets {
        if t.exists {
            out.push_str(&format!("  ✓ {} ({})\n", t.label, t.path.display()));
        } else {
            out.push_str(&format!(
                "  - {} ({}) [not found]\n",
                t.label,
                t.path.display()
            ));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Existing prune targets a plan should always list, so the assertions
    /// below can focus on what `hard` adds without hard-coding the full set.
    fn existing_paths(targets: &[PruneTarget]) -> Vec<PathBuf> {
        targets
            .iter()
            .filter(|t| t.exists)
            .map(|t| t.path.clone())
            .collect()
    }

    #[test]
    fn hard_empties_whole_workspace_including_arbitrary_dirs() {
        let home = tempfile::tempdir().unwrap();
        let ws_dir = tempfile::tempdir().unwrap();
        let ws = ws_dir.path().to_path_buf();

        // Known + arbitrary agent-created generated files under the workspace.
        std::fs::create_dir_all(ws.join(".tengu/memory")).unwrap();
        std::fs::create_dir_all(ws.join("memory")).unwrap();
        std::fs::create_dir_all(ws.join("image_payload/src")).unwrap();
        std::fs::create_dir_all(ws.join("keylogger_rs")).unwrap();
        std::fs::write(ws.join("TENGU_PLAN.md"), b"x").unwrap();

        // Soft prune leaves everything except the known allow-list alone.
        let soft = plan_prune(&PruneOptions {
            tengu_home: home.path(),
            workspaces: &[ws.clone()],
            project_dirs: &[],
            hard: false,
        });
        let soft_paths = existing_paths(&soft);
        assert!(soft_paths.contains(&ws.join("memory")));
        assert!(!soft_paths.contains(&ws.join(".tengu")));
        assert!(!soft_paths.contains(&ws.join("image_payload")));

        // Hard prune targets every direct child of the workspace root.
        let hard = plan_prune(&PruneOptions {
            tengu_home: home.path(),
            workspaces: &[ws.clone()],
            project_dirs: &[],
            hard: true,
        });
        let hard_paths = existing_paths(&hard);
        assert!(hard_paths.contains(&ws.join(".tengu")));
        assert!(hard_paths.contains(&ws.join("memory")));
        assert!(hard_paths.contains(&ws.join("image_payload")));
        assert!(hard_paths.contains(&ws.join("keylogger_rs")));
        assert!(hard_paths.contains(&ws.join("TENGU_PLAN.md")));
        // The workspace root itself is never a target — only emptied.
        assert!(!hard_paths.contains(&ws));
    }

    #[test]
    fn soft_parent_project_dir_dominates_child() {
        let home = tempfile::tempdir().unwrap();
        let ws_dir = tempfile::tempdir().unwrap();
        let ws = ws_dir.path().to_path_buf();
        std::fs::create_dir_all(ws.join("mint/metadata")).unwrap();

        let targets = plan_prune(&PruneOptions {
            tengu_home: home.path(),
            workspaces: &[ws.clone()],
            project_dirs: &["mint".to_string(), "mint/metadata".to_string()],
            hard: false,
        });
        let paths = existing_paths(&targets);
        assert!(paths.contains(&ws.join("mint")));
        assert!(!paths.contains(&ws.join("mint/metadata")));
    }
}
