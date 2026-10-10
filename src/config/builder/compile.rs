//! Blueprint → `config.toml` (`docs/studio-builder-2026-10-10.md` § Compile).
//! Pure: no file, no env. The real loader (`Config::load` on a temp copy,
//! `adapters/outbound/builder_store.rs`) has the last word; the checks here
//! pin a problem to the card or wire that causes it, so the page can colour
//! it.
//!
//! | Step | Does |
//! |---|---|
//! | cards | kind known, singletons once, field types / options / ranges, required fields, name rules |
//! | wires | the kind pair is in `CONNECTIONS`; then what each writes (palette module table) and its rules |
//! | after | unused cards warn; the Cloudflare store needs the proxy card and a build with `[keys]`; one planner, one bot agent, one default |
//! | emit | owned sections in a fixed order (`OWNED`); every other top-level key of the current `config.toml` kept as it is |
//!
//! Keys never enter the TOML: a secret card names an env var and where it
//! lives; only the Cloudflare store writes routes (`[keys.env]`), never a
//! key.

use std::collections::{BTreeMap, BTreeSet};

use serde::Serialize;
use serde_json::Value;

use super::palette::{connection, kind_spec, FieldType, KindSpec, CONNECTIONS};
use super::Facts;
use crate::domain::blueprint::{Blueprint, Issue, Node, Status};

/// Top-level keys the builder writes; any other key of the current file is
/// kept as it is.
pub(crate) const OWNED: &[&str] = &[
    "egress",
    "memory",
    "studio",
    "keys",
    "orchestrator",
    "telegram",
    "webhooks",
    "agents",
];

/// File tools a confining workspace scopes.
const FILE_TOOLS: &[&str] = &["read_file", "write_file", "list_directory"];

/// One key the composed sandbox needs, by name (the value never).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct SecretUse {
    pub env: String,
    /// `vault` · `env` · `cloudflare` · `vault or env` (a key no card names).
    pub backend: String,
    pub used_by: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub node: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub(crate) struct Compiled {
    pub toml: String,
    pub status: Status,
    pub secrets: Vec<SecretUse>,
    /// Top-level keys kept from the current `config.toml`.
    pub kept: Vec<String>,
}

#[derive(Default)]
struct ScopeOut {
    fs_roots: Vec<String>,
    net_hosts: Vec<String>,
    env_reads: Vec<String>,
}

#[derive(Default)]
struct AgentOut {
    tools: Vec<String>,
    skills: Vec<String>,
    workspace: Option<(String, bool)>,
    routable: bool,
    chats: bool,
    local_key_env: Option<String>,
    scopes: BTreeMap<String, ScopeOut>,
}

/// A card's fields read through its kind spec (defaults filled in).
struct Card<'a> {
    node: &'a Node,
    spec: KindSpec,
}

impl Card<'_> {
    fn raw(&self, key: &str) -> Option<Value> {
        match self.node.fields.get(key) {
            Some(Value::Null) | None => self
                .spec
                .fields
                .iter()
                .find(|f| f.key == key)
                .and_then(|f| f.default.clone()),
            Some(v) => Some(v.clone()),
        }
    }

    fn s(&self, key: &str) -> Option<String> {
        match self.raw(key)? {
            Value::String(s) if !s.trim().is_empty() => Some(s.trim().to_string()),
            _ => None,
        }
    }

    fn b(&self, key: &str) -> bool {
        matches!(self.raw(key), Some(Value::Bool(true)))
    }

    fn i(&self, key: &str) -> Option<i64> {
        match self.raw(key)? {
            Value::Number(n) => n.as_i64(),
            Value::String(s) => s.trim().parse().ok(),
            _ => None,
        }
    }

    fn list(&self, key: &str) -> Vec<String> {
        match self.raw(key) {
            Some(Value::Array(a)) => a
                .iter()
                .filter_map(|v| v.as_str())
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(String::from)
                .collect(),
            _ => Vec::new(),
        }
    }

    fn id(&self) -> &str {
        &self.node.id
    }
}

/// Whether `name` is an agent / endpoint name: `[a-z0-9][a-z0-9_-]{0,39}`.
pub(crate) fn valid_name(name: &str) -> bool {
    let mut chars = name.chars();
    matches!(chars.next(), Some(c) if c.is_ascii_lowercase() || c.is_ascii_digit())
        && name.len() <= 40
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-')
}

/// Whether `name` is an env var name: `[A-Z_][A-Z0-9_]*`, ≤ 128.
pub(crate) fn valid_env(name: &str) -> bool {
    let mut chars = name.chars();
    matches!(chars.next(), Some(c) if c.is_ascii_uppercase() || c == '_')
        && name.len() <= 128
        && chars.all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
}

