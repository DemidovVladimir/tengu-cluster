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
pub fn plan_prune(tengu_home: &Path, workspaces: &[PathBuf]) -> Vec<PruneTarget> {
    let mut targets = Vec::new();

    let push = |targets: &mut Vec<PruneTarget>, path: PathBuf, label: String| {
        let exists = path.exists();
        targets.push(PruneTarget {
            path,
            label,
            exists,
        });
    };

    // Global state
    push(
        &mut targets,
        tengu_home.join("state/flows"),
        "conversation flows".to_string(),
    );
    push(
        &mut targets,
        tengu_home.join("memory"),
        "global memory vectors".to_string(),
    );
    push(
        &mut targets,
        tengu_home.join("logs"),
        "log files".to_string(),
    );

    // Per-workspace state
    for ws in workspaces {
        let ws_display = ws.display();
        push(
            &mut targets,
            ws.join("memory"),
            format!("workspace memory ({ws_display})"),
        );
        push(
            &mut targets,
            ws.join(".tengu-tasks"),
            format!("task outcomes ({ws_display})"),
        );
        push(
            &mut targets,
            ws.join(".tengu-attachments"),
            format!("attachments ({ws_display})"),
        );
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
