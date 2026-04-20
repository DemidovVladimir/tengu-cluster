//! Placeholder — implemented in Task 4.N.

// Stubs for re-exports in mod.rs. These are replaced in Task 4.2.
#[derive(Debug, Clone, Hash, PartialEq, Eq)]
pub struct StepId(pub String);

#[derive(Debug, Clone)]
pub struct Step {
    pub id: StepId,
    pub agent: String,
    pub goal: String,
    pub depends_on: Vec<StepId>,
}

#[derive(Debug, Clone, Default)]
pub struct Plan {
    pub steps: Vec<Step>,
}
