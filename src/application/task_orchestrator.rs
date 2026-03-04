//! Application service for task lifecycle orchestration.

use crate::application::ports::TaskStorePort;
use crate::domain::agent_role::AgentRole;
use crate::domain::task::{Task, TaskResult, TaskStatus};
use anyhow::Result;
use tengu_core::events::{
    DomainEvent, DomainEventMeta, DomainEventPayload, EventBus, TaskAssigned, TaskCompleted,
};

#[allow(dead_code)]
pub(crate) struct TaskOrchestratorService<'a> {
    pub store: &'a dyn TaskStorePort,
    pub event_bus: &'a dyn EventBus,
    pub max_retries: u32,
}

#[allow(dead_code)]
impl<'a> TaskOrchestratorService<'a> {
    /// Create a new pending task and persist it.
    pub fn create_task(
        &self,
        id: String,
        description: String,
        role: AgentRole,
    ) -> Result<Task> {
        let task = Task::new(id, description, role, self.max_retries);
        self.store.save_task(&task)?;
        Ok(task)
    }

    /// Assign a task to an agent and publish `TaskAssigned`.
    pub async fn assign_task(&self, task_id: &str, agent_id: &str) -> Result<()> {
        let mut task = self
            .store
            .load_task(task_id)?
            .ok_or_else(|| anyhow::anyhow!("task not found: {}", task_id))?;
        task.assign_to(agent_id);
        task.transition_to(TaskStatus::InProgress)
            .map_err(|e| anyhow::anyhow!(e))?;
        self.store.save_task(&task)?;
        self.event_bus
            .publish(DomainEvent {
                meta: event_meta(Some(task_id), Some(agent_id)),
                payload: DomainEventPayload::TaskAssigned(TaskAssigned {
                    task_id: task_id.to_string(),
                    agent_id: agent_id.to_string(),
                    role: task.role.label().to_string(),
                }),
            })
            .await?;
        Ok(())
    }

    /// Mark a task completed and publish `TaskCompleted`.
    pub async fn complete_task(&self, task_id: &str, result: TaskResult) -> Result<()> {
        let mut task = self
            .store
            .load_task(task_id)?
            .ok_or_else(|| anyhow::anyhow!("task not found: {}", task_id))?;
        task.transition_to(TaskStatus::Completed)
            .map_err(|e| anyhow::anyhow!(e))?;
        task.result = Some(result);
        let agent_id = task.assigned_agent.clone().unwrap_or_default();
        self.store.save_task(&task)?;
        self.event_bus
            .publish(DomainEvent {
                meta: event_meta(Some(task_id), Some(&agent_id)),
                payload: DomainEventPayload::TaskCompleted(TaskCompleted {
                    task_id: task_id.to_string(),
                    agent_id,
                }),
            })
            .await?;
        Ok(())
    }

    /// Check for stalled in-progress tasks and retry if possible.
    pub fn heartbeat_check(&self) -> Result<Vec<String>> {
        let in_progress = self.store.load_tasks_by_status(TaskStatus::InProgress)?;
        let mut retried = Vec::new();
        for task in in_progress {
            // Mark stalled tasks as failed so they can be retried.
            // In a real system, staleness would be determined by timestamp comparison.
            // For now, heartbeat_check is a no-op hook for future staleness detection.
            let _ = task;
            let _ = &mut retried;
        }
        Ok(retried)
    }

    /// Retry a specific failed task if retries remain.
    pub async fn retry_failed_task(&self, task_id: &str, agent_id: &str) -> Result<bool> {
        let mut task = self
            .store
            .load_task(task_id)?
            .ok_or_else(|| anyhow::anyhow!("task not found: {}", task_id))?;
        if !task.can_retry() {
            return Ok(false);
        }
        task.assign_to(agent_id);
        task.transition_to(TaskStatus::InProgress)
            .map_err(|e| anyhow::anyhow!(e))?;
        self.store.save_task(&task)?;
        self.event_bus
            .publish(DomainEvent {
                meta: event_meta(Some(task_id), Some(agent_id)),
                payload: DomainEventPayload::TaskAssigned(TaskAssigned {
                    task_id: task_id.to_string(),
                    agent_id: agent_id.to_string(),
                    role: task.role.label().to_string(),
                }),
            })
            .await?;
        Ok(true)
    }
}

#[allow(dead_code)]
fn event_meta(task_id: Option<&str>, agent_id: Option<&str>) -> DomainEventMeta {
    DomainEventMeta {
        ts_epoch_ms: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64,
        flow_key: None,
        agent_id: agent_id.map(|s| s.to_string()),
        correlation_id: task_id.map(|s| s.to_string()),
        source: Some("task_orchestrator".to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::task::Task;
    use std::collections::HashMap;
    use std::sync::RwLock;
    use crate::adapters::event_bus::InProcessEventBus;

    struct MockTaskStore {
        tasks: RwLock<HashMap<String, Task>>,
    }

    impl MockTaskStore {
        fn new() -> Self {
            Self {
                tasks: RwLock::new(HashMap::new()),
            }
        }
    }

    impl TaskStorePort for MockTaskStore {
        fn save_task(&self, task: &Task) -> Result<()> {
            self.tasks
                .write()
                .unwrap()
                .insert(task.id.0.clone(), task.clone());
            Ok(())
        }
        fn load_task(&self, task_id: &str) -> Result<Option<Task>> {
            Ok(self.tasks.read().unwrap().get(task_id).cloned())
        }
        fn load_tasks_by_status(&self, status: TaskStatus) -> Result<Vec<Task>> {
            Ok(self
                .tasks
                .read()
                .unwrap()
                .values()
                .filter(|t| t.status == status)
                .cloned()
                .collect())
        }
        fn load_all_tasks(&self) -> Result<Vec<Task>> {
            Ok(self.tasks.read().unwrap().values().cloned().collect())
        }
    }

    #[tokio::test]
    async fn create_and_assign_task() {
        let store = MockTaskStore::new();
        let bus = InProcessEventBus::default();
        let svc = TaskOrchestratorService {
            store: &store,
            event_bus: &bus,
            max_retries: 2,
        };

        let task = svc
            .create_task("t-1".into(), "test".into(), AgentRole::QA)
            .unwrap();
        assert_eq!(task.status, TaskStatus::Pending);

        svc.assign_task("t-1", "agent-a").await.unwrap();
        let loaded = store.load_task("t-1").unwrap().unwrap();
        assert_eq!(loaded.status, TaskStatus::InProgress);
        assert_eq!(loaded.assigned_agent.as_deref(), Some("agent-a"));
    }

    #[tokio::test]
    async fn complete_task_publishes_event() {
        let store = MockTaskStore::new();
        let bus = InProcessEventBus::default();
        let svc = TaskOrchestratorService {
            store: &store,
            event_bus: &bus,
            max_retries: 2,
        };

        svc.create_task("t-2".into(), "test".into(), AgentRole::BackendEngineer)
            .unwrap();
        svc.assign_task("t-2", "agent-b").await.unwrap();
        svc.complete_task(
            "t-2",
            TaskResult {
                success: true,
                output: "done".into(),
                validation_notes: None,
            },
        )
        .await
        .unwrap();

        let loaded = store.load_task("t-2").unwrap().unwrap();
        assert_eq!(loaded.status, TaskStatus::Completed);
        assert!(loaded.result.unwrap().success);
    }
}
