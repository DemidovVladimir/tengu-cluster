//! Router for slash commands declared by skills.

use std::collections::HashMap;

use crate::application::skill_registry::SkillRegistry;

/// Routes user slash commands to the skill that declared them.
pub(crate) struct SkillCommandRouter {
    /// command name (e.g. "wallet") → skill name (e.g. "privy").
    routes: HashMap<String, String>,
}

/// Result of attempting to route a slash command.
pub(crate) enum SkillCommandMatch {
    /// The command matched a skill-declared command.
    Matched {
        skill_name: String,
        command: String,
        args: String,
    },
    /// No skill-declared command matched.
    NotMatched,
}

impl SkillCommandRouter {
    /// Build a router from the current state of the skill registry.
    pub(crate) fn from_registry(registry: &SkillRegistry) -> Self {
        let mut routes = HashMap::new();
        for (skill_name, entry) in registry.entries() {
            for cmd in &entry.commands {
                routes.insert(cmd.name.clone(), skill_name.clone());
            }
        }
        Self { routes }
    }

    /// Try to route a slash command input (e.g. "/wallet create").
    ///
    /// Returns `Matched` if the first word after `/` matches a skill command.
    pub(crate) fn route(&self, input: &str) -> SkillCommandMatch {
        let trimmed = input.trim();
        let without_slash = match trimmed.strip_prefix('/') {
            Some(s) => s,
            None => return SkillCommandMatch::NotMatched,
        };

        // Strip bot suffix (e.g. "/wallet@mybot" → "wallet")
        let (first, rest) = match without_slash.split_once(char::is_whitespace) {
            Some((cmd, args)) => (cmd, args.trim().to_string()),
            None => (without_slash, String::new()),
        };
        let command = first.split('@').next().unwrap_or(first);

        match self.routes.get(command) {
            Some(skill_name) => SkillCommandMatch::Matched {
                skill_name: skill_name.clone(),
                command: command.to_string(),
                args: rest,
            },
            None => SkillCommandMatch::NotMatched,
        }
    }

    /// List all registered skill commands as `(command, skill_name)` pairs.
    pub(crate) fn list(&self) -> Vec<(String, String)> {
        let mut list: Vec<_> = self
            .routes
            .iter()
            .map(|(cmd, skill)| (cmd.clone(), skill.clone()))
            .collect();
        list.sort();
        list
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn route_matches_skill_command() {
        let mut routes = HashMap::new();
        routes.insert("wallet".to_string(), "privy".to_string());
        let router = SkillCommandRouter { routes };

        match router.route("/wallet create") {
            SkillCommandMatch::Matched {
                skill_name,
                command,
                args,
            } => {
                assert_eq!(skill_name, "privy");
                assert_eq!(command, "wallet");
                assert_eq!(args, "create");
            }
            SkillCommandMatch::NotMatched => panic!("Expected match"),
        }
    }

    #[test]
    fn route_no_args() {
        let mut routes = HashMap::new();
        routes.insert("wallet".to_string(), "privy".to_string());
        let router = SkillCommandRouter { routes };

        match router.route("/wallet") {
            SkillCommandMatch::Matched { args, .. } => {
                assert!(args.is_empty());
            }
            SkillCommandMatch::NotMatched => panic!("Expected match"),
        }
    }

    #[test]
    fn route_strips_bot_suffix() {
        let mut routes = HashMap::new();
        routes.insert("wallet".to_string(), "privy".to_string());
        let router = SkillCommandRouter { routes };

        match router.route("/wallet@mybot create") {
            SkillCommandMatch::Matched { command, args, .. } => {
                assert_eq!(command, "wallet");
                assert_eq!(args, "create");
            }
            SkillCommandMatch::NotMatched => panic!("Expected match"),
        }
    }

    #[test]
    fn route_not_matched() {
        let router = SkillCommandRouter {
            routes: HashMap::new(),
        };
        assert!(matches!(
            router.route("/unknown"),
            SkillCommandMatch::NotMatched
        ));
    }

    #[test]
    fn route_non_slash_not_matched() {
        let mut routes = HashMap::new();
        routes.insert("wallet".to_string(), "privy".to_string());
        let router = SkillCommandRouter { routes };

        assert!(matches!(
            router.route("wallet create"),
            SkillCommandMatch::NotMatched
        ));
    }

    #[test]
    fn list_returns_sorted() {
        let mut routes = HashMap::new();
        routes.insert("wallet".to_string(), "privy".to_string());
        routes.insert("deploy".to_string(), "infra".to_string());
        let router = SkillCommandRouter { routes };

        let list = router.list();
        assert_eq!(list.len(), 2);
        assert_eq!(list[0].0, "deploy");
        assert_eq!(list[1].0, "wallet");
    }
}
