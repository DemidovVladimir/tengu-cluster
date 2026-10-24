//! `Config::load` — `${VAR}` substitution, `fold_default_scopes`, validation
//! (errors fail the load, warnings are logged) — and the scope lookup.

use super::*;

/// Aggregates config validation issues.
#[derive(Debug, Default)]
pub(super) struct ValidationErrors {
    issues: Vec<String>,
}

impl ValidationErrors {
    pub(super) fn push(&mut self, message: impl Into<String>) {
        self.issues.push(message.into());
    }

    pub(super) fn require(&mut self, condition: bool, message: impl Into<String>) {
        if !condition {
            self.push(message);
        }
    }

    pub(super) fn require_nonempty(&mut self, path: &str, value: &str) {
        self.require(!value.trim().is_empty(), format!("{path} cannot be empty"));
    }

    pub(super) fn require_one_of(&mut self, path: &str, value: &str, allowed: &[&str]) {
        let normalized = value.trim();
        if allowed.contains(&normalized) {
            return;
        }
        self.push(format!(
            "{path} must be one of {} (got '{}')",
            allowed.join("|"),
            value
        ));
    }

    pub(super) fn require_positive_opt_u32(&mut self, path: &str, value: Option<u32>) {
        if matches!(value, Some(0)) {
            self.push(format!("{path} must be greater than 0 when set"));
        }
    }

    pub(super) fn into_vec(self) -> Vec<String> {
        self.issues
    }
}

impl Config {
    /// Load config from path and apply `${ENV_VAR}` substitution.
    pub fn load(path: &std::path::Path) -> anyhow::Result<Self> {
        let raw = std::fs::read_to_string(path)?;
        let content = Self::substitute_env_vars(&raw)?;
        for name in Self::unset_env_vars(&raw) {
            tracing::warn!(
                path = %path.display(),
                "${{{name}}} is not set: it stays literal in the config (set it, or remove the reference)"
            );
        }
        let mut config: Config = toml::from_str(&content)?;
        let mut errors = config.validation_errors();
        errors.extend(hardening::config_file_errors(&config, path));
        // Pins hash the raw text (no `${VAR}` substitution), as `verify --pins`.
        let (generation_errors, scope) = lineage::binding_errors(&config, path, &raw);
        errors.extend(generation_errors);
        let (ranking_errors, ranking) = strategy_ranking::section_errors(&config, path);
        errors.extend(ranking_errors);
        Self::fail_on(errors)?;
        for warning in config.validation_warnings() {
            tracing::warn!(path = %path.display(), "{warning}");
        }
        config.generation_scope = scope.map(std::sync::Arc::new);
        config.ranking_section = ranking.map(std::sync::Arc::new);
        config.loaded_from = Some(path.to_path_buf());
        config.source_sha256 = crate::domain::canonical::toml_digest(&raw).ok();
        config.fold_default_scopes();
        Ok(config)
    }

    /// Post-load normalisation: fold `[default_scopes]` into every agent's
    /// `scopes` map (per-agent entries win wholesale, never field-merged) and
    /// expand `~` in `fs_roots`. `build_tool_executor` only holds an
    /// `AgentConfig`, so the fallback has to be materialised here; `run-agent`
    /// children load the parent config through this same path.
    pub fn fold_default_scopes(&mut self) {
        let hardened = hardening::requires_hardened_claude_code(self);
        let sections = std::sync::Arc::new(self.sandbox_sections());
        for agent in self.agents.values_mut() {
            agent.no_shell_fallback = hardened;
            agent.sandbox = std::sync::Arc::clone(&sections);
            for (tool, scope) in &self.default_scopes {
                agent
                    .scopes
                    .entry(tool.clone())
                    .or_insert_with(|| scope.clone());
            }
            for scope in agent.scopes.values_mut() {
                for root in scope.fs_roots.iter_mut() {
                    *root = crate::config::paths::expand_tilde(root);
                }
            }
        }
        for scope in self.default_scopes.values_mut() {
            for root in scope.fs_roots.iter_mut() {
                *root = crate::config::paths::expand_tilde(root);
            }
        }
    }

