//! Studio builder composition (`application/builder.rs`): the palette
//! facts from the tool catalog and the skill tiers, the file store under
//! `sandboxes/`, and the "is its runtime running" probe.
//!
//! | Helper | Builds |
//! |---|---|
//! | [`facts`] | every catalog tool `config::builder::palette::offered` keeps (name, description, JSON schema, opt-in, needs memory) · every skill found from the working directory (`skills/`, `~/.tengu/skills/`) · whether this build knows `[keys]` |
//! | [`drafts`] | `FsDrafts` over `./sandboxes`, busy = a fresh `tengu run` heartbeat (`run-<sandbox>.json` in the sandbox's runtime state dir) |
//! | [`builder`] | the `Builder` of one sandbox |

use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::adapters::outbound::builder_store::FsDrafts;
use crate::adapters::outbound::runtime_store::read_heartbeat;
use crate::adapters::outbound::tools::catalog;
use crate::application::builder::Builder;
use crate::application::skills::registry::{FileSystemSkillSource, SkillRegistry};
use crate::config::builder::palette::{offered, tool_group};
use crate::config::builder::{keys_supported, Facts, OfferedTool};
use crate::config::Config;
use crate::domain::observation::now_ms;
use crate::domain::runtime::RunState;

/// Where `--sandbox <name>` looks.
pub(crate) const SANDBOXES_DIR: &str = "sandboxes";

/// The palette facts (module table), skills found from `cwd`.
pub(crate) fn facts(cwd: &Path) -> Facts {
    let mut tools: Vec<OfferedTool> = Vec::new();
    for row in catalog() {
        for def in (row.defs)() {
            let t = OfferedTool {
                group: tool_group(&def.name),
                name: def.name,
                description: def.description,
                parameters: def.parameters,
                opt_in: row.opt_in.is_some(),
                needs_memory: row.needs_memory,
            };
            if offered(&t) && !tools.iter().any(|x| x.name == t.name) {
                tools.push(t);
            }
        }
    }
    let mut registry = SkillRegistry::new(Vec::new());
    registry.reload(&FileSystemSkillSource::new(cwd.to_path_buf()));
    let mut skills: Vec<(String, String)> = registry
        .entries()
        .iter()
        .map(|(name, e)| (name.clone(), e.definition.description.clone()))
        .collect();
    skills.sort();
    Facts {
        tools,
        skills,
        keys_supported: keys_supported(),
    }
}

/// Why `sandbox`'s runtime blocks a rewrite: its `tengu run` heartbeat is
/// fresh and not stopped.
fn running(root: &Path, sandbox: &str) -> Option<String> {
    let cfg = Config::load(&root.join(sandbox).join("config.toml")).ok()?;
    let dir = crate::bootstrap::runtime::runtime_state_dir(&cfg);
    let hb = read_heartbeat(&dir, sandbox).ok()??;
    let window_ms = (hb.heartbeat_secs.max(1) as i64) * 3 * 1000;
    (hb.state != RunState::Stopped && now_ms() - hb.ts_ms < window_ms).then(|| {
        format!(
            "its runtime is {} (holder {}, pid {}): stop it first (Studio Stop or Ctrl-C on `tengu run`)",
            hb.state.as_str(),
            hb.holder,
            hb.pid
        )
    })
}

pub(crate) fn drafts(root: impl Into<PathBuf>) -> Arc<FsDrafts> {
    let root: PathBuf = root.into();
    let probe_root = root.clone();
    Arc::new(FsDrafts::new(
        root,
        Box::new(move |s| running(&probe_root, s)),
    ))
}

/// The builder of `sandbox` over `./sandboxes`.
pub(crate) fn builder(sandbox: &str) -> Builder {
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    Builder::new(sandbox, facts(&cwd), drafts(SANDBOXES_DIR))
}
