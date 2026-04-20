//! Agent roster rendering and template substitution.
//!
//! The orchestrator's system prompt contains `{{ roster }}` which is
//! replaced once at conversation-init time with a stable Markdown table
//! of `(name, description)` pairs derived from `Config.agents`.
//! One-time substitution preserves prompt cache.

use std::collections::HashMap;

use crate::adapters::config::AgentConfig;

pub fn render_roster(agents: &HashMap<String, AgentConfig>, exclude: &[&str]) -> String {
    let mut rows: Vec<(String, String)> = agents
        .iter()
        .filter(|(name, _)| !exclude.contains(&name.as_str()))
        .map(|(name, cfg)| {
            let desc = cfg
                .identity
                .instructions
                .as_deref()
                .and_then(|s| s.lines().next())
                .unwrap_or("(no description)")
                .to_string();
            (name.clone(), desc)
        })
        .collect();
    rows.sort_by(|a, b| a.0.cmp(&b.0));

    let mut out = String::from("| Agent | Description |\n|---|---|\n");
    for (name, desc) in rows {
        out.push_str(&format!("| {} | {} |\n", name, desc));
    }
    out
}

pub fn substitute_roster(template: &str, roster_md: &str) -> String {
    template.replace("{{ roster }}", roster_md)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapters::config::IdentityConfig;
    use std::collections::HashMap;

    fn agent_with_desc(desc: &str) -> AgentConfig {
        AgentConfig {
            default: false,
            engine: "openrouter".into(),
            model: "any".into(),
            workspace: None,
            default_lens: "eco".into(),
            identity: IdentityConfig {
                name: None,
                instructions: Some(desc.into()),
            },
            flow: Default::default(),
            limits: Default::default(),
            lens: Default::default(),
            role: None,
            skill_packages: Vec::new(),
            prompt_budget: Default::default(),
            requires: Vec::new(),
            workspace_tools: Vec::new(),
            scopes: HashMap::new(),
            claude_code: None,
        }
    }

    #[test]
    fn renders_sorted_table_with_first_line_of_instructions() {
        let mut agents = HashMap::new();
        agents.insert(
            "zebra".into(),
            agent_with_desc("Does zebra things.\nextra detail"),
        );
        agents.insert("alpha".into(), agent_with_desc("Does alpha things."));
        let md = render_roster(&agents, &[]);
        assert!(md.find("alpha").unwrap() < md.find("zebra").unwrap());
        assert!(md.contains("Does alpha things."));
        assert!(md.contains("Does zebra things."));
        assert!(!md.contains("extra detail"));
    }

    #[test]
    fn exclude_filters_orchestrator_itself() {
        let mut agents = HashMap::new();
        agents.insert(
            "orchestrator".into(),
            agent_with_desc("I am the conductor."),
        );
        agents.insert("writer".into(), agent_with_desc("I write."));
        let md = render_roster(&agents, &["orchestrator"]);
        assert!(!md.contains("I am the conductor"));
        assert!(md.contains("I write"));
    }

    #[test]
    fn substitute_replaces_placeholder() {
        let out = substitute_roster("prefix {{ roster }} suffix", "ROSTER_HERE");
        assert_eq!(out, "prefix ROSTER_HERE suffix");
    }
}
