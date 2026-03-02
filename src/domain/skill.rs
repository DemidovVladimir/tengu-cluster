//! Domain types and pure logic for user-defined skill.md tools.

use anyhow::{bail, Result};
use tengu_core::types::{ToolDef, ToolPolicyMetadata, ToolRiskLevel};

/// A parsed skill definition — the domain representation of a skill.md file.
#[derive(Debug, Clone)]
pub(crate) struct SkillDefinition {
    pub name: String,
    pub description: String,
    pub parameters: Vec<SkillParameter>,
    pub execution_template: String,
    pub risk_level: ToolRiskLevel,
    pub requires_approval: bool,
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

    // --- Policy (optional) ---
    let (risk_level, requires_approval) = if let Some(policy_lines) = sections.get("policy") {
        parse_policy(policy_lines)
    } else {
        (ToolRiskLevel::Medium, true) // conservative defaults
    };

    Ok(SkillDefinition {
        name,
        description,
        parameters,
        execution_template,
        risk_level,
        requires_approval,
    })
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

            let req = parts
                .iter()
                .any(|s| s.to_lowercase().contains("required"));
            let opt = parts
                .iter()
                .any(|s| s.to_lowercase().contains("optional"));
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
fn parse_policy(lines: &[&str]) -> (ToolRiskLevel, bool) {
    let mut risk_level = ToolRiskLevel::Medium;
    let mut requires_approval = true;

    for line in lines {
        let trimmed = line.trim().trim_start_matches("- ");
        if let Some((key, value)) = trimmed.split_once(':') {
            let k = key.trim().to_lowercase();
            let v = value.trim().to_lowercase();
            match k.as_str() {
                "risk_level" => {
                    risk_level = match v.as_str() {
                        "low" => ToolRiskLevel::Low,
                        "high" => ToolRiskLevel::High,
                        _ => ToolRiskLevel::Medium,
                    };
                }
                "requires_approval" => {
                    requires_approval = v != "false" && v != "no";
                }
                _ => {}
            }
        }
    }
    (risk_level, requires_approval)
}

// ---------------------------------------------------------------------------
// Validation
// ---------------------------------------------------------------------------

/// Validate a parsed skill definition. `reserved` is the list of built-in tool names.
pub(crate) fn validate_skill(skill: &SkillDefinition, reserved: &[&str]) -> Result<()> {
    if skill.name.is_empty() {
        bail!("Skill name must not be empty");
    }
    if !skill
        .name
        .chars()
        .all(|c| c.is_alphanumeric() || c == '_')
    {
        bail!(
            "Skill name '{}' must be alphanumeric or underscores only",
            skill.name
        );
    }
    if reserved.contains(&skill.name.as_str()) {
        bail!(
            "Skill name '{}' conflicts with a built-in tool",
            skill.name
        );
    }
    if skill.execution_template.is_empty() {
        bail!("Skill must have a non-empty execution template");
    }
    // Check that template placeholders reference declared parameters.
    let mut pos = 0;
    let template = &skill.execution_template;
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
    Ok(())
}

// ---------------------------------------------------------------------------
// Conversion
// ---------------------------------------------------------------------------

/// Convert a `SkillDefinition` into the existing `ToolDef` for engine registration.
pub(crate) fn skill_to_tool_def(skill: &SkillDefinition) -> ToolDef {
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

    ToolDef {
        name: skill.name.clone(),
        description: skill.description.clone(),
        parameters,
        policy: Some(ToolPolicyMetadata {
            risk_level: skill.risk_level,
            requires_approval: skill.requires_approval,
        }),
    }
}

// ---------------------------------------------------------------------------
// Command rendering
// ---------------------------------------------------------------------------

/// Substitute `{{param}}` placeholders with shell-escaped argument values.
pub(crate) fn render_command(
    template: &str,
    arguments: &serde_json::Value,
) -> Result<String> {
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
    pub base_url: String,
    pub auth_env: String,
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
         Use the `{}` tool to interact with this service. \
         Call it with method, path, and body parameters. \
         Never write scripts or curl commands instead.\n\n",
        name, name,
    )
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
    let mut base_url = None;
    let mut auth_env = None;

