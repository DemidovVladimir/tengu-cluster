//! Domain types and pure logic for user-defined skill.md tools.

use crate::domain::capability::{CapabilityId, EffectClass, RegisteredTool};
use anyhow::{bail, Result};

/// A parsed skill definition — the domain representation of a skill.md file.
#[derive(Debug, Clone)]
pub(crate) struct SkillDefinition {
    pub name: String,
    pub description: String,
    pub activity_description: Option<String>,
    pub parameters: Vec<SkillParameter>,
    pub execution: SkillExecution,
    pub capability: CapabilityId,
    pub effect_class: EffectClass,
}

#[derive(Debug, Clone)]
pub(crate) enum SkillExecution {
    Shell { template: String },
    Api(ApiExecution),
}

#[derive(Debug, Clone)]
pub(crate) struct ApiExecution {
    pub base_url: String,
    pub auth: ApiAuth,
    pub headers: Vec<(String, String)>,
}

#[derive(Debug, Clone)]
pub(crate) enum ApiAuth {
    None,
    BearerEnv {
        env: String,
    },
    BasicEnv {
        username_env: String,
        password_env: String,
    },
}

/// A single parameter declared in the skill markdown.
#[derive(Debug, Clone)]
pub(crate) struct SkillParameter {
    pub name: String,
    pub param_type: SkillParamType,
    pub required: bool,
    pub description: String,
}

/// Supported JSON Schema types for skill parameters.
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

// ---------------------------------------------------------------------------
// Parsing
// ---------------------------------------------------------------------------

/// Parse a skill.md markdown string into a `SkillDefinition`.
///
/// Expected format:
/// ```text
/// # tool_name
///
/// Description text here.
///
/// ## Parameters
/// - `name` (type, required): description
/// - `name` (type, optional): description
///
/// ## Execution
/// ```bash
/// command {{param}} ...
/// ```
///
/// ## Policy
/// - risk_level: low
/// - requires_approval: false
/// ```
pub(crate) fn parse_skill_markdown(content: &str) -> Result<SkillDefinition> {
    let lines: Vec<&str> = content.lines().collect();

    // --- H1: tool name ---
    let name_line = lines
        .iter()
        .find(|l| l.starts_with("# ") && !l.starts_with("## "))
        .ok_or_else(|| anyhow::anyhow!("Missing '# <tool_name>' heading"))?;
    let name = name_line.trim_start_matches("# ").trim().to_string();

    // --- Description: text between H1 and first H2 ---
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

    // --- Sections by H2 ---
    let sections = extract_h2_sections(&lines);

    // --- Parameters ---
    let parameters = if let Some(param_lines) = sections.get("parameters") {
        parse_parameters(param_lines)?
    } else {
        Vec::new()
    };

    // --- Execution template ---
    let execution_template = sections
        .get("execution")
        .ok_or_else(|| anyhow::anyhow!("Missing '## Execution' section"))
        .and_then(|lines| extract_fenced_code(lines))?;

    let default_capability = CapabilityId::new(format!("skill.{}", normalize_skill_name(&name)))?;
    let default_effect_class = EffectClass::ShellExec;
    let (capability, effect_class, activity_description) =
        if let Some(policy_lines) = sections.get("policy") {
            let (parsed_capability, parsed_effect_class, parsed_activity_description) =
            parse_policy(policy_lines, default_effect_class);
            let capability = if parsed_capability.as_str() == "skill.unknown" {
                default_capability
            } else {
                parsed_capability
            };
            (capability, parsed_effect_class, parsed_activity_description)
        } else {
            (default_capability, default_effect_class, None)
        };

    Ok(SkillDefinition {
        name,
        description,
        activity_description,
        parameters,
        execution: SkillExecution::Shell {
            template: execution_template,
        },
        capability,
        effect_class,
    })
}

fn normalize_skill_name(name: &str) -> String {
    name.trim().to_lowercase().replace('-', "_")
}

/// Collect H2 sections as lowercase-key → Vec of body lines.
fn extract_h2_sections<'a>(lines: &[&'a str]) -> std::collections::HashMap<String, Vec<&'a str>> {
    let mut map = std::collections::HashMap::new();
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

