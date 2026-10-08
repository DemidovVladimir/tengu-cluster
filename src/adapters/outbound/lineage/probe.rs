//! `ContractProbe` over the repo and the tool catalog
//! (`docs/lineage-2026-10-06.md` § 2 pin targets; hashes `domain/lineage/pins.rs`).
//!
//! | Target | Read |
//! |---|---|
//! | `tool_schema:<tool>` | the input schema of the catalog row defining it (`outbound/tools::catalog`, every row, opt-ins included) |
//! | `config:<s>/<path>` · `spec:<s>/<strategy>` | `<repo>/sandboxes/<s>/config.toml` (`config/lineage.rs::sandbox_pin`) |
//! | `skill:<name>` | `<repo>/skills/<name>/SKILL.md` |
//! | `repo:<path>` | `<repo>/<path>` (a file) |
//! | a sandbox's binding | `[generation]` of `<repo>/sandboxes/<s>/config.toml`; its `registry` resolved against that file, compared with the registry dir (canonical paths) |
//!
//! `<repo>` = the registry dir's parent.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::config::lineage::{declared_binding, repo_root, same_dir, sandbox_pin};
use crate::domain::backtest::spec::KINDS;
use crate::domain::lineage::pins::{bytes_sha256, schema_pin};
use crate::domain::lineage::value::PinTarget;
use crate::ports::lineage::{ContractProbe, SandboxBinding};

/// Module table.
pub(crate) struct RepoProbe {
    registry_dir: PathBuf,
    repo: PathBuf,
    /// Catalog tool name → input schema.
    tools: BTreeMap<String, Value>,
}

impl RepoProbe {
    pub(crate) fn new(registry_dir: &Path) -> Self {
        let mut tools = BTreeMap::new();
        for row in crate::adapters::outbound::tools::catalog() {
            for d in (row.defs)() {
                tools.insert(d.name, d.parameters);
            }
        }
        RepoProbe {
            registry_dir: registry_dir.to_path_buf(),
            repo: repo_root(registry_dir),
            tools,
        }
    }

    fn file_sha(&self, rel: &str) -> Result<String, String> {
        let path = self.repo.join(rel);
        if path.is_dir() {
            return Err(format!("{} is a directory", path.display()));
        }
        std::fs::read(&path)
            .map(|b| bytes_sha256(&b))
            .map_err(|e| format!("{}: {e}", path.display()))
    }
}

impl ContractProbe for RepoProbe {
    fn pin_sha256(&self, target: &PinTarget) -> Result<String, String> {
        if let Some(r) = sandbox_pin(&self.registry_dir, target) {
            return r;
        }
        match target {
            PinTarget::ToolSchema(t) => self
                .tools
                .get(t)
                .map(schema_pin)
                .ok_or_else(|| format!("no catalog tool `{t}`")),
            PinTarget::Skill(n) => self.file_sha(&format!("skills/{n}/SKILL.md")),
            PinTarget::Repo(p) => self.file_sha(p),
            PinTarget::Config { .. } | PinTarget::Spec { .. } => {
                unreachable!("sandbox_pin answers config: and spec:")
            }
        }
    }

    fn tool_exists(&self, name: &str) -> bool {
        self.tools.contains_key(name)
    }

    fn strategy_kind_exists(&self, kind: &str) -> bool {
        KINDS.contains(&kind)
    }

    fn sandbox_binding(&self, sandbox: &str) -> Result<Option<SandboxBinding>, String> {
        Ok(
            declared_binding(&self.registry_dir, sandbox)?.map(|(b, dir)| SandboxBinding {
                generation: b.id,
                registry: b.registry.display().to_string(),
                same_registry: same_dir(&dir, &self.registry_dir),
            }),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pins_resolve_against_the_fixture_repo() {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/lineage/registry");
        let p = RepoProbe::new(&dir);
        let pin = |s: &str| p.pin_sha256(&s.parse().unwrap());
        assert_eq!(
            pin("spec:w1/rule_w").unwrap(),
            "cba7a380444a5e6648f0d1421837ce5504c0a1e55e6b3126a8de6596ea343a7f"
        );
        assert_eq!(
            pin("config:w1/risk").unwrap(),
            "241b167a509e9e561b11424f877a61c0aa13d75189b11acfaaee000869d98eeb"
        );
        assert_eq!(
            pin("repo:docs/study.md").unwrap(),
            "91d4fe312748426f951dcbe714ff2d98a14223015ffe1400879dee66419cfa05"
        );
        assert_eq!(pin("tool_schema:backtest").unwrap().len(), 64);
        assert_eq!(
            pin("tool_schema:w2_news_probe").unwrap_err(),
            "no catalog tool `w2_news_probe`"
        );
        assert!(pin("skill:no-such-skill").is_err());
        assert!(pin("repo:docs").unwrap_err().contains("is a directory"));
        assert!(pin("config:w1/backtest.costs.\"solana:\"").is_err());
        assert!(pin("spec:nowhere/rule_w").is_err());
        assert!(p.tool_exists("backtest") && p.tool_exists("paper_order"));
        assert!(!p.tool_exists("w2_news_probe"));
        assert!(p.strategy_kind_exists("event_window") && !p.strategy_kind_exists("news"));
        let w1 = p.sandbox_binding("w1").unwrap().unwrap();
        assert_eq!(
            (
                w1.generation.as_str(),
                w1.registry.as_str(),
                w1.same_registry
            ),
            ("W1", "../../registry", true)
        );
        assert!(p.sandbox_binding("nowhere").is_err());
    }
}
