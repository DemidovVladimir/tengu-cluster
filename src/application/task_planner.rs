//! Multi-agent task planning service.
//!
//! Decomposes a high-level goal into tasks with dependency information,
//! enabling parallel execution of independent tasks.

use crate::application::engine_runtime::collect_engine_response;
use anyhow::Result;
use std::collections::HashMap;
use tengu_core::types::{Message, Role};
use tengu_core::{Engine, EngineContext};

/// A single task in an execution plan.
pub(crate) struct PlanTask {
    /// Unique identifier for this task (e.g. "research", "mint").
    pub id: String,
    /// Agent role key that should execute this task.
    pub role: String,
    /// What the agent should do.
    pub task: String,
    /// IDs of tasks that must complete before this one starts.
    pub depends_on: Vec<String>,
}

/// Generate an execution plan by asking an LLM to decompose a goal into tasks
/// with dependency information.
///
/// `agent_descriptions` maps role keys to human-readable descriptions.
pub(crate) async fn generate_plan(
    engine: &dyn Engine,
    goal: &str,
    agent_descriptions: &HashMap<String, String>,
) -> Result<Vec<PlanTask>> {
    let mut team = String::new();
    for (role, desc) in agent_descriptions {
        team.push_str(&format!("- {} : {}\n", role, desc));
    }

    let system = format!(
        "You are a project coordinator. Break down a goal into tasks for the available team.\n\n\
         Team members:\n{}\n\
         Respond with ONLY valid JSON, no markdown fences, no extra text:\n\
         {{\"tasks\":[\
           {{\"id\":\"short_snake_case_id\",\
             \"role\":\"exact_role_key\",\
             \"task\":\"specific actionable task\",\
             \"depends_on\":[]}}\
         ]}}\n\n\
         Rules:\n\
         - Each task gets a unique short id (snake_case)\n\
         - Use ONLY the exact role keys listed above\n\
         - depends_on lists task ids that MUST complete first (empty = can run immediately)\n\
         - Tasks with no dependency on each other SHOULD have empty depends_on so they run in parallel\n\
         - Each task must be specific and actionable\n\
         - Keep to 2-6 tasks\n\
         - Think about what can run in parallel vs what needs sequential execution",
        team
    );

    let messages = vec![Message {
        role: Role::User,
        content: format!("Goal: {}", goal),
        tool_call_id: None,
        tool_calls: None,
    }];

    let context = EngineContext {
        workspace: None,
        system_prompt: Some(system),
    };

    let response =
        collect_engine_response(engine, &messages, &[], &context, None, None, None).await?;

    parse_plan_json(&response.text)
}

/// Resolve execution order: returns batches of task indices that can run in parallel.
/// Each batch depends only on tasks in previous batches.
pub(crate) fn resolve_execution_order(tasks: &[PlanTask]) -> Result<Vec<Vec<usize>>> {
    let id_to_idx: HashMap<&str, usize> = tasks
        .iter()
        .enumerate()
        .map(|(i, t)| (t.id.as_str(), i))
        .collect();

    let mut completed: Vec<bool> = vec![false; tasks.len()];
    let mut batches: Vec<Vec<usize>> = Vec::new();
    let mut remaining = tasks.len();

    // Safety: max iterations = number of tasks (prevents infinite loop on bad deps).
    for _ in 0..tasks.len() {
        if remaining == 0 {
            break;
        }
        let mut batch = Vec::new();
        for (i, task) in tasks.iter().enumerate() {
            if completed[i] {
                continue;
            }
            let deps_met = task.depends_on.iter().all(|dep| {
                id_to_idx
                    .get(dep.as_str())
                    .map_or(true, |&idx| completed[idx])
            });
            if deps_met {
                batch.push(i);
            }
        }
        if batch.is_empty() {
            anyhow::bail!("Circular dependency detected — some tasks can never execute");
        }
        for &idx in &batch {
            completed[idx] = true;
        }
        remaining -= batch.len();
        batches.push(batch);
    }

    Ok(batches)
}

/// Extract a JSON object from text that may contain markdown fences or prose.
fn extract_json(text: &str) -> Option<String> {
    if let Some(start) = text.find("```json") {
        let inner_start = start + 7;
        if let Some(end) = text[inner_start..].find("```") {
            return Some(text[inner_start..inner_start + end].trim().to_string());
        }
    }
    if let Some(start) = text.find("```") {
        let inner_start = start + 3;
        if let Some(end) = text[inner_start..].find("```") {
            let inner = text[inner_start..inner_start + end].trim();
            if inner.starts_with('{') {
                return Some(inner.to_string());
            }
        }
    }
    let start = text.find('{')?;
    let end = text.rfind('}')?;
    if start <= end {
        Some(text[start..=end].to_string())
    } else {
        None
    }
}

