//! Tests of `config/{mod,agent,channels,memory,load}.rs`.
use super::*;

/// An unset `${VAR}` outside comments is named once (it stays literal and
/// `Config::load` warns); a set one, or one in a `#` line, is not.
#[test]
fn unset_env_references_are_named_once_outside_comments() {
    let text = "a = \"${TENGU_TEST_UNSET_9F3A}\"\n# b = \"${TENGU_TEST_UNSET_COMMENT_9F3A}\"\nc = \"${HOME}\"\nd = \"x${TENGU_TEST_UNSET_9F3A}y\"\n";
    assert_eq!(
        Config::unset_env_vars(text),
        vec!["TENGU_TEST_UNSET_9F3A".to_string()]
    );
}

/// A `tools` name that is no catalog tool and no `<server>__<tool>` of a
/// configured `[[mcp_servers]]` entry warns at load (it used to be dropped
/// silently) but still loads — a shell skill's tool is legal there.
#[test]
fn unknown_agent_tools_warn() {
    let cfg = |tools: &str| -> Config {
        toml::from_str(&format!(
                "[agents.a]\nengine = \"local\"\nmodel = \"m\"\ntools = {tools}\n\n[[mcp_servers]]\nname = \"docs\"\ntransport = \"stdio\"\ncommand = [\"x\"]\n"
            ))
            .unwrap()
    };
    let warned = |tools: &str| {
        let c = cfg(tools);
        assert!(
            !c.validation_errors().iter().any(|e| e.contains(".tools")),
            "{tools}"
        );
        c.validation_warnings()
            .into_iter()
            .filter(|w| w.starts_with("agents.a.tools:"))
            .collect::<Vec<_>>()
    };
    assert!(warned(r#"["read_fiel"]"#)[0].contains("'read_fiel'"));
    assert!(warned(r#"["other__search"]"#)[0].contains("'other__search'"));
    for ok in [r#"["read_file", "hl_ctx"]"#, r#"["docs__search"]"#] {
        assert!(warned(ok).is_empty(), "{ok}");
    }
}

#[test]
fn validate_accepts_default_config() {
    let config = Config::default();
    assert!(config.validate().is_ok());
}

/// Webhook listener — defaults are `enabled = false`, loopback bind,
/// port 7080, no endpoints. The `tengu webhooks` subcommand refuses
/// to start under these defaults; users opt in by editing TOML.
#[test]
fn webhook_config_defaults_off_and_safe() {
    let cfg = WebhookConfig::default();
    assert!(!cfg.enabled);
    assert_eq!(cfg.bind, "127.0.0.1");
    assert_eq!(cfg.port, 7080);
    assert!(cfg.endpoints.is_empty());
}

/// Webhook listener — full TOML round-trip through the standard
/// `[webhooks]` + `[webhooks.endpoints.<name>]` shape.
#[test]
fn webhook_config_parses_endpoints() {
    let toml_str = r#"
[webhooks]
enabled = true
bind = "0.0.0.0"
port = 9000

[webhooks.endpoints.github]
agent = "reviewer"
secret_env = "GITHUB_WEBHOOK_SECRET"
goal_template = "GitHub PR webhook arrived."

[webhooks.endpoints.local_test]
agent = "main"
secret = "literal-dev-secret"
"#;
    let parsed: toml::Value = toml::from_str(toml_str).unwrap();
    let webhooks: WebhookConfig = parsed.get("webhooks").unwrap().clone().try_into().unwrap();
    assert!(webhooks.enabled);
    assert_eq!(webhooks.bind, "0.0.0.0");
    assert_eq!(webhooks.port, 9000);
    assert_eq!(webhooks.endpoints.len(), 2);

    let github = webhooks.endpoints.get("github").unwrap();
    assert_eq!(github.agent, "reviewer");
    assert_eq!(github.secret_env.as_deref(), Some("GITHUB_WEBHOOK_SECRET"));
    assert!(github.secret.is_none());
    assert_eq!(github.goal_template, "GitHub PR webhook arrived.");

    let local = webhooks.endpoints.get("local_test").unwrap();
    assert_eq!(local.secret.as_deref(), Some("literal-dev-secret"));
    assert!(local.secret_env.is_none());
    // Default goal_template applied when omitted.
    assert!(local.goal_template.contains("webhook arrived"));
}

/// `within_session_output_top_k` defaults to 0 (off), like
/// `cross_session_msg_top_k`.
#[test]
fn memory_config_within_session_output_top_k_defaults_off() {
    let cfg = MemoryConfig::default();
    assert_eq!(cfg.within_session_output_top_k, 0);
    assert_eq!(cfg.cross_session_msg_top_k, 0);
    // Replan-side cross-plan recall stays on by default — it's only
    // wired into the replan() path, never plan().
    assert_eq!(cfg.cross_plan_top_k, 5);
}

/// The field round-trips through TOML and keeps the default when
/// absent: a `[memory]` block without it still parses.
#[test]
fn memory_config_within_session_output_top_k_optional_in_toml() {
    let toml_str = "[memory]\nenabled = true\n";
    let parsed: toml::Value = toml::from_str(toml_str).unwrap();
    let mem_section = parsed.get("memory").unwrap().clone();
    let mem: MemoryConfig = mem_section.try_into().unwrap();
    assert_eq!(mem.within_session_output_top_k, 0);

    let toml_str_with = "[memory]\nenabled = true\nwithin_session_output_top_k = 4\n";
    let parsed: toml::Value = toml::from_str(toml_str_with).unwrap();
    let mem_section = parsed.get("memory").unwrap().clone();
    let mem: MemoryConfig = mem_section.try_into().unwrap();
    assert_eq!(mem.within_session_output_top_k, 4);
}

#[test]
fn validate_rejects_output_cap_above_context_override() {
    let mut config = Config::default();
    let main = config.agents.get_mut("main").expect("main agent");
    main.limits.context_window = 4_096;
    main.limits.max_output_tokens_per_turn = Some(8_192);

    let err = config.validate().expect_err("expected validation error");
    assert!(err.to_string().contains("cannot exceed context_window"));
}

#[test]
fn validate_rejects_unknown_engine() {
    let mut config = Config::default();
    let main = config.agents.get_mut("main").expect("main agent");
    main.engine = "ollama".to_string();

    let err = config.validate().expect_err("expected validation error");
    assert!(err.to_string().contains("engine must be one of"));
}

#[test]
fn removed_top_level_keys_fail_the_load() {
    for key in [
        "runtime_profile = \"auto\"",
        "[hub]\nport = 7070",
        "[orchestrator]\nagent = \"main\"\nengine = \"rag\"",
    ] {
        let text = format!("{key}\n[agents.main]\nengine = \"openrouter\"\nmodel = \"m\"\n");
        let err = toml::from_str::<Config>(&text).expect_err(key);
        assert!(err.to_string().contains("unknown field"), "{key}: {err}");
    }
}

#[test]
fn memory_config_ignores_removed_qdrant_keys() {
    // Older sandbox configs may still carry the Qdrant-era keys;
    // `MemoryConfig` has no `deny_unknown_fields`, so they must parse
    // cleanly (and be ignored).
    let toml_str = r#"
enabled = true
backend = "disk"
qdrant_url = "http://localhost:6334"
vector_size = 1536
ttl_days = 7
"#;
    let cfg: MemoryConfig = toml::from_str(toml_str).unwrap();
    assert!(cfg.enabled);
}

#[test]
fn validate_accepts_claude_code_engine() {
    let mut config = Config::default();
    let main = config.agents.get_mut("main").expect("main agent");
    main.engine = "claude_code".to_string();
    main.claude_code = Some(AgentClaudeCodeConfig {
        builtin_tools_profile: "editor_shell".to_string(),
    });
    assert!(config.validate().is_ok());
}

#[test]
fn local_engine_parses_with_and_without_block() {
    let bare: Config = toml::from_str(
        r#"
            [agents.main]
            default = true
            engine = "local"
            model = "unsloth/gemma-4-26B-A4B-it-GGUF:UD-Q4_K_XL"
            "#,
    )
    .unwrap();
    assert!(bare.validate().is_ok());
    assert!(bare.agents["main"].local.is_none());

    let custom: Config = toml::from_str(
        r#"
            [agents.main]
            default = true
            engine = "local"
            model = "qwen3"
            [agents.main.local]
            base_url = "http://127.0.0.1:11434"
            api_key_env = ""
            "#,
    )
    .unwrap();
    let local = custom.agents["main"].local.as_ref().unwrap();
    assert_eq!(local.base_url, "http://127.0.0.1:11434");
    assert!(toml::from_str::<Config>(
        "[agents.main]\nengine = \"local\"\nmodel = \"m\"\n[agents.main.local]\nport = 1\n"
    )
    .is_err());
}

#[test]
fn validate_rejects_bad_claude_profile() {
    let mut config = Config::default();
    let main = config.agents.get_mut("main").expect("main agent");
    main.engine = "claude_code".to_string();
    main.claude_code = Some(AgentClaudeCodeConfig {
        builtin_tools_profile: "dangerous".to_string(),
    });

    let err = config.validate().expect_err("expected validation error");
    assert!(err
        .to_string()
        .contains("builtin_tools_profile must be one of"));
}

/// Profiles are read trimmed (`BuiltinToolsProfile::parse`); an unknown
/// value fails the load on any agent's `[agents.<a>.claude_code]` block.
#[test]
fn claude_profile_is_trimmed_and_unknown_fails_on_any_block() {
    let with = |engine: &str, profile: &str| {
        let mut config = Config::default();
        let main = config.agents.get_mut("main").expect("main agent");
        main.engine = engine.to_string();
        main.claude_code = Some(AgentClaudeCodeConfig {
            builtin_tools_profile: profile.to_string(),
        });
        config.validate()
    };
    assert!(with("claude_code", " none").is_ok());
    assert!(with("claude_code", "editor\n").is_ok());
    assert!(with("claude_code", "research").is_ok());
    for engine in ["claude_code", "openrouter"] {
        let err = with(engine, "nnone").expect_err(engine).to_string();
        assert!(
            err.contains("builtin_tools_profile must be one of"),
            "{err}"
        );
    }
}

/// `engine = "codex"` with its optional block: defaults when absent, a
/// known `sandbox` (trimmed), unknown keys refused at parse.
#[test]
fn codex_agent_block_parses_and_validates() {
    let parse = |block: &str| {
        toml::from_str::<Config>(&format!(
            "[agents.cx]\ndefault = true\nengine = \"codex\"\nmodel = \"gpt-5.5\"\n{block}"
        ))
    };
    let plain = parse("").expect("no block");
    assert!(plain.agents["cx"].codex.is_none());
    assert!(plain.validate().is_ok(), "codex is a known engine");
    let set = parse("[agents.cx.codex]\nsandbox = \" workspace-write\"\ncli_path = \"/opt/codex\"")
        .expect("block");
    let cx = set.agents["cx"].codex.clone().unwrap();
    assert_eq!(cx.cli_path, "/opt/codex");
    assert!(set.validate().is_ok());
    let defaults = parse("[agents.cx.codex]").expect("empty block");
    assert_eq!(
        defaults.agents["cx"].codex.as_ref().unwrap().sandbox,
        "read-only"
    );
    let bad = parse("[agents.cx.codex]\nsandbox = \"danger-full-access\"").expect("parses");
    let err = bad.validate().expect_err("unknown sandbox").to_string();
    assert!(
        err.contains("agents.cx.codex.sandbox must be one of"),
        "{err}"
    );
    assert!(parse("[agents.cx.codex]\nsandbox_mode = \"read-only\"").is_err());
    // forward_env: plain names pass; the master password, the seal-proxy
    // session and its [keys.env] exports, API keys and CODEX_* are refused.
    let fwd = parse("[agents.cx.codex]\nforward_env = [\"OPENROUTER_API_KEY\", \"MY_VAR\"]")
        .expect("forward_env");
    assert!(fwd.validate().is_ok());
    for bad in [
        "TENGU_MASTER_PASSWORD",
        "TENGU_KEYS_SESSION_TOKEN",
        "TENGU_KEYS_PROXY",
        "OPENAI_API_KEY",
        "CODEX_API_KEY",
        "1X",
        "A-B",
        "",
    ] {
        let c = parse(&format!("[agents.cx.codex]\nforward_env = [\"{bad}\"]")).expect("parses");
        let err = c.validate().expect_err(bad).to_string();
        assert!(err.contains("agents.cx.codex.forward_env"), "{bad}: {err}");
    }
    let mut routed =
        parse("[agents.cx.codex]\nforward_env = [\"OPENROUTER_BASE_URL\"]").expect("parses");
    routed
        .keys
        .env
        .insert("OPENROUTER_BASE_URL".into(), "openrouter".into());
    let err = routed
        .validate()
        .expect_err("[keys.env] export")
        .to_string();
    assert!(err.contains("seal-proxy session"), "{err}");
}

#[test]
fn existing_config_no_scopes_parses() {
    let toml_str = r#"
            [agents.main]
            default = true
            engine = "openrouter"
            model = "anthropic/claude-sonnet-4.6"
        "#;
    let config: Config = toml::from_str(toml_str).expect("should parse");
    assert!(config.validate().is_ok());
    assert!(config.agents["main"].scopes.is_empty());
    assert!(config.default_scopes.is_empty());
}

#[test]
fn agent_scopes_roundtrip() {
    let toml_str = r#"
            [agents.main]
            default = true
            engine = "openrouter"
            model = "anthropic/claude-sonnet-4.6"

            [agents.main.scopes.write_file]
            fs_roots = ["./research", "./drafts"]

            [agents.main.scopes.http_request]
            net_hosts = ["api.linear.app", "*.anthropic.com"]
            env_reads = ["LINEAR_API_KEY"]
        "#;
    let config: Config = toml::from_str(toml_str).expect("should parse");
    let scopes = &config.agents["main"].scopes;
    assert_eq!(scopes["write_file"].fs_roots.len(), 2);
    assert_eq!(scopes["http_request"].net_hosts.len(), 2);
    assert_eq!(scopes["http_request"].env_reads.len(), 1);
}

#[test]
fn default_scopes_roundtrip() {
    let toml_str = r#"
            [agents.main]
            default = true
            engine = "openrouter"
            model = "anthropic/claude-sonnet-4.6"

            [default_scopes.read_file]
            fs_roots = ["./"]

            [default_scopes.run_command]
            shell_bins = ["git", "rg", "cargo"]
            fs_roots = ["./"]
        "#;
    let config: Config = toml::from_str(toml_str).expect("should parse");
    assert_eq!(config.default_scopes["read_file"].fs_roots.len(), 1);
    assert_eq!(config.default_scopes["run_command"].shell_bins.len(), 3);
}

#[test]
fn resolve_scope_agent_overrides_default() {
    let toml_str = r#"
            [default_scopes.write_file]
            fs_roots = ["./global"]

            [agents.main]
            default = true
            engine = "openrouter"
            model = "anthropic/claude-sonnet-4.6"

            [agents.main.scopes.write_file]
            fs_roots = ["./agent-specific"]
        "#;
    let config: Config = toml::from_str(toml_str).expect("should parse");
    let scope = config.resolve_scope("main", "write_file").unwrap();
    assert_eq!(scope.fs_roots.len(), 1);
    assert_eq!(scope.fs_roots[0].to_str().unwrap(), "./agent-specific");
}

#[test]
fn resolve_scope_falls_back_to_default() {
    let toml_str = r#"
            [default_scopes.read_file]
            fs_roots = ["./"]

            [agents.main]
            default = true
            engine = "openrouter"
            model = "anthropic/claude-sonnet-4.6"
        "#;
    let config: Config = toml::from_str(toml_str).expect("should parse");
    let scope = config.resolve_scope("main", "read_file").unwrap();
    assert_eq!(scope.fs_roots.len(), 1);
}

/// `Config::load` names the sandbox of the file in every agent's
/// sections — the paper ledger's account owner; another file is
/// `default`, a config built in code too.
#[test]
fn load_names_the_sandbox_of_the_file() {
    let tmp = tempfile::TempDir::new().unwrap();
    let dir = tmp.path().join("sandboxes/xm-own");
    std::fs::create_dir_all(&dir).unwrap();
    let toml = "[agents.main]\ndefault = true\nengine = \"openrouter\"\nmodel = \"m\"\n\
                    [agents.exec]\nengine = \"openrouter\"\nmodel = \"m\"\n";
    std::fs::write(dir.join("config.toml"), toml).unwrap();
    let c = Config::load(&dir.join("config.toml")).unwrap();
    assert_eq!(c.loaded_from.as_deref(), Some(&*dir.join("config.toml")));
    for agent in c.agents.values() {
        assert_eq!(agent.sandbox.sandbox.as_deref(), Some("xm-own"));
        assert_eq!(agent.sandbox.owner(), "xm-own");
    }
    std::fs::write(tmp.path().join("config.toml"), toml).unwrap();
    let c = Config::load(&tmp.path().join("config.toml")).unwrap();
    assert_eq!(c.agents["main"].sandbox.owner(), "default");
    let mut c = Config::default();
    c.fold_default_scopes();
    assert_eq!(c.agents["main"].sandbox.owner(), "default");
}

/// `source_sha256` = `toml_digest` of the raw file: 64 hex, blind to
/// comments and layout, changed by any value; `None` built in code.
#[test]
fn load_records_the_source_sha256() {
    let tmp = tempfile::TempDir::new().unwrap();
    let path = tmp.path().join("config.toml");
    let toml = "[agents.main]\ndefault = true\nengine = \"openrouter\"\nmodel = \"m\"\n";
    std::fs::write(&path, toml).unwrap();
    let a = Config::load(&path).unwrap().source_sha256.unwrap();
    assert_eq!(a.len(), 64);
    assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
    assert_eq!(
        Some(a.clone()),
        crate::domain::lineage::pins::toml_digest(toml).ok()
    );
    std::fs::write(&path, format!("# a comment\n\n{toml}")).unwrap();
    assert_eq!(Config::load(&path).unwrap().source_sha256, Some(a.clone()));
    std::fs::write(&path, toml.replace("\"m\"", "\"m2\"")).unwrap();
    assert_ne!(Config::load(&path).unwrap().source_sha256, Some(a));
    assert_eq!(Config::default().source_sha256, None);
}

#[test]
fn fold_default_scopes_materialises_fallback_per_agent() {
    let toml_str = r#"
            [default_scopes.read_file]
            fs_roots = ["~/ws"]

            [default_scopes.write_file]
            fs_roots = ["./global"]

            [agents.main]
            default = true
            engine = "openrouter"
            model = "anthropic/claude-sonnet-4-6"

            [agents.main.scopes.write_file]
            fs_roots = ["./agent-specific"]
        "#;
    let mut config: Config = toml::from_str(toml_str).expect("should parse");
    config.fold_default_scopes();
    let scopes = &config.agents["main"].scopes;
    // Unconfigured tool inherits the default entry.
    assert_eq!(scopes["read_file"].fs_roots.len(), 1);
    // `~` is expanded so `ToolScope::check_fs` matches canonical paths.
    assert!(!scopes["read_file"].fs_roots[0].starts_with("~"));
    // Per-agent entry wins wholesale over the default.
    assert_eq!(scopes["write_file"].fs_roots.len(), 1);
    assert_eq!(
        scopes["write_file"].fs_roots[0].to_str().unwrap(),
        "./agent-specific"
    );
}

/// `[xmarket]` resolves to one state dir that every agent's tools see
/// through the shared `AgentConfig::sandbox` handle; absent = none.
#[test]
fn fold_shares_sandbox_sections_with_every_agent() {
    let toml_str = r#"
            [xmarket]
            state = "xmarket-weekend"

            [agents.main]
            default = true
            engine = "openrouter"
            model = "anthropic/claude-sonnet-4-6"

            [agents.exec]
            engine = "openrouter"
            model = "anthropic/claude-haiku-4.5"
        "#;
    let mut config: Config = toml::from_str(toml_str).expect("should parse");
    config.validate().expect("valid");
    config.fold_default_scopes();
    let dir = config.agents["main"].sandbox.xm_state_dir.clone().unwrap();
    assert!(dir.ends_with("state/xmarket-weekend"), "{}", dir.display());
    assert!(std::sync::Arc::ptr_eq(
        &config.agents["main"].sandbox,
        &config.agents["exec"].sandbox
    ));

    let mut plain = Config::default();
    plain.fold_default_scopes();
    assert!(plain.agents["main"].sandbox.xm_state_dir.is_none());

    let bad: Config = toml::from_str(
        "[xmarket]\nstate = \"../x\"\n[agents.main]\nengine = \"openrouter\"\nmodel = \"m\"\n",
    )
    .unwrap();
    assert!(bad.validate().is_err());
}

#[test]
fn resolve_scope_none_when_absent() {
    let config = Config::default();
    assert!(config.resolve_scope("main", "write_file").is_none());
}

#[test]
fn validate_passes_with_scopes() {
    let toml_str = r#"
            [default_scopes.read_file]
            fs_roots = ["./"]

            [agents.main]
            default = true
            engine = "openrouter"
            model = "anthropic/claude-sonnet-4.6"

            [agents.main.scopes.write_file]
            fs_roots = ["./research"]
        "#;
    let config: Config = toml::from_str(toml_str).expect("should parse");
    assert!(config.validate().is_ok());
}

#[test]
fn agent_block_rejects_unknown_keys() {
    // The old agents/*.toml spec was strict (`max_turns`, `timeout_secs`,
    // `skills`, `sandbox`); the merged schema keeps a typo from being
    // silently dropped — and a removed knob (`default_lens`, `lens`)
    // from looking live.
    for bad in [
        "max_turns = 20",
        "timeout_secs = 180",
        "sandbox = \"x\"",
        "tool = []",
        "default_lens = \"eco\"",
        "lens = {}",
    ] {
        let toml = format!("[agents.x]\nengine = \"openrouter\"\nmodel = \"m\"\n{bad}\n");
        let err = toml::from_str::<Config>(&toml).unwrap_err().to_string();
        let key = bad.split(' ').next().unwrap();
        assert!(err.contains(key), "{bad}: {err}");
    }
}

#[test]
fn agent_block_accepts_subagent_fields_and_skills_alias() {
    let cfg: Config = toml::from_str(
        "[agents.researcher]\nengine = \"openrouter\"\nmodel = \"m\"\n\
             description = \"web research\"\nexample_queries = [\"btc price?\"]\n\
             tools = [\"http_request\", \"shared_cache\"]\nskills = [\"a\", \"b\"]\n\
             [agents.researcher.limits]\nstep_timeout_secs = 42\n",
    )
    .unwrap();
    let a = &cfg.agents["researcher"];
    assert_eq!(a.description.as_deref(), Some("web research"));
    assert_eq!(a.skill_packages, vec!["a", "b"]);
    assert_eq!(a.tools, vec!["http_request", "shared_cache"]);
    assert_eq!(a.limits.step_timeout_secs, 42);
    assert!(cfg.validate().is_ok());
}

#[test]
fn validate_rejects_blank_description_and_zero_step_timeout() {
    let blank: Config = toml::from_str(
        "[agents.x]\nengine = \"openrouter\"\nmodel = \"m\"\ndescription = \"  \"\n",
    )
    .unwrap();
    let err = blank.validate().unwrap_err().to_string();
    assert!(err.contains("description must not be empty"), "{err}");
    let zero: Config = toml::from_str(
            "[agents.x]\nengine = \"openrouter\"\nmodel = \"m\"\n[agents.x.limits]\nstep_timeout_secs = 0\n",
        )
        .unwrap();
    let err = zero.validate().unwrap_err().to_string();
    assert!(err.contains("step_timeout_secs"), "{err}");
}

#[test]
fn local_agent_on_the_default_context_window_warns_only() {
    let cfg: Config = toml::from_str(
        "[agents.gemma]\nengine = \"local\"\nmodel = \"gemma4:latest\"\n\
             [agents.sized]\nengine = \"local\"\nmodel = \"gemma4:latest\"\n\
             [agents.sized.limits]\ncontext_window = 16384\n\
             [agents.main]\nengine = \"openrouter\"\nmodel = \"m\"\n",
    )
    .unwrap();
    assert!(cfg.validate().is_ok(), "a warning, not an error");
    let warnings: Vec<String> = cfg
        .validation_warnings()
        .into_iter()
        .filter(|w| w.contains("context_window"))
        .collect();
    assert_eq!(warnings.len(), 1, "{warnings:?}");
    assert!(
        warnings[0].starts_with("agents.gemma.limits.context_window is the 1000000 default"),
        "{}",
        warnings[0]
    );
}

/// A routable agent without `workspace` whose tools write into it (files,
/// skills) is warned about: its plan steps run in a temp dir removed
/// after each. Read-only tools, a pinned workspace or no `description`
/// (never a plan step): no warning.
#[test]
fn routable_agent_writing_into_a_step_temp_dir_warns() {
    let cfg: Config = toml::from_str(
        "[agents.learner]\nengine = \"openrouter\"\nmodel = \"m\"\ndescription = \"d\"\n\
             tools = [\"view_skill\", \"manage_skill\", \"read_file\"]\n\
             [agents.reader]\nengine = \"claude_code\"\nmodel = \"m\"\ndescription = \"d\"\n\
             tools = [\"http_request\", \"read_file\"]\n\
             [agents.everything]\nengine = \"openrouter\"\nmodel = \"m\"\ndescription = \"d\"\n\
             [agents.pinned]\nengine = \"openrouter\"\nmodel = \"m\"\ndescription = \"d\"\n\
             tools = [\"manage_skill\"]\nworkspace = \"/srv/ws\"\n\
             [agents.main]\nengine = \"openrouter\"\nmodel = \"m\"\ntools = [\"write_file\"]\n",
    )
    .unwrap();
    let warnings: Vec<String> = cfg
        .validation_warnings()
        .into_iter()
        .filter(|w| w.contains("no `workspace`"))
        .collect();
    assert_eq!(warnings.len(), 2, "{warnings:?}");
    assert!(
        warnings[0].starts_with("agents.everything has no `workspace`")
            && warnings[0].contains("[\"write_file\"]"),
        "{}",
        warnings[0]
    );
    assert!(
        warnings[1].starts_with("agents.learner has no `workspace`")
            && warnings[1].contains("[\"manage_skill\"]"),
        "{}",
        warnings[1]
    );
}

/// The removed approval keys (`tool_approvals` / `approve_only` gated
/// nothing) fail the load naming the key, so no operator reads them as a
/// guard; `[telegram]` without them loads.
#[test]
fn telegram_approval_keys_fail_the_load() {
    let agent = "[agents.m]\nengine = \"openrouter\"\nmodel = \"m\"\n";
    for key in ["tool_approvals = false", "approve_only = [\"run_command\"]"] {
        let err = toml::from_str::<Config>(&format!("[telegram]\n{key}\n{agent}"))
            .unwrap_err()
            .to_string();
        let name = key.split_whitespace().next().unwrap();
        assert!(err.contains(&format!("unknown field `{name}`")), "{err}");
    }
    let ok: Config =
        toml::from_str(&format!("[telegram]\nallowed_users = [\"1\"]\n{agent}")).unwrap();
    assert_eq!(ok.telegram.allowed_users, ["1"]);
}
