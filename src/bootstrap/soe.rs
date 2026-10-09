//! SOE composition (O3 / O4; `docs/soe-2026-10-08.md` § 10): the
//! `soe_cycle` job a `[feeds.<n>] kind = "job"` feed runs under `tengu run`
//! (`bootstrap/runtime.rs::job_for`), built over the sandbox's adapters.
//!
//! | Piece | Built from |
//! |---|---|
//! | state root | the `[sources]` state dir `<TENGU_HOME>/state/<sources.state>/` (critic C8) |
//! | cycle store | `outbound::soe::store::FsCycleStore` on the state root |
//! | source store | opened each run (`outbound::sources::open_source_store`); no `sources.db` yet ⇒ none — an empty packet |
//! | stage runner ([`stage_runner`]) | `CachedStageRunner` over `SubprocessStageRunner` (`--sandbox`, the config's agents) — online: a live stage is recorded under `stage-cache/` for replays; offline: a miss is an error. Each stage agent's identity = its `model` + `runner::skill_sha256` over its workspace's skill dirs |
//! | generation pin ([`generation_pin`]) | bound (`[generation]`) ⇒ its id + `toml_digest` of `<registry>/generations/<id>.toml`; unbound ⇒ `UNBOUND` + `toml_digest` of the config file `Config::load` read |
//! | profile | `<state root>/operator.toml` (`config::soe::SoeConfig::profile_path`); a synthetic profile refused |
//! | clock · zone | `SystemClock` (latency only; the decision time is the slot) · the feed's `tz` |
//! | ownership | the runtime's leases (`bootstrap::runtime::OwnerLeases::ownership`): the cycle renews them before each stage, its freeze and each state-log append, and stops on a loss (`lease_lost`, nothing more written) |

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;

use anyhow::{anyhow, bail, Context, Result};

use crate::adapters::outbound::clock::SystemClock;
use crate::adapters::outbound::soe::cache::{CachedStageRunner, StageIdent};
use crate::adapters::outbound::soe::runner::{skill_sha256, SubprocessStageRunner};
use crate::adapters::outbound::soe::store::FsCycleStore;
use crate::adapters::outbound::sources::open_source_store;
use crate::application::soe::job::{SoeCycleJob, SourceOpener};
use crate::application::soe::submit::GenerationPin;
use crate::bootstrap::runtime::agent_workspace;
use crate::config::lineage::record_path;
use crate::config::paths::resolve_tengu_home;
use crate::config::sections::SandboxSections;
use crate::config::soe::SoeConfig;
use crate::config::sources::sources_db;
use crate::config::Config;
use crate::domain::lineage::pins::toml_digest;
use crate::domain::lineage::value::RecordKind;
use crate::domain::tz::Zone;
use crate::ports::runtime::{Ownership, RuntimeJob};
use crate::ports::soe::{CycleStore, StageRunner};

/// The id of an unbound sandbox's pin (the `soe_*` tools stamp the same).
pub(crate) use crate::application::soe::submit::UNBOUND;

/// The weekly cycle job of `config` for a feed in `zone`, writing under
/// `owner` (module table).
pub(crate) fn soe_cycle_job(
    config: &Config,
    zone: Zone,
    owner: Arc<dyn Ownership>,
) -> Result<Arc<dyn RuntimeJob>> {
    let soe = config
        .soe
        .clone()
        .ok_or_else(|| anyhow!("no [soe] section"))?;
    let registry = config
        .sources
        .clone()
        .ok_or_else(|| anyhow!("[soe] needs [sources]: its state dir is the SOE state root"))?;
    let root = registry.state_dir(&resolve_tengu_home());
    let store: Arc<dyn CycleStore> = Arc::new(FsCycleStore::new(&root));
    let sections = Arc::new(SandboxSections {
        sources_state_dir: Some(root.clone()),
        ..SandboxSections::default()
    });
    let db_dir = root.clone();
    let sources: SourceOpener = Arc::new(move || {
        if !sources_db(&db_dir).is_file() {
            return Ok(None);
        }
        open_source_store(&sections).map(Some)
    });
    let runner = stage_runner(config, &soe, &root, Arc::clone(&store), true)?;
    Ok(Arc::new(SoeCycleJob {
        store,
        sources,
        registry: Arc::new(registry),
        runner: Some(runner),
        clock: Arc::new(SystemClock),
        profile_path: SoeConfig::profile_path(&root),
        allow_synthetic: false,
        zone,
        generation: generation_pin(config)?,
        soe,
        owner,
    }))
}

