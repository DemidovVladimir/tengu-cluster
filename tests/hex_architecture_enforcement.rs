use std::fs;
use std::path::Path;

fn read(path: &str) -> String {
    fs::read_to_string(path).unwrap_or_else(|e| panic!("failed to read {path}: {e}"))
}

#[test]
fn architecture_doc_exists_and_is_mandatory() {
    let path = Path::new("ARCHITECTURE.md");
    assert!(path.exists(), "ARCHITECTURE.md must exist");
    let doc = read("ARCHITECTURE.md");
    assert!(
        doc.to_lowercase().contains("not optional"),
        "ARCHITECTURE.md must explicitly state mandatory status"
    );
}

#[test]
fn readme_references_mandatory_hex_architecture() {
    let readme = read("README.md").to_lowercase();
    assert!(
        readme.contains("hexagonal architecture"),
        "README.md must reference hexagonal architecture"
    );
    assert!(
        readme.contains("mandatory"),
        "README.md must state architecture is mandatory"
    );
}

#[test]
fn tui_does_not_bypass_application_service_for_tool_execution() {
    let tui = read("src/adapters/tui/mod.rs");
    assert!(
        tui.contains("ToolUseService::new"),
        "TUI must wire tool execution via ToolUseService"
    );
    assert!(
        !tui.contains("workspace_tools::execute_tool("),
        "TUI must not call workspace_tools::execute_tool directly"
    );
}

#[test]
fn application_service_stays_infrastructure_free() {
    let service = read("src/application/tool_use_service.rs");
    assert!(
        !service.contains("cursive"),
        "Application service must not depend on TUI crate"
    );
    assert!(
        !service.contains("std::fs"),
        "Application service must not do direct filesystem access"
    );
}

#[test]
fn workspace_tools_is_adapter_for_execution_port() {
    let workspace_tools = read("src/adapters/workspace_tools.rs");
    assert!(
        workspace_tools.contains("impl ToolExecutionPort for WorkspaceToolExecutionAdapter"),
        "workspace_tools must implement ToolExecutionPort adapter"
    );
}

#[test]
fn project_has_explicit_hexagonal_module_layout() {
    assert!(
        Path::new("src/domain/mod.rs").exists(),
        "domain module must exist"
    );
    assert!(
        Path::new("src/application/mod.rs").exists(),
        "application module must exist"
    );
    assert!(
        Path::new("src/adapters/mod.rs").exists(),
        "adapters module must exist"
    );
    assert!(
        Path::new("src/application/prompt_budget.rs").exists(),
        "prompt budgeting must live in application layer"
    );
    assert!(
        Path::new("src/adapters/system_prompt.rs").exists(),
        "system prompt file loading must live in adapter layer"
    );
    assert!(
        !Path::new("src/runtime_commands.rs").exists()
            && !Path::new("src/runtime_engine.rs").exists()
            && !Path::new("src/runtime_prompt.rs").exists()
            && !Path::new("src/flow_store.rs").exists()
            && !Path::new("src/workspace_tools.rs").exists()
            && !Path::new("src/tool_use").exists()
            && !Path::new("src/tui").exists(),
        "legacy runtime_* modules must not exist"
    );
}

#[test]
fn tui_is_thin_and_calls_application_runtime() {
    let tui = read("src/adapters/tui/mod.rs");
    assert!(
        tui.contains("ChatRuntimeService") && tui.contains("process_user_text"),
        "tui adapter must delegate turn orchestration to application runtime service"
    );
    assert!(
        !tui.contains("collect_engine_response("),
        "tui must not orchestrate engine turn loop directly"
    );
}

#[test]
fn main_entrypoint_stays_thin_and_uses_adapters() {
    let main_src = read("src/main.rs");
    assert!(
        main_src.contains("mod adapters;")
            && main_src.contains("mod application;")
            && main_src.contains("mod domain;"),
        "main.rs must wire hexagonal modules explicitly"
    );
    assert!(
        !main_src.contains("reqwest::Client"),
        "main.rs must not perform provider HTTP probing directly"
    );
    assert!(
        !main_src.contains("tokio::process::Command::new"),
        "main.rs must not run provider process probes directly"
    );
}

#[test]
fn domain_layer_is_infrastructure_free() {
    for file in [
        "src/domain/chat.rs",
        "src/domain/usage.rs",
        "src/domain/tool_policy.rs",
        "src/domain/agent_role.rs",
        "src/domain/task.rs",
        "src/domain/skill.rs",
        "src/domain/memory.rs",
        "src/domain/evm.rs",
        "src/domain/skill_transpile.rs",
    ] {
        let src = read(file);
        assert!(
            !src.contains("reqwest")
                && !src.contains("cursive")
                && !src.contains("std::fs")
                && !src.contains("tokio::process"),
            "domain file {file} must not depend on infrastructure concerns"
        );
    }
}

#[test]
fn application_layer_is_infrastructure_free() {
    for file in [
        "src/application/chat_runtime.rs",
        "src/application/chat_commands.rs",
        "src/application/engine_runtime.rs",
        "src/application/flow_compaction.rs",
        "src/application/flow_policy.rs",
        "src/application/workspace_tools_catalog.rs",
        "src/application/prompt_budget.rs",
        "src/application/tool_use_service.rs",
        "src/application/task_orchestrator.rs",
        "src/application/fleet_runtime.rs",
        "src/application/heartbeat.rs",
        "src/application/skill_catalog.rs",
        "src/application/memory_service.rs",
        "src/application/skill_registry.rs",
        "src/application/skill_transpile.rs",
    ] {
        let src = read(file);
        assert!(
            !src.contains("reqwest")
                && !src.contains("cursive")
                && !src.contains("std::fs")
                && !src.contains("tokio::process"),
            "application file {file} must not depend on UI/provider process concerns"
        );
    }
}