    /// The sandbox this config is: `<name>` of `sandboxes/<name>/config.toml`
    /// however it was loaded, else `default` (`SandboxSections::owner`).
    pub fn sandbox_owner(&self) -> String {
        self.loaded_from
            .as_deref()
            .and_then(paths::sandbox_of_config_file)
            .unwrap_or_else(|| sections::DEFAULT_SANDBOX.to_string())
    }

    /// The sandbox sections every agent's tools read (`config/sections.rs`).
    pub(super) fn sandbox_sections(&self) -> sections::SandboxSections {
        let home = crate::config::paths::resolve_tengu_home();
        sections::SandboxSections {
            sandbox: self
                .loaded_from
                .as_deref()
                .and_then(paths::sandbox_of_config_file),
            xm_state_dir: self.xmarket.as_ref().map(|x| x.state_dir(&home)),
            risk: self.risk.as_ref().map(risk::RiskConfig::resolved),
            paper: self.paper.clone(),
            calendars: self
                .xmarket
                .as_ref()
                .map(|x| x.calendars())
                .unwrap_or_default(),
            rate_limits: self.rate_limits.clone(),
            recorder: self.recorder.clone(),
            history_dir: self
                .xmarket
                .as_ref()
                .filter(|_| self.recorder.enabled)
                .map(|x| x.history_dir(&home)),
            weekend_fade: self.xmarket.as_ref().and_then(|x| x.weekend_fade.clone()),
            backtest: self.backtest.clone(),
            generation: self.generation_scope.clone(),
            sources: self.sources.clone().map(std::sync::Arc::new),
            sources_state_dir: self.sources.as_ref().map(|s| s.state_dir(&home)),
            soe: self.soe.clone().map(std::sync::Arc::new),
            ranking: self.ranking_section.clone(),
            a2a: self.a2a.clone().map(std::sync::Arc::new),
            privy_wallet_id: self.solana.privy_wallet_id.clone(),
        }
    }

    /// Validate cross-field configuration invariants. `Config::load` adds the
    /// rules that need the file's path (`hardening::config_file_errors`).
    pub fn validate(&self) -> anyhow::Result<()> {
        Self::fail_on(self.validation_errors())
    }

    /// `Ok` for no errors, else one error listing them all.
    pub(super) fn fail_on(errors: Vec<String>) -> anyhow::Result<()> {
        if errors.is_empty() {
            return Ok(());
        }

        let message = format!(
            "Config validation failed ({} issue{}):\n{}",
            errors.len(),
            if errors.len() == 1 { "" } else { "s" },
            errors
                .iter()
                .map(|entry| format!("- {}", entry))
                .collect::<Vec<_>>()
                .join("\n")
        );
        Err(anyhow::anyhow!(message))
    }

    /// Non-fatal smells, logged by `Config::load`: a `local` agent on the
    /// 1_000_000 default `context_window` (its tool-result cap and history
    /// budget derive from it; the server's real window is far smaller).
    pub fn validation_warnings(&self) -> Vec<String> {
        let default = default_context_window();
        let mut out = Vec::new();
        for (id, agent) in &self.agents {
            for name in agent.tools.iter().filter(|n| !self.is_known_tool(n)) {
                out.push(format!(
                    "agents.{id}.tools: '{name}' is no catalog tool (`tengu tool list`) and no \
                     `<server>__<tool>` of a [[mcp_servers]] entry — dropped unless a shell skill \
                     the agent loads defines it"
                ));
            }
            if agent.engine == "local" && agent.limits.context_window == default {
                out.push(format!(
                    "agents.{id}.limits.context_window is the {default} default; set the local \
                     server's real window (Ollama: num_ctx) — docs/engine-backends.md § Local"
                ));
            }
            // A routable agent without `workspace` runs each plan step in a
            // temp dir removed after it (`bootstrap::tools::workspace_or_temp`).
            if agent.description.is_some() && agent.workspace.is_none() {
                // Listed in `tools` or opted in; `write_file` is a base tool
                // (an empty `tools` = every base tool).
                let has = |t: &str| {
                    agent.tools.iter().any(|x| x == t)
                        || agent.workspace_tools.iter().any(|x| x == t)
                        || (agent.tools.is_empty() && t == "write_file")
                };
                let writers: Vec<&str> = STEP_WORKSPACE_WRITERS
                    .iter()
                    .copied()
                    .filter(|t| has(t))
                    .collect();
                if !writers.is_empty() {
                    out.push(format!(
                        "agents.{id} has no `workspace`: each plan step (and webhook turn) runs in a temp \
                         dir removed after it, and what {writers:?} write there goes with it — set \
                         `workspace` to keep it"
                    ));
                }
            }
        }
        out.extend(a2a::validation_warnings(self));
        out.extend(readers::warnings(self));
        out.sort();
        out
    }

