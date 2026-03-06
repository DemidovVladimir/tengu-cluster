//! Mutable registry for user-defined skills with hot-reload and enable/disable.

use std::collections::HashMap;
use std::hash::{Hash, Hasher};

use crate::application::ports::SkillSourcePort;
use crate::domain::skill::{
    api_skill_preamble, diff_skill_sets, parse_skill_file, skill_to_tool_def, validate_skill,
    ParsedSkill, SkillDefinition, SkillDiff, SkillEntry, SkillStatus,
};
use tengu_core::types::ToolDef;

/// Application-layer registry for discovered skills.
///
/// Lives on the engine thread. Receives raw file data through `SkillSourcePort` —
/// never touches the filesystem directly.
pub(crate) struct SkillRegistry {
    entries: HashMap<String, SkillEntry>,
    reserved_names: Vec<String>,
}

impl SkillRegistry {
    /// Create an empty registry with the given reserved (built-in) tool names.
    pub(crate) fn new(reserved: Vec<String>) -> Self {
        Self {
            entries: HashMap::new(),
            reserved_names: reserved,
        }
    }

    /// Re-scan the source, parse, diff, and apply changes.
    ///
    /// Returns `true` if anything changed (skills added, removed, or content changed).
    /// User enable/disable choices are preserved for skills that survive the reload.
    pub(crate) fn reload(&mut self, source: &dyn SkillSourcePort) -> bool {
        let raw_files = source.discover_skill_files();
        let reserved_strs: Vec<&str> = self.reserved_names.iter().map(|s| s.as_str()).collect();

        let mut fresh: Vec<(String, SkillDefinition, u64, Option<String>)> = Vec::new();

        for (filename, content) in &raw_files {
            let hash = content_hash(content);
            match parse_skill_file(content) {
                Ok(ParsedSkill::Classic(skill)) => {
                    if validate_skill(&skill, &reserved_strs).is_err() {
                        tracing::warn!("Skipping invalid skill '{}'", filename);
                        continue;
                    }
                    let name = skill.name.clone();
                    fresh.push((name, skill, hash, None));
                }
                Ok(ParsedSkill::Api {
                    definition,
                    context_body,
                }) => {
                    if validate_skill(&definition, &reserved_strs).is_err() {
                        tracing::warn!("Skipping invalid API skill '{}'", filename);
                        continue;
                    }
                    let preamble = api_skill_preamble(&definition.name);
                    let name = definition.name.clone();
                    fresh.push((name, definition, hash, Some(format!("{preamble}{context_body}"))));
                }
                Err(e) => {
                    tracing::warn!("Failed to parse skill '{}': {}", filename, e);
                }
            }
        }

        let diff: SkillDiff = diff_skill_sets(&self.entries, &fresh);
        if diff.is_empty() {
            return false;
        }

        // Remove gone skills.
        for name in &diff.removed {
            self.entries.remove(name);
            tracing::info!("Skill removed: {name}");
        }

        // Apply adds and changes — preserve status for changed skills.
        for (name, definition, hash, ctx) in fresh {
            if diff.added.contains(&name) {
                tracing::info!("Skill added: {name}");
                self.entries.insert(
                    name,
                    SkillEntry {
                        definition,
                        status: SkillStatus::Active,
                        content_hash: hash,
                        context_body: ctx,
                    },
                );
            } else if diff.changed.contains(&name) {
                tracing::info!("Skill updated: {name}");
                let prev_status = self
                    .entries
                    .get(&name)
                    .map(|e| e.status)
                    .unwrap_or(SkillStatus::Active);
                self.entries.insert(
                    name,
                    SkillEntry {
                        definition,
                        status: prev_status,
                        content_hash: hash,
                        context_body: ctx,
                    },
                );
            }
            // unchanged — leave in place
        }

        true
    }

