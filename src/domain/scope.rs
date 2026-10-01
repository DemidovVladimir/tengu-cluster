//! `ToolScope` — default-deny, per-tool access control. Pure policy logic;
//! enforced by every `Tool::execute` (see `tests/scope_lint.rs`).

use anyhow::Context;
use serde::{Deserialize, Serialize};
use std::path::{Component, Path, PathBuf};

/// Fine-grained scope for tool execution. Default-deny: every field empty
/// means the tool can do nothing. Config must grant access explicitly.
///
/// This type is defined by Phase 0 and consumed by Phase A's `ToolCtx`.
/// Subject to refinement during Phase A if additional fields are needed.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub(crate) struct ToolScope {
    /// Allowed filesystem roots. Every fs-touching tool must reject
    /// paths that, after canonicalization, do not start with one of these.
    /// Empty = no fs access.
    #[serde(default)]
    pub fs_roots: Vec<PathBuf>,

    /// Allowed outbound host patterns (exact or glob: "api.linear.app",
    /// "*.anthropic.com"). Empty = no network access.
    #[serde(default)]
    pub net_hosts: Vec<String>,

    /// Env var names the tool is allowed to read (via SecretVault $VAR refs).
    /// Empty = no env reads.
    #[serde(default)]
    pub env_reads: Vec<String>,

    /// For run_command: allowed binary names (basename match).
    /// Empty = no shell execution.
    #[serde(default)]
    pub shell_bins: Vec<String>,

    /// For crypto tools: allowed wallet labels.
    /// Empty = no crypto access.
    #[serde(default)]
    pub wallets: Vec<String>,
}

/// Host pattern match shared by `ToolScope::check_net_host` and the
/// `[egress]` host ceiling: exact, `*.suffix` (subdomains only, not the
/// apex), or `"*"` — the allow-any wildcard `permissive_scope` uses.
pub(crate) fn host_matches(pattern: &str, host: &str) -> bool {
    if pattern == "*" || pattern == host {
        return true;
    }
    pattern
        .strip_prefix("*.")
        .and_then(|suffix| host.strip_suffix(suffix))
        .is_some_and(|prefix| prefix.ends_with('.') && prefix.len() > 1)
}

/// `path` as the OS resolves it once the missing directories are created:
/// the longest existing ancestor canonicalized (symlinks followed), the rest
/// applied lexically — `..` pops a component. A `..` after a directory that
/// does not exist yet (`new/../../x`) would otherwise pass a prefix check
/// and land outside the root once a writer creates `new`. An entry that
/// exists but does not resolve (a dangling symlink) is an error: writing
/// through it would create its target.
pub(crate) fn resolve_path(path: &Path) -> anyhow::Result<PathBuf> {
    let mut head = path;
    let mut tail = Vec::new();
    while head.symlink_metadata().is_err() {
        match (head.parent(), head.components().next_back()) {
            (Some(parent), Some(last)) => {
                tail.push(last);
                head = parent;
            }
            // No existing ancestor (a relative path): lexical only.
            _ => return Ok(apply(PathBuf::new(), path.components())),
        }
    }
    let real = head
        .canonicalize()
        .with_context(|| format!("cannot resolve '{}'", head.display()))?;
    Ok(apply(real, tail.into_iter().rev()))
}

/// `components` on top of `base`: `..` pops, `.` is skipped.
fn apply<'a>(mut base: PathBuf, components: impl Iterator<Item = Component<'a>>) -> PathBuf {
    for c in components {
        match c {
            Component::ParentDir => {
                base.pop();
            }
            Component::CurDir => {}
            c => base.push(c.as_os_str()),
        }
    }
    base
}

