//! `tengu prune` — wipe cached/ephemeral state while preserving config,
//! secrets, skills, and project files.
//!
//! | Run | Targets |
//! |---|---|
//! | `tengu prune` | the shared `<TENGU_HOME>/memory` and `<TENGU_HOME>/logs` (every sandbox's traces, the audit logs) |
//! | `tengu prune --sandbox <s>` | only `<s>`'s: its traces `logs/trace/<s>/`, its workspaces' state, the `vectors.bin` of its own `[memory] store_path` |
//! | `… --hard` | each workspace root of `<s>` emptied instead of the allow-list |
//!
//! `<TENGU_HOME>/state` is never a target: it holds the install-wide stores
//! (the xmarket state dirs — `config/xmarket.rs` layout — and
//! `solana-writes.db`), so a target that is it, holds it (a `--hard`
//! workspace around `TENGU_HOME`) or lies inside it is left out.

use std::path::{Path, PathBuf};

use crate::adapters::outbound::trace_store::{check_sandbox_name, trace_root};

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
    /// `Some(name)`: prune only this sandbox's state (module table) — the
    /// shared logs and memory stay. `None`: the shared logs and memory.
    pub sandbox: Option<&'a str>,
    pub workspaces: &'a [PathBuf],
    pub project_dirs: &'a [String],
    /// The sandbox's own `[memory] store_path`, when it sets one: its
    /// `vectors.bin` goes with the sandbox (never the dir — it may hold
    /// anything), unless it is the shared `<TENGU_HOME>/memory`.
    pub memory_store: Option<&'a Path>,
    /// Hard reset: empty each workspace root entirely (every child, including
    /// `.tengu/`, arbitrary agent output dirs, and root runtime artifacts),
    /// instead of pruning only the known allow-list.
    pub hard: bool,
}

/// True when removing `path` would remove `<tengu_home>/state` or anything
/// in it — compared as written and, when both exist, with symlinks resolved.
fn touches_state(path: &Path, tengu_home: &Path) -> bool {
    let hits = |p: &Path, state: &Path| state.starts_with(p) || p.starts_with(state);
    let state = tengu_home.join("state");
    hits(path, &state)
        || matches!(
            (std::fs::canonicalize(path), std::fs::canonicalize(&state)),
            (Ok(p), Ok(s)) if hits(&p, &s)
        )
}

