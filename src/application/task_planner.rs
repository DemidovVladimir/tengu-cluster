//! Multi-agent task planning service.
//!
//! Decomposes a high-level goal into tasks with dependency information,
//! enabling parallel execution of independent tasks.

use crate::application::engine_runtime::collect_engine_response;
use anyhow::Result;
use std::collections::HashMap;
use tengu_core::types::{Message, Role};
use tengu_core::{Engine, EngineContext};

/// Routing decision from the lightweight classifier.
pub(crate) enum RouteDecision {
    /// Request can be handled by a single agent. Contains the role key.
    SingleAgent(String),
    /// Request requires multi-agent planning.
    MultiAgent,
}

/// Lightweight LLM classifier that decides whether a request needs one agent
/// or full multi-agent planning. Much cheaper than `generate_plan`.
pub(crate) async fn classify_request(
    engine: &dyn Engine,
    message: &str,
    agent_descriptions: &HashMap<String, String>,
) -> Result<RouteDecision> {
    let mut team = String::new();
    for (role, desc) in agent_descriptions {
        team.push_str(&format!("- {} : {}\n", role, desc));
    }

    let system = format!(
        "You are a request router for a multi-agent team.\n\n\
         Team members:\n{}\n\
         Given a user request, decide:\n\
         1. If ONE agent can fully handle it alone, respond: {{\"route\":\"single\",\"role\":\"exact_role_key\"}}\n\
         2. If it needs MULTIPLE agents or is a compound goal, respond: {{\"route\":\"multi\"}}\n\n\
         Rules:\n\
         - Choose \"single\" when the request clearly falls under one agent's expertise\n\
         - Choose \"multi\" ONLY when the user EXPLICITLY asks for work that spans multiple agents\n\
         - Do NOT choose \"multi\" just because additional agents COULD be useful. Only if the user asked for their specific work.\n\
         - Route based on the CURRENT user request only; do not expand scope based on prior context or likely follow-up work\n\
         - Use ONLY the exact role keys listed above\n\
         - Respond with ONLY valid JSON, no markdown fences, no extra text",
        team
    );

    let messages = vec![Message {
        role: Role::User,
        content: message.to_string(),
        tool_call_id: None,
        tool_calls: None,
    }];

    let context = EngineContext {
        workspace: None,
        system_prompt: Some(system),
    };

    let response =
        collect_engine_response(engine, &messages, &[], &context, None, None, None, None).await?;

    parse_route_decision(&response.text, agent_descriptions)
}

/// Parse the classifier's JSON response into a `RouteDecision`.
/// Falls back to `MultiAgent` on any parse failure or unknown role.
fn parse_route_decision(
    text: &str,
    agent_descriptions: &HashMap<String, String>,
) -> Result<RouteDecision> {
    let json_str = match extract_json(text) {
        Some(j) => j,
        None => return Ok(RouteDecision::MultiAgent),
    };
    let value: serde_json::Value = match serde_json::from_str(&json_str) {
        Ok(v) => v,
        Err(_) => return Ok(RouteDecision::MultiAgent),
    };
    let route = value
        .get("route")
        .and_then(|v| v.as_str())
        .unwrap_or("multi");
    if route == "single" {
        if let Some(role) = value.get("role").and_then(|v| v.as_str()) {
            if agent_descriptions.contains_key(role) {
                return Ok(RouteDecision::SingleAgent(role.to_string()));
            }
        }
    }
    Ok(RouteDecision::MultiAgent)
}

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

