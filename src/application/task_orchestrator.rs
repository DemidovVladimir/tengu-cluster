//! Application service for task lifecycle orchestration.

use crate::application::ports::TaskStorePort;
use crate::domain::agent_role::AgentRole;
use crate::domain::task::{Task, TaskResult, TaskStatus};
use anyhow::Result;

pub(crate) struct TaskOrchestratorService<'a> {
    pub store: &'a dyn TaskStorePort,
    pub max_retries: u32,
}

impl<'a> TaskOrchestratorService<'a> {
    /// Create a new pending task and persist it.
    pub fn create_task(&self, id: String, description: String, role: AgentRole) -> Result<Task> {
        let task = Task::new(id, description, role, self.max_retries);
        self.store.save_task(&task)?;
        Ok(task)
    }

    /// Assign a task to an agent.
    pub fn assign_task(&self, task_id: &str, agent_id: &str) -> Result<()> {
        let mut task = self
            .store
            .load_task(task_id)?
            .ok_or_else(|| anyhow::anyhow!("task not found: {}", task_id))?;
        task.assign_to(agent_id);
        task.transition_to(TaskStatus::InProgress)
            .map_err(|e| anyhow::anyhow!(e))?;
        self.store.save_task(&task)?;
        Ok(())
    }

    /// Mark a task completed.
    pub fn complete_task(&self, task_id: &str, result: TaskResult) -> Result<()> {
        let mut task = self
            .store
            .load_task(task_id)?
            .ok_or_else(|| anyhow::anyhow!("task not found: {}", task_id))?;
        task.transition_to(TaskStatus::Completed)
            .map_err(|e| anyhow::anyhow!(e))?;
        task.result = Some(result);
        self.store.save_task(&task)?;
        Ok(())
    }

}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::task::Task;
    use std::collections::HashMap;
    use std::sync::RwLock;

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
        fn load_all_tasks(&self) -> Result<Vec<Task>> {
            Ok(self.tasks.read().unwrap().values().cloned().collect())
        }
    }

    #[test]
    fn create_and_assign_task() {
        let store = MockTaskStore::new();
        let svc = TaskOrchestratorService {
            store: &store,
            max_retries: 2,
        };

        let task = svc
            .create_task(
                "t-1".into(),
                "test".into(),
                "qa".parse::<AgentRole>().unwrap(),
            )
            .unwrap();
        assert_eq!(task.status, TaskStatus::Pending);

        svc.assign_task("t-1", "agent-a").unwrap();
        let loaded = store.load_task("t-1").unwrap().unwrap();
        assert_eq!(loaded.status, TaskStatus::InProgress);
        assert_eq!(loaded.assigned_agent.as_deref(), Some("agent-a"));
    }

    #[test]
    fn complete_task_persists_result() {
        let store = MockTaskStore::new();
        let svc = TaskOrchestratorService {
            store: &store,
            max_retries: 2,
        };

        svc.create_task(
            "t-2".into(),
            "test".into(),
            "backend_engineer".parse::<AgentRole>().unwrap(),
        )
        .unwrap();
        svc.assign_task("t-2", "agent-b").unwrap();
        svc.complete_task(
            "t-2",
            TaskResult {
                output: "done".into(),
            },
        )
        .unwrap();

        let loaded = store.load_task("t-2").unwrap().unwrap();
        assert_eq!(loaded.status, TaskStatus::Completed);
        assert_eq!(loaded.result.unwrap().output, "done");
    }
}
