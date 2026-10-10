use std::collections::BTreeSet;

use serde_json::{json, Value};

use super::compile::compile;
use super::palette::{engines, palette, CONNECTIONS};
use super::template::{template, TEMPLATES};
use super::*;
use crate::domain::blueprint::{Blueprint, State};

fn tool(name: &str, needs_memory: bool) -> OfferedTool {
    OfferedTool {
        name: name.into(),
        description: format!("{name} tool"),
        parameters: json!({"type": "object"}),
        opt_in: false,
        needs_memory,
        group: palette::tool_group(name),
    }
}

fn facts() -> Facts {
    Facts {
        tools: vec![
            tool("read_file", false),
            tool("write_file", false),
            tool("list_directory", false),
            tool("http_request", false),
            tool("memory_search", true),
        ],
        skills: vec![("git".into(), "git helper".into())],
        keys_supported: false,
    }
}

fn bp(nodes: Value, edges: Value) -> Blueprint {
    serde_json::from_value(json!({
        "schema_version": 1, "sandbox": "t", "nodes": nodes, "edges": edges
    }))
    .unwrap()
}

fn sandbox_node() -> Value {
    json!({"id":"sb","kind":"sandbox","fields":{"network":"open"}})
}

fn agent_node(id: &str, name: &str, extra: Value) -> Value {
    let mut fields =
        json!({"name": name, "engine": "openrouter", "model": "anthropic/claude-sonnet-4-6"});
    if let Value::Object(m) = extra {
        for (k, v) in m {
            fields[k] = v;
        }
    }
    json!({"id": id, "kind": "agent", "fields": fields})
}

/// Writes `toml` as `<tmp>/sandboxes/t/config.toml` and loads it.
fn loads(toml: &str) -> LoadReport {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("sandboxes").join("t").join("config.toml");
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, toml).unwrap();
    load_report(&path)
}

#[test]
fn engines_match_config_engines() {
    let described: BTreeSet<&str> = engines().iter().map(|e| e.id).collect();
    let known: BTreeSet<&str> = crate::config::ENGINES.iter().copied().collect();
    assert_eq!(described, known);
}

#[test]
fn every_connection_names_known_kinds() {
    let p = palette(&facts());
    let kinds: BTreeSet<&str> = p.kinds.iter().map(|k| k.kind).collect();
    for c in CONNECTIONS {
        assert!(kinds.contains(c.from) && kinds.contains(c.to), "{c:?}");
    }
    // Every item is a known kind; tools and skills come from the facts.
    assert!(p.items.iter().all(|i| kinds.contains(i.kind)));
    assert!(p.items.iter().any(|i| i.id == "tool.http_request"));
    assert!(p.items.iter().any(|i| i.id == "skill.git"));
    let proxy = p.items.iter().find(|i| i.kind == "seal_proxy").unwrap();
    assert!(proxy.disabled.is_some(), "no [keys] in this build");
}

#[test]
fn every_template_compiles_clean_and_loads() {
    for (name, _) in TEMPLATES {
        let b = template(name, "t").unwrap();
        let c = compile(&b, &facts(), None);
        assert!(c.status.ok, "{name}: {:?}\n{}", c.status.issues, c.toml);
        let r = loads(&c.toml);
        assert!(r.ok, "{name}: {:?}\n{}", r.errors, c.toml);
    }
    assert!(template("nope", "t").is_none());
}

