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
fn domain_layer_is_infrastructure_free() {
    for file in [
        "src/domain/chat.rs",
        "src/domain/usage.rs",
        "src/domain/tool_policy.rs",
        "src/domain/agent_role.rs",
        "src/domain/task.rs",
        "src/domain/skill.rs",
        "src/domain/memory.rs",
        "src/domain/secret_registry.rs",
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
        "src/application/skill_catalog.rs",
        "src/application/memory_service.rs",
        "src/application/skill_registry.rs",
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
