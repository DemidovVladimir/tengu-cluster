//! The builder palette (`GET /api/v1/builder` → `palette`): every card kind
//! with its fields, the wires each pair of kinds may have, the engines and
//! the secret stores — built here, in Rust, from the config it writes. The
//! page renders forms and drag hints from it and hard-codes none of it.
//!
//! | Kind | Writes | Fields |
//! |---|---|---|
//! | `sandbox` (always one) | `[egress]` `network` / `allow_hosts`, `[memory] enabled`, `[studio] control` | network, allow_hosts, memory, studio_control |
//! | `agent` | `[agents.<name>]` (+ `identity`, `limits`, `local`, `claude_code`, `codex`) | name, engine, model, default, display_name, instructions, description, example_queries, limits, engine knobs |
//! | `orchestrator` (one) | `[orchestrator]` | max_attempts_per_step, max_replans |
//! | `tool` | the agent's `tools` + `[agents.<a>.scopes.<tool>]` | tool, restrict, fs_roots, net_hosts, env_reads |
//! | `skill` | the agent's `skill_packages` | skill |
//! | `workspace` | the agent's `workspace` (+ file-tool scopes) | path, confine |
//! | `secret` | nothing (keys never go in TOML) · `[keys.env]` / `strip` for the Cloudflare store | env, backend, keys_env, strip |
//! | `seal_proxy` (one) | `[keys]` `proxy` / `client` / `agent_socket` | proxy, client, agent_socket |
//! | `telegram` (one) | `[telegram]` | allowed_users |
//! | `webhook` | `[webhooks.endpoints.<name>]` | name, goal_template |
//!
//! [`CONNECTIONS`] is the wire table (from kind → to kind → what it writes);
//! `compile.rs` reads the same table.

use std::collections::BTreeMap;

use serde::Serialize;
use serde_json::{json, Value};

use super::{Facts, OfferedTool};

/// Version of the palette JSON shape.
pub(crate) const PALETTE_SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum FieldType {
    String,
    Text,
    Int,
    Bool,
    Select,
    List,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub(crate) struct Choice {
    pub value: String,
    pub label: String,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub(crate) struct ShowIf {
    pub field: &'static str,
    #[serde(rename = "in")]
    pub values: Vec<Value>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub(crate) struct SuggestFrom {
    pub field: &'static str,
    pub map: BTreeMap<String, Vec<String>>,
}

/// One form field of a card.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub(crate) struct FieldSpec {
    pub key: &'static str,
    pub label: &'static str,
    #[serde(rename = "type")]
    pub ty: FieldType,
    pub required: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub default: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub placeholder: Option<&'static str>,
    pub help: &'static str,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub options: Vec<Choice>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub suggest: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub suggest_from: Option<SuggestFrom>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub show_if: Option<ShowIf>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub min: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max: Option<i64>,
    pub readonly: bool,
    /// The TOML key it writes (`<name>` = the card's name).
    pub writes: &'static str,
}

fn field(key: &'static str, label: &'static str, ty: FieldType) -> FieldSpec {
    FieldSpec {
        key,
        label,
        ty,
        required: false,
        default: None,
        placeholder: None,
        help: "",
        options: Vec::new(),
        suggest: Vec::new(),
        suggest_from: None,
        show_if: None,
        min: None,
        max: None,
        readonly: false,
        writes: "",
    }
}

impl FieldSpec {
    fn required(mut self) -> Self {
        self.required = true;
        self
    }
    fn default(mut self, v: Value) -> Self {
        self.default = Some(v);
        self
    }
    fn placeholder(mut self, p: &'static str) -> Self {
        self.placeholder = Some(p);
        self
    }
    fn help(mut self, h: &'static str) -> Self {
        self.help = h;
        self
    }
    fn options(mut self, o: &[(&str, &str)]) -> Self {
        self.options = o
            .iter()
            .map(|(v, l)| Choice {
                value: v.to_string(),
                label: l.to_string(),
            })
            .collect();
        self
    }
    fn suggest(mut self, s: &[&str]) -> Self {
        self.suggest = s.iter().map(|x| x.to_string()).collect();
        self
    }
    fn show_if(mut self, field: &'static str, values: Vec<Value>) -> Self {
        self.show_if = Some(ShowIf { field, values });
        self
    }
    fn range(mut self, min: i64, max: i64) -> Self {
        self.min = Some(min);
        self.max = Some(max);
        self
    }
    fn readonly(mut self) -> Self {
        self.readonly = true;
        self
    }
    fn writes(mut self, w: &'static str) -> Self {
        self.writes = w;
        self
    }
}

/// One card kind.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub(crate) struct KindSpec {
    pub kind: &'static str,
    pub label: &'static str,
    pub group: &'static str,
    pub icon: &'static str,
    pub color: &'static str,
    pub singleton: bool,
    pub deletable: bool,
    /// The field that names the card (`None`: the kind's label / the sandbox).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title_field: Option<&'static str>,
    pub subtitle_fields: Vec<&'static str>,
    pub help: &'static str,
    pub fields: Vec<FieldSpec>,
}

/// A card to drag: a kind with preset fields.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub(crate) struct Item {
    pub id: String,
    pub kind: &'static str,
    pub label: String,
    pub group: String,
    pub description: String,
    pub preset: serde_json::Map<String, Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parameters: Option<Value>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub badges: Vec<String>,
    /// Why this card cannot be used in this build (shown, drop refused).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub disabled: Option<String>,
}