/// Why a workspace writer must not write `rel` — a resolved path relative to
/// the workspace root — or `None`. Names compare case-insensitively (APFS and
/// NTFS fold case), at any depth: a CLI agent loads instruction files below
/// its cwd too, and Telegram `/project` moves a workspace into a subfolder.
///
/// | Refused | Why |
/// |---|---|
/// | a `.tengu` component | tengu's own state: observation store, cache, skills, memory |
/// | a `.claude` component | Claude Code settings (hooks run shell commands), commands, agents, skills |
/// | a `.git` component | git hooks run on the operator's next git command |
/// | a `CLAUDE.md`, `CLAUDE.local.md` or `AGENTS.md` file | instructions a CLI agent (and tengu's memory, `AGENTS.md`) loads on its own |
/// | a `.mcp.json` file | MCP servers a CLI agent starts |
pub(crate) fn protected_write(rel: &Path) -> Option<&'static str> {
    const DIRS: [(&str, &str); 3] = [
        (".tengu", "`.tengu/` holds tengu's own state"),
        (
            ".claude",
            "`.claude/` configures Claude Code (hooks, commands, skills)",
        ),
        (".git", "`.git/` holds git hooks"),
    ];
    const FILES: [(&str, &str); 4] = [
        ("CLAUDE.md", "a CLI agent loads `CLAUDE.md` as instructions"),
        (
            "CLAUDE.local.md",
            "a CLI agent loads `CLAUDE.local.md` as instructions",
        ),
        ("AGENTS.md", "agents load `AGENTS.md` as instructions"),
        (".mcp.json", "a CLI agent starts the servers in `.mcp.json`"),
    ];
    fn named(name: &std::ffi::OsStr, list: &[(&str, &'static str)]) -> Option<&'static str> {
        list.iter()
            .find(|(n, _)| name.eq_ignore_ascii_case(n))
            .map(|(_, why)| *why)
    }
    rel.components()
        .find_map(|c| match c {
            Component::Normal(name) => named(name, &DIRS),
            _ => None,
        })
        .or_else(|| rel.file_name().and_then(|name| named(name, &FILES)))
}

/// Instruction files tengu reads from an agent's workspace into its system
/// prompt besides `AGENTS.md` (which [`protected_write`] refuses
/// everywhere): `MemoryManager`'s builtin provider
/// (`outbound/memory/builtin.rs`) and the skill registry's identity block
/// (`application/skills/registry.rs`) — each loader's test keeps its list
/// inside this one.
pub(crate) const PROMPT_FILES: [&str; 5] = [
    "MEMORY.md",
    "USER.md",
    "IDENTITY.md",
    "PROFILE.md",
    "CONTEXT.md",
];

/// [`protected_write`] for a writer in a hardened sandbox (`[risk]` or a
/// `[solana]` signer: `AgentConfig::hardened`): also the [`PROMPT_FILES`],
/// by name at any depth, case-insensitive — text an agent writes there
/// would persist into every later system prompt (prompt injection). Not
/// hardened: agents keep their profile files current.
pub(crate) fn protected_write_in(rel: &Path, hardened: bool) -> Option<&'static str> {
    protected_write(rel).or_else(|| {
        let name = rel.file_name()?;
        (hardened && PROMPT_FILES.iter().any(|f| name.eq_ignore_ascii_case(f))).then_some(
            "a hardened sandbox ([risk] or a Solana signer) keeps the workspace prompt files \
             (MEMORY.md, USER.md, IDENTITY.md, PROFILE.md, CONTEXT.md) out of agents' reach — \
             tengu loads them into the system prompt",
        )
    })
}

#[allow(dead_code)] // Phase A wires these into ToolCtx
impl ToolScope {
    /// Every field empty: the scope grants nothing — an explicit deny
    /// (`[default_scopes.write_file]` with no keys). A `run-agent` child's
    /// workspace grant (`bootstrap::tools::grant_workspace_root`) keeps it
    /// a deny.
    pub(crate) fn is_deny_all(&self) -> bool {
        self.fs_roots.is_empty()
            && self.net_hosts.is_empty()
            && self.env_reads.is_empty()
            && self.shell_bins.is_empty()
            && self.wallets.is_empty()
    }

    pub(crate) fn check_fs_read(&self, path: &Path) -> anyhow::Result<PathBuf> {
        self.check_fs(path, "read")
    }

    pub(crate) fn check_fs_write(&self, path: &Path) -> anyhow::Result<PathBuf> {
        self.check_fs(path, "write")
    }

    pub(crate) fn check_net_host(&self, host: &str) -> anyhow::Result<()> {
        if self
            .net_hosts
            .iter()
            .any(|pattern| host_matches(pattern, host))
        {
            return Ok(());
        }
        anyhow::bail!(
            "host '{}' not in allowed net_hosts {:?}",
            host,
            self.net_hosts
        )
    }

    /// Check whether `var` is allowed by this scope's `env_reads` list.
    ///
    /// `"*"` is an allow-any wildcard — used by `permissive_scope` (the
    /// fallback for tools with no configured scope) to keep env reads
    /// ungated. Mirrors the `check_net_host` / `check_shell_bin` wildcard.
    pub(crate) fn check_env_read(&self, var: &str) -> anyhow::Result<()> {
        if self.env_reads.iter().any(|v| v == "*" || v == var) {
            return Ok(());
        }
        anyhow::bail!(
            "env var '{}' not in allowed env_reads {:?}",
            var,
            self.env_reads
        )
    }

    /// Check whether `bin` is allowed by this scope's `shell_bins` list.
    ///
    /// `"*"` is an allow-any wildcard — used by `permissive_scope` (the
    /// fallback for tools with no configured scope) to keep shell access
    /// ungated. Per-agent `[agents.*.scopes.<tool>].shell_bins` narrows it.
    ///
    /// Callers must guard against empty-string `bin` values themselves
    /// (`run_command` rejects empty commands before reaching this check).
    pub(crate) fn check_shell_bin(&self, bin: &str) -> anyhow::Result<()> {
        let basename = Path::new(bin)
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or(bin);
        if self.shell_bins.iter().any(|b| b == "*" || b == basename) {
            return Ok(());
        }
        anyhow::bail!(
            "binary '{}' not in allowed shell_bins {:?}",
            bin,
            self.shell_bins
        )
    }

    pub(crate) fn check_wallet(&self, label: &str) -> anyhow::Result<()> {
        if self.wallets.iter().any(|w| w == label) {
            return Ok(());
        }
        anyhow::bail!(
            "wallet '{}' not in allowed wallets {:?}",
            label,
            self.wallets
        )
    }

    fn check_fs(&self, path: &Path, op: &str) -> anyhow::Result<PathBuf> {
        if self.fs_roots.is_empty() {
            anyhow::bail!(
                "fs {} denied for '{}': no fs_roots configured (default-deny)",
                op,
                path.display()
            );
        }

        let canonical = resolve_path(path)?;

        for root in &self.fs_roots {
            let canonical_root = if root.exists() {
                root.canonicalize().unwrap_or_else(|_| root.clone())
            } else {
                root.clone()
            };
            if canonical.starts_with(&canonical_root) {
                return Ok(canonical);
            }
        }
        anyhow::bail!(
            "fs {} denied for '{}' (resolved: '{}'): not under any allowed fs_roots {:?}",
            op,
            path.display(),
            canonical.display(),
            self.fs_roots
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    #[test]
    fn fs_allowed_root_passes() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();
        let scope = ToolScope {
            fs_roots: vec![root.clone()],
            ..Default::default()
        };
        let file = root.join("test.txt");
        fs::write(&file, "hello").unwrap();
        assert!(scope.check_fs_read(&file).is_ok());
    }

    #[test]
    fn fs_disallowed_rejects() {
        let tmp = TempDir::new().unwrap();
        let scope = ToolScope {
            fs_roots: vec![tmp.path().join("allowed")],
            ..Default::default()
        };
        let outside = tmp.path().join("outside.txt");
        fs::write(&outside, "hello").unwrap();
        assert!(scope.check_fs_read(&outside).is_err());
    }

    #[test]
    fn fs_subdir_passes() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();
        let sub = root.join("deep").join("nested");
        fs::create_dir_all(&sub).unwrap();
        let file = sub.join("file.txt");
        fs::write(&file, "data").unwrap();
        let scope = ToolScope {
            fs_roots: vec![root],
            ..Default::default()
        };
        assert!(scope.check_fs_read(&file).is_ok());
    }

    #[test]
    fn fs_traversal_rejects() {
        let tmp = TempDir::new().unwrap();
        let allowed = tmp.path().join("allowed");
        fs::create_dir_all(&allowed).unwrap();
        let scope = ToolScope {
            fs_roots: vec![allowed],
            ..Default::default()
        };
        let escaped = tmp.path().join("allowed").join("..").join("outside.txt");
        fs::write(tmp.path().join("outside.txt"), "data").unwrap();
        assert!(scope.check_fs_read(&escaped).is_err());
    }

    #[test]
    fn fs_nonexistent_leaf_passes() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();
        let scope = ToolScope {
            fs_roots: vec![root.clone()],
            ..Default::default()
        };
        let new_file = root.join("new-doc.md");
        assert!(scope.check_fs_write(&new_file).is_ok());
    }

    #[test]
    fn fs_write_same_logic_as_read() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();
        let file = root.join("test.txt");
        fs::write(&file, "hello").unwrap();
        let scope = ToolScope {
            fs_roots: vec![root],
            ..Default::default()
        };
        assert!(scope.check_fs_write(&file).is_ok());
    }

    #[test]
    fn net_exact_match() {
        let scope = ToolScope {
            net_hosts: vec!["api.linear.app".into()],
            ..Default::default()
        };
        assert!(scope.check_net_host("api.linear.app").is_ok());
    }

    #[test]
    fn net_wildcard_match() {
        let scope = ToolScope {
            net_hosts: vec!["*.anthropic.com".into()],
            ..Default::default()
        };
        assert!(scope.check_net_host("api.anthropic.com").is_ok());
    }

    #[test]
    fn net_wildcard_no_bare() {
        let scope = ToolScope {
            net_hosts: vec!["*.anthropic.com".into()],
            ..Default::default()
        };
        assert!(scope.check_net_host("anthropic.com").is_err());
    }

    #[test]
    fn net_empty_rejects_all() {
        let scope = ToolScope::default();
        assert!(scope.check_net_host("anything.com").is_err());
    }

    #[test]
    fn net_wildcard_allows_any_host() {
        let scope = ToolScope {
            net_hosts: vec!["*".into()],
            ..Default::default()
        };
        assert!(scope.check_net_host("api.example.com").is_ok());
        assert!(scope.check_net_host("raw-host").is_ok());
    }

    #[test]
    fn env_allowed_passes() {
        let scope = ToolScope {
            env_reads: vec!["API_KEY".into()],
            ..Default::default()
        };
        assert!(scope.check_env_read("API_KEY").is_ok());
    }

    #[test]
    fn env_unlisted_rejects() {
        let scope = ToolScope {
            env_reads: vec!["API_KEY".into()],
            ..Default::default()
        };
        assert!(scope.check_env_read("SECRET_TOKEN").is_err());
    }

    #[test]
    fn env_wildcard_allows_any_var() {
        let scope = ToolScope {
            env_reads: vec!["*".into()],
            ..Default::default()
        };
        assert!(scope.check_env_read("API_KEY").is_ok());
        assert!(scope.check_env_read("ANY_OTHER_VAR").is_ok());
    }

    #[test]
    fn shell_basename_match() {
        let scope = ToolScope {
            shell_bins: vec!["git".into()],
            ..Default::default()
        };
        assert!(scope.check_shell_bin("/usr/bin/git").is_ok());
        assert!(scope.check_shell_bin("git").is_ok());
    }

    #[test]
    fn shell_unlisted_rejects() {
        let scope = ToolScope {
            shell_bins: vec!["git".into()],
            ..Default::default()
        };
        assert!(scope.check_shell_bin("rm").is_err());
    }

    #[test]
    fn shell_wildcard_allows_any_binary() {
        let scope = ToolScope {
            shell_bins: vec!["*".into()],
            ..Default::default()
        };
        assert!(scope.check_shell_bin("git").is_ok());
        assert!(scope.check_shell_bin("rm").is_ok());
        // Empty-string guarding lives in `run_command` (step 2), not in the
        // scope check — the wildcard accepts any input here.
        assert!(scope.check_shell_bin("").is_ok());
    }

    #[test]
    fn wallet_exact_match() {
        let scope = ToolScope {
            wallets: vec!["research-wallet".into()],
            ..Default::default()
        };
        assert!(scope.check_wallet("research-wallet").is_ok());
    }

    #[test]
    fn wallet_unlisted_rejects() {
        let scope = ToolScope {
            wallets: vec!["research-wallet".into()],
            ..Default::default()
        };
        assert!(scope.check_wallet("prod-wallet").is_err());
    }

    #[test]
    fn default_denies_all() {
        let scope = ToolScope::default();
        let tmp = TempDir::new().unwrap();
        let file = tmp.path().join("any.txt");
        fs::write(&file, "data").unwrap();

        assert!(scope.check_fs_read(&file).is_err());
        assert!(scope.check_fs_write(&file).is_err());
        assert!(scope.check_net_host("any.com").is_err());
        assert!(scope.check_env_read("ANY").is_err());
        assert!(scope.check_shell_bin("any").is_err());
        assert!(scope.check_wallet("any").is_err());
        assert!(scope.is_deny_all());
    }

    /// Any one field set makes a scope something other than a deny.
    #[test]
    fn deny_all_is_every_field_empty() {
        let set: [fn(&mut ToolScope); 5] = [
            |s| s.fs_roots = vec!["/srv".into()],
            |s| s.net_hosts = vec!["api.example.com".into()],
            |s| s.env_reads = vec!["KEY".into()],
            |s| s.shell_bins = vec!["ls".into()],
            |s| s.wallets = vec!["w".into()],
        ];
        for f in set {
            let mut scope = ToolScope::default();
            f(&mut scope);
            assert!(!scope.is_deny_all(), "{scope:?}");
        }
    }

    /// `..` after a directory that does not exist yet resolves as the OS
    /// will once a writer creates it: `ws/new/../../x` is outside `ws`.
    #[test]
    fn fs_dotdot_after_a_missing_dir_resolves_physically() {
        let tmp = TempDir::new().unwrap();
        let ws = tmp.path().join("ws");
        fs::create_dir_all(&ws).unwrap();
        let real = ws.canonicalize().unwrap();
        let scope = ToolScope {
            fs_roots: vec![ws.clone()],
            ..Default::default()
        };
        let escape = ws.join("new/../../outside.txt");
        assert!(scope.check_fs_write(&escape).is_err());
        assert_eq!(
            resolve_path(&escape).unwrap(),
            real.parent().unwrap().join("outside.txt")
        );
        let inside = ws.join("new/./deeper/../file.txt");
        assert_eq!(
            scope.check_fs_write(&inside).unwrap(),
            real.join("new/file.txt")
        );
    }

    /// Writing through a dangling symlink would create its target: refused.
    #[cfg(unix)]
    #[test]
    fn fs_dangling_symlink_is_refused() {
        let tmp = TempDir::new().unwrap();
        let ws = tmp.path().join("ws");
        fs::create_dir_all(&ws).unwrap();
        std::os::unix::fs::symlink(tmp.path().join("outside/target"), ws.join("link")).unwrap();
        let scope = ToolScope {
            fs_roots: vec![ws.clone()],
            ..Default::default()
        };
        let err = scope.check_fs_write(&ws.join("link")).unwrap_err();
        assert!(err.to_string().contains("cannot resolve"), "{err:#}");
    }

    #[test]
    fn protected_write_names_tool_and_agent_state() {
        for (rel, want) in [
            (".tengu/observations.db", "`.tengu/`"),
            ("sub/.TENGU/x", "`.tengu/`"),
            (".tengu", "`.tengu/`"),
            (".claude/settings.json", "`.claude/`"),
            ("a/.Claude/commands/x.md", "`.claude/`"),
            (".git/hooks/pre-commit", "`.git/`"),
            ("CLAUDE.md", "`CLAUDE.md`"),
            ("deep/er/claude.md", "`CLAUDE.md`"),
            ("CLAUDE.local.md", "`CLAUDE.local.md`"),
            ("x/AGENTS.md", "`AGENTS.md`"),
            (".mcp.json", "`.mcp.json`"),
        ] {
            let why = protected_write(Path::new(rel)).unwrap_or_else(|| panic!("{rel} allowed"));
            assert!(why.contains(want), "{rel}: {why}");
        }
        for rel in [
            "answer.txt",
            ".tengu-attachments/a.pdf",
            "notes/claude.md.bak",
            "CLAUDE.md/inner.txt",
            ".github/workflows/ci.yml",
            "",
        ] {
            assert_eq!(protected_write(Path::new(rel)), None, "{rel}");
        }
    }

    /// A hardened sandbox also keeps the system-prompt files out of
    /// writers' reach (any depth, any case); elsewhere they stay writable.
    /// The everywhere-names stay refused either way.
    #[test]
    fn hardened_writers_refuse_the_prompt_files() {
        for rel in [
            "MEMORY.md",
            "USER.md",
            "IDENTITY.md",
            "PROFILE.md",
            "CONTEXT.md",
            "memory.md",
            "project/Identity.MD",
            "a/b/context.md",
        ] {
            let why = protected_write_in(Path::new(rel), true)
                .unwrap_or_else(|| panic!("{rel} allowed when hardened"));
            assert!(why.contains("hardened sandbox"), "{rel}: {why}");
            assert_eq!(protected_write_in(Path::new(rel), false), None, "{rel}");
        }
        for rel in ["CLAUDE.md", ".tengu/x", "AGENTS.md"] {
            for hardened in [false, true] {
                assert_eq!(
                    protected_write_in(Path::new(rel), hardened),
                    protected_write(Path::new(rel)),
                    "{rel}"
                );
            }
        }
        for rel in [
            "memory.md.bak",
            "MEMORY.md/notes.txt",
            "user.txt",
            "profiles.md",
        ] {
            assert_eq!(protected_write_in(Path::new(rel), true), None, "{rel}");
        }
    }
}