fn check_fields(card: &Card<'_>, issues: &mut Vec<Issue>) {
    let id = card.id();
    for (key, value) in &card.node.fields {
        let Some(f) = card.spec.fields.iter().find(|f| f.key == key) else {
            issues.push(
                Issue::warn(format!("unknown field '{key}' (ignored)"))
                    .node(id)
                    .field(key),
            );
            continue;
        };
        if value.is_null() {
            continue;
        }
        let ok = match f.ty {
            FieldType::String | FieldType::Text | FieldType::Select => value.is_string(),
            FieldType::Bool => value.is_boolean(),
            FieldType::Int => value.is_i64() || value.as_str().is_some_and(|s| s.trim().is_empty()),
            FieldType::List => value
                .as_array()
                .is_some_and(|a| a.iter().all(Value::is_string)),
        };
        if !ok {
            issues.push(
                Issue::error(format!("{} has the wrong type", f.label))
                    .node(id)
                    .field(key),
            );
            continue;
        }
        if f.ty == FieldType::Select {
            let v = value.as_str().unwrap_or_default();
            if !v.is_empty() && !f.options.iter().any(|o| o.value == v) {
                issues.push(
                    Issue::error(format!("{}: '{v}' is not one of the choices", f.label))
                        .node(id)
                        .field(key),
                );
            }
        }
        if let (FieldType::Int, Some(n)) = (f.ty, value.as_i64()) {
            if f.min.is_some_and(|m| n < m) || f.max.is_some_and(|m| n > m) {
                issues.push(
                    Issue::error(format!(
                        "{} must be {}–{}",
                        f.label,
                        f.min.unwrap_or(i64::MIN),
                        f.max.unwrap_or(i64::MAX)
                    ))
                    .node(id)
                    .field(key),
                );
            }
        }
    }
    for f in card.spec.fields.iter().filter(|f| f.required) {
        let present = match f.ty {
            FieldType::List => !card.list(f.key).is_empty(),
            FieldType::Bool => true,
            FieldType::Int => card.i(f.key).is_some(),
            _ => card.s(f.key).is_some(),
        };
        if !present {
            issues.push(
                Issue::error(format!("{} is required", f.label))
                    .node(id)
                    .field(f.key),
            );
        }
    }
}

fn title_of(card: &Card<'_>, bp: &Blueprint) -> (String, String) {
    let s = |k: &str| card.s(k).unwrap_or_default();
    match card.spec.kind {
        "sandbox" => (bp.sandbox.clone(), format!("network: {}", s("network"))),
        "agent" => (
            card.s("name").unwrap_or_else(|| "(unnamed agent)".into()),
            format!("{} · {}", s("engine"), s("model")),
        ),
        "tool" => (
            s("tool"),
            if card.b("restrict") {
                "scoped".into()
            } else {
                "default scope".into()
            },
        ),
        "workspace" => (
            card.s("path").unwrap_or_else(|| "(no folder)".into()),
            if card.b("confine") {
                "file tools confined".into()
            } else {
                "not confined".into()
            },
        ),
        "secret" => (
            card.s("env").unwrap_or_else(|| "(no env var)".into()),
            s("backend"),
        ),
        "seal_proxy" => ("Cloudflare seal proxy".into(), s("proxy")),
        "telegram" => (
            "Telegram".into(),
            format!("{} allowed users", card.list("allowed_users").len()),
        ),
        "webhook" => (format!("/webhooks/{}", s("name")), String::new()),
        "skill" => (s("skill"), "skill".into()),
        _ => (card.spec.label.to_string(), String::new()),
    }
}