/// One allowed wire (module doc).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub(crate) struct Connection {
    pub from: &'static str,
    pub to: &'static str,
    pub edge: &'static str,
    pub label: &'static str,
    pub writes: &'static str,
    pub help: &'static str,
}

/// Every wire the builder knows: (from kind, to kind) → edge kind.
pub(crate) const CONNECTIONS: &[Connection] = &[
    Connection {
        from: "agent",
        to: "tool",
        edge: "uses",
        label: "uses",
        writes: "agents.<agent>.tools",
        help: "The agent may call this tool. An agent with no tool wire gets every default tool; \
               one with wires gets exactly those.",
    },
    Connection {
        from: "agent",
        to: "skill",
        edge: "loads",
        label: "loads",
        writes: "agents.<agent>.skill_packages",
        help: "The skill's SKILL.md is loaded into the agent's prompt.",
    },
    Connection {
        from: "agent",
        to: "workspace",
        edge: "works_in",
        label: "works in",
        writes: "agents.<agent>.workspace",
        help: "The agent's working folder (file tools, memory, plan-step cwd).",
    },
    Connection {
        from: "agent",
        to: "agent",
        edge: "delegates",
        label: "delegates to",
        writes: "agents.<to>.description",
        help:
            "The planner may hand plan steps to this agent: it becomes routable (its description \
               is written into TENGU_PLANNER_REGISTRY.md). Only the planner agent delegates.",
    },
    Connection {
        from: "orchestrator",
        to: "agent",
        edge: "plans",
        label: "plans with",
        writes: "orchestrator.agent",
        help: "This agent is the planner: it only writes plan JSON (tools stripped on its turn).",
    },
    Connection {
        from: "secret",
        to: "agent",
        edge: "authenticates",
        label: "key for",
        writes: "the engine's key (local: agents.<agent>.local.api_key_env)",
        help: "The key the agent's engine sends. OpenRouter reads OPENROUTER_API_KEY; a local \
               server reads the env var named here; Claude and Codex sign in with their CLI.",
    },
    Connection {
        from: "secret",
        to: "tool",
        edge: "grants",
        label: "readable by",
        writes: "agents.<agent>.scopes.<tool>.env_reads",
        help:
            "The tool may read this env var (scoped tools only — an unrestricted tool reads any).",
    },
    Connection {
        from: "secret",
        to: "seal_proxy",
        edge: "served_by",
        label: "served by",
        writes: "keys.env / keys.strip",
        help: "The key lives in the Cloudflare Worker; tengu gets a route, never the key.",
    },
    Connection {
        from: "secret",
        to: "telegram",
        edge: "bot_token",
        label: "bot token",
        writes: "TELEGRAM_BOT_TOKEN",
        help: "The Telegram bot token (the bot reads TELEGRAM_BOT_TOKEN).",
    },
    Connection {
        from: "secret",
        to: "webhook",
        edge: "signs",
        label: "signs",
        writes: "webhooks.endpoints.<name>.secret_env",
        help: "HMAC secret the sender signs each request with.",
    },
    Connection {
        from: "telegram",
        to: "agent",
        edge: "chats",
        label: "chats with",
        writes: "agents.<agent>.default = true",
        help: "Telegram messages go to this agent (the default agent; @name: routes to others).",
    },
    Connection {
        from: "webhook",
        to: "agent",
        edge: "triggers",
        label: "triggers",
        writes: "webhooks.endpoints.<name>.agent",
        help: "A POST to /webhooks/<name> becomes a goal for the planner, aimed at this agent.",
    },
];

