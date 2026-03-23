//! Task lifecycle: types, planning, state machine, and in-memory history.
//!
//! Consolidates task model, live plan, planner, and storage into a single module.

use crate::adapters::config::LimitsConfig;
use crate::adapters::engine_builder::collect_engine_response;
use crate::adapters::types::{Message, PlanTask, Role, RoleDependencies, RouteDecision};
use crate::adapters::{Engine, EngineContext};
use anyhow::Result;
use std::collections::HashMap;

// ── Planning ────────────────────────────────────────────────────────────────

/// Lightweight LLM classifier that decides whether a request needs one agent
/// or full multi-agent planning.
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

    let defaults = LimitsConfig::default();
    let response = collect_engine_response(
        engine, &messages, &[], &context, None, None, None, None,
        defaults.max_tool_rounds, defaults.max_tool_result_chars, defaults.stream_event_timeout_secs, defaults.compact_result_limit,
    ).await?;

    parse_route_decision(&response.text, agent_descriptions)
}

/// Parse the classifier's JSON response into a `RouteDecision`.
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

/// Generate an execution plan by asking an LLM to decompose a goal into tasks
/// with dependency information.
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

    let defaults = LimitsConfig::default();
    let response = collect_engine_response(
        engine, &messages, &[], &context, None, None, None, None,
        defaults.max_tool_rounds, defaults.max_tool_result_chars, defaults.stream_event_timeout_secs, defaults.compact_result_limit,
    ).await?;

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

/// Fix `depends_on` entries that reference role names instead of task IDs.
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
/// declared `requires` constraints.
pub(crate) fn repair_plan_dependencies(
    tasks: &mut [PlanTask],
    role_deps: &RoleDependencies,
) -> usize {
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

    let mut repairs: Vec<(usize, String)> = Vec::new();
    for i in 0..tasks.len() {
        let required_roles = match role_deps.get(&tasks[i].role) {
            Some(deps) => deps.clone(),
            None => continue,
        };

        for required_role in &required_roles {
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
pub(crate) fn validate_plan_dependencies(
    tasks: &[PlanTask],
    role_deps: &RoleDependencies,
) -> Result<()> {
    let mut role_tasks: HashMap<&str, Vec<&str>> = HashMap::new();
    for task in tasks {
        role_tasks
            .entry(task.role.as_str())
            .or_default()
            .push(task.id.as_str());
    }

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

    for (role, required_roles) in role_deps {
        let tasks_for_role: Vec<&PlanTask> = tasks.iter().filter(|t| t.role == *role).collect();
        if tasks_for_role.is_empty() {
            continue;
        }

        for required_role in required_roles {
            let required_task_ids: Vec<&str> = role_tasks
                .get(required_role.as_str())
                .map(|v| v.as_slice())
                .unwrap_or(&[])
                .to_vec();
            if required_task_ids.is_empty() {
                continue;
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

