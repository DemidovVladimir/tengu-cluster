//! Plan types and topology helpers.

use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};

#[derive(Debug, Clone, Hash, PartialEq, Eq, Serialize, Deserialize)]
pub struct StepId(pub String);

impl StepId {
    pub fn new(s: impl Into<String>) -> Self { Self(s.into()) }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Step {
    pub id: StepId,
    pub agent: String,
    pub goal: String,
    #[serde(default)]
    pub depends_on: Vec<StepId>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Plan {
    pub steps: Vec<Step>,
}

#[derive(Debug, thiserror::Error)]
pub enum PlanError {
    #[error("plan has a cycle including step {0:?}")]
    Cycle(StepId),
    #[error("step {0:?} depends on unknown step {1:?}")]
    UnknownDependency(StepId, StepId),
    #[error("plan has no leaf (all steps have dependents)")]
    NoLeaf,
    #[error("plan has multiple leaves {0:?} — include a synthesizer step")]
    MultipleLeaves(Vec<StepId>),
    #[error("step {0:?} references unknown agent {1:?}")]
    UnknownAgent(StepId, String),
    #[error("duplicate step id {0:?}")]
    DuplicateId(StepId),
}

impl Plan {
    /// Return all steps whose dependencies are satisfied by `completed`
    /// and that are not themselves in `completed`.
    pub fn ready_steps(&self, completed: &HashSet<StepId>) -> Vec<&Step> {
        self.steps.iter().filter(|s| {
            !completed.contains(&s.id)
                && s.depends_on.iter().all(|d| completed.contains(d))
        }).collect()
    }

    /// Full topology validation: no duplicate IDs, no cycles, no unknown
    /// dependencies, exactly one leaf, every agent present in
    /// `known_agents`.
    pub fn validate(&self, known_agents: &[&str]) -> Result<(), PlanError> {
        // duplicate IDs
        let mut seen = HashSet::new();
        for s in &self.steps {
            if !seen.insert(s.id.clone()) {
                return Err(PlanError::DuplicateId(s.id.clone()));
            }
        }
        // agent resolution
        for s in &self.steps {
            if !known_agents.iter().any(|a| *a == s.agent) {
                return Err(PlanError::UnknownAgent(s.id.clone(), s.agent.clone()));
            }
        }
        // dependency resolution
        let id_set: HashSet<_> = self.steps.iter().map(|s| &s.id).collect();
        for s in &self.steps {
            for d in &s.depends_on {
                if !id_set.contains(d) {
                    return Err(PlanError::UnknownDependency(s.id.clone(), d.clone()));
                }
            }
        }
        // cycle detection (Kahn's algorithm)
        let mut in_deg: HashMap<StepId, usize> = self.steps.iter().map(|s| (s.id.clone(), s.depends_on.len())).collect();
        let mut queue: Vec<StepId> = in_deg.iter().filter(|(_, &d)| d == 0).map(|(k, _)| k.clone()).collect();
        let mut removed = 0;
        while let Some(id) = queue.pop() {
            removed += 1;
            for s in &self.steps {
                if s.depends_on.contains(&id) {
                    if let Some(d) = in_deg.get_mut(&s.id) {
                        *d -= 1;
                        if *d == 0 { queue.push(s.id.clone()); }
                    }
                }
            }
        }
        if removed != self.steps.len() {
            let cycle_id = self.steps.iter().find(|s| in_deg[&s.id] > 0).map(|s| s.id.clone()).unwrap();
            return Err(PlanError::Cycle(cycle_id));
        }
        // single leaf
        let has_dep_on: HashSet<&StepId> = self.steps.iter().flat_map(|s| &s.depends_on).collect();
        let leaves: Vec<&Step> = self.steps.iter().filter(|s| !has_dep_on.contains(&s.id)).collect();
        match leaves.len() {
            0 => Err(PlanError::NoLeaf),
            1 => Ok(()),
            _ => Err(PlanError::MultipleLeaves(leaves.into_iter().map(|s| s.id.clone()).collect())),
        }
    }

    pub fn single_leaf(&self) -> Option<&Step> {
        let has_dep_on: HashSet<&StepId> = self.steps.iter().flat_map(|s| &s.depends_on).collect();
        let mut leaves = self.steps.iter().filter(|s| !has_dep_on.contains(&s.id));
        let first = leaves.next()?;
        if leaves.next().is_some() { None } else { Some(first) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn step(id: &str, agent: &str, deps: &[&str]) -> Step {
        Step {
            id: StepId::new(id),
            agent: agent.into(),
            goal: format!("goal-{}", id),
            depends_on: deps.iter().map(|d| StepId::new(*d)).collect(),
        }
    }

    #[test]
    fn ready_steps_respects_dependencies() {
        let plan = Plan { steps: vec![step("a", "x", &[]), step("b", "x", &["a"])] };
        let ready = plan.ready_steps(&HashSet::new());
        assert_eq!(ready.len(), 1);
        assert_eq!(ready[0].id, StepId::new("a"));
    }

    #[test]
    fn validate_detects_cycle() {
        let plan = Plan { steps: vec![step("a", "x", &["b"]), step("b", "x", &["a"])] };
        assert!(matches!(plan.validate(&["x"]), Err(PlanError::Cycle(_))));
    }

    #[test]
    fn validate_detects_unknown_dep() {
        let plan = Plan { steps: vec![step("a", "x", &["ghost"])] };
        assert!(matches!(plan.validate(&["x"]), Err(PlanError::UnknownDependency(_, _))));
    }

    #[test]
    fn validate_rejects_unknown_agent() {
        let plan = Plan { steps: vec![step("a", "ghost-agent", &[])] };
        assert!(matches!(plan.validate(&["x"]), Err(PlanError::UnknownAgent(_, _))));
    }

    #[test]
    fn validate_rejects_multiple_leaves() {
        let plan = Plan { steps: vec![step("a", "x", &[]), step("b", "x", &[])] };
        assert!(matches!(plan.validate(&["x"]), Err(PlanError::MultipleLeaves(_))));
    }

    #[test]
    fn validate_accepts_single_leaf() {
        let plan = Plan { steps: vec![
            step("a", "x", &[]),
            step("b", "x", &[]),
            step("c", "x", &["a", "b"]),
        ] };
        assert!(plan.validate(&["x"]).is_ok());
    }

    #[test]
    fn single_leaf_returns_leaf() {
        let plan = Plan { steps: vec![step("a", "x", &[]), step("b", "x", &["a"])] };
        assert_eq!(plan.single_leaf().map(|s| s.id.clone()), Some(StepId::new("b")));
    }
}