/// The wire `from` → `to` may be, if any.
pub(crate) fn connection(from: &str, to: &str) -> Option<&'static Connection> {
    CONNECTIONS.iter().find(|c| c.from == from && c.to == to)
}

/// One engine `[agents.<a>] engine` may name (`config::ENGINES`).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub(crate) struct EngineSpec {
    pub id: &'static str,
    pub label: &'static str,
    pub models: Vec<&'static str>,
    /// Env var holding its key (`None`: signs in with a CLI login).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub key_env: Option<&'static str>,
    pub note: &'static str,
}

/// Every engine, described for the palette; a test keeps it equal to
/// `config::ENGINES`.
pub(crate) fn engines() -> Vec<EngineSpec> {
    let mut all = vec![
        EngineSpec {
            id: "openrouter",
            label: "OpenRouter API",
            models: vec![
                "anthropic/claude-sonnet-4-6",
                "openai/gpt-5",
                "google/gemini-2.5-flash-lite",
            ],
            key_env: Some("OPENROUTER_API_KEY"),
            note: "Pay-per-token API through openrouter.ai; key OPENROUTER_API_KEY (vault, .env or \
                   the Cloudflare seal proxy).",
        },
        EngineSpec {
            id: "claude_code",
            label: "Claude (subscription · Claude Code CLI)",
            models: vec![
                "claude-sonnet-5-5",
                "claude-opus-5-5",
                "claude-haiku-5-5",
                "claude-sonnet-4-6",
            ],
            key_env: None,
            note: "Runs the `claude` CLI signed in with your Claude subscription; tengu tools reach it \
                   through `tengu mcp-bridge`. Never the planner.",
        },
        EngineSpec {
            id: "local",
            label: "Local model (OpenAI-compatible server)",
            models: vec!["gemma4:latest", "qwen3:8b", "llama3.1:8b"],
            key_env: None,
            note: "Ollama / Unsloth / llama.cpp / vLLM on a machine you name (base URL). Set the \
                   context window to the server's real one.",
        },
    ];
    if super::super::ENGINES.contains(&"codex") {
        all.push(EngineSpec {
            id: "codex",
            label: "OpenAI (ChatGPT subscription · Codex CLI)",
            models: vec!["gpt-5.5", "gpt-5.6-sol", "gpt-6-sol", "gpt-6.1-sol"],
            key_env: None,
            note: "Runs the `codex` CLI signed in with your ChatGPT plan (`codex login`); tengu tools \
                   reach it through `tengu mcp-bridge`. Never the planner.",
        });
    }
    all
}

/// One place a secret can live.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub(crate) struct SecretBackend {
    pub id: &'static str,
    pub label: &'static str,
    pub available: bool,
    pub note: String,
}

pub(crate) fn secret_backends(facts: &Facts) -> Vec<SecretBackend> {
    vec![
        SecretBackend {
            id: "vault",
            label: "Local encrypted vault",
            available: true,
            note: "~/.tengu/secrets.vault (AES-256-GCM, master password): `tengu secret set NAME \
                   <value>`. Loaded into the process env at start; never written to TOML."
                .into(),
        },
        SecretBackend {
            id: "env",
            label: "Environment / .env",
            available: true,
            note: "Exported in the shell or a line in .env. Never written to TOML.".into(),
        },
        SecretBackend {
            id: "cloudflare",
            label: "Cloudflare seal proxy",
            available: facts.keys_supported,
            note: if facts.keys_supported {
                "The key is a Worker secret (`npx wrangler secret put NAME`); tengu gets a route \
                 and a session, never the key ([keys], docs/sealed-keys-2026-10-09.md)."
                    .into()
            } else {
                "This build has no [keys] section yet (the seal proxy, PR #50). Merge it and this \
                 store turns on by itself."
                    .into()
            },
        },
    ]
}

