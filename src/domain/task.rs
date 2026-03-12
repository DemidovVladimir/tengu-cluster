//! Task model for fleet orchestration — pure domain logic, no I/O.
//!
//! Used by the `orchestrate` subcommand for task lifecycle management.

use crate::domain::agent_role::AgentRole;

/// Unique task identifier.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct TaskId(pub String);

/// Lifecycle status of a task.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TaskStatus {
    Pending,
    InProgress,
    Completed,
    Failed,
}

/// Result of a completed task execution.
#[derive(Debug, Clone)]
pub(crate) struct TaskResult {
    #[allow(dead_code)] // stored for debugging/display, read via load_all_tasks
    pub output: String,
}

/// A unit of work assigned to a fleet agent.
#[derive(Debug, Clone)]
pub(crate) struct Task {
    pub id: TaskId,
    pub description: String,
    pub assigned_agent: Option<String>,
    pub status: TaskStatus,
    pub role: AgentRole,
    pub retry_count: u32,
    pub max_retries: u32,
    pub result: Option<TaskResult>,
    pub updated_at: u64,
}

impl Task {
    /// Create a new pending task.
    pub fn new(id: String, description: String, role: AgentRole, max_retries: u32) -> Self {
        Self {
            id: TaskId(id),
            description,
            assigned_agent: None,
            status: TaskStatus::Pending,
            role,
            retry_count: 0,
            max_retries,
            result: None,
            updated_at: now_epoch_ms(),
        }
    }

    /// Whether the task can be retried after failure.
    pub fn can_retry(&self) -> bool {
        self.status == TaskStatus::Failed && self.retry_count < self.max_retries
    }

    /// Attempt a state transition. Returns `Err` with reason if illegal.
    pub fn transition_to(&mut self, target: TaskStatus) -> Result<(), &'static str> {
        let allowed = match (self.status, target) {
            (TaskStatus::Pending, TaskStatus::InProgress) => true,
            (TaskStatus::InProgress, TaskStatus::Completed) => true,
            (TaskStatus::InProgress, TaskStatus::Failed) => true,
            (TaskStatus::Failed, TaskStatus::InProgress) if self.can_retry() => true,
            _ => false,
        };
        if !allowed {
            return Err("illegal task state transition");
        }
        if self.status == TaskStatus::Failed && target == TaskStatus::InProgress {
            self.retry_count += 1;
        }
        self.status = target;
        self.updated_at = now_epoch_ms();
        Ok(())
    }

    /// Assign the task to a specific agent.
    pub fn assign_to(&mut self, agent_id: &str) {
        self.assigned_agent = Some(agent_id.to_string());
        self.updated_at = now_epoch_ms();
    }
}

fn now_epoch_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_task() -> Task {
        Task::new(
            "t-1".to_string(),
            "test task".to_string(),
            "qa".parse::<AgentRole>().unwrap(),
            2,
        )
    }

    #[test]
    fn new_task_is_pending() {
        let task = make_task();
        assert_eq!(task.status, TaskStatus::Pending);
        assert!(task.assigned_agent.is_none());
        assert_eq!(task.retry_count, 0);
    }

    #[test]
    fn transition_pending_to_in_progress() {
        let mut task = make_task();
        assert!(task.transition_to(TaskStatus::InProgress).is_ok());
        assert_eq!(task.status, TaskStatus::InProgress);
    }

    #[test]
    fn transition_in_progress_to_completed() {
        let mut task = make_task();
        task.transition_to(TaskStatus::InProgress).unwrap();
        assert!(task.transition_to(TaskStatus::Completed).is_ok());
        assert_eq!(task.status, TaskStatus::Completed);
    }

    #[test]
    fn transition_in_progress_to_failed() {
        let mut task = make_task();
        task.transition_to(TaskStatus::InProgress).unwrap();
        assert!(task.transition_to(TaskStatus::Failed).is_ok());
        assert_eq!(task.status, TaskStatus::Failed);
    }

    #[test]
    fn can_retry_after_failure() {
        let mut task = make_task();
        task.transition_to(TaskStatus::InProgress).unwrap();
        task.transition_to(TaskStatus::Failed).unwrap();
        assert!(task.can_retry());
        assert!(task.transition_to(TaskStatus::InProgress).is_ok());
        assert_eq!(task.retry_count, 1);
    }

    #[test]
    fn retry_exhaustion() {
        let mut task = Task::new(
            "t-2".to_string(),
            "test".to_string(),
            "qa".parse::<AgentRole>().unwrap(),
            1,
        );
        task.transition_to(TaskStatus::InProgress).unwrap();
        task.transition_to(TaskStatus::Failed).unwrap();
        // First retry allowed
        assert!(task.can_retry());
        task.transition_to(TaskStatus::InProgress).unwrap();
        task.transition_to(TaskStatus::Failed).unwrap();
        // Second retry blocked
        assert!(!task.can_retry());
        assert!(task.transition_to(TaskStatus::InProgress).is_err());
    }

    #[test]
    fn illegal_transition_rejected() {
        let mut task = make_task();
        assert!(task.transition_to(TaskStatus::Completed).is_err());
        assert!(task.transition_to(TaskStatus::Failed).is_err());
    }

    #[test]
    fn assign_to_sets_agent() {
        let mut task = make_task();
        task.assign_to("agent-1");
        assert_eq!(task.assigned_agent.as_deref(), Some("agent-1"));
    }
}