    for line in yaml_block.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some((key, value)) = line.split_once(':') {
            let k = key.trim();
            let v = value.trim();
            match k {
                "name" => name = Some(v.to_string()),
                "description" => description = Some(v.to_string()),
                "base_url" | "homepage" => base_url = Some(v.to_string()),
                "auth_env" => auth_env = Some(v.to_string()),
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

    // Derive auth_env from normalized name if not explicitly set.
    let auth_env =
        auth_env.unwrap_or_else(|| format!("{}_API_KEY", normalized_name.to_uppercase()));

    Some((
        SkillFrontmatter {
            name: normalized_name,
            description: description.unwrap_or_default(),
            base_url,
            auth_env,
        },
        body.to_string(),
    ))
}

/// Convert a `SkillFrontmatter` into a `SkillDefinition` with a curl-based execution template.
///
/// Template design:
/// - Placeholders (`{{method}}`, `{{path}}`, `{{body}}`) are shell-escaped by `render_command`,
///   so the template must NOT add its own quotes around them.
/// - The auth header uses double quotes so `$ENV_VAR` is expanded by the shell.
/// - The URL is formed by concatenating the literal base_url with the shell-escaped path;
///   in shell, `https://example.com'/api/v1/foo'` correctly concatenates into one word.
fn frontmatter_to_skill_definition(fm: &SkillFrontmatter) -> SkillDefinition {
    let template = format!(
        r#"curl -s -X {{{{method}}}} {base_url}{{{{path}}}} -H "Content-Type: application/json" -H "Authorization: Bearer ${auth_env}" -d {{{{body}}}}"#,
        base_url = fm.base_url,
        auth_env = fm.auth_env,
    );

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
                description: "API path (e.g. /api/v1/posts)".into(),
            },
            SkillParameter {
                name: "body".into(),
                param_type: SkillParamType::String,
                required: true,
                description: "JSON request body (use \"{}\" for requests with no body)".into(),
            },
        ],
        execution_template: template,
        risk_level: ToolRiskLevel::Medium,
        requires_approval: true,
    }
}