fn kinds(facts: &Facts) -> Vec<KindSpec> {
    use FieldType::*;
    let engines = engines();
    let engine_options: Vec<(&str, &str)> = engines.iter().map(|e| (e.id, e.label)).collect();
    let models: BTreeMap<std::string::String, Vec<std::string::String>> = engines
        .iter()
        .map(|e| {
            (
                e.id.to_string(),
                e.models.iter().map(|m| m.to_string()).collect(),
            )
        })
        .collect();
    let mut backends: Vec<(&str, &str)> = vec![
        ("vault", "Local encrypted vault"),
        ("env", "Environment / .env"),
    ];
    if facts.keys_supported {
        backends.push(("cloudflare", "Cloudflare seal proxy"));
    }
    let restrict = || vec![json!(true)];
    vec![
        KindSpec {
            kind: "sandbox",
            label: "Sandbox",
            group: "Sandbox",
            icon: "sandbox",
            color: "slate",
            singleton: true,
            deletable: false,
            title_field: None,
            subtitle_fields: vec!["network"],
            help: "Sandbox-wide settings: network policy, memory, Studio control.",
            fields: vec![
                field("network", "Network", Select)
                    .required()
                    .default(json!("tor"))
                    .options(&[
                        ("tor", "Tor (default: every request via the Tor proxy)"),
                        ("open", "Open (direct connections)"),
                    ])
                    .help("Tor needs the proxy running (`make tor`); open goes direct.")
                    .writes("egress.network"),
                field("allow_hosts", "Allowed hosts", List)
                    .placeholder("openrouter.ai")
                    .help("Only these hosts may be reached (empty = any). `*.example.com` matches subdomains.")
                    .writes("egress.allow_hosts"),
                field("memory", "Local memory", Bool)
                    .default(json!(false))
                    .help("Vector memory in each workspace (memory/vectors.bin) + the memory tools.")
                    .writes("memory.enabled"),
                field("studio_control", "Studio may run it", Bool)
                    .default(json!(false))
                    .help("Let `tengu studio` Play / Stop this sandbox's runtime.")
                    .writes("studio.control"),
            ],
        },
        KindSpec {
            kind: "agent",
            label: "Agent",
            group: "Agents",
            icon: "agent",
            color: "violet",
            singleton: false,
            deletable: true,
            title_field: Some("name"),
            subtitle_fields: vec!["engine", "model"],
            help: "One [agents.<name>] block: an LLM with tools, skills and a workspace.",
            fields: vec![
                field("name", "Name", String)
                    .required()
                    .placeholder("researcher")
                    .help("Lower-case letters, digits, - and _. The TOML table name and the @name: route.")
                    .writes("agents.<name>"),
                field("engine", "Engine", Select)
                    .required()
                    .default(json!("openrouter"))
                    .options(&engine_options)
                    .writes("agents.<name>.engine"),
                FieldSpec {
                    suggest_from: Some(SuggestFrom {
                        field: "engine",
                        map: models,
                    }),
                    ..field("model", "Model", String)
                        .required()
                        .help("OpenRouter: provider/model. Claude / Codex: the bare model id. Local: the server's model id.")
                        .writes("agents.<name>.model")
                },
                field("default", "Default agent", Bool)
                    .default(json!(false))
                    .help("Chat starts here (one agent per sandbox).")
                    .writes("agents.<name>.default"),
                field("display_name", "Display name", String)
                    .placeholder("Researcher")
                    .writes("agents.<name>.identity.name"),
                field("instructions", "Instructions", Text)
                    .placeholder("You are …")
                    .help("Injected into the system prompt.")
                    .writes("agents.<name>.identity.instructions"),
                field("description", "Description (for the planner)", Text)
                    .help("What the planner may hand this agent and what it is NOT for. Written only when the planner agent delegates to it (a wire planner → this agent).")
                    .writes("agents.<name>.description"),
                field("example_queries", "Example queries", List)
                    .help("Short questions the planner should route here (with a description).")
                    .writes("agents.<name>.example_queries"),
                field("max_tool_rounds", "Max tool rounds", Int)
                    .range(1, 1000)
                    .help("Turn cap per plan step / chat turn (empty = the default).")
                    .writes("agents.<name>.limits.max_tool_rounds"),
                field("step_timeout_secs", "Step timeout (s)", Int)
                    .range(10, 86_400)
                    .help("Wall clock per plan step (default 600).")
                    .writes("agents.<name>.limits.step_timeout_secs"),
                field("context_window", "Context window (tokens)", Int)
                    .range(1024, 10_000_000)
                    .show_if("engine", vec![json!("local")])
                    .help("The local server's real window (Ollama: num_ctx).")
                    .writes("agents.<name>.limits.context_window"),
                field("local_base_url", "Server URL", String)
                    .default(json!("http://127.0.0.1:8888"))
                    .suggest(&["http://127.0.0.1:8888", "http://127.0.0.1:11434/v1", "http://192.168.1.50:11434/v1"])
                    .show_if("engine", vec![json!("local")])
                    .help("Unsloth :8888, Ollama :11434, llama.cpp :8080, vLLM :8000 — another machine on your LAN works too.")
                    .writes("agents.<name>.local.base_url"),
                field("local_api_key_env", "Key env var", String)
                    .placeholder("UNSLOTH_API_KEY")
                    .show_if("engine", vec![json!("local")])
                    .help("Env var with the server's bearer key (empty env = no key). A wired secret sets it.")
                    .writes("agents.<name>.local.api_key_env"),
                field("claude_tools", "Claude built-in tools", Select)
                    .default(json!("read_only"))
                    .options(&[
                        ("none", "none — tengu tools only"),
                        ("read_only", "read_only — Read/Grep/Glob"),
                        ("editor", "editor — + Edit/Write"),
                        ("editor_shell", "editor_shell — + Bash"),
                    ])
                    .show_if("engine", vec![json!("claude_code")])
                    .help("Claude Code's own tools ignore tengu scopes; keep them narrow.")
                    .writes("agents.<name>.claude_code.builtin_tools_profile"),
                field("codex_sandbox", "Codex sandbox", Select)
                    .default(json!("read-only"))
                    .options(&[
                        ("read-only", "read-only — Codex's shell cannot write"),
                        ("workspace-write", "workspace-write — writes inside the workspace"),
                    ])
                    .show_if("engine", vec![json!("codex")])
                    .help("Codex's own shell ignores tengu scopes; read-only keeps it from writing.")
                    .writes("agents.<name>.codex.sandbox"),
            ],
        },
        KindSpec {
            kind: "orchestrator",
            label: "Orchestrator",
            group: "Agents",
            icon: "planner",
            color: "indigo",
            singleton: true,
            deletable: true,
            title_field: None,
            subtitle_fields: vec![],
            help: "Turns chat into plans: wire it to the planner agent, then wire the planner to the agents it may delegate to.",
            fields: vec![
                field("max_attempts_per_step", "Attempts per step", Int)
                    .default(json!(3))
                    .range(1, 10)
                    .writes("orchestrator.max_attempts_per_step"),
                field("max_replans", "Replans", Int)
                    .default(json!(2))
                    .range(0, 10)
                    .writes("orchestrator.max_replans"),
            ],
        },
        KindSpec {
            kind: "tool",
            label: "Tool",
            group: "Tools",
            icon: "tool",
            color: "teal",
            singleton: false,
            deletable: true,
            title_field: Some("tool"),
            subtitle_fields: vec![],
            help: "A catalog tool. Wire agents to it; turn on Restrict to give it a scope (deny by default per field).",
            fields: vec![
                field("tool", "Tool", String)
                    .required()
                    .readonly()
                    .writes("agents.<agent>.tools"),
                field("restrict", "Restrict (scope)", Bool)
                    .default(json!(false))
                    .help("Off: the tool runs with the permissive default. On: only the folders, hosts and env vars below.")
                    .writes("agents.<agent>.scopes.<tool>"),
                field("fs_roots", "Folders", List)
                    .placeholder("~/tengu-work")
                    .show_if("restrict", restrict())
                    .writes("agents.<agent>.scopes.<tool>.fs_roots"),
                field("net_hosts", "Hosts", List)
                    .placeholder("api.example.com")
                    .show_if("restrict", restrict())
                    .writes("agents.<agent>.scopes.<tool>.net_hosts"),
                field("env_reads", "Env vars", List)
                    .placeholder("MY_API_KEY")
                    .show_if("restrict", restrict())
                    .help("Wired secrets are added here.")
                    .writes("agents.<agent>.scopes.<tool>.env_reads"),
            ],
        },
        KindSpec {
            kind: "skill",
            label: "Skill",
            group: "Skills",
            icon: "skill",
            color: "amber",
            singleton: false,
            deletable: true,
            title_field: Some("skill"),
            subtitle_fields: vec![],
            help: "A SKILL.md package (skills/, ~/.tengu/skills/, <workspace>/.tengu/skills/).",
            fields: vec![field("skill", "Skill", String)
                .required()
                .readonly()
                .writes("agents.<agent>.skill_packages")],
        },
        KindSpec {
            kind: "workspace",
            label: "Workspace",
            group: "Workspace & secrets",
            icon: "workspace",
            color: "blue",
            singleton: false,
            deletable: true,
            title_field: Some("path"),
            subtitle_fields: vec![],
            help: "A folder the wired agents work in. Confine keeps read_file / write_file / list_directory inside it.",
            fields: vec![
                field("path", "Folder", String)
                    .required()
                    .placeholder("~/tengu-work/my-team")
                    .writes("agents.<agent>.workspace"),
                field("confine", "Confine file tools", Bool)
                    .default(json!(true))
                    .help("read_file / write_file / list_directory only inside this folder.")
                    .writes("agents.<agent>.scopes.{read_file,write_file,list_directory}.fs_roots"),
            ],
        },
        KindSpec {
            kind: "secret",
            label: "Secret",
            group: "Workspace & secrets",
            icon: "secret",
            color: "rose",
            singleton: false,
            deletable: true,
            title_field: Some("env"),
            subtitle_fields: vec!["backend"],
            help: "A key by NAME only — the value never enters the canvas or the TOML. Pick where it lives.",
            fields: vec![
                field("env", "Env var", String)
                    .required()
                    .placeholder("OPENROUTER_API_KEY")
                    .help("Upper-case name the code reads.")
                    .writes("(name only)"),
                field("backend", "Stored in", Select)
                    .required()
                    .default(json!("vault"))
                    .options(&backends)
                    .writes("(not in TOML) · keys.env for cloudflare"),
                field("keys_env", "[keys.env] lines", List)
                    .placeholder("OPENROUTER_BASE_URL=openrouter")
                    .show_if("backend", vec![json!("cloudflare")])
                    .help("VAR=route (or VAR=@session): where tengu's client is pointed instead of the key.")
                    .writes("keys.env"),
                field("strip", "Strip local copies", List)
                    .placeholder("TELEGRAM_BOT_TOKEN")
                    .show_if("backend", vec![json!("cloudflare")])
                    .help("Env vars removed at start so no tool sees an old local key.")
                    .writes("keys.strip"),
            ],
        },
        KindSpec {
            kind: "seal_proxy",
            label: "Cloudflare seal proxy",
            group: "Workspace & secrets",
            icon: "proxy",
            color: "amber",
            singleton: true,
            deletable: true,
            title_field: None,
            subtitle_fields: vec!["proxy"],
            help: "The tengu-seal Worker: provider keys are its secrets; tengu signs a session with an ssh-agent key.",
            fields: vec![
                field("proxy", "Worker URL", String)
                    .required()
                    .placeholder("https://tengu-seal.<subdomain>.workers.dev")
                    .writes("keys.proxy"),
                field("client", "Client key", String)
                    .placeholder("tengu-attended")
                    .help("ssh-agent key comment or SHA256: fingerprint (empty = the only key).")
                    .writes("keys.client"),
                field("agent_socket", "ssh-agent socket", String)
                    .placeholder("~/Library/Containers/com.maxgoedjen.Secretive.SecretAgent/Data/socket.ssh")
                    .writes("keys.agent_socket"),
            ],
        },
        KindSpec {
            kind: "telegram",
            label: "Telegram",
            group: "Channels",
            icon: "telegram",
            color: "blue",
            singleton: true,
            deletable: true,
            title_field: None,
            subtitle_fields: vec![],
            help: "`tengu telegram --sandbox <name>`: a bot that chats with the wired agent. Fails closed without an allow-list.",
            fields: vec![field("allowed_users", "Allowed user ids", List)
                .placeholder("123456789")
                .help("Telegram user ids the bot answers (or TENGU_TELEGRAM_ALLOWED_USERS).")
                .writes("telegram.allowed_users")],
        },
        KindSpec {
            kind: "webhook",
            label: "Webhook",
            group: "Channels",
            icon: "webhook",
            color: "green",
            singleton: false,
            deletable: true,
            title_field: Some("name"),
            subtitle_fields: vec![],
            help: "POST /webhooks/<name> (`tengu webhooks`, build with --features webhooks): the body becomes a goal for the planner.",
            fields: vec![
                field("name", "Endpoint", String)
                    .required()
                    .placeholder("github")
                    .writes("webhooks.endpoints.<name>"),
                field("goal_template", "Goal template", Text)
                    .placeholder("A webhook arrived. Process the payload below.")
                    .writes("webhooks.endpoints.<name>.goal_template"),
            ],
        },
    ]
}

