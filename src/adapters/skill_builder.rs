//! Skill subsystem — types, parsing, registry, filesystem discovery,
//! command routing, context fragments, and system prompt building.
//!
//! Shell-skill tool dispatch was moved to `plugins/skill/` in task A8.

use std::collections::{HashMap, HashSet};
use std::hash::{Hash, Hasher};
use std::path::PathBuf;

use anyhow::{bail, Result};

use crate::adapters::config::AgentConfig;
use crate::adapters::ports::SkillSourcePort;
use crate::adapters::prompt_budget::truncate_to_token_budget;
use crate::adapters::token::estimate_tokens_approx_min1;
use crate::adapters::types::ToolDef;

// ===========================================================================
// Types
// ===========================================================================

/// A parsed skill definition — the domain representation of a skill.md file.
#[derive(Debug, Clone)]
pub(crate) struct SkillDefinition {
    pub name: String,
    pub description: String,
    pub parameters: Vec<SkillParameter>,
    pub execution: SkillExecution,
}

#[derive(Debug, Clone)]
pub(crate) enum SkillExecution {
    Shell { template: String },
    Api(ApiExecution),
}

#[derive(Debug, Clone)]
pub(crate) struct ApiExecution {
    pub base_url: String,
}

#[derive(Debug, Clone)]
pub(crate) struct SkillParameter {
    pub name: String,
    pub param_type: SkillParamType,
    pub required: bool,
    pub description: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SkillParamType {
    String,
    Number,
    Boolean,
}

impl SkillParamType {
    fn json_type_str(self) -> &'static str {
        match self {
            Self::String => "string",
            Self::Number => "number",
            Self::Boolean => "boolean",
        }
    }
}

/// Whether a skill is active or disabled by the user.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SkillStatus {
    Active,
    Inactive,
}

/// An environment variable declared by a skill.
#[derive(Debug, Clone)]
pub(crate) struct SkillEnvVar {
    pub name: String,
    pub required: bool,
}

/// A slash command declared by a skill.
#[derive(Debug, Clone)]
pub(crate) struct SkillCommand {
    pub name: String,
}

/// A tracked skill in the registry.
#[derive(Debug, Clone)]
pub(crate) struct SkillEntry {
    pub definition: SkillDefinition,
    pub status: SkillStatus,
    pub content_hash: u64,
    pub context_body: Option<String>,
    pub env_vars: Vec<SkillEnvVar>,
    pub commands: Vec<SkillCommand>,
}

impl SkillEntry {
    pub fn missing_required_env_vars(&self) -> Vec<String> {
        self.env_vars
            .iter()
            .filter(|ev| ev.required && std::env::var(&ev.name).unwrap_or_default().is_empty())
            .map(|ev| ev.name.clone())
            .collect()
    }
}

// --- Internal types (not exposed outside this module) ---

#[derive(Debug, Clone)]
enum ParsedSkill {
    Classic(SkillDefinition),
    Api {
        definition: SkillDefinition,
        context_body: String,
        env_vars: Vec<SkillEnvVar>,
        commands: Vec<SkillCommand>,
    },
}

#[derive(Debug, Clone)]
struct SkillFrontmatter {
    name: String,
    description: String,
    base_url: String,
    env_vars: Vec<SkillEnvVar>,
    commands: Vec<SkillCommand>,
}

#[derive(Debug, Default)]
struct SkillDiff {
    added: Vec<String>,
    removed: Vec<String>,
    changed: Vec<String>,
}

impl SkillDiff {
    fn is_empty(&self) -> bool {
        self.added.is_empty() && self.removed.is_empty() && self.changed.is_empty()
    }
}

struct FreshSkillEntry {
    name: String,
    definition: SkillDefinition,
    content_hash: u64,
    context_body: Option<String>,
    env_vars: Vec<SkillEnvVar>,
    commands: Vec<SkillCommand>,
}

// ===========================================================================
// Parsing
// ===========================================================================

/// Parse a skill file, trying frontmatter format first, falling back to classic.
fn parse_skill_file(content: &str) -> Result<ParsedSkill> {
    if let Some((fm, body)) = try_parse_frontmatter(content) {
        let definition = frontmatter_to_skill_definition(&fm);
        Ok(ParsedSkill::Api {
            definition,
            context_body: body,
            env_vars: fm.env_vars,
            commands: fm.commands,
        })
    } else {
        parse_skill_markdown(content).map(ParsedSkill::Classic)
    }
}

