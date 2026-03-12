//! In-memory task store adapter implementing `TaskStorePort`.

use crate::application::ports::TaskStorePort;
use crate::domain::task::Task;
use anyhow::Result;
use std::collections::HashMap;
use std::sync::RwLock;

/// KISS in-memory task store for single-process fleet orchestration.
pub(crate) struct InMemoryTaskStore {
    tasks: RwLock<HashMap<String, Task>>,
}

impl InMemoryTaskStore {
    pub fn new() -> Self {
        Self {
            tasks: RwLock::new(HashMap::new()),
        }
    }
}

impl TaskStorePort for InMemoryTaskStore {
    fn save_task(&self, task: &Task) -> Result<()> {
        self.tasks
            .write()
            .map_err(|e| anyhow::anyhow!("lock poisoned: {}", e))?
            .insert(task.id.0.clone(), task.clone());
        Ok(())
    }

    fn load_task(&self, task_id: &str) -> Result<Option<Task>> {
        Ok(self
            .tasks
            .read()
            .map_err(|e| anyhow::anyhow!("lock poisoned: {}", e))?
            .get(task_id)
            .cloned())
    }

    fn load_all_tasks(&self) -> Result<Vec<Task>> {
        Ok(self
            .tasks
            .read()
            .map_err(|e| anyhow::anyhow!("lock poisoned: {}", e))?
            .values()
            .cloned()
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::agent_role::AgentRole;

    #[test]
    fn save_and_load_task() {
        let store = InMemoryTaskStore::new();
        let task = Task::new(
            "t-1".into(),
            "test".into(),
            "qa".parse::<AgentRole>().unwrap(),
            2,
        );
        store.save_task(&task).unwrap();

        let loaded = store.load_task("t-1").unwrap();
        assert!(loaded.is_some());
        assert_eq!(loaded.unwrap().id.0, "t-1");
    }

    #[test]
    fn load_nonexistent_returns_none() {
        let store = InMemoryTaskStore::new();
        assert!(store.load_task("nope").unwrap().is_none());
    }

    #[test]
    fn load_all_returns_everything() {
        let store = InMemoryTaskStore::new();
        store
            .save_task(&Task::new(
                "t-1".into(),
                "a".into(),
                "qa".parse::<AgentRole>().unwrap(),
                1,
            ))
            .unwrap();
        store
            .save_task(&Task::new(
                "t-2".into(),
                "b".into(),
                "backend_engineer".parse::<AgentRole>().unwrap(),
                1,
            ))
            .unwrap();
        assert_eq!(store.load_all_tasks().unwrap().len(), 2);
    }
}
