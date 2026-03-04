//! System prompt builder — always produces a prompt from identity + role + workspace files.

use crate::application::prompt_budget::truncate_to_token_budget;
use crate::domain::agent_role::AgentRole;
use tengu_core::token::estimate_tokens_approx_min1;

/// Build bounded system prompt from config identity, role, custom instructions, workspace files,
/// and optional skill context fragments (API docs from frontmatter skills).
///
/// Always returns a non-empty prompt. At minimum this includes the default preamble
/// (`"You are {name}, an AI assistant."`), guaranteeing the model never runs without identity.
pub(crate) fn build_system_prompt(
    agent_config: &tengu_core::config::AgentConfig,
    advertise_workspace_tools: bool,
    skill_contexts: &[String],
    #[allow(unused_variables)] evm_tools_available: bool,
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
         - When asked to perform an action, use the available tools (run_command, API tools) \
         to actually execute it. Report only real results from tool output."
    );
    total_tokens += estimate_tokens_approx_min1(&preamble);
    parts.push(preamble);

    // 2. Role fragment — if agent has an orchestration role.
    if let Some(ref role_str) = agent_config.role {
        if let Ok(role) = role_str.parse::<AgentRole>() {
            let fragment = role.system_prompt_fragment().to_string();
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
        let instruction = "\
            # Tool usage policy\n\n\
            When the user asks you to interact with an external service for which you have \
            a registered tool (e.g. beach_science), you MUST call that tool directly with \
            the appropriate method, path, and body parameters. \
            Do NOT write scripts, generate curl commands, or suggest manual steps. \
            Always use the tool.";
        let inst_tokens = estimate_tokens_approx_min1(instruction);
        if total_tokens + inst_tokens <= max_total_tokens + max_total_tokens / 20 {
            total_tokens += inst_tokens;
            parts.push(instruction.to_string());
        }
    }

    // 6. Workspace tools description — only when backend supports runtime tool use.
    if advertise_workspace_tools {
        if agent_config.workspace.is_some() {
            let tools_note = "\
                # Workspace\n\n\
                 You have access to a local workspace.\n\
                 Available tools:\n\
                 - read_file(path): Read file contents from the workspace (supports text files and PDFs)\n\
                 - list_directory(path): List files and directories (use \".\" for root)\n\
                 - write_file(path, content): Write content to a file (requires user approval)\n\
                 - run_command(command): Execute a shell command in the workspace (requires user approval)\n\
                 \n\
                 All paths are relative to the workspace root.\n\
                 \n\
                 Tool-use policy:\n\
                 - If asked about file contents, call read_file before answering.\n\
                 - Do not claim file contents you have not read via tools in this turn.\n\
                 - When the user asks you to perform an action (install packages, run scripts, call APIs, compile code, \
                 execute commands), use run_command to execute it directly. Do NOT create script files for the user to \
                 run manually — always execute actions yourself using run_command.\n\
                 - When an action needs time to propagate (e.g. blockchain indexing, deployment, CI), \
                 use run_command with a polling loop to check results automatically. Example: \
                 for i in $(seq 1 10); do sleep 30; curl -s <check_url> && exit 0; done. \
                 Do NOT tell the user to wait and check manually — poll for them."
                .to_string();
            let tools_tokens = estimate_tokens_approx_min1(&tools_note);
            if total_tokens + tools_tokens <= max_total_tokens + max_total_tokens / 10 {
                parts.push(tools_note);
            }
        }
    }

    // 7. EVM tools description — when native signing is available.
    #[cfg(feature = "evm")]
    if evm_tools_available {
        let evm_note = "\
            # EVM Wallet (Native Signing)\n\n\
             You have a live Ethereum wallet connected. Use these native tools for ALL \
             on-chain operations — do NOT use JavaScript/viem/ethers code from skill docs.\n\n\
             Available EVM tools:\n\
             - evm_get_address(): Get your wallet's Ethereum address\n\
             - evm_sign_message(message): Sign a message with your private key (EIP-191 personal_sign)\n\
             - evm_send_transaction(to, data, value, chain_id): Build, sign, and submit a transaction. \
             Returns the mined receipt with transaction hash, block number, success status, and gas used.\n\
             \n\
             IMPORTANT:\n\
             - When a skill workflow includes JavaScript/viem/ethers code for signing or \
             sending transactions, translate those into evm_sign_message or evm_send_transaction \
             calls instead. The native tools handle key management and signing internally.\n\
             - For contract calls, encode the calldata yourself (ABI-encode the function selector + args) \
             and pass it as the `data` parameter to evm_send_transaction.\n\
             - For message signing (e.g. EIP-191 terms acceptance), use evm_sign_message.\n\
             - The `value` parameter is in wei (decimal string). For example, 0.001 ETH = \"1000000000000000\".\n\
             - All EVM tools require user approval before execution."
            .to_string();
        let evm_tokens = estimate_tokens_approx_min1(&evm_note);
        if total_tokens + evm_tokens <= max_total_tokens + max_total_tokens * 3 / 20 {
            parts.push(evm_note);
        }
    }

    parts.join("\n\n---\n\n")
}