    pub(super) fn validation_errors(&self) -> Vec<String> {
        let mut errors = ValidationErrors::default();

        errors.require(
            !self.agents.is_empty(),
            "at least one agent must be configured",
        );

        let default_count = self.agents.values().filter(|agent| agent.default).count();
        if default_count > 1 {
            errors.push("only one agent can have default=true");
        }

        self.agents.iter().for_each(|(agent_id, agent)| {
            Self::validate_agent(agent_id, agent, &mut errors);
        });
        let keys_env: Vec<&str> = self.keys.env.keys().map(String::as_str).collect();
        for (agent_id, agent) in &self.agents {
            for name in agent.codex.iter().flat_map(|cx| &cx.forward_env) {
                if let Some(why) = codex_forward_env_error(name, &keys_env) {
                    errors.push(format!("agents.{agent_id}.codex.forward_env: {why}"));
                }
            }
        }

        for issue in self.egress.validation_errors() {
            errors.push(issue);
        }
        for issue in self.keys.validation_errors(&self.egress.allow_hosts) {
            errors.push(issue);
        }
        for issue in solana::validation_errors(self) {
            errors.push(issue);
        }
        for issue in hardening::validation_errors(self) {
            errors.push(issue);
        }
        for issue in readers::errors(self) {
            errors.push(issue);
        }
        for issue in xmarket::validation_errors(self) {
            errors.push(issue);
        }
        for issue in risk::validation_errors(self) {
            errors.push(issue);
        }
        for issue in rate_limits::validation_errors(&self.rate_limits) {
            errors.push(issue);
        }
        for issue in self.runtime.validation_errors() {
            errors.push(issue);
        }
        for issue in self.recorder.validation_errors(self.xmarket.is_some()) {
            errors.push(issue);
        }
        if let Some(bt) = &self.backtest {
            let loops: Vec<&str> = self.decision_loops.keys().map(String::as_str).collect();
            let exchange_calendars: Vec<&str> = self
                .xmarket
                .iter()
                .flat_map(|x| &x.calendars)
                .filter(|(_, c)| c.kind == xmarket::CalendarKind::Exchange)
                .map(|(id, _)| id.as_str())
                .collect();
            for issue in bt.validation_errors(self.xmarket.is_some(), &loops, &exchange_calendars) {
                errors.push(issue);
            }
        }
        for issue in feeds::validation_errors(self) {
            errors.push(issue);
        }
        for issue in studio::validation_errors(self) {
            errors.push(issue);
        }
        for issue in sources::validation_errors(self) {
            errors.push(issue);
        }
        for issue in soe::validation_errors(self) {
            errors.push(issue);
        }
        for issue in a2a::validation_errors(self) {
            errors.push(issue);
        }

        for (name, dl) in &self.decision_loops {
            for issue in dl.validation_errors(name) {
                errors.push(issue);
            }
            match self.agents.get(&dl.agent) {
                None => errors.push(format!(
                    "decision_loops.{name}.agent: no [agents.{}] block",
                    dl.agent
                )),
                Some(agent) if !agent.tools.is_empty() => {
                    for (an, action) in &dl.actions {
                        // A dry-run loop may name write tools that are not
                        // built yet — they are logged, never invoked.
                        let never_runs = dl.logs_only(action);
                        if let Some(tool) = action.tool.as_ref().filter(|_| !never_runs) {
                            if !agent.tools.contains(tool) {
                                errors.push(format!(
                                    "decision_loops.{name}.actions.{an}.tool: `{tool}` is not in [agents.{}].tools",
                                    dl.agent
                                ));
                            }
                        }
                    }
                }
                Some(_) => {}
            }
        }

        errors.into_vec()
    }