fn parse_skill_markdown(content: &str) -> Result<SkillDefinition> {
    let lines: Vec<&str> = content.lines().collect();

    let name_line = lines
        .iter()
        .find(|l| l.starts_with("# ") && !l.starts_with("## "))
        .ok_or_else(|| anyhow::anyhow!("Missing '# <tool_name>' heading"))?;
    let name = name_line.trim_start_matches("# ").trim().to_string();

    let h1_idx = lines.iter().position(|l| l == name_line).unwrap();
    let first_h2_idx = lines
        .iter()
        .enumerate()
        .skip(h1_idx + 1)
        .find(|(_, l)| l.starts_with("## "))
        .map(|(i, _)| i)
        .unwrap_or(lines.len());
    let description = lines[h1_idx + 1..first_h2_idx]
        .iter()
        .copied()
        .collect::<Vec<&str>>()
        .join("\n")
        .trim()
        .to_string();

    let sections = extract_h2_sections(&lines);

    let parameters = if let Some(param_lines) = sections.get("parameters") {
        parse_parameters(param_lines)?
    } else {
        Vec::new()
    };

    let execution_template = sections
        .get("execution")
        .ok_or_else(|| anyhow::anyhow!("Missing '## Execution' section"))
        .and_then(|lines| extract_fenced_code(lines))?;

    Ok(SkillDefinition {
        name,
        description,
        parameters,
        execution: SkillExecution::Shell {
            template: execution_template,
        },
    })
}

fn extract_h2_sections<'a>(lines: &[&'a str]) -> HashMap<String, Vec<&'a str>> {
    let mut map = HashMap::new();
    let mut current_key: Option<String> = None;
    let mut current_lines: Vec<&str> = Vec::new();

    for line in lines {
        if line.starts_with("## ") {
            if let Some(key) = current_key.take() {
                map.insert(key, std::mem::take(&mut current_lines));
            }
            let heading = line.trim_start_matches("## ").trim().to_lowercase();
            current_key = Some(heading);
            current_lines = Vec::new();
        } else if current_key.is_some() {
            current_lines.push(line);
        }
    }
    if let Some(key) = current_key {
        map.insert(key, current_lines);
    }
    map
}

fn parse_parameters(lines: &[&str]) -> Result<Vec<SkillParameter>> {
    let mut params = Vec::new();
    for line in lines {
        let trimmed = line.trim();
        if !trimmed.starts_with("- `") {
            continue;
        }
        let after_dash = trimmed.trim_start_matches("- ");
        let name_end = after_dash
            .find("` ")
            .or_else(|| after_dash.find(")`"))
            .ok_or_else(|| anyhow::anyhow!("Malformed parameter line: {}", trimmed))?;
        let name = after_dash[1..name_end].to_string();

        let rest = &after_dash[name_end + 1..];
        let (param_type, required, description) = if let Some(paren_start) = rest.find('(') {
            let paren_end = rest
                .find(')')
                .ok_or_else(|| anyhow::anyhow!("Unclosed parenthesis: {}", trimmed))?;
            let paren_content = &rest[paren_start + 1..paren_end];
            let parts: Vec<&str> = paren_content.split(',').map(|s| s.trim()).collect();

            let pt = match parts.first().map(|s| s.to_lowercase()).as_deref() {
                Some("string") => SkillParamType::String,
                Some("number") | Some("integer") | Some("int") | Some("float") => {
                    SkillParamType::Number
                }
                Some("boolean") | Some("bool") => SkillParamType::Boolean,
                _ => SkillParamType::String,
            };

            let req = parts.iter().any(|s| s.to_lowercase().contains("required"));
            let opt = parts.iter().any(|s| s.to_lowercase().contains("optional"));
            let is_required = req || !opt;

            let desc = rest[paren_end + 1..]
                .trim_start_matches(':')
                .trim()
                .to_string();

            (pt, is_required, desc)
        } else {
            (SkillParamType::String, true, rest.trim().to_string())
        };

        params.push(SkillParameter {
            name,
            param_type,
            required,
            description,
        });
    }
    Ok(params)
}

fn extract_fenced_code(lines: &[&str]) -> Result<String> {
    let mut in_block = false;
    let mut code = Vec::new();

    for line in lines {
        if line.starts_with("```") {
            if in_block {
                break;
            }
            in_block = true;
            continue;
        }
        if in_block {
            code.push(*line);
        }
    }

    if code.is_empty() {
        bail!("No fenced code block found in Execution section");
    }
    Ok(code.join("\n").trim().to_string())
}


