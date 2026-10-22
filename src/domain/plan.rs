//! Plan types and topology helpers.

use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};

#[derive(Debug, Clone, Hash, PartialEq, Eq, Serialize, Deserialize)]
pub struct StepId(pub String);

impl StepId {
    pub fn new(s: impl Into<String>) -> Self {
        Self(s.into())
    }
}

/// Phase 6.7 (C→B B-half) — optional override that lets the planner LLM
/// compose a transient agent on the fly when no listed agent fits the
/// request.
///
/// `base_agent` is the closest-matching `[agents.<name>]` block to use as a
/// starting point (it must exist in the active config). `skills` and
/// `tools`, when present, REPLACE the corresponding fields on that base
/// agent for THIS run only. The override is not persisted; the config on
/// disk is unchanged.
///
/// Use case (the C → B fallback): user asks a question no listed agent fits.
/// Planner emits a Direct asking the user to confirm the closest match
/// or describe what they need. On the next turn, when the user confirms,
/// the planner emits a Plan whose single step has `compose: Some(...)`
/// referencing the closest base agent + the most-relevant skills/tools
/// from the rejected RAG roster.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentCompose {
    /// Name of the base `[agents.<base_agent>]` block.
    pub base_agent: String,
    /// Skills to attach for this run. Replaces the base spec's `skills`.
    /// Must reference skills the three-tier scanner can find on disk.
    #[serde(default)]
    pub skills: Vec<String>,
    /// Tools to attach for this run. Replaces the base spec's `tools`.
    /// Must reference tools that exist in the registry / built-in set.
    #[serde(default)]
    pub tools: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Step {
    pub id: StepId,
    pub agent: String,
    pub goal: String,
    #[serde(default)]
    pub depends_on: Vec<StepId>,
    /// Phase 6.7 (C→B B-half) — if set, the runner builds a transient
    /// agent from `compose.base_agent` overriding `skills`/`tools`,
    /// rather than using `[agents.<step.agent>]` verbatim. `step.agent`
    /// becomes a label for the composed agent (used in events / logs);
    /// the actual base lives in `compose.base_agent`. None for normal
    /// fixed-roster routing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compose: Option<AgentCompose>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Plan {
    pub steps: Vec<Step>,
}

/// Why a plan cannot run as written (`Plan::validate`). `replan::drive`
/// checks every plan the planner returns before any step runs, and sends a
/// broken one straight back to the planner with this text.
#[derive(Debug, thiserror::Error)]
pub enum PlanError {
    #[error("the plan has no steps")]
    Empty,
    #[error("duplicate step id {0:?}")]
    DuplicateId(StepId),
    #[error(
        "step {0:?} names agent {1:?}, which is not a routable agent \
         (an `[agents.<name>]` block with a `description`); routable agents: {2:?}"
    )]
    UnknownAgent(StepId, String, Vec<String>),
    #[error("step {0:?} depends on unknown step {1:?}")]
    UnknownDependency(StepId, StepId),
    #[error("the plan has a cycle including step {0:?}")]
    Cycle(StepId),
}

impl PlanError {
    /// The step the error is about (`None`: the plan as a whole).
    pub fn step(&self) -> Option<&StepId> {
        match self {
            PlanError::Empty => None,
            PlanError::DuplicateId(s)
            | PlanError::UnknownAgent(s, ..)
            | PlanError::UnknownDependency(s, _)
            | PlanError::Cycle(s) => Some(s),
        }
    }
}

impl Plan {
    /// Return all steps whose dependencies are satisfied by `completed`
    /// and that are not themselves in `completed`.
    pub fn ready_steps(&self, completed: &HashSet<StepId>) -> Vec<&Step> {
        self.steps
            .iter()
            .filter(|s| {
                !completed.contains(&s.id) && s.depends_on.iter().all(|d| completed.contains(d))
            })
            .collect()
    }