#[test]
fn task_store_adapter_implements_port() {
    let src = read("src/adapters/task_store.rs");
    assert!(
        src.contains("impl TaskStorePort for InMemoryTaskStore"),
        "task_store adapter must implement TaskStorePort"
    );
}

#[test]
fn memory_store_adapter_implements_port() {
    let src = read("src/adapters/memory_store.rs");
    assert!(
        src.contains("impl MemoryStorePort for DiskVectorMemoryStore"),
        "memory_store adapter must implement MemoryStorePort"
    );
}

#[test]
fn qdrant_memory_store_adapter_exists_and_implements_port() {
    let path = Path::new("src/adapters/qdrant_memory_store.rs");
    assert!(
        path.exists(),
        "qdrant_memory_store adapter must exist"
    );
    let src = read("src/adapters/qdrant_memory_store.rs");
    assert!(
        src.contains("impl MemoryStorePort for QdrantMemoryStore"),
        "qdrant_memory_store must implement MemoryStorePort"
    );
}

#[test]
fn qdrant_does_not_leak_into_domain_or_application() {
    for file in [
        "src/domain/memory.rs",
        "src/application/memory_service.rs",
        "src/application/ports.rs",
    ] {
        let src = read(file);
        // Check for actual code imports, not documentation references.
        assert!(
            !src.contains("use qdrant") && !src.contains("qdrant_client::"),
            "{file} must not import qdrant (hexagonal boundary violation)"
        );
    }
}

#[test]
fn embedding_adapter_implements_port() {
    let src = read("src/adapters/embedding.rs");
    assert!(
        src.contains("impl EmbeddingPort for OpenRouterEmbeddingAdapter"),
        "embedding adapter must implement EmbeddingPort"
    );
}

#[test]
fn memory_modules_exist() {
    assert!(
        Path::new("src/domain/memory.rs").exists(),
        "memory domain module must exist"
    );
    assert!(
        Path::new("src/application/memory_service.rs").exists(),
        "memory_service application module must exist"
    );
    assert!(
        Path::new("src/adapters/embedding.rs").exists(),
        "embedding adapter must exist"
    );
    assert!(
        Path::new("src/adapters/memory_store.rs").exists(),
        "memory_store adapter must exist"
    );
    assert!(
        Path::new("src/adapters/memory_tool_executor.rs").exists(),
        "memory_tool_executor adapter must exist"
    );
}

#[test]
fn skill_registry_module_exists() {
    assert!(
        Path::new("src/application/skill_registry.rs").exists(),
        "skill_registry application service must exist"
    );
}

#[test]
fn orchestrator_modules_exist() {
    assert!(
        Path::new("src/domain/agent_role.rs").exists(),
        "agent_role domain module must exist"
    );
    assert!(
        Path::new("src/domain/task.rs").exists(),
        "task domain module must exist"
    );
    assert!(
        Path::new("src/application/task_orchestrator.rs").exists(),
        "task_orchestrator application service must exist"
    );
    assert!(
        Path::new("src/application/fleet_runtime.rs").exists(),
        "fleet_runtime application service must exist"
    );
    assert!(
        Path::new("src/adapters/orchestrator.rs").exists(),
        "orchestrator adapter must exist"
    );
    assert!(
        Path::new("src/adapters/task_store.rs").exists(),
        "task_store adapter must exist"
    );
}

#[test]
fn evm_domain_types_exist_and_are_infrastructure_free() {
    let path = Path::new("src/domain/evm.rs");
    assert!(path.exists(), "evm domain types file must exist");
    let src = read("src/domain/evm.rs");
    assert!(
        !src.contains("alloy") && !src.contains("reqwest") && !src.contains("std::fs"),
        "evm domain file must not depend on infrastructure (alloy, reqwest, std::fs)"
    );
}

#[test]
fn evm_does_not_leak_into_application() {
    let src = read("src/application/ports.rs");
    assert!(
        !src.contains("use alloy") && !src.contains("alloy::"),
        "ports.rs must not import alloy (hexagonal boundary violation)"
    );
}

#[test]
fn evm_signer_adapter_exists_and_implements_port() {
    let path = Path::new("src/adapters/evm_signer.rs");
    assert!(path.exists(), "evm_signer adapter must exist");
    let src = read("src/adapters/evm_signer.rs");
    assert!(
        src.contains("impl EvmPort for AlloySigner"),
        "evm_signer must implement EvmPort"
    );
}

#[test]
fn evm_tool_executor_adapter_exists_and_implements_port() {
    let path = Path::new("src/adapters/evm_tool_executor.rs");
    assert!(path.exists(), "evm_tool_executor adapter must exist");
    let src = read("src/adapters/evm_tool_executor.rs");
    assert!(
        src.contains("impl ToolExecutionPort for EvmToolExecutionAdapter"),
        "evm_tool_executor must implement ToolExecutionPort"
    );
}