// --- Frontmatter parsing ---

fn validate_base_url(url: &str) -> bool {
    (url.starts_with("https://") || url.starts_with("http://"))
        && !url
            .chars()
            .any(|c| matches!(c, ';' | '|' | '&' | '`' | '$' | '(' | ')' | '\n' | '\r'))
}

fn api_skill_preamble(name: &str) -> String {
    format!(
        "# {} — API reference\n\n\
         CRITICAL: Follow the documented examples EXACTLY. The platform expands \
         `$ENV_VAR` references automatically at runtime — pass them as literal strings \
         (e.g. pass `$MOLECULE_LABS_URL` as the url value, do NOT guess what it resolves to). \
         Use ONLY the URLs and env var names shown below. \
         Do NOT invent, modify, or construct your own URLs or variable names.\n\n\
         Parameters: `url`, `method`, `headers` (JSON object, values support $ENV_VAR), \
         `body` (JSON string), `file_path`, `file_field_name`, \
         `auth_bearer_env` (env var name for Bearer token), \
         `auth_basic_user_env` / `auth_basic_pass_env` (env var names for Basic auth).\n\n",
        name,
    )
}

fn try_parse_frontmatter(content: &str) -> Option<(SkillFrontmatter, String)> {
    let trimmed = content.trim_start();
    if !trimmed.starts_with("---") {
        return None;
    }

    let after_first = &trimmed[3..].trim_start_matches(|c: char| c == '-');
    let after_first = after_first.strip_prefix('\n').unwrap_or(after_first);

    let closing = after_first.find("\n---")?;
    let yaml_block = &after_first[..closing];
    let body = after_first[closing + 4..].trim_start_matches('-').trim();

    let mut name = None;
    let mut description = None;
    let mut base_url = None;
    let mut env_vars: Vec<SkillEnvVar> = Vec::new();
    let mut commands: Vec<SkillCommand> = Vec::new();

    #[derive(PartialEq)]
    enum ListBlock {
        EnvVars,
        Commands,
    }
    let mut current_block: Option<ListBlock> = None;

    for raw_line in yaml_block.lines() {
        let line = raw_line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }

        let is_indented = raw_line.starts_with("  ") || raw_line.starts_with('\t');

        if is_indented {
            if let Some(ref block) = current_block {
                match block {
                    ListBlock::EnvVars => {
                        let item = line.trim_start_matches('-').trim();
                        if !item.is_empty() {
                            let (var_name, required) =
                                if let Some(stripped) = item.strip_suffix('?') {
                                    (stripped.to_string(), false)
                                } else {
                                    (item.to_string(), true)
                                };
                            env_vars.push(SkillEnvVar {
                                name: var_name,
                                required,
                            });
                        }
                    }
                    ListBlock::Commands => {
                        let item = line.trim_start_matches('-').trim();
                        if !item.is_empty() {
                            commands.push(SkillCommand {
                                name: item.to_string(),
                            });
                        }
                    }
                }
                continue;
            }
        }

        current_block = None;

        if let Some((key, value)) = line.split_once(':') {
            let k = key.trim();
            let v = value.trim();
            match k {
                "name" => name = Some(v.to_string()),
                "description" => description = Some(v.to_string()),
                "base_url" | "homepage" => base_url = Some(v.to_string()),
                "env_vars" => {
                    if v.is_empty() {
                        current_block = Some(ListBlock::EnvVars);
                    }
                }
                "commands" => {
                    if v.is_empty() {
                        current_block = Some(ListBlock::Commands);
                    }
                }
                _ => {}
            }
        }
    }

    let raw_name = name?;
    let normalized_name = raw_name.replace('-', "_").to_lowercase();
    let base_url = base_url?;

    if !validate_base_url(&base_url) {
        return None;
    }

    Some((
        SkillFrontmatter {
            name: normalized_name,
            description: description.unwrap_or_default(),
            base_url,
            env_vars,
            commands,
        },
        body.to_string(),
    ))
}