/// Compile `bp` (module table). `current` = the `config.toml` on disk now,
/// whose non-owned top-level keys are kept.
pub(crate) fn compile(bp: &Blueprint, facts: &Facts, current: Option<&str>) -> Compiled {
    let mut issues = bp.shape_errors();
    let mut titles = BTreeMap::new();
    let mut edge_kinds: BTreeMap<String, (String, String, String)> = BTreeMap::new();

    // ── cards ──
    let mut cards: BTreeMap<&str, Card<'_>> = BTreeMap::new();
    let mut seen_kind: BTreeMap<&str, usize> = BTreeMap::new();
    for n in &bp.nodes {
        let Some(spec) = kind_spec(facts, &n.kind) else {
            issues.push(Issue::error(format!("unknown card kind '{}'", n.kind)).node(&n.id));
            continue;
        };
        let count = seen_kind.entry(spec.kind).or_default();
        *count += 1;
        if spec.singleton && *count > 1 {
            issues.push(
                Issue::error(format!("only one {} card per sandbox", spec.label)).node(&n.id),
            );
        }
        let card = Card { node: n, spec };
        check_fields(&card, &mut issues);
        titles.insert(n.id.clone(), title_of(&card, bp));
        cards.insert(n.id.as_str(), card);
    }
    let of_kind = |k: &str| -> Vec<&Card<'_>> {
        bp.nodes
            .iter()
            .filter_map(|n| cards.get(n.id.as_str()))
            .filter(|c| c.spec.kind == k)
            .collect()
    };
    let sandbox = of_kind("sandbox").into_iter().next();
    if sandbox.is_none() {
        issues.push(Issue::error("the Sandbox card is missing"));
    }

    // Agents by card id, named; names unique and valid.
    let mut agent_name: BTreeMap<&str, String> = BTreeMap::new();
    let mut names_seen: BTreeMap<String, &str> = BTreeMap::new();
    for c in of_kind("agent") {
        let Some(name) = c.s("name") else { continue };
        if !valid_name(&name) {
            issues.push(
                Issue::error(format!(
                    "'{name}': lower-case letters, digits, - and _ (start with a letter or digit, ≤ 40)"
                ))
                .node(c.id())
                .field("name"),
            );
            continue;
        }
        if let Some(other) = names_seen.insert(name.clone(), c.id()) {
            issues.push(
                Issue::error(format!("two agents are named '{name}'"))
                    .node(c.id())
                    .field("name"),
            );
            issues.push(
                Issue::error(format!("two agents are named '{name}'"))
                    .node(other)
                    .field("name"),
            );
        }
        agent_name.insert(c.id(), name);
    }
    for c in of_kind("tool") {
        if let Some(t) = c.s("tool") {
            if !facts.tools.iter().any(|o| o.name == t) {
                issues.push(
                    Issue::error(format!(
                        "'{t}' is not offered by the builder (no such catalog tool, or one that needs hand-written hardened config)"
                    ))
                    .node(c.id())
                    .field("tool"),
                );
            }
        }
    }
    for c in of_kind("skill") {
        if let Some(s) = c.s("skill") {
            if !facts.skills.iter().any(|(n, _)| *n == s) {
                issues.push(
                    Issue::warn(format!(
                        "no skill '{s}' found on this machine (skills/, ~/.tengu/skills/)"
                    ))
                    .node(c.id())
                    .field("skill"),
                );
            }
        }
    }
    for c in of_kind("secret") {
        if let Some(env) = c.s("env") {
            if !valid_env(&env) {
                issues.push(
                    Issue::error(format!("'{env}': an env var name is A-Z, 0-9 and _"))
                        .node(c.id())
                        .field("env"),
                );
            }
        }
    }
    for c in of_kind("webhook") {
        if let Some(name) = c.s("name") {
            if !valid_name(&name) {
                issues.push(
                    Issue::error(format!("'{name}': lower-case letters, digits, - and _"))
                        .node(c.id())
                        .field("name"),
                );
            }
        }
    }

    // ── wires ──
    let mut agents: BTreeMap<&str, AgentOut> = agent_name
        .keys()
        .map(|id| (*id, AgentOut::default()))
        .collect();
    let mut planner: Option<&str> = None;
    let mut used: BTreeSet<&str> = BTreeSet::new();
    let mut tool_users: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    let mut served: BTreeSet<&str> = BTreeSet::new();
    let mut secret_users: BTreeMap<&str, Vec<String>> = BTreeMap::new();
    let mut bot_agent: Option<&str> = None;
    let mut webhook_agent: BTreeMap<&str, &str> = BTreeMap::new();
    let mut webhook_secret: BTreeMap<&str, String> = BTreeMap::new();
    let name_of = |id: &str| -> String {
        agent_name
            .get(id)
            .cloned()
            .unwrap_or_else(|| "<unnamed>".into())
    };

    // Planner first: delegation rules read it.
    for e in &bp.edges {
        let (Some(f), Some(t)) = (cards.get(e.from.as_str()), cards.get(e.to.as_str())) else {
            continue;
        };
        if f.spec.kind == "orchestrator" && t.spec.kind == "agent" {
            if planner.is_some() {
                issues.push(Issue::error("one planner per sandbox").edge(&e.id));
            } else {
                planner = Some(t.id());
            }
        }
    }

    for e in &bp.edges {
        let (Some(f), Some(t)) = (cards.get(e.from.as_str()), cards.get(e.to.as_str())) else {
            continue;
        };
        let Some(c) = connection(f.spec.kind, t.spec.kind) else {
            let to_label = t.spec.label;
            let from_label = f.spec.label;
            let hint: Vec<&str> = CONNECTIONS
                .iter()
                .filter(|c| c.from == f.spec.kind)
                .map(|c| c.to)
                .collect();
            issues.push(
                Issue::error(format!(
                    "no wire from {from_label} to {to_label}{}",
                    if hint.is_empty() {
                        String::new()
                    } else {
                        format!(" (it wires to: {})", hint.join(", "))
                    }
                ))
                .edge(&e.id),
            );
            continue;
        };
        used.insert(f.id());
        used.insert(t.id());
        let mut writes = c.writes.to_string();
        match c.edge {
            "uses" => {
                let tool = t.s("tool").unwrap_or_default();
                let a = agents.entry(f.id()).or_default();
                if a.tools.contains(&tool) {
                    issues.push(
                        Issue::error(format!(
                            "{} already uses {tool} (another card)",
                            name_of(f.id())
                        ))
                        .edge(&e.id),
                    );
                } else {
                    a.tools.push(tool.clone());
                }
                if t.b("restrict") {
                    a.scopes.insert(
                        tool.clone(),
                        ScopeOut {
                            fs_roots: t.list("fs_roots"),
                            net_hosts: t.list("net_hosts"),
                            env_reads: t.list("env_reads"),
                        },
                    );
                }
                tool_users.entry(t.id()).or_default().push(f.id());
                let needs_memory = facts.tools.iter().any(|o| o.name == tool && o.needs_memory);
                if needs_memory && !sandbox.is_some_and(|s| s.b("memory")) {
                    issues.push(
                        Issue::warn(format!(
                            "{tool} needs Local memory on (Sandbox card); without it the agent does not get it"
                        ))
                        .edge(&e.id),
                    );
                }
                writes = format!("agents.{}.tools", name_of(f.id()));
            }
            "loads" => {
                let skill = t.s("skill").unwrap_or_default();
                let a = agents.entry(f.id()).or_default();
                if a.skills.contains(&skill) {
                    issues.push(Issue::warn(format!("{skill} is loaded twice")).edge(&e.id));
                } else {
                    a.skills.push(skill);
                }
                writes = format!("agents.{}.skill_packages", name_of(f.id()));
            }
            "works_in" => {
                let a = agents.entry(f.id()).or_default();
                if a.workspace.is_some() {
                    issues.push(Issue::error("one workspace per agent").edge(&e.id));
                } else if let Some(path) = t.s("path") {
                    a.workspace = Some((path, t.b("confine")));
                }
                writes = format!("agents.{}.workspace", name_of(f.id()));
            }
            "delegates" => {
                if planner != Some(f.id()) {
                    issues.push(
                        Issue::error(format!(
                            "only the planner delegates — wire the Orchestrator to {} first",
                            name_of(f.id())
                        ))
                        .edge(&e.id),
                    );
                } else if planner == Some(t.id()) {
                    issues.push(Issue::error("the planner cannot delegate to itself").edge(&e.id));
                } else {
                    agents.entry(t.id()).or_default().routable = true;
                    if t.s("description").is_none() {
                        issues.push(
                            Issue::error(
                                "needs a description: the planner routes by it (what this agent does, what it is not for)",
                            )
                            .node(t.id())
                            .field("description"),
                        );
                        issues.push(
                            Issue::error(format!("{} has no description yet", name_of(t.id())))
                                .edge(&e.id),
                        );
                    }
                }
                writes = format!("agents.{}.description", name_of(t.id()));
            }
            "plans" => {
                writes = format!("orchestrator.agent = \"{}\"", name_of(t.id()));
            }
            "authenticates" => {
                let env = f.s("env").unwrap_or_default();
                let engine = t.s("engine").unwrap_or_default();
                secret_users
                    .entry(f.id())
                    .or_default()
                    .push(name_of(t.id()));
                match engine.as_str() {
                    "openrouter" => {
                        if env != "OPENROUTER_API_KEY" {
                            issues.push(
                                Issue::warn(format!(
                                    "the OpenRouter engine reads OPENROUTER_API_KEY, not {env}"
                                ))
                                .edge(&e.id),
                            );
                        }
                        writes = "OPENROUTER_API_KEY (read by the OpenRouter engine)".into();
                    }
                    "local" => {
                        agents.entry(t.id()).or_default().local_key_env = Some(env.clone());
                        writes = format!("agents.{}.local.api_key_env", name_of(t.id()));
                    }
                    other => {
                        issues.push(
                            Issue::warn(format!(
                                "{other} signs in with its CLI login — this key is not used"
                            ))
                            .edge(&e.id),
                        );
                        writes = "(not used)".into();
                    }
                }
            }
            "grants" => {
                // Resolved once every `uses` wire is known (below).
                writes = format!(
                    "agents.<each user>.scopes.{}.env_reads",
                    t.s("tool").unwrap_or_default()
                );
            }
            "served_by" => {
                served.insert(f.id());
                secret_users
                    .entry(f.id())
                    .or_default()
                    .push("seal proxy".into());
                if f.s("backend").as_deref() != Some("cloudflare") {
                    issues.push(
                        Issue::error(
                            "set Stored in = Cloudflare seal proxy to serve it from the Worker",
                        )
                        .edge(&e.id),
                    );
                }
                writes = "keys.env · keys.strip".into();
            }
            "bot_token" => {
                let env = f.s("env").unwrap_or_default();
                secret_users
                    .entry(f.id())
                    .or_default()
                    .push("telegram".into());
                if env != "TELEGRAM_BOT_TOKEN" {
                    issues.push(
                        Issue::warn(format!("the bot reads TELEGRAM_BOT_TOKEN, not {env}"))
                            .edge(&e.id),
                    );
                }
                writes = "TELEGRAM_BOT_TOKEN (read by tengu telegram)".into();
            }
            "signs" => {
                let env = f.s("env").unwrap_or_default();
                let name = t.s("name").unwrap_or_default();
                secret_users
                    .entry(f.id())
                    .or_default()
                    .push(format!("webhook {name}"));
                if webhook_secret.insert(t.id(), env).is_some() {
                    issues.push(Issue::error("one signing secret per webhook").edge(&e.id));
                }
                writes = format!("webhooks.endpoints.{name}.secret_env");
            }
            "chats" => {
                if bot_agent.is_some() {
                    issues.push(
                        Issue::error("the bot chats with one agent (the default)").edge(&e.id),
                    );
                } else {
                    bot_agent = Some(t.id());
                    agents.entry(t.id()).or_default().chats = true;
                }
                writes = format!("agents.{}.default = true", name_of(t.id()));
            }
            "triggers" => {
                let name = f.s("name").unwrap_or_default();
                if webhook_agent.insert(f.id(), t.id()).is_some() {
                    issues.push(Issue::error("a webhook triggers one agent").edge(&e.id));
                }
                writes = format!("webhooks.endpoints.{name}.agent");
            }
            _ => {}
        }
        edge_kinds.insert(
            e.id.clone(),
            (c.edge.to_string(), c.label.to_string(), writes),
        );
    }

    // `grants`: the secret joins the scope of each agent using the tool.
    for e in &bp.edges {
        let (Some(f), Some(t)) = (cards.get(e.from.as_str()), cards.get(e.to.as_str())) else {
            continue;
        };
        if f.spec.kind != "secret" || t.spec.kind != "tool" {
            continue;
        }
        let env = f.s("env").unwrap_or_default();
        let tool = t.s("tool").unwrap_or_default();
        let users = tool_users.get(t.id()).cloned().unwrap_or_default();
        if users.is_empty() {
            issues.push(Issue::warn("no agent uses this tool yet").edge(&e.id));
        } else if !t.b("restrict") {
            issues.push(
                Issue::warn(
                    "the tool is unrestricted (it may read any env var) — turn on Restrict to limit it to wired secrets",
                )
                .edge(&e.id),
            );
        }
        for u in users {
            secret_users
                .entry(f.id())
                .or_default()
                .push(format!("{} · {tool}", name_of(u)));
            if let Some(scope) = agents.get_mut(u).and_then(|a| a.scopes.get_mut(&tool)) {
                if !scope.env_reads.contains(&env) {
                    scope.env_reads.push(env.clone());
                }
            }
        }
    }

    // Confined workspaces scope the file tools.
    for a in agents.values_mut() {
        let Some((path, true)) = a.workspace.clone() else {
            continue;
        };
        for t in FILE_TOOLS {
            if !a.tools.is_empty() && !a.tools.iter().any(|x| x == t) {
                continue;
            }
            let scope = a.scopes.entry(t.to_string()).or_default();
            if !scope.fs_roots.contains(&path) {
                scope.fs_roots.push(path.clone());
            }
        }
    }

    // ── after the wires ──
    for c in bp.nodes.iter().filter_map(|n| cards.get(n.id.as_str())) {
        let unused = !used.contains(c.id());
        match c.spec.kind {
            "tool" | "skill" | "workspace" if unused => issues.push(
                Issue::warn(format!(
                    "no agent is wired to this {}",
                    c.spec.label.to_lowercase()
                ))
                .node(c.id()),
            ),
            "secret" => {
                if unused {
                    issues.push(
                        Issue::warn("not wired — it is only listed in the secrets checklist")
                            .node(c.id()),
                    );
                }
                if c.s("backend").as_deref() == Some("cloudflare") {
                    if !facts.keys_supported {
                        issues.push(
                            Issue::error(
                                "this build has no [keys] section (the seal proxy, PR #50)",
                            )
                            .node(c.id())
                            .field("backend"),
                        );
                    }
                    if !served.contains(c.id()) {
                        issues.push(
                            Issue::error("wire it to the Cloudflare seal proxy card").node(c.id()),
                        );
                    }
                    let lines = c.list("keys_env");
                    if lines.is_empty() {
                        issues.push(
                            Issue::error("name at least one [keys.env] line (VAR=route)")
                                .node(c.id())
                                .field("keys_env"),
                        );
                    }
                    for l in &lines {
                        if parse_keys_line(l).is_none() {
                            issues.push(
                                Issue::error(format!("'{l}' is not VAR=route"))
                                    .node(c.id())
                                    .field("keys_env"),
                            );
                        }
                    }
                }
            }
            "seal_proxy" => {
                if !facts.keys_supported {
                    issues.push(
                        Issue::error("this build has no [keys] section (the seal proxy, PR #50)")
                            .node(c.id()),
                    );
                }
                if served.is_empty() {
                    issues.push(Issue::warn("no secret is served by the proxy").node(c.id()));
                }
            }
            "orchestrator" if planner.is_none() => issues
                .push(Issue::error("wire the Orchestrator to the agent that plans").node(c.id())),
            "telegram" => {
                if bot_agent.is_none() {
                    issues.push(Issue::error("wire it to the agent it chats with").node(c.id()));
                }
                if c.list("allowed_users").is_empty() {
                    issues.push(
                        Issue::warn(
                            "no allowed users: the bot refuses to start unless TENGU_TELEGRAM_ALLOWED_USERS is set",
                        )
                        .node(c.id())
                        .field("allowed_users"),
                    );
                }
            }
            "webhook" if !webhook_agent.contains_key(c.id()) => {
                issues.push(Issue::error("wire it to the agent it triggers").node(c.id()))
            }
            "agent" => {
                let engine = c.s("engine").unwrap_or_default();
                if engine == "local" && c.i("context_window").is_none() {
                    issues.push(
                        Issue::warn("set the local server's real context window")
                            .node(c.id())
                            .field("context_window"),
                    );
                }
                if planner == Some(c.id()) && (engine == "claude_code" || engine == "codex") {
                    issues.push(
                        Issue::warn(
                            "a CLI engine as planner may call tools instead of writing plan JSON — use OpenRouter for the planner",
                        )
                        .node(c.id())
                        .field("engine"),
                    );
                }
            }
            _ => {}
        }
    }
    for c in of_kind("webhook") {
        if !webhook_secret.contains_key(c.id()) && webhook_agent.contains_key(c.id()) {
            issues.push(
                Issue::warn("unsigned: anyone who reaches the port can trigger it — wire a secret")
                    .node(c.id()),
            );
        }
    }
    let defaults: Vec<&str> = of_kind("agent")
        .into_iter()
        .filter(|c| c.b("default") || agents.get(c.id()).is_some_and(|a| a.chats))
        .map(|c| c.id())
        .collect();
    if defaults.len() > 1 {
        for id in &defaults {
            issues.push(
                Issue::error("only one default agent (the Telegram bot's agent is the default)")
                    .node(id)
                    .field("default"),
            );
        }
    }
    if of_kind("agent").is_empty() {
        issues.push(Issue::error("add at least one agent"));
    } else if defaults.is_empty() {
        issues.push(Issue::warn(
            "no default agent: tick Default agent on the one chat starts with",
        ));
    }

    // ── secrets checklist ──
    let mut secrets: Vec<SecretUse> = Vec::new();
    for c in of_kind("secret") {
        let Some(env) = c.s("env") else { continue };
        secrets.push(SecretUse {
            env,
            backend: c.s("backend").unwrap_or_else(|| "vault".into()),
            used_by: secret_users.remove(c.id()).unwrap_or_default(),
            node: Some(c.id().to_string()),
        });
    }
    let mut implicit = |env: &str, user: String| {
        if let Some(s) = secrets.iter_mut().find(|s| s.env == env) {
            if !s.used_by.contains(&user) {
                s.used_by.push(user);
            }
        } else {
            secrets.push(SecretUse {
                env: env.into(),
                backend: "vault or env".into(),
                used_by: vec![user],
                node: None,
            });
        }
    };
    for c in of_kind("agent") {
        if c.s("engine").as_deref() == Some("openrouter") {
            implicit("OPENROUTER_API_KEY", name_of(c.id()));
        }
    }
    if !of_kind("telegram").is_empty() {
        implicit("TELEGRAM_BOT_TOKEN", "telegram".into());
    }

    // ── emit ──
    let (toml, kept) = emit(
        bp,
        &cards,
        &of_kind("agent"),
        &agents,
        &agent_name,
        planner,
        &webhook_agent,
        &webhook_secret,
        current,
        &mut issues,
    );
    let status = Status::fold(bp, issues, &titles, &edge_kinds);
    Compiled {
        toml,
        status,
        secrets,
        kept,
    }
}