/// Declared dependency constraints from agent config.
/// Maps role_key -> list of role_keys it must depend on.
pub(crate) type RoleDependencies = HashMap<String, Vec<String>>;

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
         - ONLY create tasks that the user EXPLICITLY asked for. Do NOT infer additional work.\n\
         - NOT every team member needs a task. Most requests only need 1-3 agents.\n\
         - If the user says \"register POI and mint IP-NFT\" → only hypothesis_researcher + onchain_minter. Do NOT add mol_labs, beach_scientist, custodian, or any other agent.\n\
         - If the user says \"create a post on beach-science\" → only beach_scientist (+ dependencies for data it needs).\n\
         - Do NOT add publication, upload, announcement, transfer, or follow-up tasks unless the user LITERALLY asked for them by name.\n\
         - Each task gets a unique short id (snake_case)\n\
         - Use ONLY the exact role keys listed above\n\
         - depends_on lists task ids that MUST complete first (empty = can run immediately)\n\
         - CRITICAL: If an agent's description says REQUIRES (must depend on), then every task for that agent \
           MUST have depends_on that includes a task from each required role. This is mandatory, not optional.\n\
         - CRITICAL: Create at most ONE task per agent/role. Each agent handles its own internal workflow \
           (e.g. authenticate → create → upload → announce). Do NOT split an agent's workflow into multiple tasks.\n\
         - If the user asked for only part of a larger workflow, plan ONLY that subset\n\
         - If the input contains sections like \"Relevant Prior Work\", treat them as background context only. They do NOT expand the requested deliverables\n\
         - Only parallelize tasks whose agents have no REQUIRES relationship with each other\n\
         - Keep to 2-6 tasks",
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
        collect_engine_response(engine, &messages, &[], &context, None, None, None, None).await?;

    if response.text.is_empty() {
        anyhow::bail!("Planner received empty response from engine");
    }

    match parse_plan_json(&response.text) {
        Ok(plan) => Ok(plan),
        Err(e) => {
            tracing::warn!(
                "Plan parsing failed. Raw response ({} chars): {}",
                response.text.len(),
                if response.text.len() > 500 {
                    format!("{}...", &response.text[..500])
                } else {
                    response.text.clone()
                }
            );
            Err(e)
        }
    }
}

