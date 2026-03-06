//! Filesystem adapter for discovering skill files from the workspace.
//!
//! Scans `<dir>/<name>/SKILL.md` subdirectories. Priority: `.tengu/skills/` wins
//! over `skills/` via `seen_names` dedup.

use crate::application::ports::SkillSourcePort;
use std::path::PathBuf;

pub(crate) struct FileSystemSkillSource {
    workspace: PathBuf,
}

impl FileSystemSkillSource {
    pub(crate) fn new(workspace: PathBuf) -> Self {
        Self { workspace }
    }

    fn skill_directories(&self) -> Vec<PathBuf> {
        vec![
            self.workspace.join(".tengu/skills"),
            self.workspace.join("skills"),
        ]
    }
}

impl SkillSourcePort for FileSystemSkillSource {
    fn discover_skill_files(&self) -> Vec<(String, String)> {
        let mut results = Vec::new();
        let mut seen_names = std::collections::HashSet::new();

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

                // New layout: <dir>/<skill_name>/SKILL.md
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

                // Legacy flat layout: <dir>/<name>.md  (backwards compat)
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
