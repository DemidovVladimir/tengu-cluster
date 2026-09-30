//! Hardened sandboxes (tracker convention 12): where signing or money is
//! involved nothing runs outside tengu scopes — a `[solana]` signer or a
//! `[risk]` section (the xmarket paper / live budget).
//! Doc: `docs/engine-backends.md` § Claude Code.
//!
//! | Rule (hardened sandbox) | Enforced by | Why |
//! |---|---|---|
//! | Every `engine = "claude_code"` agent sets `[agents.<a>.claude_code] builtin_tools_profile = "none"` — no block = `editor_shell` = refused | `Config::load` (`validation_errors`) | built-in Read / Write / Bash ignore tengu scopes: they could read the key or edit a store |
//! | The CLI sees only the tengu bridge | `--strict-mcp-config` on every run (`engines/claude_code.rs::cli_args`) | the operator's own MCP servers run outside tengu scopes and egress |
//! | Tools without a configured scope run no shell (in-process and bridge) | `AgentConfig::no_shell_fallback`, set by `Config::fold_default_scopes` | a shell reads any file |
//! | Signer: no `[[mcp_servers]]`, no scope granting `shell_bins`, key outside every fs root / workspace, wallet grants | `config/solana.rs` | |

use super::Config;

/// True when the sandbox is hardened: `claude_code` agents only with
/// built-in tools off, and a no-shell fallback scope on every agent.
pub(crate) fn requires_hardened_claude_code(cfg: &Config) -> bool {
    cfg.solana.signer_key_file.is_some() || cfg.risk.is_some()
}

pub(crate) fn validation_errors(cfg: &Config) -> Vec<String> {
    if !requires_hardened_claude_code(cfg) {
        return Vec::new();
    }
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

#[cfg(test)]
mod tests {
    use super::*;

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
        let errs = validation_errors(&risk);
        assert_eq!(errs.len(), 1, "{errs:?}");
        assert!(errs[0].contains("(Solana signer or [risk])"), "{errs:?}");
        let mut hardened = risk.clone();
        hardened.agents.get_mut("exec").unwrap().claude_code =
            Some(crate::config::AgentClaudeCodeConfig {
                builtin_tools_profile: "none".into(),
            });
        assert!(validation_errors(&hardened).is_empty());
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
        kill_switch_file = "~/.tengu/state/xmarket/KILL"
        allow_reduce_degraded = true

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
}
