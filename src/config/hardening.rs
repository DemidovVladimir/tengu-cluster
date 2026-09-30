//! Hardened sandboxes (tracker convention 12): where signing or money is
//! involved nothing runs outside tengu scopes — a `[solana]` signer or a
//! `[risk]` section (the xmarket paper / live budget). One code path for
//! both (`risk-gate-enforcement`). Doc: `docs/engine-backends.md` § Claude
//! Code, `docs/xmarket-risk-paper-2026-09-30.md` § Load rules.
//!
//! | Rule (hardened sandbox — any violation fails `Config::load`) | Enforced by | Why |
//! |---|---|---|
//! | Every `engine = "claude_code"` agent sets `[agents.<a>.claude_code] builtin_tools_profile = "none"` — no block = `editor_shell` = refused | `validation_errors` | built-in Read / Write / Bash ignore tengu scopes: they could read the key or edit a store |
//! | The CLI sees only the tengu bridge | `--strict-mcp-config` on every run (`engines/claude_code.rs::cli_args`) | the operator's own MCP servers run outside tengu scopes and egress |
//! | No `[[mcp_servers]]` | `validation_errors` | foreign processes with this filesystem |
//! | No configured scope grants `shell_bins`; tools without a scope run no shell (`AgentConfig::no_shell_fallback`, set by `Config::fold_default_scopes`; the bridge too) | `validation_errors` + runtime | a shell reads and writes any file |
//! | Outside every `fs_roots` and agent `workspace` (symlinks resolved): the signer key; `<TENGU_HOME>/state` (no overlap either way); `[risk] kill_switch_file`; the config file itself | `validation_errors`, the config file in `config_file_errors` (`Config::load`) | `read_file` / `write_file` must not read the key, edit `ledger.db` or a lease, delete the kill-switch file or raise the limits |
//!
//! Signer-only rules (key path, wallet grants) live in `config/solana.rs`;
//! `[risk]`-only rules (exec tools on a private agent, Privy signing off) in
//! `config/risk.rs`; the xmarket workspace + state-dir rules in
//! `config/xmarket.rs` (which borrows the path check, [`dir_reach_errors`],
//! for the `[xmarket]` state dir of a sandbox that is not hardened).

use std::path::{Component, Path, PathBuf};

use super::paths::{expand_tilde, resolve_tengu_home};
use super::Config;

/// True when the sandbox is hardened: `claude_code` agents only with
/// built-in tools off, and a no-shell fallback scope on every agent.
pub(crate) fn requires_hardened_claude_code(cfg: &Config) -> bool {
    cfg.solana.signer_key_file.is_some() || cfg.risk.is_some()
}

/// Ends every path error of the hardened rules.
const HARDENED: &str = "hardened sandbox: Solana signer or [risk]";

pub(crate) fn validation_errors(cfg: &Config) -> Vec<String> {
    if !requires_hardened_claude_code(cfg) {
        return Vec::new();
    }
    let mut errors = claude_code_errors(cfg);
    foreign_process_errors(cfg, &mut errors);
    path_errors(
        cfg,
        &protected_paths(cfg, &resolve_tengu_home()),
        HARDENED,
        &mut errors,
    );
    errors
}

/// The config file `Config::load` read must be out of every agent's reach
/// too: an agent that could edit it would lift the limits at the next load.
pub(crate) fn config_file_errors(cfg: &Config, file: &Path) -> Vec<String> {
    if !requires_hardened_claude_code(cfg) {
        return Vec::new();
    }
    let mut errors = Vec::new();
    let protected = [Protected::file("the config file", file)];
    path_errors(cfg, &protected, HARDENED, &mut errors);
    errors
}

/// The path check above for one directory any sandbox may protect (the
/// `[xmarket]` state dir, `config/xmarket.rs`): no `fs_roots` entry and no
/// agent `workspace` overlaps `dir` either way (symlinks resolved). `why`
/// ends each message.
pub(crate) fn dir_reach_errors(cfg: &Config, what: &str, dir: &Path, why: &str) -> Vec<String> {
    let mut errors = Vec::new();
    path_errors(cfg, &[Protected::dir(what, dir)], why, &mut errors);
    errors
}