/// Parse a JSON execution plan from LLM output.
fn parse_plan_json(text: &str) -> Result<Vec<PlanTask>> {
    let json_str =
        extract_json(text).ok_or_else(|| anyhow::anyhow!("No JSON found in planner response"))?;
    let value: serde_json::Value = serde_json::from_str(&json_str)?;
    let tasks = value
        .get("tasks")
        .and_then(|s| s.as_array())
        .ok_or_else(|| anyhow::anyhow!("Plan missing 'tasks' array"))?;

    let mut result = Vec::new();
    for (i, task) in tasks.iter().enumerate() {
        let id = task
            .get("id")
            .and_then(|v| v.as_str())
            .unwrap_or(&format!("task_{}", i))
            .to_string();
        let role = task
            .get("role")
            .and_then(|r| r.as_str())
            .ok_or_else(|| anyhow::anyhow!("Task missing 'role'"))?;
        let description = task
            .get("task")
            .and_then(|t| t.as_str())
            .ok_or_else(|| anyhow::anyhow!("Task missing 'task'"))?;
        let depends_on = task
            .get("depends_on")
            .and_then(|d| d.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(|s| s.to_string()))
                    .collect()
            })
            .unwrap_or_default();

        result.push(PlanTask {
            id,
            role: role.to_string(),
            task: description.to_string(),
            depends_on,
        });
    }
    if result.is_empty() {
        anyhow::bail!("Plan has no tasks");
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_plan_with_dependencies() {
        let text = r#"{"tasks":[
            {"id":"research","role":"researcher","task":"analyze PDF","depends_on":[]},
            {"id":"mint","role":"minter","task":"mint IPNFT","depends_on":[]},
            {"id":"publish","role":"publisher","task":"create post","depends_on":["research","mint"]}
        ]}"#;
        let tasks = parse_plan_json(text).unwrap();
        assert_eq!(tasks.len(), 3);
        assert_eq!(tasks[0].id, "research");
        assert!(tasks[0].depends_on.is_empty());
        assert_eq!(tasks[2].depends_on, vec!["research", "mint"]);
    }

    #[test]
    fn resolve_parallel_and_sequential() {
        let tasks = vec![
            PlanTask {
                id: "a".into(),
                role: "r1".into(),
                task: "do A".into(),
                depends_on: vec![],
            },
            PlanTask {
                id: "b".into(),
                role: "r2".into(),
                task: "do B".into(),
                depends_on: vec![],
            },
            PlanTask {
                id: "c".into(),
                role: "r3".into(),
                task: "do C".into(),
                depends_on: vec!["b".into()],
            },
        ];
        let batches = resolve_execution_order(&tasks).unwrap();
        assert_eq!(batches.len(), 2);
        assert_eq!(batches[0], vec![0, 1]); // a and b in parallel
        assert_eq!(batches[1], vec![2]); // c after b
    }

    #[test]
    fn resolve_circular_dependency() {
        let tasks = vec![
            PlanTask {
                id: "a".into(),
                role: "r1".into(),
                task: "do A".into(),
                depends_on: vec!["b".into()],
            },
            PlanTask {
                id: "b".into(),
                role: "r2".into(),
                task: "do B".into(),
                depends_on: vec!["a".into()],
            },
        ];
        assert!(resolve_execution_order(&tasks).is_err());
    }

    #[test]
    fn parse_plan_from_markdown_fence() {
        let text = "Here:\n```json\n{\"tasks\":[{\"id\":\"t\",\"role\":\"qa\",\"task\":\"test\",\"depends_on\":[]}]}\n```";
        let tasks = parse_plan_json(text).unwrap();
        assert_eq!(tasks.len(), 1);
    }

    #[test]
    fn parse_plan_empty_tasks() {
        assert!(parse_plan_json(r#"{"tasks":[]}"#).is_err());
    }

    #[test]
    fn extract_json_no_json() {
        assert!(extract_json("no json here").is_none());
    }

    #[test]
    fn extract_json_raw() {
        let j = extract_json(r#"{"tasks":[]}"#).unwrap();
        assert!(j.contains("tasks"));
    }

    #[test]
    fn resolve_all_parallel() {
        let tasks = vec![
            PlanTask {
                id: "a".into(),
                role: "r1".into(),
                task: "do A".into(),
                depends_on: vec![],
            },
            PlanTask {
                id: "b".into(),
                role: "r2".into(),
                task: "do B".into(),
                depends_on: vec![],
            },
        ];
        let batches = resolve_execution_order(&tasks).unwrap();
        assert_eq!(batches.len(), 1);
        assert_eq!(batches[0], vec![0, 1]);
    }

    #[test]
    fn resolve_all_sequential() {
        let tasks = vec![
            PlanTask {
                id: "a".into(),
                role: "r1".into(),
                task: "do A".into(),
                depends_on: vec![],
            },
            PlanTask {
                id: "b".into(),
                role: "r2".into(),
                task: "do B".into(),
                depends_on: vec!["a".into()],
            },
            PlanTask {
                id: "c".into(),
                role: "r3".into(),
                task: "do C".into(),
                depends_on: vec!["b".into()],
            },
        ];
        let batches = resolve_execution_order(&tasks).unwrap();
        assert_eq!(batches.len(), 3);
    }
}