    /// Tool definitions for only active skills.
    pub(crate) fn active_tool_defs(&self) -> Vec<ToolDef> {
        self.entries
            .values()
            .filter(|e| e.status == SkillStatus::Active)
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

    /// Enable a skill by name. Returns `Ok(true)` if status changed, `Ok(false)` if already active.
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

    /// Disable a skill by name. Returns `Ok(true)` if status changed, `Ok(false)` if already inactive.
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

/// Stable content hash for change detection.
fn content_hash(content: &str) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    content.hash(&mut hasher);
    hasher.finish()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    struct MockSource {
        files: Vec<(String, String)>,
    }

    impl SkillSourcePort for MockSource {
        fn discover_skill_files(&self) -> Vec<(String, String)> {
            self.files.clone()
        }
    }

    const CLASSIC_SKILL: &str = r#"# greet

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

    const API_SKILL: &str = r#"---
name: my-api
description: A test API
homepage: https://api.example.com
---

# My API

Documentation body here.
"#;

    #[test]
    fn reload_adds_new_skills() {
        let source = MockSource {
            files: vec![("greet".into(), CLASSIC_SKILL.into())],
        };
        let mut reg = SkillRegistry::new(vec![]);
        let changed = reg.reload(&source);
        assert!(changed);
        assert_eq!(reg.list_all().len(), 1);
        assert_eq!(reg.list_all()[0].0, "greet");
        assert_eq!(reg.list_all()[0].1, SkillStatus::Active);
    }

    #[test]
    fn reload_removes_deleted_skills() {
        let source1 = MockSource {
            files: vec![("greet".into(), CLASSIC_SKILL.into())],
        };
        let mut reg = SkillRegistry::new(vec![]);
        reg.reload(&source1);

        let source2 = MockSource { files: vec![] };
        let changed = reg.reload(&source2);
        assert!(changed);
        assert!(reg.list_all().is_empty());
    }

    #[test]
    fn reload_detects_content_change() {
        let source1 = MockSource {
            files: vec![("greet".into(), CLASSIC_SKILL.into())],
        };
        let mut reg = SkillRegistry::new(vec![]);
        reg.reload(&source1);

        let modified = CLASSIC_SKILL.replace("Say hello.", "Say goodbye.");
        let source2 = MockSource {
            files: vec![("greet".into(), modified)],
        };
        let changed = reg.reload(&source2);
        assert!(changed);
    }

    #[test]
    fn reload_noop_when_unchanged() {
        let source = MockSource {
            files: vec![("greet".into(), CLASSIC_SKILL.into())],
        };
        let mut reg = SkillRegistry::new(vec![]);
        reg.reload(&source);
        let changed = reg.reload(&source);
        assert!(!changed);
    }

    #[test]
    fn reload_preserves_user_disable() {
        let source = MockSource {
            files: vec![("greet".into(), CLASSIC_SKILL.into())],
        };
        let mut reg = SkillRegistry::new(vec![]);
        reg.reload(&source);
        reg.disable("greet").unwrap();

        // Modify the content so a change is detected
        let modified = CLASSIC_SKILL.replace("Say hello.", "Updated.");
        let source2 = MockSource {
            files: vec![("greet".into(), modified)],
        };
        reg.reload(&source2);

        // Status should still be Inactive (preserved across reload)
        assert_eq!(reg.list_all()[0].1, SkillStatus::Inactive);
    }

    #[test]
    fn enable_disable_toggle() {
        let source = MockSource {
            files: vec![("greet".into(), CLASSIC_SKILL.into())],
        };
        let mut reg = SkillRegistry::new(vec![]);
        reg.reload(&source);

        assert_eq!(reg.active_tool_defs().len(), 1);
        assert!(reg.disable("greet").unwrap());
        assert!(reg.active_tool_defs().is_empty());
        assert!(!reg.disable("greet").unwrap()); // already disabled
        assert!(reg.enable("greet").unwrap());
        assert_eq!(reg.active_tool_defs().len(), 1);
        assert!(!reg.enable("greet").unwrap()); // already enabled
    }

    #[test]
    fn enable_unknown_returns_error() {
        let mut reg = SkillRegistry::new(vec![]);
        assert!(reg.enable("nope").is_err());
    }

    #[test]
    fn disable_unknown_returns_error() {
        let mut reg = SkillRegistry::new(vec![]);
        assert!(reg.disable("nope").is_err());
    }

    #[test]
    fn active_filtering_respects_status() {
        let source = MockSource {
            files: vec![
                ("greet".into(), CLASSIC_SKILL.into()),
                ("api".into(), API_SKILL.into()),
            ],
        };
        let mut reg = SkillRegistry::new(vec![]);
        reg.reload(&source);

        assert_eq!(reg.active_tool_defs().len(), 2);
        assert_eq!(reg.active_skill_definitions().len(), 2);
        assert_eq!(reg.active_context_fragments().len(), 1); // only API skill

        reg.disable("my_api").unwrap();
        assert_eq!(reg.active_tool_defs().len(), 1);
        assert!(reg.active_context_fragments().is_empty());
    }

    #[test]
    fn reserved_names_are_rejected() {
        let source = MockSource {
            files: vec![("rf".into(), CLASSIC_SKILL.replace("greet", "read_file"))],
        };
        let mut reg = SkillRegistry::new(vec!["read_file".into()]);
        reg.reload(&source);
        assert!(reg.list_all().is_empty());
    }
}