/// Every field of every kind, set to a non-default value, compiles to TOML
/// the real `deny_unknown_fields` structs accept — a misspelled key in the
/// compiler fails here.
#[test]
fn every_field_round_trips_through_the_config_structs() {
    let mut nodes = vec![json!({"id":"sb","kind":"sandbox","fields":{
        "network":"open","allow_hosts":["openrouter.ai"],"memory":true,"studio_control":true}})];
    let mut edges = vec![];
    nodes.push(json!({"id":"orch","kind":"orchestrator","fields":{"max_attempts_per_step":4,"max_replans":1}}));
    for (i, e) in crate::config::ENGINES.iter().enumerate() {
        let name = format!("a{i}");
        nodes.push(agent_node(
            &format!("n{i}"),
            &name,
            json!({
                "engine": e, "model": "m", "default": i == 0,
                "display_name": "D", "instructions": "I", "description": "Does X, not Y.",
                "example_queries": ["q"], "max_tool_rounds": 7, "step_timeout_secs": 90,
                "context_window": 32768, "local_base_url": "http://127.0.0.1:11434/v1",
                "local_api_key_env": "LOCAL_KEY", "claude_tools": "none", "codex_sandbox": "read-only"
            }),
        ));
        if i == 0 {
            edges.push(json!({"id":"plans","from":"orch","to":"n0"}));
        } else {
            edges.push(json!({"id": format!("d{i}"), "from":"n0","to": format!("n{i}")}));
        }
    }
    nodes.push(
        json!({"id":"tl","kind":"tool","fields":{"tool":"http_request","restrict":true,
        "fs_roots":["/tmp"],"net_hosts":["api.example.com"],"env_reads":["X_KEY"]}}),
    );
    nodes.push(json!({"id":"sk","kind":"skill","fields":{"skill":"git"}}));
    nodes.push(json!({"id":"ws","kind":"workspace","fields":{"path":"/tmp/tengu-builder-test","confine":true}}));
    nodes.push(json!({"id":"sec","kind":"secret","fields":{"env":"X_KEY","backend":"env"}}));
    nodes.push(json!({"id":"tg","kind":"telegram","fields":{"allowed_users":["123"]}}));
    nodes
        .push(json!({"id":"wh","kind":"webhook","fields":{"name":"github","goal_template":"Go."}}));
    nodes.push(
        json!({"id":"whs","kind":"secret","fields":{"env":"WEBHOOK_SECRET","backend":"vault"}}),
    );
    edges.extend([
        json!({"id":"u1","from":"n1","to":"tl"}),
        json!({"id":"l1","from":"n1","to":"sk"}),
        json!({"id":"w1","from":"n1","to":"ws"}),
        json!({"id":"g1","from":"sec","to":"tl"}),
        json!({"id":"c1","from":"tg","to":"n0"}),
        json!({"id":"t1","from":"wh","to":"n1"}),
        json!({"id":"s1","from":"whs","to":"wh"}),
    ]);
    let b = bp(Value::Array(nodes), Value::Array(edges));
    let c = compile(&b, &facts(), None);
    let errors: Vec<_> = c
        .status
        .issues
        .iter()
        .filter(|i| i.level == crate::domain::blueprint::Level::Error)
        .collect();
    assert!(errors.is_empty(), "{errors:?}\n{}", c.toml);
    let cfg: Config = toml::from_str(&c.toml).unwrap_or_else(|e| panic!("{e}\n{}", c.toml));
    let r = loads(&c.toml);
    assert!(r.ok, "{:?}\n{}", r.errors, c.toml);
    let a1 = &cfg.agents["a1"];
    assert_eq!(a1.tools, vec!["http_request"]);
    assert_eq!(a1.skill_packages, vec!["git"]);
    assert_eq!(a1.limits.max_tool_rounds, 7);
    assert!(a1.description.is_some(), "delegated ⇒ routable");
    assert!(
        cfg.agents["a0"].description.is_none(),
        "the planner is not routable"
    );
    assert!(cfg.agents["a0"].default, "the bot's agent is the default");
    let scope = &a1.scopes["http_request"];
    assert_eq!(scope.env_reads, vec!["X_KEY"]);
    assert_eq!(scope.net_hosts, vec!["api.example.com"]);
    assert!(
        !a1.scopes.contains_key("read_file"),
        "no read_file scope: the agent's tools do not list it"
    );
    assert_eq!(cfg.orchestrator.as_ref().unwrap().agent, "a0");
    assert_eq!(
        cfg.webhooks.endpoints["github"].secret_env.as_deref(),
        Some("WEBHOOK_SECRET")
    );
    assert!(cfg.telegram.enabled && cfg.memory.enabled && cfg.studio.control);
    assert_eq!(cfg.egress.allow_hosts, vec!["openrouter.ai"]);
}

