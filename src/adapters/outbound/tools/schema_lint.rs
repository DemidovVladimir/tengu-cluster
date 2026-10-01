//! Tool schema lint (`x-tool-schema-lint`): every tool definition an engine
//! can receive stays in the JSON-Schema subset that OpenRouter's providers
//! (OpenAI-style functions, Gemini), local OpenAI-compatible servers (Ollama,
//! llama.cpp grammar-constrained calls) and Claude (API, and Claude Code via
//! `mcp__tengu-tools__<name>`) all accept. Rules, sources and the live
//! evidence behind them: `docs/tools.md` § Tool schema subset. Every
//! violation is reported at once.
//!
//! | Source | Covers | When |
//! |---|---|---|
//! | `advertised_defs(true, WORKSPACE_TOOLS)` | every catalog row, opt-ins and memory included (`agentic_memory` under `postgres_memory`) | CI (test below) |
//! | `compress_and_store::definition()` | appended to every subagent | CI |
//! | `mcp_client::enumerate_tools` on the fake stdio server | `[[mcp_servers]]` tools as `{server}__{tool}` | CI |
//! | `SkillRegistry` over a fixture SKILL.md | shell-skill tools (`skill_to_tool_def`) | CI |
//! | every tool a real `[[mcp_servers]]` entry lists (`mcp_client::linted_tool`: `McpPlugin::tools`, `enumerate_tools`) | external schemas tengu does not control | runtime: a violator is dropped with a warn naming the server, the tool and each rule |

use serde_json::{Map, Value};

use crate::domain::message::ToolDef;

/// OpenAI-style function name limit (Claude and Gemini allow 128).
const MAX_NAME: usize = 64;
/// OpenAI's function description limit.
const MAX_TOOL_DESCRIPTION: usize = 1024;
/// Bounded per field — tool lists must fit a local model's context.
const MAX_PROPERTY_DESCRIPTION: usize = 512;
/// One JSON type per schema: no unions (Gemini, Ollama's typed tool structs).
const TYPES: [&str; 6] = ["string", "number", "integer", "boolean", "array", "object"];
/// String formats every target accepts: Gemini takes only `enum` and
/// `date-time`; OpenAI strict and llama.cpp grammars include `date-time`.
const STRING_FORMATS: [&str; 1] = ["date-time"];
/// Never, at any depth. OpenAI and Claude reject combinators at the top level;
/// Gemini's schema and Ollama's tool structs have none of them nor refs;
/// `const` = a one-value `enum`.
const FORBIDDEN: [&str; 8] = [
    "oneOf",
    "anyOf",
    "allOf",
    "not",
    "const",
    "$ref",
    "$defs",
    "definitions",
];

/// Every rule `def` breaks, one line each: `<tool>: <where>: <problem>`.
pub(crate) fn violations(def: &ToolDef) -> Vec<String> {
    let mut lint = Lint {
        tool: &def.name,
        out: Vec::new(),
    };
    lint.name();
    let n = def.description.chars().count();
    if n > MAX_TOOL_DESCRIPTION {
        lint.bad(
            "description",
            format!("{n} chars (> {MAX_TOOL_DESCRIPTION})"),
        );
    }
    let root = &def.parameters;
    if root.get("type").and_then(Value::as_str) != Some("object")
        || !root.get("properties").is_some_and(Value::is_object)
    {
        lint.bad(
            "parameters",
            "root must be `type: \"object\"` with a `properties` object",
        );
    }
    lint.schema(root, "parameters");
    lint.out
}

struct Lint<'a> {
    tool: &'a str,
    out: Vec<String>,
}