/// Scan `tengu_home` and workspace directories, returning every prunable
/// target — never `<tengu_home>/state` (module doc).
pub fn plan_prune(opts: &PruneOptions) -> Vec<PruneTarget> {
    let mut targets: Vec<PruneTarget> = Vec::new();

    let mut push = |path: PathBuf, label: String| {
        if touches_state(&path, opts.tengu_home) || targets.iter().any(|t| t.path == path) {
            return;
        }
        let exists = path.exists();
        targets.push(PruneTarget {
            path,
            label,
            exists,
        });
    };

    let shared_memory = opts.tengu_home.join("memory");
    match opts.sandbox {
        None => {
            push(shared_memory, "global memory vectors".to_string());
            push(
                opts.tengu_home.join("logs"),
                "log files, audit logs and every sandbox's traces".to_string(),
            );
        }
        Some(name) => {
            // A name that is not a trace dir name has no trace dir.
            if check_sandbox_name(name).is_ok() {
                push(
                    trace_root(opts.tengu_home).join(name),
                    format!("execution traces of {name}"),
                );
            }
            if let Some(store) = opts.memory_store.filter(|s| *s != shared_memory) {
                push(
                    store.join("vectors.bin"),
                    format!("memory vectors of {name}"),
                );
            }
        }
    }

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
            ws.join(".tengu-attachments"),
            format!("attachments ({ws_display})"),
        );
        // `persistent_store` files + manifests (`tools/memory/persistent_store.rs`);
        // their vectors live in `memory/` above. (Was `<ws>/storage`, which no
        // code writes — a user folder of that name was deleted instead.)
        push(
            ws.join(".tengu").join("storage"),
            format!("persistent_store files ({ws_display})"),
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
            sandbox: Some("s"),
            workspaces: &[ws.clone()],
            project_dirs: &[],
            memory_store: None,
            hard: false,
        });
        let soft_paths = existing_paths(&soft);
        assert!(soft_paths.contains(&ws.join("memory")));
        assert!(!soft_paths.contains(&ws.join(".tengu")));
        assert!(!soft_paths.contains(&ws.join("image_payload")));

        // Hard prune targets every direct child of the workspace root.
        let hard = plan_prune(&PruneOptions {
            tengu_home: home.path(),
            sandbox: Some("s"),
            workspaces: &[ws.clone()],
            project_dirs: &[],
            memory_store: None,
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

    /// Soft prune clears `persistent_store` data where the tool writes it
    /// (`.tengu/storage`) and leaves a user's own `storage/` folder alone.
    #[test]
    fn soft_prune_targets_persistent_store_not_a_user_storage_dir() {
        let home = tempfile::tempdir().unwrap();
        let ws_dir = tempfile::tempdir().unwrap();
        let ws = ws_dir.path().to_path_buf();
        std::fs::create_dir_all(ws.join(".tengu/storage/f1")).unwrap();
        std::fs::create_dir_all(ws.join("storage")).unwrap();
        let soft = plan_prune(&PruneOptions {
            tengu_home: home.path(),
            sandbox: Some("s"),
            workspaces: &[ws.clone()],
            project_dirs: &[],
            memory_store: None,
            hard: false,
        });
        let paths = existing_paths(&soft);
        assert!(paths.contains(&ws.join(".tengu/storage")));
        assert!(!paths.contains(&ws.join("storage")));
    }

    /// The xmarket state dir survives every mode and workspace — even a
    /// `--hard` workspace around `TENGU_HOME`, the state dir itself as a
    /// workspace or a `[memory] store_path` inside it; nothing in `state/`
    /// goes (`state/flows` was a target with no writer).
    #[test]
    fn never_deletes_the_xmarket_state_dir() {
        use crate::config::runtime::RUNTIME_DB;
        use crate::config::xmarket::{
            AUDIT_DB, CATALOG_DB, EVENTS_DB, HISTORY_DIR, LEDGER_DB, SPEND_DB,
        };
        let history_day = format!("{HISTORY_DIR}/20261002.db");
        let files = [
            LEDGER_DB,
            RUNTIME_DB,
            "run-xmarket-weekend.json",
            history_day.as_str(),
            CATALOG_DB,
            EVENTS_DB,
            AUDIT_DB,
            SPEND_DB,
            "KILL",
        ];
        // (label, hard, workspace relative to the temp root: "" = the root,
        // which holds TENGU_HOME = <root>/.tengu)
        for (label, hard, ws) in [
            ("soft, a plain workspace", false, "ws"),
            ("hard, a plain workspace", true, "ws"),
            ("soft, around TENGU_HOME", false, ""),
            ("hard, around TENGU_HOME", true, ""),
            (
                "soft, the state dir as workspace",
                false,
                ".tengu/state/xmarket-weekend",
            ),
            (
                "hard, the state dir as workspace",
                true,
                ".tengu/state/xmarket-weekend",
            ),
            (
                "hard, <TENGU_HOME>/state as workspace",
                true,
                ".tengu/state",
            ),
        ] {
            let root = tempfile::tempdir().unwrap();
            let home = root.path().join(".tengu");
            let xm = home.join("state/xmarket-weekend");
            std::fs::create_dir_all(xm.join(HISTORY_DIR)).unwrap();
            for f in files {
                std::fs::write(xm.join(f), b"x").unwrap();
            }
            std::fs::create_dir_all(home.join("state/flows")).unwrap();
            std::fs::write(home.join("state/flows/f.json"), b"{}").unwrap();
            std::fs::write(home.join("state/solana-writes.db"), b"x").unwrap();
            std::fs::create_dir_all(root.path().join("ws/memory")).unwrap();
            std::fs::create_dir_all(xm.join("memory")).unwrap();

            let workspaces = [root.path().join(ws)];
            let targets = plan_prune(&PruneOptions {
                tengu_home: &home,
                sandbox: Some("xmarket-weekend"),
                workspaces: &workspaces,
                project_dirs: &[],
                memory_store: Some(&home.join("state")),
                hard,
            });
            for t in &targets {
                assert!(
                    !touches_state(&t.path, &home),
                    "{label}: {}",
                    t.path.display()
                );
            }
            for (_, result) in execute_prune(&targets) {
                result.unwrap();
            }
            for f in files {
                assert!(xm.join(f).exists(), "{label}: {f} deleted");
            }
            assert!(xm.join("memory").exists(), "{label}: inside the state dir");
            assert!(home.join("state/solana-writes.db").exists(), "{label}");
            assert!(home.join("state/flows/f.json").exists(), "{label}: flows");
            if ws == "ws" {
                assert!(!root.path().join("ws/memory").exists(), "{label}");
            }
        }
    }

    /// `--sandbox s` prunes only `s`: its traces and its own store's
    /// vectors — never another sandbox's traces, the shared audit logs or
    /// the shared memory. Without `--sandbox` the shared dirs go.
    #[test]
    fn sandbox_prune_keeps_other_sandboxes_and_shared_logs() {
        let home = tempfile::tempdir().unwrap();
        let home = home.path();
        for f in [
            "logs/trace/lab/r1.jsonl",
            "logs/trace/xmarket/r2.jsonl",
            "logs/decisions.jsonl",
            "logs/risk.jsonl",
            "logs/maps/m.json",
            "memory/vectors.bin",
        ] {
            std::fs::create_dir_all(home.join(f).parent().unwrap()).unwrap();
            std::fs::write(home.join(f), b"x").unwrap();
        }
        let own = tempfile::tempdir().unwrap();
        std::fs::write(own.path().join("vectors.bin"), b"x").unwrap();
        std::fs::write(own.path().join("notes.md"), b"keep").unwrap();
        let plan = |sandbox, memory_store| {
            plan_prune(&PruneOptions {
                tengu_home: home,
                sandbox,
                workspaces: &[],
                project_dirs: &[],
                memory_store,
                hard: false,
            })
        };

        let scoped = existing_paths(&plan(Some("lab"), Some(own.path())));
        assert_eq!(
            scoped,
            [home.join("logs/trace/lab"), own.path().join("vectors.bin")]
        );
        // The shared store is never "the sandbox's own".
        let shared_store = home.join("memory");
        let shared = existing_paths(&plan(Some("lab"), Some(&shared_store)));
        assert_eq!(shared, [home.join("logs/trace/lab")]);
        // A name that is no trace dir name names no dir.
        assert!(existing_paths(&plan(Some("../logs"), None)).is_empty());

        let global = existing_paths(&plan(None, None));
        assert_eq!(global, [home.join("memory"), home.join("logs")]);
    }

    /// Nothing writes `.tengu-tasks` any more: a soft prune leaves a folder
    /// of that name alone.
    #[test]
    fn soft_prune_skips_the_dead_task_dir() {
        let home = tempfile::tempdir().unwrap();
        let ws_dir = tempfile::tempdir().unwrap();
        let ws = ws_dir.path().to_path_buf();
        std::fs::create_dir_all(ws.join(".tengu-tasks")).unwrap();
        std::fs::create_dir_all(ws.join(".tengu-attachments")).unwrap();
        let paths = existing_paths(&plan_prune(&PruneOptions {
            tengu_home: home.path(),
            sandbox: Some("s"),
            workspaces: &[ws.clone()],
            project_dirs: &[],
            memory_store: None,
            hard: false,
        }));
        assert_eq!(paths, [ws.join(".tengu-attachments")]);
    }

    #[test]
    fn soft_parent_project_dir_dominates_child() {
        let home = tempfile::tempdir().unwrap();
        let ws_dir = tempfile::tempdir().unwrap();
        let ws = ws_dir.path().to_path_buf();
        std::fs::create_dir_all(ws.join("mint/metadata")).unwrap();

        let targets = plan_prune(&PruneOptions {
            tengu_home: home.path(),
            sandbox: Some("s"),
            workspaces: &[ws.clone()],
            project_dirs: &["mint".to_string(), "mint/metadata".to_string()],
            memory_store: None,
            hard: false,
        });
        let paths = existing_paths(&targets);
        assert!(paths.contains(&ws.join("mint")));
        assert!(!paths.contains(&ws.join("mint/metadata")));
    }
}