fn frontmatter_to_skill_definition(fm: &SkillFrontmatter) -> SkillDefinition {
    SkillDefinition {
        name: fm.name.clone(),
        description: fm.description.clone(),
        parameters: vec![
            SkillParameter {
                name: "method".into(),
                param_type: SkillParamType::String,
                required: true,
                description: "HTTP method (GET, POST, PUT, DELETE)".into(),
            },
            SkillParameter {
                name: "path".into(),
                param_type: SkillParamType::String,
                required: true,
                description: if fm.base_url.ends_with("/graphql") {
                    "API path — for GraphQL use empty string \"\"".into()
                } else {
                    "API path (e.g. /api/v1/posts)".into()
                },
            },
            SkillParameter {
                name: "body".into(),
                param_type: SkillParamType::String,
                required: true,
                description: "JSON request body (use \"{}\" for requests with no body)".into(),
            },
            SkillParameter {
                name: "headers".into(),
                param_type: SkillParamType::String,
                required: false,
                description: "Optional JSON object of additional headers to merge into the request"
                    .into(),
            },
        ],
        execution: SkillExecution::Api(ApiExecution {
            base_url: fm.base_url.clone(),
        }),
    }
}

// ===========================================================================
// Validation
// ===========================================================================

fn validate_skill(skill: &SkillDefinition, reserved: &[&str]) -> Result<()> {
    if skill.name.is_empty() {
        bail!("Skill name must not be empty");
    }
    if !skill.name.chars().all(|c| c.is_alphanumeric() || c == '_') {
        bail!(
            "Skill name '{}' must be alphanumeric or underscores only",
            skill.name
        );
    }
    if reserved.contains(&skill.name.as_str()) {
        bail!("Skill name '{}' conflicts with a built-in tool", skill.name);
    }
    match &skill.execution {
        SkillExecution::Shell { template } => {
            if template.is_empty() {
                bail!("Skill must have a non-empty execution template");
            }
            let mut pos = 0;
            while let Some(start) = template[pos..].find("{{") {
                let abs_start = pos + start + 2;
                if let Some(end) = template[abs_start..].find("}}") {
                    let placeholder = template[abs_start..abs_start + end].trim();
                    if !skill.parameters.iter().any(|p| p.name == placeholder) {
                        bail!(
                            "Template placeholder '{{{{{}}}}}' does not match any declared parameter",
                            placeholder
                        );
                    }
                    pos = abs_start + end + 2;
                } else {
                    break;
                }
            }
        }
        SkillExecution::Api(api) => {
            if api.base_url.is_empty() {
                bail!("API skill must declare a base_url");
            }
        }
    }
    Ok(())
}

// ===========================================================================
// Conversion — SkillDefinition → ToolDef
// ===========================================================================

fn skill_to_tool_def(skill: &SkillDefinition) -> ToolDef {
    let mut properties = serde_json::Map::new();
    let mut required = Vec::new();

    for param in &skill.parameters {
        let mut prop = serde_json::Map::new();
        prop.insert(
            "type".into(),
            serde_json::Value::String(param.param_type.json_type_str().into()),
        );
        if !param.description.is_empty() {
            prop.insert(
                "description".into(),
                serde_json::Value::String(param.description.clone()),
            );
        }
        properties.insert(param.name.clone(), serde_json::Value::Object(prop));
        if param.required {
            required.push(serde_json::Value::String(param.name.clone()));
        }
    }

    let parameters = serde_json::json!({
        "type": "object",
        "properties": properties,
        "required": required,
    });

    ToolDef::new(&skill.name, &skill.description, parameters)
}

// ===========================================================================
// Command rendering — moved to `plugins/skill/shell_tool.rs` in A8.
// ===========================================================================

// ===========================================================================
// Diff (internal to registry reload)
// ===========================================================================

fn diff_skill_sets(
    current: &HashMap<String, SkillEntry>,
    fresh: &[FreshSkillEntry],
) -> SkillDiff {
    let fresh_names: HashSet<&str> = fresh.iter().map(|f| f.name.as_str()).collect();
    let current_names: HashSet<&str> = current.keys().map(|n| n.as_str()).collect();

    let added: Vec<String> = fresh
        .iter()
        .filter(|f| !current_names.contains(f.name.as_str()))
        .map(|f| f.name.clone())
        .collect();

    let removed: Vec<String> = current
        .keys()
        .filter(|n| !fresh_names.contains(n.as_str()))
        .cloned()
        .collect();

    let changed: Vec<String> = fresh
        .iter()
        .filter(|f| {
            current
                .get(f.name.as_str())
                .map(|e| e.content_hash != f.content_hash)
                .unwrap_or(false)
        })
        .map(|f| f.name.clone())
        .collect();

    SkillDiff {
        added,
        removed,
        changed,
    }
}