/// The kind spec named `kind`.
pub(crate) fn kind_spec(facts: &Facts, kind: &str) -> Option<KindSpec> {
    kinds(facts).into_iter().find(|k| k.kind == kind)
}

fn preset(pairs: &[(&str, Value)]) -> serde_json::Map<String, Value> {
    pairs
        .iter()
        .map(|(k, v)| (k.to_string(), v.clone()))
        .collect()
}

fn items(facts: &Facts) -> Vec<Item> {
    let mut out = Vec::new();
    let item = |id: &str, kind: &'static str, label: &str, group: &str, description: &str| Item {
        id: id.to_string(),
        kind,
        label: label.to_string(),
        group: group.to_string(),
        description: description.to_string(),
        preset: serde_json::Map::new(),
        parameters: None,
        badges: Vec::new(),
        disabled: None,
    };
    for e in engines() {
        let mut it = item(
            &format!("agent.{}", e.id),
            "agent",
            &format!("Agent · {}", e.label),
            "Agents",
            e.note,
        );
        it.preset = preset(&[
            ("engine", json!(e.id)),
            (
                "model",
                json!(e.models.first().copied().unwrap_or_default()),
            ),
        ]);
        out.push(it);
    }
    out.push(item(
        "orchestrator",
        "orchestrator",
        "Orchestrator (planner)",
        "Agents",
        "Plans each chat turn and hands steps to routable agents.",
    ));
    for t in &facts.tools {
        let mut it = item(
            &format!("tool.{}", t.name),
            "tool",
            &t.name,
            t.group,
            &t.description,
        );
        it.preset = preset(&[("tool", json!(t.name))]);
        it.parameters = Some(t.parameters.clone());
        if t.opt_in {
            it.badges.push("opt-in".into());
        }
        if t.needs_memory {
            it.badges.push("needs memory".into());
        }
        out.push(it);
    }
    for (name, description) in &facts.skills {
        let mut it = item(
            &format!("skill.{name}"),
            "skill",
            name,
            "Skills",
            description,
        );
        it.preset = preset(&[("skill", json!(name))]);
        out.push(it);
    }
    let mut ws = item(
        "workspace",
        "workspace",
        "Workspace folder",
        "Workspace & secrets",
        "Where the wired agents read and write files.",
    );
    ws.preset = preset(&[("confine", json!(true))]);
    out.push(ws);
    for (id, label, env, keys_env, strip, why) in SECRET_PRESETS {
        let mut it = item(
            &format!("secret.{id}"),
            "secret",
            label,
            "Workspace & secrets",
            why,
        );
        it.preset = preset(&[
            ("env", json!(env)),
            ("backend", json!("vault")),
            ("keys_env", json!(keys_env)),
            ("strip", json!(strip)),
        ]);
        out.push(it);
    }
    let mut proxy = item(
        "seal_proxy",
        "seal_proxy",
        "Cloudflare seal proxy",
        "Workspace & secrets",
        "Keys stay in a Cloudflare Worker; wire secrets stored there to it.",
    );
    if !facts.keys_supported {
        proxy.disabled = Some(
            "this build has no [keys] section (the seal proxy, PR #50) — merge it first".into(),
        );
    }
    out.push(proxy);
    out.push(item(
        "telegram",
        "telegram",
        "Telegram bot",
        "Channels",
        "Chat with an agent from Telegram.",
    ));
    out.push(item(
        "webhook",
        "webhook",
        "Webhook endpoint",
        "Channels",
        "Trigger the planner with an HTTP POST.",
    ));
    out
}