/// Module table: stage runner — `online` asks the agents on a miss.
pub(crate) fn stage_runner(
    config: &Config,
    soe: &SoeConfig,
    state_root: &Path,
    store: Arc<dyn CycleStore>,
    online: bool,
) -> Result<Arc<dyn StageRunner>> {
    let mut idents = BTreeMap::new();
    for name in [&soe.architect, &soe.critic] {
        let agent = config
            .agents
            .get(name)
            .ok_or_else(|| anyhow!("no [agents.{name}] block for a stage"))?;
        let workspace = agent_workspace(config, name);
        idents.insert(
            name.clone(),
            StageIdent {
                model: agent.model.clone(),
                skill_sha256: skill_sha256(agent, &workspace),
            },
        );
    }
    let inner = online.then(|| {
        Arc::new(SubprocessStageRunner::new(
            config.sandbox_name.clone(),
            config.agents.clone(),
        )) as Arc<dyn StageRunner>
    });
    Ok(Arc::new(CachedStageRunner::new(
        state_root, store, inner, idents,
    )))
}

/// Module table: generation pin.
pub(crate) fn generation_pin(config: &Config) -> Result<GenerationPin> {
    let file = config.loaded_from.as_deref().ok_or_else(|| {
        anyhow!("the config file is unknown — a cycle pins its generation from it")
    })?;
    let (id, path) = match &config.generation {
        Some(b) => (
            b.id.clone(),
            record_path(&b.registry_dir(file), RecordKind::Generation, &b.id),
        ),
        None => (UNBOUND.to_string(), file.to_path_buf()),
    };
    let text =
        std::fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?;
    let sha256 = toml_digest(&text).map_err(|e| anyhow!("{}: {e}", path.display()))?;
    let pin = GenerationPin { id, sha256 };
    let problems = pin.problems();
    if !problems.is_empty() {
        bail!("generation pin: {}", problems.join("; "));
    }
    Ok(pin)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Unbound: `UNBOUND` + the config file's digest; bound: the
    /// generation record's digest; no file known: refused.
    #[test]
    fn generation_pin_hashes_the_bound_record_or_the_config() {
        let tmp = tempfile::TempDir::new().unwrap();
        let file = tmp.path().join("config.toml");
        let text = "[agents.a]\nengine = \"openrouter\"\nmodel = \"m\"\n";
        std::fs::write(&file, text).unwrap();
        let mut cfg: Config = toml::from_str(text).unwrap();
        let e = generation_pin(&cfg).unwrap_err().to_string();
        assert!(e.contains("config file is unknown"), "{e}");
        cfg.loaded_from = Some(file.clone());
        let pin = generation_pin(&cfg).unwrap();
        assert_eq!(
            (pin.id.as_str(), pin.sha256),
            (UNBOUND, toml_digest(text).unwrap())
        );

        let gen = tmp.path().join("lineage/generations");
        std::fs::create_dir_all(&gen).unwrap();
        let record = "id = \"SOE-G0\"\nnote = \"synthetic\"\n";
        std::fs::write(gen.join("SOE-G0.toml"), record).unwrap();
        cfg.generation = Some(toml::from_str("id = \"SOE-G0\"\nregistry = \"lineage\"").unwrap());
        let pin = generation_pin(&cfg).unwrap();
        assert_eq!(
            (pin.id.as_str(), pin.sha256),
            ("SOE-G0", toml_digest(record).unwrap())
        );
    }

    /// The job builds from a loaded config and, with no signed profile in
    /// its state root, fails the run before anything is created.
    #[tokio::test]
    async fn the_built_job_refuses_without_a_profile() {
        use crate::ports::runtime::JobOutcome;
        let tmp = tempfile::TempDir::new().unwrap();
        let state = format!("soe-bootstrap-test-{}", uuid::Uuid::new_v4());
        let text = format!(
            "[agents.soe_architect]\nengine = \"openrouter\"\nmodel = \"a-model\"\ndescription = \"d\"\ntools = [\"soe_propose\"]\n\
             [agents.soe_critic]\nengine = \"openrouter\"\nmodel = \"c-model\"\ndescription = \"d\"\ntools = [\"soe_challenge\"]\n\
             [sources]\nstate = \"{state}\"\n\
             [soe]\narchitect = \"soe_architect\"\ncritic = \"soe_critic\"\nmax_proposals = 12\nforecast_max_weeks = 12\n"
        );
        let file = tmp.path().join("config.toml");
        std::fs::write(&file, &text).unwrap();
        let mut cfg: Config = toml::from_str(&text).unwrap();
        cfg.loaded_from = Some(file);
        cfg.fold_default_scopes();
        let owner: Arc<dyn Ownership> = Arc::new(crate::ports::runtime::Unleased);
        let job = soe_cycle_job(&cfg, Zone::Paris, Arc::clone(&owner)).unwrap();
        let root = resolve_tengu_home().join("state").join(&state);
        // Mon 2026-10-05 07:00 Paris.
        match job.run(1_791_176_400_000, "feed:soe_week:1").await {
            JobOutcome::Failed { message, .. } => {
                assert!(
                    message.starts_with("operator_profile_missing: "),
                    "{message}"
                )
            }
            other => panic!("{other:?}"),
        }
        assert!(!root.exists(), "nothing created in {}", root.display());
        let mut no_soe = cfg.clone();
        no_soe.soe = None;
        assert!(soe_cycle_job(&no_soe, Zone::Paris, owner).is_err());
    }

    /// § 13 no unapproved side effect (roadmap O4 safety): no catalog tool
    /// reachable from the shipped `soe` sandbox can contact, spend, publish
    /// or deploy. Every agent's effective tool set (its list within the
    /// generation, `agent_base_tools`) sits in the closed world; no tool of
    /// the closed world is a side-effect, signing, exec or Solana write tool;
    /// every side-effect tool's scope is deny-all on every agent; Claude Code
    /// built-ins are off and no shell runs; a plan step's `compose` cannot
    /// widen a stage agent; no loop, server or non-job feed exists; the
    /// network reaches the registry's hosts only.
    #[test]
    fn soe_sandbox_has_no_side_effect_surface() {
        use crate::bootstrap::tools::{agent_base_tools, compose_agent};
        use crate::domain::plan::AgentCompose;
        use crate::domain::tools::{
            PRIVY_SIGNING_TOOLS, SIDE_EFFECT_TOOLS, SOE_ALLOWED, SOLANA_WRITE_TOOLS, XM_EXEC_TOOLS,
        };
        let file = Path::new(env!("CARGO_MANIFEST_DIR")).join("sandboxes/soe/config.toml");
        let cfg = Config::load(&file).unwrap_or_else(|e| panic!("{e:#}"));
        let reaching: Vec<&str> = SIDE_EFFECT_TOOLS
            .iter()
            .chain(PRIVY_SIGNING_TOOLS)
            .chain(SOLANA_WRITE_TOOLS)
            .chain(XM_EXEC_TOOLS)
            .chain(&["manage_skill", "apply_improver_proposal", "skill_distill"])
            .copied()
            .collect();
        for t in SOE_ALLOWED {
            assert!(
                !reaching.contains(t),
                "`{t}` reaches out yet is in SOE_ALLOWED"
            );
        }
        assert!(!cfg.agents.is_empty());
        for (name, agent) in &cfg.agents {
            let tools = agent_base_tools(agent, true, cfg.memory.enabled);
            let soe = cfg.soe.as_ref().unwrap();
            if [&soe.architect, &soe.critic].contains(&name) {
                assert!(tools.len() >= 2, "{name}: {tools:?}");
            }
            for t in &tools {
                assert!(
                    SOE_ALLOWED.contains(&t.name.as_str()),
                    "{name} reaches `{}` outside the closed world",
                    t.name
                );
            }
            for t in SIDE_EFFECT_TOOLS {
                assert!(
                    agent.scopes.get(*t).is_some_and(|s| s.is_deny_all()),
                    "{name}: the scope of `{t}` is not deny-all"
                );
            }
            assert!(agent.no_shell_fallback, "{name} may run a shell");
            if agent.engine == "claude_code" {
                let profile = agent
                    .claude_code
                    .as_ref()
                    .map(|c| c.builtin_tools_profile.trim());
                assert_eq!(profile, Some("none"), "{name}: Claude Code built-ins on");
            }
            for widen in [vec!["http_request"], vec!["write_file"], Vec::new()] {
                let compose = AgentCompose {
                    base_agent: name.clone(),
                    skills: agent.skill_packages.clone(),
                    tools: widen.iter().map(|t| t.to_string()).collect(),
                };
                assert!(
                    compose_agent(name, agent, &compose, true, cfg.memory.enabled).is_err(),
                    "{name}: a compose with {widen:?} widened it"
                );
            }
        }
        assert!(cfg.mcp_servers.is_empty() && cfg.decision_loops.is_empty());
        assert!(cfg.feeds.values().all(|f| f.kind == "job"));
        assert!(
            cfg.risk.is_none() && cfg.xmarket.is_none() && cfg.solana.signer_key_file.is_none()
        );
        let hosts: std::collections::BTreeSet<&str> = cfg
            .sources
            .iter()
            .flat_map(|s| s.registry.values())
            .flat_map(|e| e.hosts.iter().map(String::as_str))
            .collect();
        let allowed: std::collections::BTreeSet<&str> =
            cfg.egress.allow_hosts.iter().map(String::as_str).collect();
        assert_eq!(allowed, hosts);
    }
}