fn content_hash(content: &str) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    content.hash(&mut hasher);
    hasher.finish()
}

// ===========================================================================
// Registry
// ===========================================================================

/// Mutable registry for user-defined skills with hot-reload and enable/disable.
pub(crate) struct SkillRegistry {
    entries: HashMap<String, SkillEntry>,
    reserved_names: Vec<String>,
    skill_allowlist: Option<Vec<String>>,
}

impl SkillRegistry {
    pub(crate) fn new(reserved: Vec<String>) -> Self {
        Self {
            entries: HashMap::new(),
            reserved_names: reserved,
            skill_allowlist: None,
        }
    }

    pub(crate) fn with_allowlist(mut self, allowlist: Option<Vec<String>>) -> Self {
        self.skill_allowlist = allowlist;
        self
    }

    /// Re-scan the source, parse, diff, and apply changes.
    /// Returns `true` if anything changed.
    pub(crate) fn reload(&mut self, source: &dyn SkillSourcePort) -> bool {
        let raw_files = source.discover_skill_files();
        let reserved_strs: Vec<&str> = self.reserved_names.iter().map(|s| s.as_str()).collect();

        let mut fresh: Vec<FreshSkillEntry> = Vec::new();

        for (filename, content) in &raw_files {
            let hash = content_hash(content);
            match parse_skill_file(content) {
                Ok(ParsedSkill::Classic(skill)) => {
                    if validate_skill(&skill, &reserved_strs).is_err() {
                        tracing::warn!("Skipping invalid skill '{}'", filename);
                        continue;
                    }
                    let name = skill.name.clone();
                    fresh.push(FreshSkillEntry {
                        name,
                        definition: skill,
                        content_hash: hash,
                        context_body: None,
                        env_vars: vec![],
                        commands: vec![],
                    });
                }
                Ok(ParsedSkill::Api {
                    definition,
                    context_body,
                    env_vars,
                    commands,
                }) => {
                    if validate_skill(&definition, &reserved_strs).is_err() {
                        tracing::warn!("Skipping invalid API skill '{}'", filename);
                        continue;
                    }
                    let preamble = api_skill_preamble(&definition.name);
                    let name = definition.name.clone();
                    fresh.push(FreshSkillEntry {
                        name,
                        definition,
                        content_hash: hash,
                        context_body: Some(format!("{preamble}{context_body}")),
                        env_vars,
                        commands,
                    });
                }
                Err(e) => {
                    tracing::warn!("Failed to parse skill '{}': {}", filename, e);
                }
            }
        }

        if let Some(ref allowlist) = self.skill_allowlist {
            fresh.retain(|e| {
                allowlist
                    .iter()
                    .any(|a| a.replace('-', "_").to_lowercase() == e.name)
            });
        }

        let diff = diff_skill_sets(&self.entries, &fresh);
        if diff.is_empty() {
            return false;
        }

        for name in &diff.removed {
            self.entries.remove(name);
            tracing::info!("Skill removed: {name}");
        }

        for entry in fresh {
            if diff.added.contains(&entry.name) {
                tracing::info!("Skill added: {}", entry.name);
                let name = entry.name;
                self.entries.insert(
                    name,
                    SkillEntry {
                        definition: entry.definition,
                        status: SkillStatus::Active,
                        content_hash: entry.content_hash,
                        context_body: entry.context_body,
                        env_vars: entry.env_vars,
                        commands: entry.commands,
                    },
                );
            } else if diff.changed.contains(&entry.name) {
                tracing::info!("Skill updated: {}", entry.name);
                let prev_status = self
                    .entries
                    .get(&entry.name)
                    .map(|e| e.status)
                    .unwrap_or(SkillStatus::Active);
                let name = entry.name;
                self.entries.insert(
                    name,
                    SkillEntry {
                        definition: entry.definition,
                        status: prev_status,
                        content_hash: entry.content_hash,
                        context_body: entry.context_body,
                        env_vars: entry.env_vars,
                        commands: entry.commands,
                    },
                );
            }
        }

        true
    }

    /// Tool definitions for active shell skills only.
    pub(crate) fn active_tools(&self) -> Vec<ToolDef> {
        self.entries
            .values()
            .filter(|e| {
                e.status == SkillStatus::Active
                    && matches!(e.definition.execution, SkillExecution::Shell { .. })
            })
            .map(|e| skill_to_tool_def(&e.definition))
            .collect()
    }