#[test]
fn wires_turn_live_or_red_with_a_reason() {
    let b = bp(
        json!([
            sandbox_node(),
            agent_node("a", "alpha", json!({"default": true})),
            {"id":"t","kind":"tool","fields":{"tool":"http_request"}},
            {"id":"k","kind":"skill","fields":{"skill":"git"}},
            {"id":"s","kind":"secret","fields":{"env":"WRONG_KEY","backend":"vault"}}
        ]),
        json!([
            {"id":"ok","from":"a","to":"t"},
            {"id":"bad","from":"t","to":"k"},
            {"id":"warn","from":"s","to":"a"}
        ]),
    );
    let c = compile(&b, &facts(), None);
    assert_eq!(c.status.edges["ok"].state, State::Live);
    assert_eq!(c.status.edges["ok"].writes, "agents.alpha.tools");
    assert_eq!(c.status.edges["bad"].state, State::Error);
    assert!(c.status.edges["bad"].issues[0]
        .message
        .contains("no wire from Tool to Skill"));
    assert_eq!(c.status.edges["warn"].state, State::Warn);
    assert_eq!(c.status.nodes["a"].title, "alpha");
}

#[test]
fn delegation_needs_the_planner_and_a_description() {
    let b = bp(
        json!([
            sandbox_node(),
            agent_node("p", "planner", json!({"default": true})),
            agent_node("w", "worker", json!({})),
            agent_node("x", "other", json!({}))
        ]),
        json!([
            {"id":"d1","from":"p","to":"w"},
            {"id":"d2","from":"x","to":"w"}
        ]),
    );
    let c = compile(&b, &facts(), None);
    // No orchestrator: nobody is the planner.
    assert_eq!(c.status.edges["d1"].state, State::Error);
    assert!(c.status.edges["d2"].issues[0]
        .message
        .contains("only the planner"));

    let b = bp(
        json!([
            sandbox_node(),
            {"id":"o","kind":"orchestrator","fields":{}},
            agent_node("p", "planner", json!({"default": true})),
            agent_node("w", "worker", json!({}))
        ]),
        json!([
            {"id":"plans","from":"o","to":"p"},
            {"id":"d1","from":"p","to":"w"}
        ]),
    );
    let c = compile(&b, &facts(), None);
    assert_eq!(c.status.edges["d1"].state, State::Error);
    let w = &c.status.nodes["w"];
    assert!(w
        .issues
        .iter()
        .any(|i| i.field.as_deref() == Some("description")));
}

