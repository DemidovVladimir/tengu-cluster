//! `tengu prune` — wipe all cached/ephemeral state while preserving config,
//! secrets, skills, and project files.

use std::path::{Path, PathBuf};

/// A single item that can be pruned.
pub struct PruneTarget {
    pub path: PathBuf,
    pub label: String,
    pub exists: bool,
}

/// Scan `tengu_home` and workspace directories, returning every prunable target.
///
/// `scaffold_dirs` lists scaffold project directories (e.g. `["research", "mint", "uploads", "posts"]`)
/// whose contents are pipeline outputs and should be cleaned between runs.
pub fn plan_prune(
    tengu_home: &Path,
    workspaces: &[PathBuf],
    scaffold_dirs: &[String],
) -> Vec<PruneTarget> {
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
        tengu_home.join("state/flows"),
        "conversation flows".to_string(),
    );
    push(
        tengu_home.join("memory"),
        "global memory vectors".to_string(),
    );
    push(tengu_home.join("logs"), "log files".to_string());

    // Per-workspace state
    for ws in workspaces {
        let ws_display = ws.display();
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

        // Scaffold project output directories — pipeline artifacts that go stale between runs.
        // Only add top-level dirs; skip subdirs already covered by a parent (e.g. mint/metadata under mint/).
        let mut added_dirs: Vec<String> = Vec::new();
        for dir in scaffold_dirs {
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