    /// Can this plan run: at least one step, no duplicate ids, every agent
    /// known, no unknown dependency, no cycle. A step's agent is
    /// `compose.base_agent` when it composes one, else `agent`.
    /// `known_agents = None` skips the agent check. Several leaves are fine:
    /// the executor joins their outputs.
    pub fn validate(&self, known_agents: Option<&[String]>) -> Result<(), PlanError> {
        if self.steps.is_empty() {
            return Err(PlanError::Empty);
        }
        // duplicate IDs
        let mut seen = HashSet::new();
        for s in &self.steps {
            if !seen.insert(s.id.clone()) {
                return Err(PlanError::DuplicateId(s.id.clone()));
            }
        }
        // agent resolution
        if let Some(known) = known_agents {
            for s in &self.steps {
                let name = s
                    .compose
                    .as_ref()
                    .map_or(s.agent.as_str(), |c| c.base_agent.as_str());
                if !known.iter().any(|a| a == name) {
                    return Err(PlanError::UnknownAgent(
                        s.id.clone(),
                        name.to_string(),
                        known.to_vec(),
                    ));
                }
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
        let mut in_deg: HashMap<StepId, usize> = self
            .steps
            .iter()
            .map(|s| (s.id.clone(), s.depends_on.len()))
            .collect();
        let mut queue: Vec<StepId> = in_deg
            .iter()
            .filter(|(_, &d)| d == 0)
            .map(|(k, _)| k.clone())
            .collect();
        let mut removed = 0;
        while let Some(id) = queue.pop() {
            removed += 1;
            for s in &self.steps {
                if s.depends_on.contains(&id) {
                    if let Some(d) = in_deg.get_mut(&s.id) {
                        *d -= 1;
                        if *d == 0 {
                            queue.push(s.id.clone());
                        }
                    }
                }
            }
        }
        if removed != self.steps.len() {
            let cycle_id = self
                .steps
                .iter()
                .find(|s| in_deg[&s.id] > 0)
                .map(|s| s.id.clone())
                .unwrap();
            return Err(PlanError::Cycle(cycle_id));
        }
        Ok(())
    }

    /// Steps no other step depends on, in plan order: the outputs that make
    /// the reply (one leaf, or parallel leaves with no join step).
    pub fn leaves(&self) -> Vec<&Step> {
        let has_dep_on: HashSet<&StepId> = self.steps.iter().flat_map(|s| &s.depends_on).collect();
        self.steps
            .iter()
            .filter(|s| !has_dep_on.contains(&s.id))
            .collect()
    }

    #[cfg(test)]
    pub fn single_leaf(&self) -> Option<&Step> {
        let has_dep_on: HashSet<&StepId> = self.steps.iter().flat_map(|s| &s.depends_on).collect();
        let mut leaves = self.steps.iter().filter(|s| !has_dep_on.contains(&s.id));
        let first = leaves.next()?;
        if leaves.next().is_some() {
            None
        } else {
            Some(first)
        }
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
            compose: None,
        }
    }

    fn known() -> Vec<String> {
        vec!["x".to_string()]
    }

    #[test]
    fn ready_steps_respects_dependencies() {
        let plan = Plan {
            steps: vec![step("a", "x", &[]), step("b", "x", &["a"])],
        };
        let ready = plan.ready_steps(&HashSet::new());
        assert_eq!(ready.len(), 1);
        assert_eq!(ready[0].id, StepId::new("a"));
    }

    #[test]
    fn validate_detects_cycle() {
        let plan = Plan {
            steps: vec![step("a", "x", &["b"]), step("b", "x", &["a"])],
        };
        assert!(matches!(
            plan.validate(Some(&known())),
            Err(PlanError::Cycle(_))
        ));
    }

    #[test]
    fn validate_detects_unknown_dep() {
        let plan = Plan {
            steps: vec![step("a", "x", &["ghost"])],
        };
        assert!(matches!(
            plan.validate(Some(&known())),
            Err(PlanError::UnknownDependency(_, _))
        ));
    }

    #[test]
    fn validate_rejects_unknown_agent() {
        let plan = Plan {
            steps: vec![step("a", "ghost-agent", &[])],
        };
        assert!(matches!(
            plan.validate(Some(&known())),
            Err(PlanError::UnknownAgent(..))
        ));
    }

    /// Parallel leaves are a valid plan: the executor joins their outputs.
    #[test]
    fn validate_accepts_parallel_leaves() {
        let plan = Plan {
            steps: vec![step("a", "x", &[]), step("b", "x", &[])],
        };
        assert!(plan.validate(Some(&known())).is_ok());
    }

    #[test]
    fn validate_rejects_empty_and_duplicate_ids() {
        assert!(matches!(
            Plan { steps: vec![] }.validate(None),
            Err(PlanError::Empty)
        ));
        let dup = Plan {
            steps: vec![step("a", "x", &[]), step("a", "x", &[])],
        };
        assert!(matches!(dup.validate(None), Err(PlanError::DuplicateId(_))));
    }

    /// A composed step is checked by its base agent (its `agent` is only a
    /// label); `None` skips the agent check.
    #[test]
    fn validate_resolves_compose_base_agent() {
        let mut composed = step("a", "label-only", &[]);
        composed.compose = Some(AgentCompose {
            base_agent: "x".into(),
            skills: vec![],
            tools: vec![],
        });
        let plan = Plan {
            steps: vec![composed],
        };
        assert!(plan.validate(Some(&known())).is_ok());
        let unknown_base = Plan {
            steps: vec![step("a", "ghost", &[])],
        };
        let err = unknown_base.validate(Some(&known())).unwrap_err();
        assert_eq!(err.step(), Some(&StepId::new("a")));
        assert!(
            err.to_string().contains("routable agents: [\"x\"]"),
            "{err}"
        );
        assert!(unknown_base.validate(None).is_ok());
    }

    #[test]
    fn validate_accepts_single_leaf() {
        let plan = Plan {
            steps: vec![
                step("a", "x", &[]),
                step("b", "x", &[]),
                step("c", "x", &["a", "b"]),
            ],
        };
        assert!(plan.validate(Some(&known())).is_ok());
    }

    #[test]
    fn single_leaf_returns_leaf() {
        let plan = Plan {
            steps: vec![step("a", "x", &[]), step("b", "x", &["a"])],
        };
        assert_eq!(
            plan.single_leaf().map(|s| s.id.clone()),
            Some(StepId::new("b"))
        );
    }
}
