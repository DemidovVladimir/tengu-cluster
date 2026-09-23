//! Scratch git worktree for evolve cycles. Falls back to a plain directory
//! when the workspace isn't a git repo.

use anyhow::{anyhow, Context, Result};
use chrono::Utc;
use std::path::{Path, PathBuf};

use crate::ports::shell::ShellExecutionPort;

pub(crate) struct Scratch {
    pub path: PathBuf,
    pub is_worktree: bool,
}

pub(crate) fn create_scratch(
    shell: &dyn ShellExecutionPort,
    workspace: &Path,
    skill: &str,
    base_branch: Option<&str>,
) -> Result<Scratch> {
    let ts = Utc::now().format("%Y-%m-%dT%H-%M-%SZ").to_string();
    let dir = workspace
        .join(".tengu")
        .join("worktrees")
        .join(format!("evolve-{skill}-{ts}"));
    std::fs::create_dir_all(dir.parent().unwrap())?;

    if is_git_repo(shell, workspace)? {
        let branch_arg = match base_branch {
            Some(b) => format!("-- {}", b),
            None => String::new(),
        };
        let cmd = format!(
            "git worktree add {} {}",
            shell_escape(dir.to_string_lossy().as_ref()),
            branch_arg,
        );
        shell
            .execute_shell(&cmd, workspace)
            .with_context(|| format!("git worktree add failed: {cmd}"))?;
        Ok(Scratch {
            path: dir,
            is_worktree: true,
        })
    } else {
        // Non-git fallback: plain scratch directory
        let fallback = workspace
            .join(".tengu")
            .join("scratch")
            .join(format!("evolve-{skill}-{ts}"));
        std::fs::create_dir_all(&fallback)?;
        // Copy skills/<name>/ into the scratch so cycles have something to mutate
        let src = workspace.join("skills").join(skill);
        copy_dir_recursive(&src, &fallback.join("skills").join(skill))?;
        eprintln!(
            "warning: workspace is not a git repo — using plain scratch at {}",
            fallback.display()
        );
        Ok(Scratch {
            path: fallback,
            is_worktree: false,
        })
    }
}

/// Startup sweep: remove any directories under `<workspace>/.tengu/worktrees/`
/// older than `stale_hours` hours. Catches leaked worktrees from Ctrl-C /
/// crashes / killed processes. `stale_hours == 0` disables the sweep.
///
/// For each stale directory we try `git worktree remove --force` first (so
/// git's administrative metadata stays consistent), falling back to plain
/// `remove_dir_all`. Errors are logged and swallowed — this is best-effort.
pub(crate) fn sweep_stale_worktrees(
    shell: &dyn ShellExecutionPort,
    workspace: &Path,
    stale_hours: u32,
) {
    if stale_hours == 0 {
        return;
    }
    let parent = workspace.join(".tengu").join("worktrees");
    if !parent.exists() {
        return;
    }
    let cutoff = std::time::SystemTime::now()
        .checked_sub(std::time::Duration::from_secs(stale_hours as u64 * 3600))
        .unwrap_or(std::time::UNIX_EPOCH);
    let Ok(entries) = std::fs::read_dir(&parent) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let Ok(meta) = entry.metadata() else { continue };
        let Ok(mtime) = meta.modified() else { continue };
        if mtime >= cutoff {
            continue;
        }
        let cmd = format!(
            "git worktree remove --force {}",
            shell_escape(path.to_string_lossy().as_ref())
        );
        let _ = shell.execute_shell(&cmd, workspace);
        if path.exists() {
            if let Err(e) = std::fs::remove_dir_all(&path) {
                tracing::warn!(path = %path.display(), error = %e, "sweep_stale_worktrees: remove_dir_all failed");
            } else {
                tracing::info!(path = %path.display(), "sweep_stale_worktrees: removed stale worktree");
            }
        }
    }
}

pub(crate) fn remove_scratch(
    shell: &dyn ShellExecutionPort,
    workspace: &Path,
    scratch: &Scratch,
) -> Result<()> {
    if scratch.is_worktree {
        let cmd = format!(
            "git worktree remove --force {}",
            shell_escape(scratch.path.to_string_lossy().as_ref())
        );
        shell.execute_shell(&cmd, workspace).ok();
    }
    if scratch.path.exists() {
        std::fs::remove_dir_all(&scratch.path).ok();
    }
    Ok(())
}

fn is_git_repo(shell: &dyn ShellExecutionPort, workspace: &Path) -> Result<bool> {
    Ok(shell
        .execute_shell("git rev-parse --is-inside-work-tree", workspace)
        .is_ok())
}

fn shell_escape(s: &str) -> String {
    if s.chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '/' | '.' | '-' | '_'))
    {
        s.to_string()
    } else {
        format!("'{}'", s.replace('\'', "'\\''"))
    }
}

fn copy_dir_recursive(src: &Path, dst: &Path) -> Result<()> {
    if !src.exists() {
        return Err(anyhow!("source does not exist: {}", src.display()));
    }
    std::fs::create_dir_all(dst)?;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let from = entry.path();
        let to = dst.join(entry.file_name());
        if from.is_dir() {
            copy_dir_recursive(&from, &to)?;
        } else {
            std::fs::copy(&from, &to)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    struct FakeShell {
        is_git: bool,
    }
    impl ShellExecutionPort for FakeShell {
        fn execute_shell(&self, cmd: &str, _: &Path) -> Result<String> {
            if cmd.starts_with("git rev-parse") {
                if self.is_git {
                    Ok("true".into())
                } else {
                    Err(anyhow!("not a git repo"))
                }
            } else if cmd.starts_with("git worktree add") {
                // pretend the worktree was added; the caller already created the dir
                std::fs::create_dir_all(cmd.split_whitespace().nth(3).unwrap_or("")).ok();
                Ok(String::new())
            } else {
                Ok(String::new())
            }
        }
    }

    #[test]
    fn non_git_falls_back_to_scratch_and_copies_skill() {
        let ws = TempDir::new().unwrap();
        std::fs::create_dir_all(ws.path().join("skills/demo")).unwrap();
        std::fs::write(ws.path().join("skills/demo/SKILL.md"), "hi").unwrap();

        let shell = FakeShell { is_git: false };
        let s = create_scratch(&shell, ws.path(), "demo", None).unwrap();
        assert!(!s.is_worktree);
        assert!(s.path.join("skills/demo/SKILL.md").exists());
    }

    #[test]
    fn git_repo_uses_worktree_path() {
        let ws = TempDir::new().unwrap();
        std::fs::create_dir_all(ws.path().join("skills/demo")).unwrap();

        let shell = FakeShell { is_git: true };
        let s = create_scratch(&shell, ws.path(), "demo", Some("main")).unwrap();
        assert!(s.is_worktree);
        assert!(s
            .path
            .to_string_lossy()
            .contains(".tengu/worktrees/evolve-demo-"));
    }
}