/// (item id, label, env var, `[keys.env]` lines, `strip`, description) — the
/// shipped seal-proxy routes (`docs/sealed-keys-2026-10-09.md` § Worker)
/// and plain keys. Data only: the compiler treats every secret alike.
const SECRET_PRESETS: &[(&str, &str, &str, &[&str], &[&str], &str)] = &[
    (
        "openrouter",
        "OpenRouter API key",
        "OPENROUTER_API_KEY",
        &[
            "OPENROUTER_BASE_URL=openrouter",
            "OPENROUTER_API_KEY=@session",
        ],
        &[],
        "Key for OpenRouter agents (and Jev decisions).",
    ),
    (
        "telegram",
        "Telegram bot token",
        "TELEGRAM_BOT_TOKEN",
        &["TELEGRAM_API_URL=telegram"],
        &["TELEGRAM_BOT_TOKEN"],
        "The bot's token from @BotFather.",
    ),
    (
        "helius",
        "Helius RPC key",
        "HELIUS_API_KEY",
        &["SOLANA_RPC_URL=solana-rpc/?api-key=TENGU_SECRET"],
        &["HELIUS_API_KEY"],
        "Solana RPC for the Solana read tools (wire it to them).",
    ),
    (
        "webhook",
        "Webhook signing secret",
        "WEBHOOK_SECRET",
        &[],
        &[],
        "HMAC secret a webhook sender signs with.",
    ),
    (
        "custom",
        "Custom secret",
        "",
        &[],
        &[],
        "Any env var a tool or skill reads.",
    ),
];

