//! Application service for loading and validating user-defined skills.

use crate::application::ports::SkillSourcePort;
use crate::domain::capability::RegisteredTool;
use crate::domain::skill::{
    api_skill_preamble, filter_skills_for_agent, parse_skill_file, skill_to_registered_tool,
    validate_skill, ParsedSkill, SkillDefinition,
};

/// A context fragment injected into the system prompt from a frontmatter API skill.
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub(crate) struct SkillContextFragment {
    pub skill_name: String,
    pub body: String,
}

/// The complete result of loading skills from the workspace.
#[derive(Debug)]
#[allow(dead_code)]
pub(crate) struct LoadedSkillSet {
    pub tools: Vec<RegisteredTool>,
    pub executable_skills: Vec<SkillDefinition>,
    pub context_fragments: Vec<SkillContextFragment>,
}

/// Load all skills from a source port. Invalid skills are logged and skipped.
#[allow(dead_code)]
pub(crate) fn load_skills(
    source: &dyn SkillSourcePort,
    reserved_tool_names: &[&str],
) -> LoadedSkillSet {
    let raw_files = source.discover_skill_files();
    let mut tools = Vec::new();
    let mut executable_skills = Vec::new();
    let mut context_fragments = Vec::new();

    for (filename, content) in raw_files {
        match parse_skill_file(&content) {
            Ok(ParsedSkill::Classic(skill)) => {
                if let Err(e) = validate_skill(&skill, reserved_tool_names) {
                    tracing::warn!("Skipping invalid skill '{}': {}", filename, e);
                    continue;
                }
                tools.push(skill_to_registered_tool(&skill));
                executable_skills.push(skill);
            }
            Ok(ParsedSkill::Api {
                definition,
                context_body,
                ..
            }) => {
                if let Err(e) = validate_skill(&definition, reserved_tool_names) {
                    tracing::warn!("Skipping invalid API skill '{}': {}", filename, e);
                    continue;
                }
                tools.push(skill_to_registered_tool(&definition));
                let preamble = api_skill_preamble(&definition.name);
                context_fragments.push(SkillContextFragment {
                    skill_name: definition.name.clone(),
                    body: format!("{preamble}{context_body}"),
                });
                executable_skills.push(definition);
            }
            Err(e) => {
                tracing::warn!("Failed to parse skill '{}': {}", filename, e);
            }
        }
    }

    LoadedSkillSet {
        tools,
        executable_skills,
        context_fragments,
    }
}

/// Load skills filtered by an agent's skill allowlist.
///
/// Delegates to `load_skills()` then applies `filter_skills_for_agent()`.
#[allow(dead_code)]
pub(crate) fn load_skills_for_agent(
    source: &dyn SkillSourcePort,
    reserved_tool_names: &[&str],
    agent_skill_allowlist: Option<&[String]>,
) -> LoadedSkillSet {
    let all = load_skills(source, reserved_tool_names);
    let filtered = filter_skills_for_agent(all.executable_skills, agent_skill_allowlist);
    let filtered_names: std::collections::HashSet<&str> =
        filtered.iter().map(|s| s.name.as_str()).collect();
    let tools = all
        .tools
        .into_iter()
        .filter(|tool| filtered_names.contains(tool.def.name.as_str()))
        .collect();
    let context_fragments = all
        .context_fragments
        .into_iter()
        .filter(|cf| filtered_names.contains(cf.skill_name.as_str()))
        .collect();
    LoadedSkillSet {
        tools,
        executable_skills: filtered,
        context_fragments,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct MockSkillSource {
        files: Vec<(String, String)>,
    }

    impl SkillSourcePort for MockSkillSource {
        fn discover_skill_files(&self) -> Vec<(String, String)> {
            self.files.clone()
        }
    }

    #[test]
    fn load_skills_parses_valid_and_skips_invalid() {
        let valid = r#"# greet

Say hello.

## Parameters
- `name` (string, required): Who to greet

## Execution
```bash
echo "Hello, {{name}}"
```

## Policy
- risk_level: low
- requires_approval: false
"#;
        let invalid = "not a valid skill file at all";

        let source = MockSkillSource {
            files: vec![
                ("greet.md".into(), valid.into()),
                ("bad.md".into(), invalid.into()),
            ],
        };

        let loaded = load_skills(&source, &["read_file", "write_file", "list_directory"]);
        assert_eq!(loaded.tools.len(), 1);
        assert_eq!(loaded.executable_skills.len(), 1);
        assert_eq!(loaded.tools[0].def.name, "greet");
        assert!(loaded.context_fragments.is_empty());
    }

    #[test]
    fn load_skills_rejects_reserved_name_collision() {
        let content = r#"# read_file

Custom reader.

## Execution
```bash
cat file
```
"#;
        let source = MockSkillSource {
            files: vec![("read_file.md".into(), content.into())],
        };

        let loaded = load_skills(&source, &["read_file"]);
        assert!(loaded.tools.is_empty());
        assert!(loaded.executable_skills.is_empty());
    }

    #[test]
    fn load_skills_handles_frontmatter_api_skill() {
        let api_skill = r#"---
name: my-api
description: A test API
homepage: https://api.example.com
---

# My API

Documentation body here.
"#;
        let source = MockSkillSource {
            files: vec![("my-api.md".into(), api_skill.into())],
        };

        let loaded = load_skills(&source, &[]);
        assert_eq!(loaded.tools.len(), 1);
        assert_eq!(loaded.tools[0].def.name, "my_api");
        assert_eq!(loaded.executable_skills.len(), 1);
        assert_eq!(loaded.context_fragments.len(), 1);
        assert_eq!(loaded.context_fragments[0].skill_name, "my_api");
        assert!(loaded.context_fragments[0].body.contains("My API"));
    }

    #[test]
    fn load_skills_for_agent_filters_context_fragments() {
        let api_skill = r#"---
name: my-api
description: A test API
homepage: https://api.example.com
---

# My API docs
"#;
        let source = MockSkillSource {
            files: vec![("my-api.md".into(), api_skill.into())],
        };

        // Filter to a different skill name — should exclude the API skill.
        let allowed = vec!["other_skill".to_string()];
        let loaded = load_skills_for_agent(&source, &[], Some(&allowed));
        assert!(loaded.tools.is_empty());
        assert!(loaded.context_fragments.is_empty());
    }
}