    /// Skill definitions for only active skills (used by the executor).
    pub(crate) fn active_skill_definitions(&self) -> Vec<SkillDefinition> {
        self.entries
            .values()
            .filter(|e| e.status == SkillStatus::Active)
            .map(|e| e.definition.clone())
            .collect()
    }

    /// Context fragments for active API skills (injected into the system prompt).
    pub(crate) fn active_context_fragments(&self) -> Vec<(String, String)> {
        self.entries
            .values()
            .filter(|e| e.status == SkillStatus::Active && e.context_body.is_some())
            .map(|e| {
                (
                    e.definition.name.clone(),
                    e.context_body.clone().unwrap_or_default(),
                )
            })
            .collect()
    }

    /// All skills with their status.
    pub(crate) fn list_all(&self) -> Vec<(String, SkillStatus)> {
        let mut list: Vec<_> = self
            .entries
            .iter()
            .map(|(n, e)| (n.clone(), e.status))
            .collect();
        list.sort_by(|a, b| a.0.cmp(&b.0));
        list
    }

    pub(crate) fn entries(&self) -> &HashMap<String, SkillEntry> {
        &self.entries
    }

    pub(crate) fn enable(&mut self, name: &str) -> Result<bool, String> {
        match self.entries.get_mut(name) {
            Some(entry) => {
                if entry.status == SkillStatus::Active {
                    Ok(false)
                } else {
                    entry.status = SkillStatus::Active;
                    Ok(true)
                }
            }
            None => Err(format!("Skill '{}' not found", name)),
        }
    }

    pub(crate) fn disable(&mut self, name: &str) -> Result<bool, String> {
        match self.entries.get_mut(name) {
            Some(entry) => {
                if entry.status == SkillStatus::Inactive {
                    Ok(false)
                } else {
                    entry.status = SkillStatus::Inactive;
                    Ok(true)
                }
            }
            None => Err(format!("Skill '{}' not found", name)),
        }
    }
}

// ===========================================================================
// Command routing
// ===========================================================================

/// Result of attempting to route a slash command.
pub(crate) enum SkillCommandMatch {
    Matched {
        skill_name: String,
        command: String,
        args: String,
    },
    NotMatched,
}

/// Routes user slash commands to the skill that declared them.
pub(crate) struct SkillCommandRouter {
    routes: HashMap<String, String>,
}

impl SkillCommandRouter {
    pub(crate) fn from_registry(registry: &SkillRegistry) -> Self {
        let mut routes = HashMap::new();
        for (skill_name, entry) in registry.entries() {
            for cmd in &entry.commands {
                routes.insert(cmd.name.clone(), skill_name.clone());
            }
        }
        Self { routes }
    }

    pub(crate) fn route(&self, input: &str) -> SkillCommandMatch {
        let trimmed = input.trim();
        let without_slash = match trimmed.strip_prefix('/') {
            Some(s) => s,
            None => return SkillCommandMatch::NotMatched,
        };

        let (first, rest) = match without_slash.split_once(char::is_whitespace) {
            Some((cmd, args)) => (cmd, args.trim().to_string()),
            None => (without_slash, String::new()),
        };
        let command = first.split('@').next().unwrap_or(first);

        match self.routes.get(command) {
            Some(skill_name) => SkillCommandMatch::Matched {
                skill_name: skill_name.clone(),
                command: command.to_string(),
                args: rest,
            },
            None => SkillCommandMatch::NotMatched,
        }
    }

    pub(crate) fn list(&self) -> Vec<(String, String)> {
        let mut list: Vec<_> = self
            .routes
            .iter()
            .map(|(cmd, skill)| (cmd.clone(), skill.clone()))
            .collect();
        list.sort();
        list
    }
}

// ===========================================================================
// Filesystem discovery
// ===========================================================================

pub(crate) struct FileSystemSkillSource {
    workspace: PathBuf,
}

impl FileSystemSkillSource {
    pub(crate) fn new(workspace: PathBuf) -> Self {
        Self { workspace }
    }

    fn skill_directories(&self) -> Vec<PathBuf> {
        let mut dirs = vec![
            self.workspace.join(".tengu/skills"),
            self.workspace.join("skills"),
        ];
        if let Ok(cwd) = std::env::current_dir() {
            let global = cwd.join("skills");
            if global != self.workspace.join("skills") {
                dirs.push(global);
            }
        }
        dirs
    }
}

