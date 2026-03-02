//! Agent role definitions for fleet orchestration.

use std::fmt;
use std::str::FromStr;

/// The functional role assigned to an agent in the fleet.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum AgentRole {
    QA,
    BackendEngineer,
    IntegrationMaster,
}

impl AgentRole {
    /// Human-readable label for display/logging.
    pub fn label(self) -> &'static str {
        match self {
            Self::QA => "QA",
            Self::BackendEngineer => "Backend Engineer",
            Self::IntegrationMaster => "Integration Master",
        }
    }

    /// System prompt fragment injected for this role.
    pub fn system_prompt_fragment(self) -> &'static str {
        match self {
            Self::QA => "You are a QA specialist. Focus on testing, validation, edge cases, and ensuring correctness.",
            Self::BackendEngineer => "You are a backend engineer. Focus on implementation, architecture, and code quality.",
            Self::IntegrationMaster => "You are an integration master. Focus on connecting systems, APIs, data flow, and end-to-end coherence.",
        }
    }
}

impl fmt::Display for AgentRole {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.label())
    }
}

impl FromStr for AgentRole {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim().to_lowercase().as_str() {
            "qa" => Ok(Self::QA),
            "backend_engineer" | "backend-engineer" => Ok(Self::BackendEngineer),
            "integration_master" | "integration-master" => Ok(Self::IntegrationMaster),
            other => Err(format!(
                "unknown agent role '{}'; expected qa|backend_engineer|integration_master",
                other
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_all_roles() {
        assert_eq!("qa".parse::<AgentRole>().unwrap(), AgentRole::QA);
        assert_eq!(
            "backend_engineer".parse::<AgentRole>().unwrap(),
            AgentRole::BackendEngineer
        );
        assert_eq!(
            "integration_master".parse::<AgentRole>().unwrap(),
            AgentRole::IntegrationMaster
        );
    }

    #[test]
    fn parse_case_insensitive() {
        assert_eq!("QA".parse::<AgentRole>().unwrap(), AgentRole::QA);
        assert_eq!(
            "Backend_Engineer".parse::<AgentRole>().unwrap(),
            AgentRole::BackendEngineer
        );
    }

    #[test]
    fn parse_hyphenated_variant() {
        assert_eq!(
            "backend-engineer".parse::<AgentRole>().unwrap(),
            AgentRole::BackendEngineer
        );
        assert_eq!(
            "integration-master".parse::<AgentRole>().unwrap(),
            AgentRole::IntegrationMaster
        );
    }

    #[test]
    fn parse_unknown_role_fails() {
        assert!("unknown".parse::<AgentRole>().is_err());
    }

    #[test]
    fn labels_are_nonempty() {
        for role in [AgentRole::QA, AgentRole::BackendEngineer, AgentRole::IntegrationMaster] {
            assert!(!role.label().is_empty());
            assert!(!role.system_prompt_fragment().is_empty());
        }
    }

    #[test]
    fn display_matches_label() {
        for role in [AgentRole::QA, AgentRole::BackendEngineer, AgentRole::IntegrationMaster] {
            assert_eq!(format!("{}", role), role.label());
        }
    }
}