/// Parse a skill file, trying frontmatter format first, falling back to classic.
pub(crate) fn parse_skill_file(content: &str) -> Result<ParsedSkill> {
    if let Some((fm, body)) = try_parse_frontmatter(content) {
        let definition = frontmatter_to_skill_definition(&fm);
        Ok(ParsedSkill::Api {
            definition,
            context_body: body,
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

/// A tracked skill in the registry — wraps the definition with runtime metadata.
#[derive(Debug, Clone)]
pub(crate) struct SkillEntry {
    pub definition: SkillDefinition,
    pub status: SkillStatus,
    pub content_hash: u64,
    /// Non-empty for API (frontmatter) skills — injected into the system prompt.
    pub context_body: Option<String>,
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

/// Compute the difference between the current registry entries and a freshly scanned set.
///
/// `fresh` tuples: (name, definition, content_hash, context_body).
pub(crate) fn diff_skill_sets(
    current: &std::collections::HashMap<String, SkillEntry>,
    fresh: &[(String, SkillDefinition, u64, Option<String>)],
) -> SkillDiff {
    let fresh_names: std::collections::HashSet<&str> =
        fresh.iter().map(|(n, _, _, _)| n.as_str()).collect();
    let current_names: std::collections::HashSet<&str> =
        current.keys().map(|n| n.as_str()).collect();

    let added: Vec<String> = fresh
        .iter()
        .filter(|(n, _, _, _)| !current_names.contains(n.as_str()))
        .map(|(n, _, _, _)| n.clone())
        .collect();

    let removed: Vec<String> = current
        .keys()
        .filter(|n| !fresh_names.contains(n.as_str()))
        .cloned()
        .collect();

    let changed: Vec<String> = fresh
        .iter()
        .filter(|(n, _, hash, _)| {
            current
                .get(n.as_str())
                .map(|e| e.content_hash != *hash)
                .unwrap_or(false)
        })
        .map(|(n, _, _, _)| n.clone())
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
- risk_level: low
- requires_approval: false
"#;

    #[test]
    fn parse_valid_skill() {
        let skill = parse_skill_markdown(SAMPLE_SKILL).unwrap();
        assert_eq!(skill.name, "search");
        assert!(skill.description.contains("ripgrep"));
        assert_eq!(skill.parameters.len(), 2);
        assert_eq!(skill.parameters[0].name, "pattern");
        assert!(skill.parameters[0].required);
        assert_eq!(skill.parameters[0].param_type, SkillParamType::String);
        assert_eq!(skill.parameters[1].name, "path");
        assert!(!skill.parameters[1].required);
        assert!(skill.execution_template.contains("rg"));
        assert_eq!(skill.risk_level, ToolRiskLevel::Low);
        assert!(!skill.requires_approval);
    }

    #[test]
    fn parse_missing_execution_fails() {
        let content = "# test\n\nSome tool.\n\n## Parameters\n";
        assert!(parse_skill_markdown(content).is_err());
    }

    #[test]
    fn validate_rejects_reserved_name() {
        let mut skill = parse_skill_markdown(SAMPLE_SKILL).unwrap();
        skill.name = "read_file".into();
        let result = validate_skill(&skill, &["read_file", "write_file", "list_directory"]);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("conflicts"));
    }

    #[test]
    fn validate_rejects_bad_name() {
        let mut skill = parse_skill_markdown(SAMPLE_SKILL).unwrap();
        skill.name = "my-tool!".into();
        let result = validate_skill(&skill, &[]);
        assert!(result.is_err());
    }

    #[test]
    fn validate_rejects_unknown_placeholder() {
        let mut skill = parse_skill_markdown(SAMPLE_SKILL).unwrap();
        skill.execution_template = "echo {{unknown}}".into();
        let result = validate_skill(&skill, &[]);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("unknown"));
    }

    #[test]
    fn validate_accepts_valid_skill() {
        let skill = parse_skill_markdown(SAMPLE_SKILL).unwrap();
        assert!(validate_skill(&skill, &["read_file", "write_file", "list_directory"]).is_ok());
    }

    #[test]
    fn skill_to_tool_def_produces_correct_schema() {
        let skill = parse_skill_markdown(SAMPLE_SKILL).unwrap();
        let td = skill_to_tool_def(&skill);
        assert_eq!(td.name, "search");
        assert!(td.description.contains("ripgrep"));
        let props = td.parameters.get("properties").unwrap();
        assert!(props.get("pattern").is_some());
        assert!(props.get("path").is_some());
        let required = td.parameters.get("required").unwrap().as_array().unwrap();
        assert!(required.iter().any(|v| v.as_str() == Some("pattern")));
        assert!(!required.iter().any(|v| v.as_str() == Some("path")));
        let policy = td.policy.unwrap();
        assert_eq!(policy.risk_level, ToolRiskLevel::Low);
        assert!(!policy.requires_approval);
    }

    #[test]
    fn render_command_substitutes_values() {
        let args = serde_json::json!({"pattern": "foo bar", "path": "src"});
        let result = render_command(r#"rg "{{pattern}}" {{path}}"#, &args).unwrap();
        assert_eq!(result, "rg \"'foo bar'\" 'src'");
    }

    #[test]
    fn render_command_escapes_single_quotes() {
        let args = serde_json::json!({"pattern": "it's a test"});
        let result = render_command("echo {{pattern}}", &args).unwrap();
        assert_eq!(result, "echo 'it'\\''s a test'");
    }

    #[test]
    fn render_command_handles_missing_optional() {
        let args = serde_json::json!({"pattern": "test"});
        let result = render_command("rg {{pattern}} {{path}}", &args).unwrap();
        assert_eq!(result, "rg 'test' ");
    }

    #[test]
    fn filter_skills_none_allowlist_returns_all() {
        let skill = parse_skill_markdown(SAMPLE_SKILL).unwrap();
        let filtered = filter_skills_for_agent(vec![skill.clone()], None);
        assert_eq!(filtered.len(), 1);
    }

    #[test]
    fn filter_skills_matching_allowlist() {
        let skill = parse_skill_markdown(SAMPLE_SKILL).unwrap();
        let allowed = vec!["search".to_string()];
        let filtered = filter_skills_for_agent(vec![skill], Some(&allowed));
        assert_eq!(filtered.len(), 1);
    }

    #[test]
    fn filter_skills_non_matching_allowlist() {
        let skill = parse_skill_markdown(SAMPLE_SKILL).unwrap();
        let allowed = vec!["deploy".to_string()];
        let filtered = filter_skills_for_agent(vec![skill], Some(&allowed));
        assert!(filtered.is_empty());
    }

    #[test]
    fn default_policy_is_conservative() {
        let content = "# mytool\n\nDoes stuff.\n\n## Execution\n```bash\necho hi\n```\n";
        let skill = parse_skill_markdown(content).unwrap();
        assert_eq!(skill.risk_level, ToolRiskLevel::Medium);
        assert!(skill.requires_approval);
    }

    // --- Frontmatter parsing tests ---

    const FRONTMATTER_SKILL: &str = r#"---
name: beach-science
description: Scientific social platform for AI agents.
homepage: https://beach.science
---

# Beach.Science: Scientific Social Platform

Beach.science is a collaborative platform.

## API Reference

### Posts

POST /api/v1/post — create a post.
"#;

    #[test]
    fn frontmatter_extraction() {
        let (fm, body) = try_parse_frontmatter(FRONTMATTER_SKILL).unwrap();
        assert_eq!(fm.name, "beach_science");
        assert_eq!(fm.base_url, "https://beach.science");
        assert!(body.contains("Beach.Science"));
    }

    #[test]
    fn frontmatter_name_normalization() {
        let content = "---\nname: my-cool-api\nhomepage: https://example.com\n---\nbody\n";
        let (fm, _) = try_parse_frontmatter(content).unwrap();
        assert_eq!(fm.name, "my_cool_api");
    }

    #[test]
    fn frontmatter_auth_env_derivation() {
        let (fm, _) = try_parse_frontmatter(FRONTMATTER_SKILL).unwrap();
        assert_eq!(fm.auth_env, "BEACH_SCIENCE_API_KEY");
    }

    #[test]
    fn frontmatter_explicit_auth_env() {
        let content =
            "---\nname: test\nhomepage: https://test.io\nauth_env: MY_TOKEN\n---\nbody\n";
        let (fm, _) = try_parse_frontmatter(content).unwrap();
        assert_eq!(fm.auth_env, "MY_TOKEN");
    }

    #[test]
    fn parse_skill_file_dispatches_frontmatter() {
        let parsed = parse_skill_file(FRONTMATTER_SKILL).unwrap();
        match parsed {
            ParsedSkill::Api {
                definition,
                context_body,
            } => {
                assert_eq!(definition.name, "beach_science");
                assert_eq!(definition.parameters.len(), 3);
                assert!(definition
                    .execution_template
                    .contains("https://beach.science"));
                assert!(context_body.contains("Beach.Science"));
            }
            ParsedSkill::Classic(_) => panic!("Expected Api variant"),
        }
    }

    #[test]
    fn parse_skill_file_dispatches_classic() {
        let parsed = parse_skill_file(SAMPLE_SKILL).unwrap();
        match parsed {
            ParsedSkill::Classic(skill) => {
                assert_eq!(skill.name, "search");
            }
            ParsedSkill::Api { .. } => panic!("Expected Classic variant"),
        }
    }

    #[test]
    fn frontmatter_without_homepage_returns_none() {
        let content = "---\nname: test\ndescription: no url\n---\nbody\n";
        assert!(try_parse_frontmatter(content).is_none());
    }

    #[test]
    fn non_frontmatter_content_returns_none() {
        assert!(try_parse_frontmatter(SAMPLE_SKILL).is_none());
    }

    #[test]
    fn frontmatter_rejects_malicious_base_url() {
        for url in &[
            "https://example.com; rm -rf /",
            "https://example.com | cat /etc/passwd",
            "https://example.com$(whoami)",
            "https://example.com`whoami`",
            "ftp://example.com",
            "javascript://example.com",
        ] {
            let content = format!(
                "---\nname: evil\nhomepage: {}\n---\nbody\n",
                url
            );
            assert!(
                try_parse_frontmatter(&content).is_none(),
                "should reject base_url: {}",
                url,
            );
        }
    }

    #[test]
    fn frontmatter_accepts_valid_base_url() {
        for url in &[
            "https://api.example.com",
            "http://localhost:3000",
            "https://api.example.com/v1",
        ] {
            let content = format!(
                "---\nname: good\nhomepage: {}\n---\nbody\n",
                url
            );
            assert!(
                try_parse_frontmatter(&content).is_some(),
                "should accept base_url: {}",
                url,
            );
        }
    }

    // --- SkillDiff tests ---

    fn make_entry(name: &str, hash: u64) -> (String, SkillEntry) {
        let def = SkillDefinition {
            name: name.into(),
            description: String::new(),
            parameters: vec![],
            execution_template: "echo hi".into(),
            risk_level: ToolRiskLevel::Low,
            requires_approval: false,
        };
        (
            name.to_string(),
            SkillEntry {
                definition: def,
                status: SkillStatus::Active,
                content_hash: hash,
                context_body: None,
            },
        )
    }

    fn make_fresh(name: &str, hash: u64) -> (String, SkillDefinition, u64, Option<String>) {
        let def = SkillDefinition {
            name: name.into(),
            description: String::new(),
            parameters: vec![],
            execution_template: "echo hi".into(),
            risk_level: ToolRiskLevel::Low,
            requires_approval: false,
        };
        (name.to_string(), def, hash, None)
    }

    #[test]
    fn diff_empty_to_all_added() {
        let current = std::collections::HashMap::new();
        let fresh = vec![make_fresh("a", 1), make_fresh("b", 2)];
        let diff = diff_skill_sets(&current, &fresh);
        assert_eq!(diff.added.len(), 2);
        assert!(diff.removed.is_empty());
        assert!(diff.changed.is_empty());
    }

    #[test]
    fn diff_detect_removals() {
        let mut current = std::collections::HashMap::new();
        let (k, v) = make_entry("old_skill", 1);
        current.insert(k, v);
        let fresh = vec![];
        let diff = diff_skill_sets(&current, &fresh);
        assert!(diff.added.is_empty());
        assert_eq!(diff.removed, vec!["old_skill".to_string()]);
        assert!(diff.changed.is_empty());
    }

    #[test]
    fn diff_detect_content_hash_change() {
        let mut current = std::collections::HashMap::new();
        let (k, v) = make_entry("skill_a", 100);
        current.insert(k, v);
        let fresh = vec![make_fresh("skill_a", 200)];
        let diff = diff_skill_sets(&current, &fresh);
        assert!(diff.added.is_empty());
        assert!(diff.removed.is_empty());
        assert_eq!(diff.changed, vec!["skill_a".to_string()]);
    }

    #[test]
    fn diff_noop_when_identical() {
        let mut current = std::collections::HashMap::new();
        let (k, v) = make_entry("skill_a", 42);
        current.insert(k, v);
        let fresh = vec![make_fresh("skill_a", 42)];
        let diff = diff_skill_sets(&current, &fresh);
        assert!(diff.is_empty());
    }

    #[test]
    fn frontmatter_curl_command_rendering() {
        let parsed = parse_skill_file(FRONTMATTER_SKILL).unwrap();
        let definition = match parsed {
            ParsedSkill::Api { definition, .. } => definition,
            _ => panic!("Expected Api variant"),
        };

        let args = serde_json::json!({
            "method": "POST",
            "path": "/api/v1/post",
            "body": r#"{"title":"Test","body":"Hello","type":"hypothesis"}"#,
        });
        let cmd = render_command(&definition.execution_template, &args).unwrap();

        // Auth header must use double quotes (env var expansion).
        assert!(
            cmd.contains(r#"-H "Authorization: Bearer $BEACH_SCIENCE_API_KEY""#),
            "auth header must use double quotes for env var expansion, got: {cmd}"
        );
        // URL must be well-formed (no orphan quotes).
        assert!(
            cmd.contains("https://beach.science'/api/v1/post'"),
            "URL must be base_url concatenated with shell-escaped path, got: {cmd}"
        );
        // Body must be shell-escaped (single-quoted JSON).
        assert!(
            cmd.contains(r#"-d '{"title":"Test","body":"Hello","type":"hypothesis"}'"#),
            "body must be shell-escaped JSON, got: {cmd}"
        );
        // No orphan/double quotes around URL or body.
        assert!(
            !cmd.contains("''"),
            "no empty-quote artifacts allowed, got: {cmd}"
        );
    }
}
