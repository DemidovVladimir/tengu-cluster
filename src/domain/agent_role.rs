//! Agent role definitions for fleet orchestration.
//!
//! Roles are fully dynamic — defined as arbitrary strings in config.
//! The code imposes no restrictions on role names; any non-empty string
//! is valid. Use `identity.instructions` in config for role-specific
//! system prompt guidance.

use std::fmt;
use std::str::FromStr;

/// The functional role assigned to an agent in the fleet.
///
/// Wraps an arbitrary string from config. Role names are normalized
/// to lowercase with hyphens replaced by underscores.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct AgentRole(String);

impl AgentRole {
    /// Human-readable label for display/logging.
    pub fn label(&self) -> &str {
        &self.0
    }

    /// The canonical key used for role-based routing (lowercase, underscored).
    pub fn key(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for AgentRole {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl FromStr for AgentRole {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let normalized = s.trim().to_lowercase().replace('-', "_");
        if normalized.is_empty() {
            return Err("agent role cannot be empty".to_string());
        }
        Ok(Self(normalized))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_role() {
        let role = "qa".parse::<AgentRole>().unwrap();
        assert_eq!(role.label(), "qa");
        assert_eq!(role.key(), "qa");
    }

    #[test]
    fn parse_case_insensitive() {
        let role = "Backend_Engineer".parse::<AgentRole>().unwrap();
        assert_eq!(role.key(), "backend_engineer");
    }

    #[test]
    fn parse_hyphenated() {
        let role = "cms-guide".parse::<AgentRole>().unwrap();
        assert_eq!(role.key(), "cms_guide");
    }

    #[test]
    fn parse_empty_fails() {
        assert!("".parse::<AgentRole>().is_err());
        assert!("  ".parse::<AgentRole>().is_err());
    }

    #[test]
    fn display_matches_label() {
        let role = "frontend_engineer".parse::<AgentRole>().unwrap();
        assert_eq!(format!("{}", role), "frontend_engineer");
    }
}
