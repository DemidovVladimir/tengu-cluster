//! Port for discovering skill.md files from the workspace.

/// Port for discovering skill.md files from the workspace.
pub(crate) trait SkillSourcePort: Send + Sync {
    /// Returns a list of (filename, file_content) pairs for all discovered skill files.
    fn discover_skill_files(&self) -> Vec<(String, String)>;
}