/// The whole palette (module doc).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub(crate) struct Palette {
    pub schema_version: u32,
    pub kinds: Vec<KindSpec>,
    pub items: Vec<Item>,
    pub connections: &'static [Connection],
    pub engines: Vec<EngineSpec>,
    pub secret_backends: Vec<SecretBackend>,
}

pub(crate) fn palette(facts: &Facts) -> Palette {
    Palette {
        schema_version: PALETTE_SCHEMA_VERSION,
        kinds: kinds(facts),
        items: items(facts),
        connections: CONNECTIONS,
        engines: engines(),
        secret_backends: secret_backends(facts),
    }
}

/// The tools a builder offers: every catalog tool but the ones that need
/// hand-written, hardened config (module doc of `config/builder`).
pub(crate) fn offered(tool: &OfferedTool) -> bool {
    use crate::domain::tools::{PRIVY_SIGNING_TOOLS, SOLANA_WRITE_TOOLS, XM_EXEC_TOOLS};
    let name = tool.name.as_str();
    !(name == "run_command"
        || name == "compress_and_store"
        || PRIVY_SIGNING_TOOLS.contains(&name)
        || SOLANA_WRITE_TOOLS.contains(&name)
        || XM_EXEC_TOOLS.contains(&name))
}

/// The palette group a tool is listed under.
pub(crate) fn tool_group(name: &str) -> &'static str {
    match name {
        "read_file" | "write_file" | "list_directory" => "Tools · files",
        "http_request" => "Tools · web",
        n if n.starts_with("memory_")
            || n == "remember"
            || n == "persistent_store"
            || n == "shared_cache"
            || n == "agentic_memory" =>
        {
            "Tools · memory"
        }
        n if n.contains("skill") || n == "apply_improver_proposal" => "Tools · skills",
        "abi_encode" | "hex_to_uint256" | "get_wallet_address" => "Tools · crypto",
        n if n.starts_with("sol")
            || n.starts_with("dlmm")
            || n.starts_with("jup")
            || n.starts_with("lp_")
            || n.starts_with("hedge") =>
        {
            "Tools · solana"
        }
        _ => "Tools · other",
    }
}