impl Lint<'_> {
    fn bad(&mut self, at: &str, problem: impl std::fmt::Display) {
        self.out.push(format!("{}: {at}: {problem}", self.tool));
    }

    /// `[a-zA-Z0-9_-]` (OpenAI, Claude), first a letter or `_` (Gemini), ≤ 64.
    fn name(&mut self) {
        let n = self.tool;
        let ok = n.len() <= MAX_NAME
            && n.starts_with(|c: char| c.is_ascii_alphabetic() || c == '_')
            && n.chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-');
        if !ok {
            self.bad("name", "must match ^[a-zA-Z_][a-zA-Z0-9_-]{0,63}$");
        }
    }

    /// One (sub)schema at `at` (a JSON-pointer-like path).
    fn schema(&mut self, s: &Value, at: &str) {
        let Some(o) = s.as_object() else {
            return self.bad(at, "a schema must be a JSON object");
        };
        for k in FORBIDDEN {
            if o.contains_key(k) {
                self.bad(at, format!("`{k}` is outside the subset"));
            }
        }
        match o.get("description") {
            None => {}
            Some(Value::String(d)) => {
                let n = d.chars().count();
                if n > MAX_PROPERTY_DESCRIPTION {
                    self.bad(
                        at,
                        format!("description is {n} chars (> {MAX_PROPERTY_DESCRIPTION})"),
                    );
                }
            }
            Some(_) => self.bad(at, "description must be a string"),
        }
        let ty = match o.get("type") {
            None => None,
            Some(Value::String(t)) if TYPES.contains(&t.as_str()) => Some(t.as_str()),
            Some(other) => {
                return self.bad(at, format!("type must be one of {TYPES:?}, got {other}"));
            }
        };
        match o.get("enum") {
            Some(e) => {
                let strings = e
                    .as_array()
                    .is_some_and(|a| !a.is_empty() && a.iter().all(Value::is_string));
                if !strings {
                    self.bad(at, "enum must be a non-empty list of strings");
                }
                if ty.is_some_and(|t| t != "string") {
                    self.bad(at, "enum needs `type: \"string\"`");
                }
            }
            None if ty.is_none() => self.bad(at, "needs `type` or `enum`"),
            None => {}
        }
        if let Some(f) = o.get("format") {
            let ok =
                ty == Some("string") && f.as_str().is_some_and(|f| STRING_FORMATS.contains(&f));
            if !ok {
                self.bad(
                    at,
                    format!("format {f} is outside the subset (strings only: {STRING_FORMATS:?})"),
                );
            }
        }
        match ty {
            Some("object") => self.object(o, at),
            Some("array") => match o.get("items") {
                Some(items @ Value::Object(_)) => self.schema(items, &format!("{at}/items")),
                _ => self.bad(at, "an array needs one `items` schema"),
            },
            _ => {}
        }
    }

    fn object(&mut self, o: &Map<String, Value>, at: &str) {
        let props = match o.get("properties") {
            None => None,
            Some(Value::Object(p)) => Some(p),
            Some(_) => {
                self.bad(at, "properties must be an object");
                None
            }
        };
        match o.get("required") {
            None => {}
            Some(Value::Array(names)) => {
                for n in names {
                    let known = n
                        .as_str()
                        .is_some_and(|n| props.is_some_and(|p| p.contains_key(n)));
                    if !known {
                        self.bad(at, format!("required {n} is not in properties"));
                    }
                }
            }
            Some(_) => self.bad(at, "required must be a list"),
        }
        for (k, v) in props.into_iter().flatten() {
            self.schema(v, &format!("{at}/properties/{k}"));
        }
        if let Some(ap @ Value::Object(_)) = o.get("additionalProperties") {
            self.schema(ap, &format!("{at}/additionalProperties"));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::application::skills::registry::SkillRegistry;
    use crate::domain::tools as names;
    use crate::ports::skill_source::SkillSourcePort;
    use serde_json::json;

    /// A classic shell skill (`## Parameters` + `## Execution`), as a
    /// workspace `SKILL.md` would carry it.
    const SHELL_SKILL: &str = "# disk_usage\n\
        \n\
        Report the disk usage of a directory in the workspace.\n\
        \n\
        ## Parameters\n\
        \n\
        - `path` (string, required): Directory to measure\n\
        - `depth` (number, optional): Max directory depth\n\
        - `human` (boolean, optional): Human-readable sizes\n\
        \n\
        ## Execution\n\
        \n\
        ```sh\n\
        du -d {{depth}} {{path}}\n\
        ```\n";

    struct FixtureSkills;

    impl SkillSourcePort for FixtureSkills {
        fn discover_skill_files(&self) -> Vec<(String, String)> {
            vec![("disk_usage/SKILL.md".into(), SHELL_SKILL.into())]
        }
    }

    fn shell_skill_defs() -> Vec<ToolDef> {
        let mut registry = SkillRegistry::new(Vec::new());
        assert!(registry.reload(&FixtureSkills), "fixture skill loads");
        registry.active_tools()
    }

    /// Every definition an engine can receive (module doc table).
    async fn all_defs() -> Vec<ToolDef> {
        let opt_ins: Vec<String> = names::WORKSPACE_TOOLS
            .iter()
            .map(|s| s.to_string())
            .collect();
        let mut defs = super::super::advertised_defs(true, &opt_ins);
        defs.push(super::super::skill_lifecycle::compress_and_store::definition());
        defs.extend(
            crate::adapters::outbound::mcp_client::enumerate_tools(&[
                crate::adapters::outbound::mcp_client::tests::fake_server("fake"),
            ])
            .await,
        );
        defs.extend(shell_skill_defs());
        defs
    }

    #[tokio::test]
    async fn every_tool_schema_is_in_the_engine_subset() {
        let defs = all_defs().await;
        let have = |n: &str| defs.iter().any(|d| d.name == n);
        for n in [
            "read_file",
            "compress_and_store",
            "fake__echo",
            "disk_usage",
        ]
        .into_iter()
        .chain(names::WORKSPACE_TOOLS.iter().copied())
        {
            if n == names::AGENTIC_MEMORY && !cfg!(feature = "postgres_memory") {
                continue;
            }
            assert!(have(n), "source missing: {n}");
        }

        let mut problems: Vec<String> = defs.iter().flat_map(violations).collect();
        let mut seen = std::collections::HashSet::new();
        for d in &defs {
            if !seen.insert(d.name.as_str()) {
                problems.push(format!(
                    "{}: name: defined twice (Claude and Gemini reject duplicates)",
                    d.name
                ));
            }
        }
        assert!(
            problems.is_empty(),
            "{} schema-lint violation(s) — rules: docs/tools.md § Tool schema subset\n{}",
            problems.len(),
            problems.join("\n")
        );
    }

    /// Each rule fires — the lint cannot pass by accident.
    #[test]
    fn each_rule_reports() {
        let ok = json!({"type": "object", "properties": {"a": {"type": "string"}}});
        assert!(violations(&ToolDef::new("good_tool-1", "d", ok.clone())).is_empty());
        let date = json!({"type": "object", "properties": {"a": {"type": "string", "format": "date-time"}}});
        assert!(violations(&ToolDef::new("t", "d", date)).is_empty());

        let prop = |schema: Value| json!({"type": "object", "properties": {"a": schema}});
        let long = "x".repeat(MAX_PROPERTY_DESCRIPTION + 1);
        let cases: Vec<(ToolDef, &str)> = vec![
            (
                ToolDef::new("svc.tool", "d", ok.clone()),
                "name: must match",
            ),
            (ToolDef::new("1st", "d", ok.clone()), "name: must match"),
            (
                ToolDef::new(&"n".repeat(65), "d", ok.clone()),
                "name: must match",
            ),
            (
                ToolDef::new("t", &"d".repeat(1025), ok.clone()),
                "description: 1025 chars",
            ),
            (
                ToolDef::new("t", "d", json!({"type": "object"})),
                "root must be",
            ),
            (
                ToolDef::new(
                    "t",
                    "d",
                    json!({"type": "object", "properties": {}, "anyOf": []}),
                ),
                "parameters: `anyOf` is outside",
            ),
            (
                ToolDef::new(
                    "t",
                    "d",
                    json!({"type": "object", "properties": {}, "required": ["a"]}),
                ),
                "required \"a\" is not in properties",
            ),
            (
                ToolDef::new("t", "d", prop(json!({"$ref": "#/x"}))),
                "properties/a: `$ref` is outside",
            ),
            (
                ToolDef::new("t", "d", prop(json!({"type": "string", "const": "x"}))),
                "properties/a: `const` is outside",
            ),
            (
                ToolDef::new("t", "d", prop(json!({"description": "x"}))),
                "properties/a: needs `type` or `enum`",
            ),
            (
                ToolDef::new("t", "d", prop(json!({"type": ["string", "null"]}))),
                "type must be one of",
            ),
            (
                ToolDef::new("t", "d", prop(json!({"type": "array"}))),
                "array needs one `items` schema",
            ),
            (
                ToolDef::new("t", "d", prop(json!({"type": "array", "items": {}}))),
                "properties/a/items: needs `type` or `enum`",
            ),
            (
                ToolDef::new("t", "d", prop(json!({"enum": []}))),
                "enum must be a non-empty list of strings",
            ),
            (
                ToolDef::new("t", "d", prop(json!({"type": "integer", "enum": [1, 2]}))),
                "enum needs `type: \"string\"`",
            ),
            (
                ToolDef::new("t", "d", prop(json!({"type": "string", "format": "uri"}))),
                "format \"uri\" is outside",
            ),
            (
                ToolDef::new(
                    "t",
                    "d",
                    prop(json!({"type": "string", "description": long})),
                ),
                "description is 513 chars",
            ),
            (
                ToolDef::new(
                    "t",
                    "d",
                    prop(json!({"type": "object", "additionalProperties": {"oneOf": []}})),
                ),
                "properties/a/additionalProperties: `oneOf` is outside",
            ),
        ];
        let missed: Vec<String> = cases
            .iter()
            .filter_map(|(def, want)| {
                let got = violations(def);
                (!got.iter().any(|v| v.contains(want))).then(|| format!("{want:?} not in {got:?}"))
            })
            .collect();
        assert!(missed.is_empty(), "{}", missed.join("\n"));
    }
}