impl SkillSourcePort for FileSystemSkillSource {
    fn discover_skill_files(&self) -> Vec<(String, String)> {
        let mut results = Vec::new();
        let mut seen_names = HashSet::new();

        for dir in self.skill_directories() {
            if !dir.is_dir() {
                continue;
            }
            let entries = match std::fs::read_dir(&dir) {
                Ok(entries) => entries,
                Err(_) => continue,
            };
            for entry in entries.flatten() {
                let path = entry.path();

                if path.is_dir() {
                    let skill_file = path.join("SKILL.md");
                    if skill_file.is_file() {
                        let folder_name = path
                            .file_name()
                            .unwrap_or_default()
                            .to_string_lossy()
                            .to_string();
                        if !seen_names.insert(folder_name.clone()) {
                            continue;
                        }
                        if let Ok(content) = std::fs::read_to_string(&skill_file) {
                            results.push((folder_name, content));
                        }
                    }
                    continue;
                }

                let is_md = path.extension().map(|e| e == "md").unwrap_or(false);
                if !is_md {
                    continue;
                }
                let name = path
                    .file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .to_string();
                if !seen_names.insert(name.clone()) {
                    continue;
                }
                if let Ok(content) = std::fs::read_to_string(&path) {
                    results.push((name, content));
                }
            }
        }
        results
    }
}

// ===========================================================================
// Tool execution — moved to `plugins/skill/` (A8). Shell-skill dispatch lives
// in `SkillShellTool` + `SkillPlugin`; this module only owns parsing,
// registry state, context fragments, and system-prompt building now.
// ===========================================================================

// ===========================================================================
// System prompt building
// ===========================================================================

/// Build bounded system prompt from config identity, role, custom instructions,
/// workspace files, and optional skill context fragments.
pub(crate) fn build_system_prompt(
    agent_config: &AgentConfig,
    advertise_workspace_tools: bool,
    skill_contexts: &[String],
) -> String {
    build_system_prompt_with_tools(agent_config, advertise_workspace_tools, skill_contexts, &[])
}

/// Build system prompt with dynamic tool listing generated from ToolDef metadata.
pub(crate) fn build_system_prompt_with_tools(
    agent_config: &AgentConfig,
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

    // 1. Default preamble.
    let preamble = format!(
        "You are {name}. Use tools to execute actions. Never fabricate data."
    );
    total_tokens += estimate_tokens_approx_min1(&preamble);
    parts.push(preamble);

    // 2. Role label.
    if let Some(ref role_str) = agent_config.role {
        let role = role_str.trim().to_lowercase().replace('-', "_");
        if !role.is_empty() {
            let fragment = format!("Your role: {}.", role);
            total_tokens += estimate_tokens_approx_min1(&fragment);
            parts.push(fragment);
        }
    }

    // 3. Custom instructions.
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

    // 5. Skill context fragments.
    for ctx in skill_contexts {
        if ctx.trim().is_empty() {
            continue;
        }
        let truncated = truncate_to_token_budget(ctx, max_skill_context_tokens);
        let chunk_tokens = estimate_tokens_approx_min1(&truncated);
        if total_tokens + chunk_tokens > max_total_tokens {
            // Extract skill name from context (first line often has "# SkillName")
            let skill_hint = ctx.lines().next().unwrap_or("<unknown>").trim();
            tracing::warn!(
                skill = %skill_hint,
                total_tokens,
                chunk_tokens,
                max_total_tokens,
                "Skill context DROPPED — system prompt budget exhausted. \
                 Increase prompt_budget.max_total_tokens to include this skill."
            );
            continue; // warn for ALL dropped skills, don't break early
        }
        total_tokens += chunk_tokens;
        parts.push(truncated);
    }

    // 6. Workspace tools listing.
    if advertise_workspace_tools && agent_config.workspace.is_some() && !tools.is_empty() {
        let mut lines = vec!["# Tools".to_string()];
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
            lines.push(format!("- {}({})", tool.name, params));
        }
        let tools_note = lines.join("\n");
        let tools_tokens = estimate_tokens_approx_min1(&tools_note);
        if total_tokens + tools_tokens <= max_total_tokens + max_total_tokens / 10 {
            parts.push(tools_note);
        }
    }

    parts.join("\n\n---\n\n")
}