fn claude_code_errors(cfg: &Config) -> Vec<String> {
    let mut agents: Vec<_> = cfg
        .agents
        .iter()
        .filter(|(_, a)| a.engine.trim() == "claude_code")
        .collect();
    agents.sort_by(|a, b| a.0.cmp(b.0));
    agents
        .into_iter()
        .filter_map(|(id, agent)| {
            // Exact match, like `BuiltinToolsProfile::from_str`.
            let got = match agent.claude_code.as_ref() {
                Some(cc) if cc.builtin_tools_profile == "none" => return None,
                Some(cc) => format!("builtin_tools_profile = \"{}\"", cc.builtin_tools_profile),
                None => format!("no [agents.{id}.claude_code] block (= \"editor_shell\")"),
            };
            Some(format!(
                "agents.{id}: engine = \"claude_code\" in a hardened sandbox (Solana signer \
                 or [risk]) needs [agents.{id}.claude_code] builtin_tools_profile = \"none\" — got {got}; \
                 built-in tools run outside tengu scopes"
            ))
        })
        .collect()
}

/// Every configured scope, `default_scopes.<tool>` and
/// `agents.<a>.scopes.<tool>`, sorted by that label.
fn scopes(cfg: &Config) -> Vec<(String, &crate::domain::scope::ToolScope)> {
    let default = cfg
        .default_scopes
        .iter()
        .map(|(t, s)| (format!("default_scopes.{t}"), s));
    let per_agent = cfg.agents.iter().flat_map(|(id, a)| {
        a.scopes
            .iter()
            .map(move |(t, s)| (format!("agents.{id}.scopes.{t}"), s))
    });
    let mut out: Vec<_> = default.chain(per_agent).collect();
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

fn foreign_process_errors(cfg: &Config, errors: &mut Vec<String>) {
    if !cfg.mcp_servers.is_empty() {
        errors.push(
            "[[mcp_servers]] are not allowed in a hardened sandbox (Solana signer or [risk]): \
             foreign processes with this filesystem"
                .into(),
        );
    }
    for (at, scope) in scopes(cfg) {
        if !scope.shell_bins.is_empty() {
            errors.push(format!(
                "{at}.shell_bins grants a shell — a hardened sandbox (Solana signer or [risk]) \
                 runs no shell"
            ));
        }
    }
}

/// A path no agent may reach.
struct Protected {
    what: String,
    /// Resolved (`resolved`).
    path: PathBuf,
    /// A directory: refused when it and a root overlap either way; a file:
    /// refused when a root contains it.
    dir: bool,
}

impl Protected {
    fn file(what: &str, path: &Path) -> Self {
        Self {
            what: what.to_string(),
            path: resolved(&expand_tilde(path)),
            dir: false,
        }
    }

    fn dir(what: &str, path: &Path) -> Self {
        Self {
            dir: true,
            ..Self::file(what, path)
        }
    }

    /// Why `root` (resolved) reaches this path; `None` when it does not.
    fn reached_by(&self, root: &Path) -> Option<&'static str> {
        if self.path.starts_with(root) {
            Some("inside")
        } else if self.dir && root.starts_with(&self.path) {
            Some("around")
        } else {
            None
        }
    }
}

/// The signer key (absolute only — `config/solana.rs` reports the rest),
/// `<tengu_home>/state`, and `[risk] kill_switch_file` (absolute only —
/// `config/risk.rs` reports the rest).
fn protected_paths(cfg: &Config, tengu_home: &Path) -> Vec<Protected> {
    let mut out = Vec::new();
    if let Some(key) = cfg
        .solana
        .signer_path()
        .filter(|k| k.is_absolute() && !k.as_os_str().is_empty())
    {
        out.push(Protected::file("solana.signer_key_file", &key));
    }
    out.push(Protected::dir(
        "<TENGU_HOME>/state",
        &tengu_home.join("state"),
    ));
    if let Some(r) = &cfg.risk {
        let kill = expand_tilde(&r.kill_switch_file);
        if kill.is_absolute() {
            out.push(Protected::file("risk.kill_switch_file", &kill));
        }
    }
    out
}

fn path_errors(cfg: &Config, protected: &[Protected], why: &str, errors: &mut Vec<String>) {
    let mut agents: Vec<_> = cfg.agents.iter().collect();
    agents.sort_by(|a, b| a.0.cmp(b.0));
    let workspaces = agents.iter().filter_map(|(id, a)| {
        a.workspace
            .as_ref()
            .map(|ws| (format!("agents.{id}.workspace"), ws.clone()))
    });
    let roots = scopes(cfg).into_iter().flat_map(|(at, scope)| {
        scope
            .fs_roots
            .iter()
            .map(move |r| (format!("{at}.fs_roots"), r.clone()))
    });
    for (at, root) in workspaces.chain(roots) {
        let root = resolved(&expand_tilde(&root));
        for p in protected {
            if let Some(how) = p.reached_by(&root) {
                errors.push(format!(
                    "{} `{}` is {how} {at} `{}` — keep it outside every fs root and workspace \
                     ({why})",
                    p.what,
                    p.path.display(),
                    root.display()
                ));
            }
        }
    }
}