/// Resolve execution order: returns batches of task indices that can run in parallel.
/// Each batch depends only on tasks in previous batches.
pub(crate) fn resolve_execution_order(tasks: &[PlanTask]) -> Result<Vec<Vec<usize>>> {
    let id_to_idx: HashMap<&str, usize> = tasks
        .iter()
        .enumerate()
        .map(|(i, t)| (t.id.as_str(), i))
        .collect();

    // Reject unknown dependency IDs instead of silently treating them as satisfied.
    for task in tasks {
        for dep in &task.depends_on {
            if !id_to_idx.contains_key(dep.as_str()) {
                anyhow::bail!(
                    "Task '{}' depends on unknown task '{}' — plan is invalid",
                    task.id,
                    dep
                );
            }
        }
    }

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
                    .map_or(false, |&idx| completed[idx])
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

/// Fix `depends_on` entries that reference role names instead of task IDs.
///
/// LLMs sometimes put a role key (e.g. `"onchain_minter"`) in depends_on
/// instead of a task ID (e.g. `"mint_ipnft"`). This function resolves
/// role references to the last task with that role. Returns the number
/// of entries fixed.
pub(crate) fn resolve_role_refs_in_depends(tasks: &mut [PlanTask]) -> usize {
    let id_set: std::collections::HashSet<String> =
        tasks.iter().map(|t| t.id.clone()).collect();
    let last_task_for_role: HashMap<String, String> = {
        let mut map = HashMap::new();
        for task in tasks.iter() {
            map.insert(task.role.clone(), task.id.clone());
        }
        map
    };

    let mut fixed = 0usize;
    for task in tasks.iter_mut() {
        for dep in task.depends_on.iter_mut() {
            if !id_set.contains(dep.as_str()) {
                // Not a known task ID — try interpreting it as a role name.
                if let Some(task_id) = last_task_for_role.get(dep.as_str()) {
                    tracing::info!(
                        task = %task.id,
                        role_ref = %dep,
                        resolved_to = %task_id,
                        "Resolved role reference in depends_on to task ID"
                    );
                    *dep = task_id.clone();
                    fixed += 1;
                }
            }
        }
    }
    fixed
}

/// Auto-repair a plan by injecting missing `depends_on` entries based on
/// declared `requires` constraints. For each task whose role requires another
/// role, ensures at least one transitive dependency on a task from that role.
/// Returns the number of edges added.
pub(crate) fn repair_plan_dependencies(
    tasks: &mut [PlanTask],
    role_deps: &RoleDependencies,
) -> usize {
    // Snapshot: role -> last task id, and id -> (index, role) — all owned.
    let last_task_for_role: HashMap<String, String> = {
        let mut map = HashMap::new();
        for task in tasks.iter() {
            map.insert(task.role.clone(), task.id.clone());
        }
        map
    };
    let id_to_meta: HashMap<String, (usize, String)> = tasks
        .iter()
        .enumerate()
        .map(|(i, t)| (t.id.clone(), (i, t.role.clone())))
        .collect();

    // Collect repairs as (task_index, dep_id_to_add).
    let mut repairs: Vec<(usize, String)> = Vec::new();
    for i in 0..tasks.len() {
        let required_roles = match role_deps.get(&tasks[i].role) {
            Some(deps) => deps.clone(),
            None => continue,
        };

        for required_role in &required_roles {
            // Walk transitive deps to check if already satisfied.
            let mut visited = std::collections::HashSet::new();
            let mut stack: Vec<String> = tasks[i].depends_on.clone();
            let mut found = false;
            while let Some(dep_id) = stack.pop() {
                if !visited.insert(dep_id.clone()) {
                    continue;
                }
                if let Some((idx, role)) = id_to_meta.get(&dep_id) {
                    if role == required_role {
                        found = true;
                        break;
                    }
                    stack.extend(tasks[*idx].depends_on.clone());
                }
            }
            if found {
                continue;
            }

            if let Some(dep_task_id) = last_task_for_role.get(required_role.as_str()) {
                if !tasks[i].depends_on.contains(dep_task_id) {
                    repairs.push((i, dep_task_id.clone()));
                }
            }
        }
    }

    let added = repairs.len();
    for (idx, dep_id) in repairs {
        tasks[idx].depends_on.push(dep_id);
    }
    added
}

/// Validate a plan against declared role dependencies.
///
/// If role B `requires` role A, then every task assigned to B must
/// (transitively) depend on at least one task assigned to A.
pub(crate) fn validate_plan_dependencies(
    tasks: &[PlanTask],
    role_deps: &RoleDependencies,
) -> Result<()> {
    // Build role -> task IDs mapping.
    let mut role_tasks: HashMap<&str, Vec<&str>> = HashMap::new();
    for task in tasks {
        role_tasks
            .entry(task.role.as_str())
            .or_default()
            .push(task.id.as_str());
    }

    // Build task -> set of transitive dependencies.
    let id_to_task: HashMap<&str, &PlanTask> = tasks.iter().map(|t| (t.id.as_str(), t)).collect();
    let mut transitive_deps: HashMap<&str, std::collections::HashSet<&str>> = HashMap::new();

    fn collect_deps<'a>(
        task_id: &'a str,
        id_to_task: &HashMap<&str, &'a PlanTask>,
        cache: &mut HashMap<&'a str, std::collections::HashSet<&'a str>>,
    ) -> std::collections::HashSet<&'a str> {
        if let Some(cached) = cache.get(task_id) {
            return cached.clone();
        }
        let mut deps = std::collections::HashSet::new();
        if let Some(task) = id_to_task.get(task_id) {
            for dep_id in &task.depends_on {
                deps.insert(dep_id.as_str());
                deps.extend(collect_deps(dep_id.as_str(), id_to_task, cache));
            }
        }
        cache.insert(task_id, deps.clone());
        deps
    }

    for task in tasks {
        collect_deps(task.id.as_str(), &id_to_task, &mut transitive_deps);
    }

    // Check: for each role with declared requirements, every task assigned to it
    // must transitively depend on at least one task from each required role.
    for (role, required_roles) in role_deps {
        let tasks_for_role: Vec<&PlanTask> = tasks.iter().filter(|t| t.role == *role).collect();
        if tasks_for_role.is_empty() {
            continue; // Role not used in this plan.
        }

        for required_role in required_roles {
            let required_task_ids: Vec<&str> = role_tasks
                .get(required_role.as_str())
                .map(|v| v.as_slice())
                .unwrap_or(&[])
                .to_vec();
            if required_task_ids.is_empty() {
                continue; // Required role not in plan — can't enforce.
            }

            for task in &tasks_for_role {
                let deps = transitive_deps
                    .get(task.id.as_str())
                    .cloned()
                    .unwrap_or_default();
                let has_required = required_task_ids.iter().any(|rid| deps.contains(rid));
                if !has_required {
                    anyhow::bail!(
                        "Task '{}' (role '{}') must depend on a '{}' task but doesn't. \
                         Add depends_on to fix the plan.",
                        task.id,
                        role,
                        required_role,
                    );
                }
            }
        }
    }

    Ok(())
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
    fn parse_route_decision_single() {
        let mut descs = HashMap::new();
        descs.insert("researcher".to_string(), "Research agent".to_string());
        descs.insert("minter".to_string(), "Minting agent".to_string());
        let result =
            parse_route_decision(r#"{"route":"single","role":"researcher"}"#, &descs).unwrap();
        assert!(matches!(result, RouteDecision::SingleAgent(r) if r == "researcher"));
    }

    #[test]
    fn parse_route_decision_multi() {
        let descs = HashMap::new();
        let result = parse_route_decision(r#"{"route":"multi"}"#, &descs).unwrap();
        assert!(matches!(result, RouteDecision::MultiAgent));
    }

    #[test]
    fn parse_route_decision_unknown_role_falls_back() {
        let mut descs = HashMap::new();
        descs.insert("researcher".to_string(), "Research agent".to_string());
        let result =
            parse_route_decision(r#"{"route":"single","role":"nonexistent"}"#, &descs).unwrap();
        assert!(matches!(result, RouteDecision::MultiAgent));
    }

    #[test]
    fn parse_route_decision_invalid_json_falls_back() {
        let descs = HashMap::new();
        let result = parse_route_decision("not json at all", &descs).unwrap();
        assert!(matches!(result, RouteDecision::MultiAgent));
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

    #[test]
    fn resolve_rejects_unknown_dependency() {
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
                depends_on: vec!["nonexistent".into()],
            },
        ];
        let err = resolve_execution_order(&tasks).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("unknown task"), "got: {}", msg);
        assert!(msg.contains("nonexistent"), "got: {}", msg);
    }

    #[test]
    fn validate_deps_passes_valid_plan() {
        let tasks = vec![
            PlanTask {
                id: "research".into(),
                role: "hypothesis_researcher".into(),
                task: "analyze PDF".into(),
                depends_on: vec![],
            },
            PlanTask {
                id: "mint".into(),
                role: "onchain_minter".into(),
                task: "mint IPNFT".into(),
                depends_on: vec!["research".into()],
            },
            PlanTask {
                id: "upload".into(),
                role: "mol_labs".into(),
                task: "upload to Molecule".into(),
                depends_on: vec!["mint".into()],
            },
        ];
        let mut role_deps = RoleDependencies::new();
        role_deps.insert(
            "onchain_minter".into(),
            vec!["hypothesis_researcher".into()],
        );
        role_deps.insert("mol_labs".into(), vec!["onchain_minter".into()]);

        assert!(validate_plan_dependencies(&tasks, &role_deps).is_ok());
    }

    #[test]
    fn validate_deps_catches_missing_dependency() {
        // mol_labs requires onchain_minter, but the task has no dependency on any minter task.
        let tasks = vec![
            PlanTask {
                id: "research".into(),
                role: "hypothesis_researcher".into(),
                task: "analyze PDF".into(),
                depends_on: vec![],
            },
            PlanTask {
                id: "mint".into(),
                role: "onchain_minter".into(),
                task: "mint IPNFT".into(),
                depends_on: vec!["research".into()],
            },
            PlanTask {
                id: "upload".into(),
                role: "mol_labs".into(),
                task: "upload to Molecule".into(),
                depends_on: vec!["research".into()], // wrong: should depend on mint
            },
        ];
        let mut role_deps = RoleDependencies::new();
        role_deps.insert("mol_labs".into(), vec!["onchain_minter".into()]);

        let err = validate_plan_dependencies(&tasks, &role_deps).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("upload"), "got: {}", msg);
        assert!(msg.contains("onchain_minter"), "got: {}", msg);
    }

    #[test]
    fn validate_deps_transitive_ok() {
        // beach_scientist requires hypothesis_researcher transitively through onchain_minter.
        let tasks = vec![
            PlanTask {
                id: "research".into(),
                role: "hypothesis_researcher".into(),
                task: "analyze".into(),
                depends_on: vec![],
            },
            PlanTask {
                id: "mint".into(),
                role: "onchain_minter".into(),
                task: "mint".into(),
                depends_on: vec!["research".into()],
            },
            PlanTask {
                id: "post".into(),
                role: "beach_scientist".into(),
                task: "publish".into(),
                depends_on: vec!["mint".into()], // transitive: mint -> research
            },
        ];
        let mut role_deps = RoleDependencies::new();
        role_deps.insert(
            "beach_scientist".into(),
            vec!["hypothesis_researcher".into(), "onchain_minter".into()],
        );

        assert!(validate_plan_dependencies(&tasks, &role_deps).is_ok());
    }

    #[test]
    fn validate_deps_no_constraints_always_passes() {
        let tasks = vec![PlanTask {
            id: "a".into(),
            role: "any".into(),
            task: "do something".into(),
            depends_on: vec![],
        }];
        let role_deps = RoleDependencies::new(); // no constraints
        assert!(validate_plan_dependencies(&tasks, &role_deps).is_ok());
    }

    #[test]
    fn repair_adds_missing_dependency() {
        // Planner produced parallel tasks, but mol_labs requires onchain_minter.
        let mut tasks = vec![
            PlanTask {
                id: "research".into(),
                role: "hypothesis_researcher".into(),
                task: "analyze".into(),
                depends_on: vec![],
            },
            PlanTask {
                id: "mint".into(),
                role: "onchain_minter".into(),
                task: "mint".into(),
                depends_on: vec![],
            },
            PlanTask {
                id: "upload".into(),
                role: "mol_labs".into(),
                task: "upload".into(),
                depends_on: vec![],
            },
        ];
        let mut role_deps = RoleDependencies::new();
        role_deps.insert(
            "onchain_minter".into(),
            vec!["hypothesis_researcher".into()],
        );
        role_deps.insert("mol_labs".into(), vec!["onchain_minter".into()]);

        // Before repair: validation fails.
        assert!(validate_plan_dependencies(&tasks, &role_deps).is_err());

        let added = repair_plan_dependencies(&mut tasks, &role_deps);
        assert!(added >= 2, "expected at least 2 edges added, got {}", added);

        // After repair: validation passes.
        assert!(validate_plan_dependencies(&tasks, &role_deps).is_ok());

        // And execution order is fully sequential.
        let batches = resolve_execution_order(&tasks).unwrap();
        assert_eq!(batches.len(), 3);
    }

    #[test]
    fn repair_no_op_when_already_correct() {
        let mut tasks = vec![
            PlanTask {
                id: "research".into(),
                role: "hypothesis_researcher".into(),
                task: "analyze".into(),
                depends_on: vec![],
            },
            PlanTask {
                id: "mint".into(),
                role: "onchain_minter".into(),
                task: "mint".into(),
                depends_on: vec!["research".into()],
            },
        ];
        let mut role_deps = RoleDependencies::new();
        role_deps.insert(
            "onchain_minter".into(),
            vec!["hypothesis_researcher".into()],
        );

        let added = repair_plan_dependencies(&mut tasks, &role_deps);
        assert_eq!(added, 0);
    }
}
