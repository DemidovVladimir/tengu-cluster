//! In-memory fleet agent registry for orchestration.

use crate::domain::agent_role::AgentRole;
use tengu_core::types::ToolDef;

/// Runtime status of an agent in the fleet.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FleetAgentStatus {
    Idle,
    Busy,
    Failed,
}

/// A registered fleet agent with its runtime state.
#[derive(Debug, Clone)]
pub(crate) struct FleetAgent {
    pub agent_id: String,
    pub role: AgentRole,
    pub engine_id: String,
    pub system_prompt: String,
    pub tools: Vec<ToolDef>,
    pub status: FleetAgentStatus,
    pub current_task_id: Option<String>,
}

/// Pure in-memory fleet state management — no I/O.
pub(crate) struct FleetRuntimeService {
    agents: Vec<FleetAgent>,
}

impl FleetRuntimeService {
    pub fn new() -> Self {
        Self { agents: Vec::new() }
    }

    /// Register an agent in the fleet.
    pub fn register_agent(
        &mut self,
        agent_id: String,
        role: AgentRole,
        engine_id: String,
        system_prompt: String,
        tools: Vec<ToolDef>,
    ) {
        self.agents.push(FleetAgent {
            agent_id,
            role,
            engine_id,
            system_prompt,
            tools,
            status: FleetAgentStatus::Idle,
            current_task_id: None,
        });
    }

    /// Find the first idle agent matching the requested role.
    pub fn find_idle_agent_for_role(&self, role: AgentRole) -> Option<&FleetAgent> {
        self.agents
            .iter()
            .find(|a| a.role == role && a.status == FleetAgentStatus::Idle)
    }

    /// Mark an agent as busy with a given task.
    pub fn mark_busy(&mut self, agent_id: &str, task_id: &str) {
        if let Some(agent) = self.agents.iter_mut().find(|a| a.agent_id == agent_id) {
            agent.status = FleetAgentStatus::Busy;
            agent.current_task_id = Some(task_id.to_string());
        }
    }

    /// Mark an agent as idle and clear its current task.
    pub fn mark_idle(&mut self, agent_id: &str) {
        if let Some(agent) = self.agents.iter_mut().find(|a| a.agent_id == agent_id) {
            agent.status = FleetAgentStatus::Idle;
            agent.current_task_id = None;
        }
    }

    /// Mark an agent as failed.
    pub fn mark_failed(&mut self, agent_id: &str) {
        if let Some(agent) = self.agents.iter_mut().find(|a| a.agent_id == agent_id) {
            agent.status = FleetAgentStatus::Failed;
        }
    }

    /// List all registered agents.
    pub fn agents(&self) -> &[FleetAgent] {
        &self.agents
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn register_and_find_idle_agent() {
        let mut fleet = FleetRuntimeService::new();
        fleet.register_agent("a-1".into(), "qa".parse::<AgentRole>().unwrap(), "ollama".into(), String::new(), vec![]);
        fleet.register_agent("a-2".into(), "backend_engineer".parse::<AgentRole>().unwrap(), "anthropic".into(), String::new(), vec![]);

        let found = fleet.find_idle_agent_for_role("qa".parse::<AgentRole>().unwrap());
        assert_eq!(found.unwrap().agent_id, "a-1");
    }

    #[test]
    fn busy_agent_not_found_as_idle() {
        let mut fleet = FleetRuntimeService::new();
        fleet.register_agent("a-1".into(), "qa".parse::<AgentRole>().unwrap(), "ollama".into(), String::new(), vec![]);
        fleet.mark_busy("a-1", "t-1");
        assert!(fleet.find_idle_agent_for_role("qa".parse::<AgentRole>().unwrap()).is_none());
    }

    #[test]
    fn mark_idle_resets_agent() {
        let mut fleet = FleetRuntimeService::new();
        fleet.register_agent("a-1".into(), "qa".parse::<AgentRole>().unwrap(), "ollama".into(), String::new(), vec![]);
        fleet.mark_busy("a-1", "t-1");
        fleet.mark_idle("a-1");

        let agent = fleet.find_idle_agent_for_role("qa".parse::<AgentRole>().unwrap());
        assert!(agent.is_some());
        assert!(agent.unwrap().current_task_id.is_none());
    }

    #[test]
    fn agents_list_reflects_registrations() {
        let mut fleet = FleetRuntimeService::new();
        fleet.register_agent("a-1".into(), "qa".parse::<AgentRole>().unwrap(), "ollama".into(), String::new(), vec![]);
        fleet.register_agent("a-2".into(), "integration_master".parse::<AgentRole>().unwrap(), "openai".into(), String::new(), vec![]);
        assert_eq!(fleet.agents().len(), 2);
    }
}