/// Lexical normalisation (no symlink resolution) of an absolute path.
fn normalize(p: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            c => out.push(c),
        }
    }
    out
}

/// Absolute, normalised, and symlink-resolved when the path exists — the
/// longest existing ancestor is resolved otherwise (macOS `/var` →
/// `/private/var` must compare equal for a file not created yet).
pub(crate) fn resolved(p: &Path) -> PathBuf {
    let abs = if p.is_absolute() {
        p.to_path_buf()
    } else {
        std::env::current_dir().unwrap_or_default().join(p)
    };
    let abs = normalize(&abs);
    let mut tail = Vec::new();
    let mut head = abs.as_path();
    loop {
        if let Ok(real) = std::fs::canonicalize(head) {
            return tail.iter().rev().fold(real, |acc, c| acc.join(c));
        }
        match (head.parent(), head.file_name()) {
            (Some(parent), Some(name)) => {
                tail.push(name.to_os_string());
                head = parent;
            }
            _ => return abs,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::scope::ToolScope;

    const SIGNER: &str = "[solana]\nsigner_key_file = \"/keys/signer.json\"\n";

    /// `Config::load` of a sandbox: a signer (or not), an OpenRouter default
    /// agent and one `claude_code` agent with `extra` appended to its block.
    fn load(signer: bool, extra: &str) -> anyhow::Result<Config> {
        let toml = format!(
            "{}\
             [agents.planner]\ndefault = true\nengine = \"openrouter\"\nmodel = \"m\"\n\
             [agents.exec]\nengine = \"claude_code\"\nmodel = \"claude-haiku-4-5\"\n{extra}",
            if signer { SIGNER } else { "" }
        );
        let file = tempfile::NamedTempFile::new()?;
        std::fs::write(file.path(), toml)?;
        Config::load(file.path())
    }

    #[test]
    fn signing_sandbox_accepts_claude_code_with_builtins_off() {
        let cfg = load(
            true,
            "[agents.exec.claude_code]\nbuiltin_tools_profile = \"none\"\n",
        )
        .expect("hardened claude_code agent loads");
        assert!(requires_hardened_claude_code(&cfg));
        assert!(
            cfg.agents.values().all(|a| a.no_shell_fallback),
            "every agent's fallback runs no shell"
        );
    }

    #[test]
    fn signing_sandbox_refuses_claude_code_with_builtins_on_or_no_block() {
        for profile in ["read_only", "editor", "editor_shell", " none"] {
            let err = load(
                true,
                &format!("[agents.exec.claude_code]\nbuiltin_tools_profile = \"{profile}\"\n"),
            )
            .unwrap_err()
            .to_string();
            assert!(
                err.contains("agents.exec: engine = \"claude_code\" in a hardened sandbox")
                    && err.contains(&format!("got builtin_tools_profile = \"{profile}\"")),
                "{profile}: {err}"
            );
        }
        let err = load(true, "").unwrap_err().to_string();
        assert!(
            err.contains("got no [agents.exec.claude_code] block (= \"editor_shell\")"),
            "{err}"
        );
    }

    /// `[risk]` hardens like a signer (convention 12): the paper book and
    /// the kill-switch file must be out of reach of built-in tools.
    #[test]
    fn risk_sandbox_is_hardened_like_a_signer() {
        let risk: Config = toml::from_str(RISK_SANDBOX).expect("parse");
        assert!(requires_hardened_claude_code(&risk));
        let errs = claude_code_errors(&risk);
        assert_eq!(errs.len(), 1, "{errs:?}");
        assert!(errs[0].contains("(Solana signer or [risk])"), "{errs:?}");
        let mut hardened = risk.clone();
        hardened.agents.get_mut("exec").unwrap().claude_code =
            Some(crate::config::AgentClaudeCodeConfig {
                builtin_tools_profile: "none".into(),
            });
        assert!(claude_code_errors(&hardened).is_empty());
        hardened.fold_default_scopes();
        assert!(hardened.agents.values().all(|a| a.no_shell_fallback));
    }

    const RISK_SANDBOX: &str = r#"
        [agents.planner]
        default = true
        engine = "openrouter"
        model = "m"

        [agents.exec]
        engine = "claude_code"
        model = "claude-haiku-4-5"

        [risk]
        account = "xmarket"
        mode = "paper"
        venues = ["hyperliquid"]
        min_lifecycle = "paper_tradable"
        instruments_allow = ["hyperliquid:xyz:TSLA"]
        instruments_deny = []
        max_order_notional_usd = 25
        max_position_notional_usd = 50
        max_asset_exposure_usd = 50
        max_venue_exposure_usd = 100
        max_gross_exposure_usd = 100
        max_net_exposure_usd = 100
        max_leverage = 1
        daily_loss_limit_usd = 10
        total_loss_limit_usd = 25
        min_edge_bps = 10
        max_slippage_bps = 30
        min_depth_usd = 250
        require_hedge_for = []
        max_data_age_ms = { book = 5000, ctx = 20000, reference = 60000, quote = 20000 }
        max_skew_ms = 5000
        max_orders_per_min = 6
        max_open_orders = 4
        kill_switch_file = "/srv/xm/KILL"
        allow_reduce_degraded = true
        exits = { take_profit_bps = 200, stop_loss_bps = 100, max_hold_secs = 86400 }

        [paper]
        initial_cash_usd = 100
        latency_ms = 250
        latency_jitter_ms = 100
        fee_tier = 0
        staking_discount_pct = 0
        order_types = ["market", "ioc"]
    "#;

    #[test]
    fn no_signer_leaves_claude_code_alone() {
        let cfg = load(false, "").expect("unhardened sandbox loads");
        assert!(!requires_hardened_claude_code(&cfg));
        assert!(cfg.agents.values().all(|a| !a.no_shell_fallback));
        assert!(validation_errors(&cfg).is_empty());
    }

    fn base() -> Config {
        let mut cfg = Config::default();
        cfg.agents.get_mut("main").unwrap().default = false;
        cfg
    }

    fn with_signer(key: &str) -> Config {
        let mut cfg = base();
        cfg.solana.signer_key_file = Some(key.into());
        cfg
    }

    fn scope(f: impl FnOnce(&mut ToolScope)) -> ToolScope {
        let mut s = ToolScope::default();
        f(&mut s);
        s
    }

    /// Every rule but the `claude_code` one (the default `main` agent is an
    /// OpenRouter one anyway), against `tengu_home`.
    fn errs_at(cfg: &Config, tengu_home: &str) -> String {
        let mut errors = Vec::new();
        foreign_process_errors(cfg, &mut errors);
        path_errors(
            cfg,
            &protected_paths(cfg, Path::new(tengu_home)),
            HARDENED,
            &mut errors,
        );
        errors.join("\n")
    }

    fn errs(cfg: &Config) -> String {
        errs_at(cfg, "/srv/tengu-home")
    }

    #[test]
    fn nothing_is_checked_outside_a_hardened_sandbox() {
        let mut cfg = base();
        cfg.agents.get_mut("main").unwrap().engine = "claude_code".into();
        cfg.default_scopes.insert(
            "run_command".into(),
            scope(|s| s.shell_bins = vec!["*".into()]),
        );
        cfg.default_scopes.insert(
            "read_file".into(),
            scope(|s| s.fs_roots = vec![PathBuf::from("/srv/tengu-home")]),
        );
        assert!(validation_errors(&cfg).is_empty());
    }

    /// A signer and `[risk]` share these rules (one code path).
    #[test]
    fn hardened_sandboxes_refuse_mcp_servers_and_shell_scopes() {
        let risk: Config = toml::from_str(RISK_SANDBOX).expect("parse");
        for mut cfg in [with_signer("/keys/signer.json"), risk] {
            cfg.mcp_servers.push(crate::config::McpServerConfig {
                name: "x".into(),
                transport: "stdio".into(),
                command: vec!["x".into()],
                url: None,
                env: Default::default(),
                auth: None,
            });
            cfg.default_scopes.insert(
                "run_command".into(),
                scope(|s| s.shell_bins = vec!["ls".into()]),
            );
            cfg.agents.values_mut().next().unwrap().scopes.insert(
                "my_skill".into(),
                scope(|s| s.shell_bins = vec!["*".into()]),
            );
            let e = errs(&cfg);
            assert!(!e.contains("claude_code"), "{e}");
            assert!(e.contains("[[mcp_servers]] are not allowed"), "{e}");
            assert!(e.contains("default_scopes.run_command.shell_bins"), "{e}");
            assert!(
                e.contains(".scopes.my_skill.shell_bins grants a shell"),
                "{e}"
            );
        }
    }

    /// `/srv/…`: not a symlink on macOS (`/home` is), so messages show the
    /// paths as written.
    #[test]
    fn signer_key_must_be_outside_fs_roots_and_workspaces() {
        let mut cfg = with_signer("/srv/op/ws/keys/../signer.json");
        cfg.default_scopes.insert(
            "read_file".into(),
            scope(|s| s.fs_roots = vec![PathBuf::from("/srv/op/ws")]),
        );
        let e = errs(&cfg);
        assert!(
            e.contains("solana.signer_key_file `/srv/op/ws/signer.json` is inside default_scopes.read_file.fs_roots"),
            "{e}"
        );

        let mut cfg = with_signer("/srv/op/keys/signer.json");
        cfg.agents.get_mut("main").unwrap().workspace = Some(PathBuf::from("/srv/op"));
        assert!(errs(&cfg).contains("is inside agents.main.workspace"));

        let mut cfg = with_signer("/srv/op/keys/signer.json");
        cfg.default_scopes.insert(
            "read_file".into(),
            scope(|s| s.fs_roots = vec![PathBuf::from("/srv/op/ws")]),
        );
        cfg.agents.get_mut("main").unwrap().workspace = Some(PathBuf::from("/srv/op/ws"));
        assert_eq!(errs(&cfg), "", "sibling of the workspace is fine");
    }

    /// `<TENGU_HOME>/state` (ledger, leases, runtime) may not overlap a root
    /// either way; the kill-switch file may not sit inside one.
    #[test]
    fn state_dir_and_kill_switch_stay_outside_every_root() {
        let mut risk: Config = toml::from_str(RISK_SANDBOX).expect("parse");
        risk.agents.get_mut("exec").unwrap().workspace = Some(PathBuf::from("/srv/xm-ws"));
        assert_eq!(errs(&risk), "", "state, kill switch and workspace apart");
        for (root, want) in [
            (
                "/srv/tengu-home",
                "<TENGU_HOME>/state `/srv/tengu-home/state` is inside default_scopes.read_file.fs_roots `/srv/tengu-home`",
            ),
            (
                "/srv/tengu-home/state/xmarket",
                "<TENGU_HOME>/state `/srv/tengu-home/state` is around default_scopes.read_file.fs_roots `/srv/tengu-home/state/xmarket`",
            ),
            (
                "/srv/xm",
                "risk.kill_switch_file `/srv/xm/KILL` is inside default_scopes.read_file.fs_roots `/srv/xm`",
            ),
        ] {
            let mut cfg = risk.clone();
            cfg.default_scopes.insert(
                "read_file".into(),
                scope(|s| s.fs_roots = vec![PathBuf::from(root)]),
            );
            let e = errs(&cfg);
            assert!(e.contains(want), "{root}: {e}");
        }
        let mut cfg = risk.clone();
        cfg.agents.get_mut("exec").unwrap().workspace =
            Some(PathBuf::from("/srv/tengu-home/state/xmarket/ws"));
        assert!(
            errs(&cfg).contains("is around agents.exec.workspace"),
            "{}",
            errs(&cfg)
        );
        // A signer sandbox protects the state dir (Solana leases) too.
        let mut cfg = with_signer("/keys/signer.json");
        cfg.agents.get_mut("main").unwrap().workspace = Some(PathBuf::from("/srv"));
        let e = errs(&cfg);
        assert!(e.contains("<TENGU_HOME>/state"), "{e}");
    }

    /// The config file `Config::load` read may not sit under a root: an
    /// agent could raise the limits for the next load.
    #[test]
    fn the_config_file_stays_outside_every_root() {
        let dir = tempfile::tempdir().unwrap();
        let ws = dir.path().join("ws");
        std::fs::create_dir_all(&ws).unwrap();
        let mut cfg: Config = toml::from_str(RISK_SANDBOX).expect("parse");
        cfg.agents.get_mut("exec").unwrap().workspace = Some(ws.clone());
        let inside = ws.join("config.toml");
        let e = config_file_errors(&cfg, &inside).join("\n");
        assert!(
            e.contains("the config file") && e.contains("is inside agents.exec.workspace"),
            "{e}"
        );
        assert!(config_file_errors(&cfg, &dir.path().join("config.toml")).is_empty());
        let plain = base();
        assert!(
            config_file_errors(&plain, &inside).is_empty(),
            "not hardened"
        );
    }

    #[test]
    fn resolved_follows_symlinks_of_existing_ancestors() {
        let dir = tempfile::tempdir().unwrap();
        let real = std::fs::canonicalize(dir.path()).unwrap();
        let p = dir.path().join("not/yet/../there.json");
        assert_eq!(resolved(&p), real.join("not/there.json"));
    }
}