    /// A name an agent's `tools` may list: a default or opt-in catalog tool,
    /// or `<server>__<tool>` of a configured `[[mcp_servers]]` entry (its
    /// tools are known only once connected). Any other name warns at load,
    /// not an error: a shell skill's tool (found in the skill tiers at run
    /// time) is legal too.
    pub(super) fn is_known_tool(&self, name: &str) -> bool {
        crate::domain::tools::DEFAULT_TOOLS.contains(&name)
            || crate::domain::tools::WORKSPACE_TOOLS.contains(&name)
            || name.split_once("__").is_some_and(|(server, tool)| {
                !tool.is_empty() && self.mcp_servers.iter().any(|s| s.name == server)
            })
    }

    pub(super) fn validate_agent(
        agent_id: &str,
        agent: &AgentConfig,
        errors: &mut ValidationErrors,
    ) {
        errors.require(!agent_id.trim().is_empty(), "agent id cannot be empty");
        errors.require_nonempty(&format!("agents.{agent_id}.engine"), &agent.engine);
        errors.require_one_of(&format!("agents.{agent_id}.engine"), &agent.engine, ENGINES);
        errors.require_nonempty(&format!("agents.{agent_id}.model"), &agent.model);
        // Read trimmed everywhere (`BuiltinToolsProfile::parse`, the
        // hardening rule); an unknown value is a load error on any block.
        if let Some(ref cc) = agent.claude_code {
            errors.require_one_of(
                &format!("agents.{agent_id}.claude_code.builtin_tools_profile"),
                &cc.builtin_tools_profile,
                CLAUDE_CODE_PROFILES,
            );
        }
        if let Some(ref cx) = agent.codex {
            errors.require_one_of(
                &format!("agents.{agent_id}.codex.sandbox"),
                cx.sandbox.trim(),
                CODEX_SANDBOXES,
            );
            errors.require_nonempty(&format!("agents.{agent_id}.codex.cli_path"), &cx.cli_path);
            errors.require_one_of(
                &format!("agents.{agent_id}.codex.web_search"),
                cx.web_search.trim(),
                CODEX_WEB_SEARCH,
            );
        }

        errors.require_one_of(
            &format!("agents.{agent_id}.flow.scope"),
            &agent.flow.scope,
            &["main", "per-group", "per-pipe-sender", "per-sender"],
        );
        errors.require_one_of(
            &format!("agents.{agent_id}.flow.reset_mode"),
            &agent.flow.reset_mode,
            &["idle", "manual", "time"],
        );
        if let Some(value) = agent.flow.compaction_threshold_ratio {
            if !(0.0..=1.0).contains(&value) || value == 0.0 {
                errors.push(format!(
                    "agents.{}.flow.compaction_threshold_ratio must be within (0.0, 1.0]",
                    agent_id
                ));
            }
        }
        errors.require_positive_opt_u32(
            &format!("agents.{agent_id}.flow.max_history_turns"),
            agent.flow.max_history_turns,
        );
        errors.require_positive_opt_u32(
            &format!("agents.{agent_id}.flow.compaction_keep_turns"),
            agent.flow.compaction_keep_turns,
        );
        errors.require_positive_opt_u32(
            &format!("agents.{agent_id}.flow.compaction_summary_max_tokens"),
            agent.flow.compaction_summary_max_tokens,
        );

        errors.require(
            agent.limits.max_tokens_per_flow > 0,
            format!("agents.{agent_id}.limits.max_tokens_per_flow must be greater than 0"),
        );
        errors.require(
            agent.limits.step_timeout_secs > 0,
            format!("agents.{agent_id}.limits.step_timeout_secs must be greater than 0"),
        );
        if let Some(desc) = &agent.description {
            errors.require(
                !desc.trim().is_empty(),
                format!("agents.{agent_id}.description must not be empty when set"),
            );
        }
        if let Some(output) = agent.limits.max_output_tokens_per_turn {
            let context = agent.limits.context_window;
            if output > context {
                errors.push(format!(
                    "agents.{}.limits.max_output_tokens_per_turn cannot exceed context_window",
                    agent_id
                ));
            }
        }

        if let Some(ref role) = agent.role {
            errors.require(
                !role.trim().is_empty(),
                format!("agents.{agent_id}.role cannot be empty when set"),
            );
        }

        let pb = &agent.prompt_budget;
        let pb_prefix = format!("agents.{agent_id}.prompt_budget");
        errors.require(
            pb.max_file_tokens > 0,
            format!("{pb_prefix}.max_file_tokens must be greater than 0"),
        );
        errors.require(
            pb.max_skill_context_tokens > 0,
            format!("{pb_prefix}.max_skill_context_tokens must be greater than 0"),
        );
        errors.require(
            pb.max_total_tokens > 0,
            format!("{pb_prefix}.max_total_tokens must be greater than 0"),
        );
        errors.require(
            pb.max_file_tokens <= pb.max_total_tokens,
            format!("{pb_prefix}.max_file_tokens cannot exceed max_total_tokens"),
        );
        errors.require(
            pb.max_skill_context_tokens <= pb.max_total_tokens,
            format!("{pb_prefix}.max_skill_context_tokens cannot exceed max_total_tokens"),
        );

        let valid_workspace_tools = crate::domain::tools::WORKSPACE_TOOLS;
        for wt in &agent.workspace_tools {
            if !valid_workspace_tools.contains(&wt.as_str()) {
                errors.push(format!(
                    "agents.{agent_id}.workspace_tools: unknown tool '{}' (valid: {})",
                    wt,
                    valid_workspace_tools.join(", ")
                ));
            }
        }
    }