#[test]
fn cloudflare_store_needs_a_build_with_keys_and_the_proxy_card() {
    let b = bp(
        json!([
            sandbox_node(),
            agent_node("a", "alpha", json!({"default": true})),
            {"id":"s","kind":"secret","fields":{"env":"OPENROUTER_API_KEY","backend":"cloudflare",
              "keys_env":["OPENROUTER_BASE_URL=openrouter"]}}
        ]),
        json!([{"id":"k","from":"s","to":"a"}]),
    );
    let c = compile(&b, &facts(), None);
    let msgs: Vec<&str> = c.status.nodes["s"]
        .issues
        .iter()
        .map(|i| i.message.as_str())
        .collect();
    assert!(msgs.iter().any(|m| m.contains("no [keys]")), "{msgs:?}");
    assert!(
        msgs.iter().any(|m| m.contains("seal proxy card")),
        "{msgs:?}"
    );
    // With a build that knows [keys]: the proxy card + routes are emitted.
    let mut f = facts();
    f.keys_supported = true;
    let b = bp(
        json!([
            sandbox_node(),
            agent_node("a", "alpha", json!({"default": true})),
            {"id":"px","kind":"seal_proxy","fields":{"proxy":"https://tengu-seal.example.workers.dev"}},
            {"id":"s","kind":"secret","fields":{"env":"OPENROUTER_API_KEY","backend":"cloudflare",
              "keys_env":["OPENROUTER_BASE_URL=openrouter","OPENROUTER_API_KEY=@session"],
              "strip":["TELEGRAM_BOT_TOKEN"]}}
        ]),
        json!([{"id":"k","from":"s","to":"a"},{"id":"sv","from":"s","to":"px"}]),
    );
    let c = compile(&b, &f, None);
    assert!(c.status.ok, "{:?}", c.status.issues);
    assert!(c
        .toml
        .contains("[keys]\nproxy = \"https://tengu-seal.example.workers.dev\""));
    assert!(c.toml.contains("strip = [\"TELEGRAM_BOT_TOKEN\"]"));
    assert!(c.toml.contains(
        "[keys.env]\nOPENROUTER_API_KEY = \"@session\"\nOPENROUTER_BASE_URL = \"openrouter\""
    ));
    assert!(!c.toml.contains("sk-"), "never a key value");
}

#[test]
fn sections_not_on_the_canvas_are_kept() {
    let current = "runtime_profile = \"minimal\"\n\n[hub]\nport = 9999\n\n[agents.gone]\nengine = \"openrouter\"\nmodel = \"m\"\n";
    let b = template("blank", "t").unwrap();
    let c = compile(&b, &facts(), Some(current));
    assert_eq!(c.kept, vec!["hub", "runtime_profile"]);
    assert!(!c.toml.contains("[agents.gone]"), "agents are owned");
    let r = loads(&c.toml);
    assert!(r.ok, "{:?}\n{}", r.errors, c.toml);
    let cfg: Config = toml::from_str(&c.toml).unwrap();
    assert_eq!(cfg.runtime_profile, "minimal");
    assert_eq!(cfg.hub.port, 9999);
}

#[test]
fn checklist_names_keys_by_name_only() {
    let b = template("team", "t").unwrap();
    let c = compile(&b, &facts(), None);
    let or = c
        .secrets
        .iter()
        .find(|s| s.env == "OPENROUTER_API_KEY")
        .unwrap();
    assert_eq!(or.backend, "vault");
    for a in ["planner", "researcher", "writer"] {
        assert!(or.used_by.iter().any(|u| u == a), "{:?}", or.used_by);
    }
}

#[test]
fn view_only_sandboxes() {
    assert!(editable("[generation]\nid = \"W1\"\n").is_some());
    assert!(editable("[risk]\nmax = 1\n").is_some());
    assert!(editable("[solana]\nsigner_key_file = \"/k\"\n").is_some());
    assert!(editable("[agents.a]\nengine = \"openrouter\"\nmodel = \"m\"\n").is_none());
    assert!(sandbox_name_error("my-team").is_none());
    for bad in ["", "My", "-x", "a b", "../x", &"a".repeat(41)] {
        assert!(sandbox_name_error(bad).is_some(), "{bad}");
    }
}

#[test]
fn unknown_tool_and_bad_names_are_card_errors() {
    let b = bp(
        json!([
            sandbox_node(),
            agent_node("a", "Bad Name", json!({})),
            {"id":"t","kind":"tool","fields":{"tool":"run_command"}},
            {"id":"s","kind":"secret","fields":{"env":"lower","backend":"vault"}}
        ]),
        json!([]),
    );
    let c = compile(&b, &facts(), None);
    assert_eq!(c.status.nodes["a"].state, State::Error);
    assert_eq!(c.status.nodes["t"].state, State::Error);
    assert_eq!(c.status.nodes["s"].state, State::Error);
}