/// `VAR=route` → (VAR, route).
fn parse_keys_line(line: &str) -> Option<(String, String)> {
    let (k, v) = line.split_once('=')?;
    let (k, v) = (k.trim(), v.trim());
    (valid_env(k) && !v.is_empty()).then(|| (k.to_string(), v.to_string()))
}

fn q(s: &str) -> String {
    toml::Value::String(s.to_string()).to_string()
}

fn arr(v: &[String]) -> String {
    toml::Value::Array(v.iter().map(|s| toml::Value::String(s.clone())).collect()).to_string()
}

#[allow(clippy::too_many_arguments)]
fn emit(
    bp: &Blueprint,
    cards: &BTreeMap<&str, Card<'_>>,
    agent_cards: &[&Card<'_>],
    agents: &BTreeMap<&str, AgentOut>,
    agent_name: &BTreeMap<&str, String>,
    planner: Option<&str>,
    webhook_agent: &BTreeMap<&str, &str>,
    webhook_secret: &BTreeMap<&str, String>,
    current: Option<&str>,
    issues: &mut Vec<Issue>,
) -> (String, Vec<String>) {
    use std::fmt::Write;
    let mut o = String::new();
    let of = |k: &str| -> Vec<&Card<'_>> {
        bp.nodes
            .iter()
            .filter_map(|n| cards.get(n.id.as_str()))
            .filter(|c| c.spec.kind == k)
            .collect()
    };
    let _ = writeln!(
        o,
        "# sandboxes/{0}/config.toml — composed by the Studio builder from builder.json.\n\
         # Edit on the canvas (`tengu studio --sandbox {0} --allow-edit`) and Finalise;\n\
         # a hand edit to a section the builder owns shows in the next Finalise diff and is\n\
         # replaced. Other top-level sections are kept as they are.",
        bp.sandbox
    );

    // Kept: the current file's non-owned keys (scalars first: they must
    // precede every table).
    let mut kept_names = Vec::new();
    let mut kept_tables = toml::Table::new();
    if let Some(cur) = current {
        match cur.parse::<toml::Table>() {
            Ok(t) => {
                let mut scalars = toml::Table::new();
                for (k, v) in t {
                    if OWNED.contains(&k.as_str()) {
                        continue;
                    }
                    kept_names.push(k.clone());
                    let is_table = v.is_table()
                        || v.as_array()
                            .is_some_and(|a| !a.is_empty() && a.iter().all(toml::Value::is_table));
                    if is_table {
                        kept_tables.insert(k, v);
                    } else {
                        scalars.insert(k, v);
                    }
                }
                if !scalars.is_empty() {
                    let _ = writeln!(
                        o,
                        "\n{}",
                        toml::to_string(&scalars).unwrap_or_default().trim_end()
                    );
                }
            }
            Err(e) => issues.push(Issue::warn(format!(
                "the current config.toml does not parse ({e}); nothing is kept from it"
            ))),
        }
    }

    if let Some(s) = of("sandbox").first() {
        let _ = writeln!(
            o,
            "\n[egress]\nnetwork = {}",
            q(&s.s("network").unwrap_or_else(|| "tor".into()))
        );
        let hosts = s.list("allow_hosts");
        if !hosts.is_empty() {
            let _ = writeln!(o, "allow_hosts = {}", arr(&hosts));
        }
        let _ = writeln!(o, "\n[memory]\nenabled = {}", s.b("memory"));
        if s.b("studio_control") {
            let _ = writeln!(o, "\n[studio]\ncontrol = true");
        }
    }

    if let Some(p) = of("seal_proxy").first() {
        let _ = writeln!(o, "\n[keys]");
        if let Some(v) = p.s("proxy") {
            let _ = writeln!(o, "proxy = {}", q(&v));
        }
        if let Some(v) = p.s("client") {
            let _ = writeln!(o, "client = {}", q(&v));
        }
        if let Some(v) = p.s("agent_socket") {
            let _ = writeln!(o, "agent_socket = {}", q(&v));
        }
        let mut strip: Vec<String> = Vec::new();
        let mut env: BTreeMap<String, String> = BTreeMap::new();
        for c in of("secret") {
            if c.s("backend").as_deref() != Some("cloudflare") {
                continue;
            }
            for line in c.list("keys_env") {
                if let Some((k, v)) = parse_keys_line(&line) {
                    env.insert(k, v);
                }
            }
            for s in c.list("strip") {
                if !strip.contains(&s) {
                    strip.push(s);
                }
            }
        }
        if !strip.is_empty() {
            let _ = writeln!(o, "strip = {}", arr(&strip));
        }
        if !env.is_empty() {
            let _ = writeln!(o, "\n[keys.env]");
            for (k, v) in &env {
                let _ = writeln!(o, "{k} = {}", q(v));
            }
        }
    }

    if let Some(orch) = of("orchestrator").first() {
        if let Some(name) = planner.and_then(|p| agent_name.get(p)) {
            let _ = writeln!(o, "\n[orchestrator]\nagent = {}", q(name));
            if let Some(n) = orch.i("max_attempts_per_step") {
                let _ = writeln!(o, "max_attempts_per_step = {n}");
            }
            if let Some(n) = orch.i("max_replans") {
                let _ = writeln!(o, "max_replans = {n}");
            }
        }
    }

    if let Some(t) = of("telegram").first() {
        let _ = writeln!(o, "\n[telegram]\nenabled = true");
        let users = t.list("allowed_users");
        if !users.is_empty() {
            let _ = writeln!(o, "allowed_users = {}", arr(&users));
        }
    }

    let hooks = of("webhook");
    if !hooks.is_empty() {
        let _ = writeln!(o, "\n[webhooks]\nenabled = true");
        let mut sorted: Vec<&&Card<'_>> = hooks.iter().collect();
        sorted.sort_by_key(|c| c.s("name"));
        for c in sorted {
            let (Some(name), Some(agent)) = (
                c.s("name").filter(|n| valid_name(n)),
                webhook_agent.get(c.id()).and_then(|a| agent_name.get(a)),
            ) else {
                continue;
            };
            let _ = writeln!(o, "\n[webhooks.endpoints.{name}]\nagent = {}", q(agent));
            if let Some(env) = webhook_secret.get(c.id()) {
                let _ = writeln!(o, "secret_env = {}", q(env));
            }
            if let Some(g) = c.s("goal_template") {
                let _ = writeln!(o, "goal_template = {}", q(&g));
            }
        }
    }

    let mut ordered: Vec<&&Card<'_>> = agent_cards
        .iter()
        .filter(|c| agent_name.contains_key(c.id()))
        .collect();
    ordered.sort_by_key(|c| agent_name.get(c.id()).cloned());
    let mut done = BTreeSet::new();
    for c in ordered {
        let name = &agent_name[c.id()];
        if !done.insert(name.clone()) {
            continue;
        }
        let empty = AgentOut::default();
        let a = agents.get(c.id()).unwrap_or(&empty);
        let s = |k: &str| c.s(k);
        let engine = s("engine").unwrap_or_default();
        let _ = writeln!(o, "\n# ── agent {name} (card {}) ──", c.id());
        let _ = writeln!(o, "[agents.{name}]");
        let _ = writeln!(o, "engine = {}", q(&engine));
        let _ = writeln!(o, "model = {}", q(&s("model").unwrap_or_default()));
        if c.b("default") || a.chats {
            let _ = writeln!(o, "default = true");
        }
        if a.routable {
            if let Some(d) = s("description") {
                let _ = writeln!(o, "description = {}", q(&d));
            }
            let ex = c.list("example_queries");
            if !ex.is_empty() {
                let _ = writeln!(o, "example_queries = {}", arr(&ex));
            }
        }
        if !a.tools.is_empty() {
            let _ = writeln!(o, "tools = {}", arr(&a.tools));
        }
        if !a.skills.is_empty() {
            let _ = writeln!(o, "skill_packages = {}", arr(&a.skills));
        }
        if let Some((path, _)) = &a.workspace {
            let _ = writeln!(o, "workspace = {}", q(path));
        }
        let (dn, ins) = (s("display_name"), s("instructions"));
        if dn.is_some() || ins.is_some() {
            let _ = writeln!(o, "\n[agents.{name}.identity]");
            if let Some(v) = dn {
                let _ = writeln!(o, "name = {}", q(&v));
            }
            if let Some(v) = ins {
                let _ = writeln!(o, "instructions = {}", q(&v));
            }
        }
        let rounds = c.i("max_tool_rounds");
        let timeout = c.i("step_timeout_secs");
        let window = (engine == "local").then(|| c.i("context_window")).flatten();
        if rounds.is_some() || timeout.is_some() || window.is_some() {
            let _ = writeln!(o, "\n[agents.{name}.limits]");
            if let Some(n) = rounds {
                let _ = writeln!(o, "max_tool_rounds = {n}");
            }
            if let Some(n) = timeout {
                let _ = writeln!(o, "step_timeout_secs = {n}");
            }
            if let Some(n) = window {
                let _ = writeln!(o, "context_window = {n}");
            }
        }
        match engine.as_str() {
            "local" => {
                let _ = writeln!(o, "\n[agents.{name}.local]");
                if let Some(u) = s("local_base_url") {
                    let _ = writeln!(o, "base_url = {}", q(&u));
                }
                if let Some(k) = a.local_key_env.clone().or_else(|| s("local_api_key_env")) {
                    let _ = writeln!(o, "api_key_env = {}", q(&k));
                }
            }
            "claude_code" => {
                let p = s("claude_tools").unwrap_or_else(|| "read_only".into());
                let _ = writeln!(
                    o,
                    "\n[agents.{name}.claude_code]\nbuiltin_tools_profile = {}",
                    q(&p)
                );
            }
            "codex" => {
                let m = s("codex_sandbox").unwrap_or_else(|| "read-only".into());
                let _ = writeln!(o, "\n[agents.{name}.codex]\nsandbox = {}", q(&m));
            }
            _ => {}
        }
        for (tool, sc) in &a.scopes {
            let _ = writeln!(o, "\n[agents.{name}.scopes.{tool}]");
            let _ = writeln!(o, "fs_roots = {}", arr(&sc.fs_roots));
            let _ = writeln!(o, "net_hosts = {}", arr(&sc.net_hosts));
            let _ = writeln!(o, "env_reads = {}", arr(&sc.env_reads));
        }
    }

    if !kept_tables.is_empty() {
        let _ = writeln!(
            o,
            "\n# ── kept from the previous config.toml (not on the canvas) ──\n{}",
            toml::to_string(&kept_tables).unwrap_or_default().trim_end()
        );
    }
    (o, kept_names)
}