/// Parse `- \`name\` (type, required/optional): description` lines.
fn parse_parameters(lines: &[&str]) -> Result<Vec<SkillParameter>> {
    let mut params = Vec::new();
    for line in lines {
        let trimmed = line.trim();
        if !trimmed.starts_with("- `") {
            continue;
        }
        let after_dash = trimmed.trim_start_matches("- ");
        // Extract name between backticks
        let name_end = after_dash
            .find("` ")
            .or_else(|| after_dash.find(")`"))
            .ok_or_else(|| anyhow::anyhow!("Malformed parameter line: {}", trimmed))?;
        let name = after_dash[1..name_end].to_string();

        // Extract parenthesized type info
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

/// Extract content from the first fenced code block.
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

/// Parse policy section key-value pairs.
fn parse_policy(
    lines: &[&str],
    default_effect_class: EffectClass,
) -> (CapabilityId, EffectClass, Option<String>) {
    let mut capability = CapabilityId::new("skill.unknown").expect("static capability is valid");
    let mut effect_class = default_effect_class;
    let mut activity_description = None;

    for line in lines {
        let trimmed = line.trim().trim_start_matches("- ");
        if let Some((key, value)) = trimmed.split_once(':') {
            let k = key.trim().to_lowercase();
            let v = value.trim().to_lowercase();
            match k.as_str() {
                "capability" => {
                    if let Ok(parsed) = CapabilityId::new(value.trim().to_string()) {
                        capability = parsed;
                    }
                }
                "effect_class" | "effect-class" => {
                    if let Ok(parsed) = v.parse::<EffectClass>() {
                        effect_class = parsed;
                    }
                }
                "activity_description" | "activity-description" => {
                    let raw = value.trim();
                    if !raw.is_empty() {
                        activity_description = Some(raw.to_string());
                    }
                }
                _ => {}
            }
        }
    }
    (capability, effect_class, activity_description)
}

// ---------------------------------------------------------------------------
// Validation
// ---------------------------------------------------------------------------

/// Validate a parsed skill definition. `reserved` is the list of built-in tool names.
pub(crate) fn validate_skill(skill: &SkillDefinition, reserved: &[&str]) -> Result<()> {
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

// ---------------------------------------------------------------------------
// Conversion
// ---------------------------------------------------------------------------

/// Convert a `SkillDefinition` into a capability-bound runtime tool registration.
pub(crate) fn skill_to_registered_tool(skill: &SkillDefinition) -> RegisteredTool {
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

    let tool = RegisteredTool::new(
        &skill.name,
        &skill.description,
        parameters,
        skill.capability.clone(),
        skill.effect_class,
    );

    if let Some(activity_description) = &skill.activity_description {
        tool.with_activity_description(activity_description.clone())
    } else {
        tool
    }
}

// ---------------------------------------------------------------------------
// Command rendering
// ---------------------------------------------------------------------------

/// Substitute `{{param}}` placeholders with shell-escaped argument values.
pub(crate) fn render_command(template: &str, arguments: &serde_json::Value) -> Result<String> {
    let mut result = template.to_string();
    let mut pos = 0;

    while let Some(start) = result[pos..].find("{{") {
        let abs_start = pos + start;
        if let Some(end) = result[abs_start + 2..].find("}}") {
            let abs_end = abs_start + 2 + end;
            let placeholder = result[abs_start + 2..abs_end].trim();

            let value = arguments.get(placeholder);
            let rendered = match value {
                Some(serde_json::Value::String(s)) => shell_escape(s),
                Some(serde_json::Value::Number(n)) => n.to_string(),
                Some(serde_json::Value::Bool(b)) => b.to_string(),
                Some(serde_json::Value::Null) | None => {
                    // For missing optional params, remove the placeholder entirely.
                    String::new()
                }
                Some(other) => shell_escape(&other.to_string()),
            };

            result.replace_range(abs_start..abs_end + 2, &rendered);
            pos = abs_start + rendered.len();
        } else {
            break;
        }
    }

    Ok(result)
}

/// Shell-escape a string value to prevent injection.
fn shell_escape(s: &str) -> String {
    // Wrap in single quotes and escape any embedded single quotes.
    let escaped = s.replace('\'', "'\\''");
    format!("'{}'", escaped)
}

// ---------------------------------------------------------------------------
// Frontmatter API skill parsing
// ---------------------------------------------------------------------------

/// Parsed YAML frontmatter from an API skill file.
#[derive(Debug, Clone)]
pub(crate) struct SkillFrontmatter {
    pub name: String,
    pub description: String,
    pub activity_description: Option<String>,
    pub base_url: String,
    pub auth: ApiAuth,
    pub headers: Vec<(String, String)>,
    pub capability: CapabilityId,
    pub effect_class: EffectClass,
    /// Environment variables declared by this skill.
    pub env_vars: Vec<SkillEnvVar>,
    /// Slash commands declared by this skill.
    pub commands: Vec<SkillCommand>,
}

/// Result of parsing a skill file — either classic `# name` format or frontmatter API skill.
#[derive(Debug, Clone)]
pub(crate) enum ParsedSkill {
    /// Classic `# name` + `## Execution` format.
    Classic(SkillDefinition),
    /// Frontmatter API skill — produces a curl-based tool plus context injection.
    Api {
        definition: SkillDefinition,
        context_body: String,
        env_vars: Vec<SkillEnvVar>,
        commands: Vec<SkillCommand>,
    },
}

/// Validate that a base_url is a proper HTTP(S) URL without shell metacharacters.
fn validate_base_url(url: &str) -> bool {
    (url.starts_with("https://") || url.starts_with("http://"))
        && !url
            .chars()
            .any(|c| matches!(c, ';' | '|' | '&' | '`' | '$' | '(' | ')' | '\n' | '\r'))
}

/// Shared preamble for API skill context injection.
pub(crate) fn api_skill_preamble(name: &str) -> String {
    format!(
        "# {} — API skill\n\n\
         Use the `{}` tool for requests to this service's primary API \
         (method, path, body, optional headers parameters). \
         The runtime enforces the base URL, auth strategy, and approval policy for this tool.\n\n",
        name, name,
    )
}

/// Validate that a header value contains no dangerous shell metacharacters.
/// Allows `$` for env var expansion but rejects `;`, `|`, `&`, `` ` ``, `(`, `)`, newlines.
fn validate_header_value(value: &str) -> bool {
    !value
        .chars()
        .any(|c| matches!(c, ';' | '|' | '&' | '`' | '(' | ')' | '\n' | '\r'))
}

/// Try to extract YAML frontmatter delimited by `---` fences.
///
/// Returns `Some((frontmatter, body))` if the content starts with `---`.
fn try_parse_frontmatter(content: &str) -> Option<(SkillFrontmatter, String)> {
    let trimmed = content.trim_start();
    if !trimmed.starts_with("---") {
        return None;
    }

    // Find the closing `---` fence (skip the first line).
    let after_first = &trimmed[3..].trim_start_matches(|c: char| c == '-');
    let after_first = after_first.strip_prefix('\n').unwrap_or(after_first);

    let closing = after_first.find("\n---")?;
    let yaml_block = &after_first[..closing];
    let body = after_first[closing + 4..].trim_start_matches('-').trim();

    // Parse key-value pairs from the YAML block.
    let mut name = None;
    let mut description = None;
    let mut activity_description = None;
    let mut base_url = None;
    let mut auth_env = None;
    let mut auth_mode = None;
    let mut auth_basic_user_env = None;
    let mut auth_basic_pass_env = None;
    let mut capability = None;
    let mut effect_class = None;
    let mut headers: Vec<(String, String)> = Vec::new();
    let mut env_vars: Vec<SkillEnvVar> = Vec::new();
    let mut commands: Vec<SkillCommand> = Vec::new();

    /// Which indented-list block we are currently inside.
    #[derive(PartialEq)]
    enum ListBlock {
        Headers,
        EnvVars,
        Commands,
    }
    let mut current_block: Option<ListBlock> = None;

    for raw_line in yaml_block.lines() {
        let line = raw_line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }

        // Check if this is an indented sub-item (part of a list block).
        let is_indented = raw_line.starts_with("  ") || raw_line.starts_with('\t');

        if is_indented {
            if let Some(ref block) = current_block {
                match block {
                    ListBlock::Headers => {
                        if let Some((hk, hv)) = line.split_once(':') {
                            let hk = hk.trim().to_string();
                            let hv = hv.trim().to_string();
                            if !hk.is_empty() && validate_header_value(&hv) {
                                headers.push((hk, hv));
                            } else {
                                return None;
                            }
                        }
                    }
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

        // Non-indented line exits any list-parsing mode.
        current_block = None;

        if let Some((key, value)) = line.split_once(':') {
            let k = key.trim();
            let v = value.trim();
            match k {
                "name" => name = Some(v.to_string()),
                "description" => description = Some(v.to_string()),
                "activity_description" | "activity-description" => {
                    activity_description = Some(v.to_string())
                }
                "base_url" | "homepage" => base_url = Some(v.to_string()),
                "auth_env" => auth_env = Some(v.to_string()),
                "auth_mode" => auth_mode = Some(v.to_string()),
                "auth_basic_user_env" => auth_basic_user_env = Some(v.to_string()),
                "auth_basic_pass_env" => auth_basic_pass_env = Some(v.to_string()),
                "capability" => capability = Some(v.to_string()),
                "effect_class" | "effect-class" => effect_class = Some(v.to_string()),
                "headers" => {
                    if v.is_empty() {
                        current_block = Some(ListBlock::Headers);
                    }
                }
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
    // Normalize name: hyphens → underscores, lowercase.
    let normalized_name = raw_name.replace('-', "_").to_lowercase();

    // Derive base_url from homepage if not set separately.
    let base_url = base_url?;

    // Reject base_url with shell metacharacters to prevent command injection.
    if !validate_base_url(&base_url) {
        return None;
    }

    let capability = capability
        .map(CapabilityId::new)
        .transpose()
        .ok()
        .flatten()
        .unwrap_or_else(|| {
            CapabilityId::new(format!("skill.{}", normalized_name))
                .expect("generated capability is valid")
        });
    let effect_class = effect_class
        .and_then(|value| value.parse::<EffectClass>().ok())
        .unwrap_or(EffectClass::ExternalApi);
    let auth = match auth_mode
        .unwrap_or_else(|| {
            if auth_basic_user_env.is_some() || auth_basic_pass_env.is_some() {
                "basic".to_string()
            } else if auth_env.is_some() {
                "bearer".to_string()
            } else {
                "none".to_string()
            }
        })
        .to_lowercase()
        .as_str()
    {
        "basic" => ApiAuth::BasicEnv {
            username_env: auth_basic_user_env?,
            password_env: auth_basic_pass_env?,
        },
        "bearer" => ApiAuth::BearerEnv {
            env: auth_env.unwrap_or_else(|| format!("{}_API_KEY", normalized_name.to_uppercase())),
        },
        _ => ApiAuth::None,
    };

    Some((
        SkillFrontmatter {
            name: normalized_name,
            description: description.unwrap_or_default(),
            activity_description,
            base_url,
            auth,
            headers,
            capability,
            effect_class,
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
        activity_description: fm.activity_description.clone(),
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
            auth: fm.auth.clone(),
            headers: fm.headers.clone(),
        }),
        capability: fm.capability.clone(),
        effect_class: fm.effect_class,
    }
}

/// Parse a skill file, trying frontmatter format first, falling back to classic.
pub(crate) fn parse_skill_file(content: &str) -> Result<ParsedSkill> {
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

// ---------------------------------------------------------------------------
// Skill registry types
// ---------------------------------------------------------------------------

/// Whether a skill is active (offered to the model) or disabled by the user.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SkillStatus {
    Active,
    Inactive,
}

/// An environment variable declared by a skill.
#[derive(Debug, Clone)]
pub(crate) struct SkillEnvVar {
    pub name: String,
    /// `true` unless the name ends with `?` in the frontmatter.
    pub required: bool,
}

/// A slash command declared by a skill (e.g. `wallet` → `/wallet`).
#[derive(Debug, Clone)]
pub(crate) struct SkillCommand {
    pub name: String,
}

/// A tracked skill in the registry — wraps the definition with runtime metadata.
#[derive(Debug, Clone)]
pub(crate) struct SkillEntry {
    pub definition: SkillDefinition,
    pub status: SkillStatus,
    pub content_hash: u64,
    /// Non-empty for API (frontmatter) skills — injected into the system prompt.
    pub context_body: Option<String>,
    /// Environment variables declared by this skill.
    pub env_vars: Vec<SkillEnvVar>,
    /// Slash commands declared by this skill.
    pub commands: Vec<SkillCommand>,
}

impl SkillEntry {
    /// Returns names of required env vars that are not set in the process environment.
    pub fn missing_required_env_vars(&self) -> Vec<String> {
        self.env_vars
            .iter()
            .filter(|ev| ev.required && std::env::var(&ev.name).unwrap_or_default().is_empty())
            .map(|ev| ev.name.clone())
            .collect()
    }
}

/// Describes what changed between two skill scans.
#[derive(Debug, Default)]
pub(crate) struct SkillDiff {
    pub added: Vec<String>,
    pub removed: Vec<String>,
    pub changed: Vec<String>,
}

impl SkillDiff {
    pub fn is_empty(&self) -> bool {
        self.added.is_empty() && self.removed.is_empty() && self.changed.is_empty()
    }
}

/// A freshly scanned skill entry before being merged into the registry.
pub(crate) struct FreshSkillEntry {
    pub name: String,
    pub definition: SkillDefinition,
    pub content_hash: u64,
    pub context_body: Option<String>,
    pub env_vars: Vec<SkillEnvVar>,
    pub commands: Vec<SkillCommand>,
}

/// Compute the difference between the current registry entries and a freshly scanned set.
pub(crate) fn diff_skill_sets(
    current: &std::collections::HashMap<String, SkillEntry>,
    fresh: &[FreshSkillEntry],
) -> SkillDiff {
    let fresh_names: std::collections::HashSet<&str> =
        fresh.iter().map(|f| f.name.as_str()).collect();
    let current_names: std::collections::HashSet<&str> =
        current.keys().map(|n| n.as_str()).collect();

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

// ---------------------------------------------------------------------------
// Agent skill filtering
// ---------------------------------------------------------------------------

/// Filter skills to only those allowed for a specific agent.
///
/// - `None` allowlist means all skills are allowed.
/// - `Some(names)` restricts to only skills whose name appears in the list.
#[allow(dead_code)]
pub(crate) fn filter_skills_for_agent(
    skills: Vec<SkillDefinition>,
    allowed_names: Option<&[String]>,
) -> Vec<SkillDefinition> {
    match allowed_names {
        None => skills,
        Some(names) => skills
            .into_iter()
            .filter(|s| names.iter().any(|n| n == &s.name))
            .collect(),
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    const SAMPLE_SKILL: &str = r#"# search

Search files in the workspace using ripgrep.

## Parameters
- `pattern` (string, required): The search pattern
- `path` (string, optional): Directory to search in

## Execution
```bash
rg "{{pattern}}" {{path}}
```

## Policy
- capability: skill.search
- effect_class: shell_exec
"#;

    const API_SKILL: &str = r#"---
name: privy
description: Privy wallet operations
base_url: https://api.privy.io
auth_mode: basic
auth_basic_user_env: PRIVY_APP_ID
auth_basic_pass_env: PRIVY_APP_SECRET
capability: skill.privy
effect_class: chain_tx
headers:
  privy-app-id: $PRIVY_APP_ID
commands:
  - wallet
---

# Privy

Use this package for agentic wallet workflows.
"#;

    fn make_skill(name: &str) -> SkillDefinition {
        SkillDefinition {
            name: name.to_string(),
            description: String::new(),
            activity_description: None,
            parameters: vec![],
            execution: SkillExecution::Shell {
                template: "echo hi".to_string(),
            },
            capability: CapabilityId::new(format!("skill.{name}")).unwrap(),
            effect_class: EffectClass::ShellExec,
        }
    }

    #[test]
    fn parse_classic_skill_uses_capability_and_effect() {
        let skill = parse_skill_markdown(SAMPLE_SKILL).unwrap();
        assert_eq!(skill.name, "search");
        assert_eq!(skill.capability.as_str(), "skill.search");
        assert_eq!(skill.effect_class, EffectClass::ShellExec);
        match skill.execution {
            SkillExecution::Shell { template } => assert!(template.contains("rg")),
            SkillExecution::Api(_) => panic!("expected shell skill"),
        }
    }

    #[test]
    fn validate_rejects_reserved_name() {
        let mut skill = parse_skill_markdown(SAMPLE_SKILL).unwrap();
        skill.name = "read_file".into();
        assert!(validate_skill(&skill, &["read_file"]).is_err());
    }

    #[test]
    fn skill_to_registered_tool_uses_effect_policy() {
        let skill = parse_skill_markdown(SAMPLE_SKILL).unwrap();
        let tool = skill_to_registered_tool(&skill);
        assert_eq!(tool.def.name, "search");
        assert_eq!(tool.capability.as_str(), "skill.search");
        assert!(tool.def.policy.unwrap().requires_approval);
    }

    #[test]
    fn render_command_shell_escapes_string_values() {
        let tmpl = "echo {{msg}}";
        let args = serde_json::json!({ "msg": "hello'; rm -rf / #" });
        let rendered = render_command(tmpl, &args).unwrap();
        assert_eq!(rendered, "echo 'hello'\\''; rm -rf / #'");
    }

    #[test]
    fn filter_skills_non_matching_allowlist() {
        let filtered =
            filter_skills_for_agent(vec![make_skill("search")], Some(&["other".to_string()]));
        assert!(filtered.is_empty());
    }

    #[test]
    fn parse_frontmatter_api_skill_uses_native_api_metadata() {
        let parsed = parse_skill_file(API_SKILL).unwrap();
        match parsed {
            ParsedSkill::Api {
                definition,
                context_body,
                commands,
                ..
            } => {
                assert!(context_body.contains("agentic wallet workflows"));
                assert_eq!(commands.len(), 1);
                assert_eq!(definition.capability.as_str(), "skill.privy");
                assert_eq!(definition.effect_class, EffectClass::ChainTx);
                match definition.execution {
                    SkillExecution::Api(api) => {
                        assert_eq!(api.base_url, "https://api.privy.io");
                        assert_eq!(api.headers[0].0, "privy-app-id");
                        match api.auth {
                            ApiAuth::BasicEnv {
                                username_env,
                                password_env,
                            } => {
                                assert_eq!(username_env, "PRIVY_APP_ID");
                                assert_eq!(password_env, "PRIVY_APP_SECRET");
                            }
                            _ => panic!("expected basic auth"),
                        }
                    }
                    SkillExecution::Shell { .. } => panic!("expected api skill"),
                }
            }
            ParsedSkill::Classic(_) => panic!("expected api skill"),
        }
    }

    #[test]
    fn diff_skill_sets_detects_change() {
        let mut current = std::collections::HashMap::new();
        current.insert(
            "search".to_string(),
            SkillEntry {
                definition: make_skill("search"),
                status: SkillStatus::Active,
                content_hash: 1,
                context_body: None,
                env_vars: vec![],
                commands: vec![],
            },
        );
        let fresh = vec![FreshSkillEntry {
            name: "search".to_string(),
            definition: make_skill("search"),
            content_hash: 2,
            context_body: None,
            env_vars: vec![],
            commands: vec![],
        }];
        let diff = diff_skill_sets(&current, &fresh);
        assert_eq!(diff.changed, vec!["search".to_string()]);
    }
}
