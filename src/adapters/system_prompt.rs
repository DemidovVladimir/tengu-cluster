//! System prompt builder — always produces a prompt from identity + role + workspace files.

use crate::application::prompt_budget::truncate_to_token_budget;
use crate::domain::agent_role::AgentRole;
use tengu_core::token::estimate_tokens_approx_min1;
use tengu_core::types::ToolDef;

/// Build bounded system prompt from config identity, role, custom instructions, workspace files,
/// and optional skill context fragments (API docs from frontmatter skills).
///
/// Always returns a non-empty prompt. At minimum this includes the default preamble
/// (`"You are {name}, an AI assistant."`), guaranteeing the model never runs without identity.
pub(crate) fn build_system_prompt(
    agent_config: &tengu_core::config::AgentConfig,
    advertise_workspace_tools: bool,
    skill_contexts: &[String],
) -> String {
    build_system_prompt_with_tools(agent_config, advertise_workspace_tools, skill_contexts, &[])
}

/// Build system prompt with dynamic tool listing generated from ToolDef metadata.
pub(crate) fn build_system_prompt_with_tools(
    agent_config: &tengu_core::config::AgentConfig,
    advertise_workspace_tools: bool,
    skill_contexts: &[String],
    tools: &[ToolDef],
) -> String {
    let budget = &agent_config.prompt_budget;
    let max_file_tokens = budget.max_file_tokens;
    let max_skill_context_tokens = budget.max_skill_context_tokens;
    let max_total_tokens = budget.max_total_tokens;

    let name = agent_config.identity.name.as_deref().unwrap_or("Tengu");
    let mut parts = Vec::new();
    let mut total_tokens = 0usize;

    // 1. Default preamble — always present.
    let preamble = format!(
        "You are {name}, an AI assistant.\n\n\
         CRITICAL RULES:\n\
         - NEVER fabricate data. Do not invent transaction hashes, URLs, IDs, addresses, \
         block numbers, or any other identifiers. If you do not have real data from a tool \
         call result, say so.\n\
         - NEVER present fictional output as if a command succeeded. If you did not execute \
         an action via a tool, do not claim it happened.\n\
         - When asked to perform an action, use the available tools \
         to actually execute it. Report only real results from tool output."
    );
    total_tokens += estimate_tokens_approx_min1(&preamble);
    parts.push(preamble);

    // 2. Role label — if agent has a role, note it in the prompt.
    // Detailed role instructions come from identity.instructions in config.
    if let Some(ref role_str) = agent_config.role {
        if let Ok(role) = role_str.parse::<AgentRole>() {
            let fragment = format!("Your role: {}.", role.label());
            total_tokens += estimate_tokens_approx_min1(&fragment);
            parts.push(fragment);
        }
    }

    // 3. Custom instructions — verbatim from config identity.instructions.
    if let Some(ref instructions) = agent_config.identity.instructions {
        if !instructions.trim().is_empty() {
            let truncated = truncate_to_token_budget(instructions, max_file_tokens);
            total_tokens += estimate_tokens_approx_min1(&truncated);
            parts.push(truncated);
        }
    }

    // 4. Workspace files — IDENTITY.md, PROFILE.md, CONTEXT.md.
    if let Some(ref workspace) = agent_config.workspace {
        for filename in &["IDENTITY.md", "PROFILE.md", "CONTEXT.md"] {
            let path = workspace.join(filename);
            if let Ok(content) = std::fs::read_to_string(&path) {
                if !content.trim().is_empty() {
                    let truncated = truncate_to_token_budget(&content, max_file_tokens);
                    let chunk = format!("# {filename}\n\n{truncated}");
                    let chunk_tokens = estimate_tokens_approx_min1(&chunk);
                    if total_tokens + chunk_tokens > max_total_tokens {
                        break;
                    }
                    total_tokens += chunk_tokens;
                    parts.push(chunk);
                }
            }
        }
    }

    // 5. Skill context fragments — API docs from frontmatter skills.
    for ctx in skill_contexts {
        if ctx.trim().is_empty() {
            continue;
        }
        let truncated = truncate_to_token_budget(ctx, max_skill_context_tokens);
        let chunk_tokens = estimate_tokens_approx_min1(&truncated);
        if total_tokens + chunk_tokens > max_total_tokens {
            break;
        }
        total_tokens += chunk_tokens;
        parts.push(truncated);
    }

    // 5b. Skill tool usage instructions — tell the model to use skill tools directly.
    if !skill_contexts.is_empty() {
        let instruction = "When you have a registered tool for a service, call it directly. Do NOT write scripts or suggest manual steps.";
        let inst_tokens = estimate_tokens_approx_min1(instruction);
        if total_tokens + inst_tokens <= max_total_tokens + max_total_tokens / 20 {
            total_tokens += inst_tokens;
            parts.push(instruction.to_string());
        }
    }

    // 6. Workspace tools description — generated from ToolDef metadata.
    if advertise_workspace_tools && agent_config.workspace.is_some() {
        let tools_note = if tools.is_empty() {
            "# Workspace\n\nYou have workspace access. Paths are relative to root. Use tools to execute actions directly — never create scripts for the user."
                .to_string()
        } else {
            let mut lines = vec!["# Workspace tools".to_string()];
            for tool in tools {
                let params = tool
                    .parameters
                    .get("required")
                    .and_then(|r| r.as_array())
                    .map(|arr| {
                        arr.iter()
                            .filter_map(|v| v.as_str())
                            .collect::<Vec<_>>()
                            .join(", ")
                    })
                    .unwrap_or_default();
                let approval = tool
                    .policy
                    .as_ref()
                    .map(|p| {
                        if p.requires_approval {
                            " [approval]"
                        } else {
                            ""
                        }
                    })
                    .unwrap_or("");
                lines.push(format!(
                    "- {}({}): {}{}",
                    tool.name, params, tool.description, approval
                ));
            }
            lines.push("Paths relative to workspace root. Read before answering about files. Execute actions directly — never create scripts.".to_string());
            lines.join("\n")
        };
        let tools_tokens = estimate_tokens_approx_min1(&tools_note);
        if total_tokens + tools_tokens <= max_total_tokens + max_total_tokens / 10 {
            parts.push(tools_note);
        }
    }

    parts.join("\n\n---\n\n")
}