    /// The `${VAR}` names `substitute_env_vars` leaves literal because the
    /// variable is unset — outside `#` comment lines (an example there is
    /// no reference); each name once, in first-seen order.
    pub(super) fn unset_env_vars(content: &str) -> Vec<String> {
        let re = regex::Regex::new(r"\$\{([A-Z_][A-Z0-9_]*)\}").unwrap();
        let mut out: Vec<String> = Vec::new();
        for line in content.lines().filter(|l| !l.trim_start().starts_with('#')) {
            for cap in re.captures_iter(line) {
                let name = cap[1].to_string();
                if std::env::var(&name).is_err() && !out.contains(&name) {
                    out.push(name);
                }
            }
        }
        out
    }

    pub(super) fn substitute_env_vars(content: &str) -> anyhow::Result<String> {
        let mut result = content.to_string();
        let re = regex::Regex::new(r"\$\{([A-Z_][A-Z0-9_]*)\}").unwrap();

        for cap in re.captures_iter(content) {
            let full_match = cap.get(0).unwrap().as_str();
            let var_name = &cap[1];
            if let Ok(value) = std::env::var(var_name) {
                result = result.replace(full_match, &value);
            }
        }

        Ok(result)
    }

    /// Resolve the effective ToolScope for a given agent + tool.
    /// Per-agent scopes override default_scopes wholesale (not field-merged).
    /// Returns None if neither agent nor default_scopes has an entry.
    #[allow(dead_code)] // Phase A wires this into tool registry construction
    pub fn resolve_scope(&self, agent_name: &str, tool_name: &str) -> Option<ToolScope> {
        if let Some(agent) = self.agents.get(agent_name) {
            if let Some(scope) = agent.scopes.get(tool_name) {
                return Some(scope.clone());
            }
        }
        self.default_scopes.get(tool_name).cloned()
    }
}
