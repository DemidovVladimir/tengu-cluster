//! SOE files (`docs/soe-2026-10-08.md` § 2): the private operator profile and
//! the record dirs (opportunities, eval cases). Pure file IO like
//! `config/lineage.rs`, whose dir walk it reuses; the records and their rules
//! are `domain/soe/`. Also the `[soe]` section (O3, [`SoeConfig`]) and its
//! closed-world load rules (below).
//!
//! | Loader | Rule (each refusal starts with its code) |
//! |---|---|
//! | [`default_profile_path`] | `<TENGU_HOME>/state/soe/operator.toml` (under `state/`, so `state:soe/…` locators resolve) |
//! | [`load_profile`] | no file ⇒ `operator_profile_missing` — never a built-in default (PRD § 14); a parse or rule error ⇒ `invalid_profile`, every problem listed; `synthetic = true` ⇒ `synthetic_profile_refused` unless allowed (public fixtures); a real profile inside a git work tree (a `.git` dir or file in any ancestor) ⇒ `profile_in_repo`, with any group / other permission bit ⇒ `profile_mode` (`chmod 600`); `signed_by = "UNSIGNED"` ⇒ `operator_profile_unsigned` |
//! | [`load_opportunity`] | one `soe.opportunity/1` file, any name; problems listed |
//! | [`load_record_dir`] | `<id>.toml` per record (file stem = `id`); `*.md` and dotfiles skipped; another entry refused; sorted by id; every error names its file |
//! | [`load_replay_set`] | one `soe.replay_set/1` file (O4 replay): problems listed (`invalid_replay_set`); `synthetic = true` ⇒ `synthetic_replay_set_refused` unless allowed; the operator's labels (`synthetic = false`) inside a git work tree ⇒ `replay_set_in_repo`, with a group / other permission bit ⇒ `replay_set_mode` — holdout outcomes stay private |
//! | [`load_cited`] | a `--cited` file: `[[cited]]` `gates::CitedRecord` views (what each cited source record shows; the O2 as-of view builds them later) — record ids unique, unknown keys refused, problems listed |
//! | digest | `canonical::toml_digest` of the file text (the profile's is `profile_sha256`) |
//!
//! `[soe]` — the weekly cycle of a Software Opportunity Engine sandbox
//! (`application/soe/`, run by the `soe_cycle` job of a `kind = "job"` feed,
//! `config/feeds.rs`). Every key required (no built-in default);
//! `deny_unknown_fields`. Its state root is the `[sources]` state dir
//! (critic C8/C9): `<TENGU_HOME>/state/<sources.state>/` holds `sources.db`,
//! `operator.toml`, `cycles/`, `replays/`, `stage-cache/` and the state logs.
//!
//! ```toml
//! [soe]
//! architect = "soe_architect"
//! critic = "soe_critic"
//! max_proposals = 12
//! forecast_max_weeks = 12
//! # token_prices = { currency = "USD", prompt_per_million = "15.00", completion_per_million = "75.00" }
//! ```
//!
//! | Key | Rule |
//! |---|---|
//! | `architect` · `critic` | two different `[agents.<name>]` with a `description` (a stage is a `run-agent` step); the Architect lists no `soe_challenge`, the Critic no `soe_propose` |
//! | `max_proposals` | 1–[`MAX_PROPOSALS`] per cycle |
//! | `forecast_max_weeks` | 1–[`MAX_FORECAST_WEEKS`]: a forecast resolves within this many weeks of the decision |
//! | `token_prices` | optional `domain::soe::ops::TokenPrices`, ≥ 0; absent ⇒ the cycle's cost is `UNKNOWN` |
//!
//! | `[soe]` load rule ([`validation_errors`]; a violation fails `Config::load`) | Why |
//! |---|---|
//! | `[sources]` present | the state root is its state dir |
//! | every agent's `tools` non-empty (empty = every base tool) and, like `workspace_tools`, inside `domain::tools::SOE_ALLOWED` | closed world: no write, contact, spend, publish or shell tool |
//! | `[default_scopes.<t>]` deny-all (no keys) for each `domain::tools::SIDE_EFFECT_TOOLS`; an agent's own scope for one stays deny-all | defence in depth behind the tool lists |
//! | no `[decision_loops]`; every `[feeds.*]` `kind = "job"` | the cycle is the only scheduled work |
//! | no `[risk]`, `[paper]`, `[xmarket]`, `[backtest]`, `[solana] privy_wallet_id`, `[telegram]` (enabled or users), `[webhooks]` (enabled or endpoints) | no trading, signing or inbound surface |
//! | `[egress] allow_hosts` ⊆ the hosts of the `[sources.registry.*]` rows (every listed row: `[sources]` checks the other way, so they are equal); non-empty under `network = "open"` | the sandbox reaches its listed sources only |
//! | the state root outside every git work tree; an existing `operator.toml` there with no group / other permission bit | private data never sits in a repo or opens to other users |
//! | hardened (`config/hardening.rs`): `claude_code` agents `builtin_tools_profile = "none"`, no shell fallback, no `[[mcp_servers]]`, `<TENGU_HOME>/state` outside every fs root and workspace | nothing runs outside tengu scopes |
//!
//! The rules read file metadata only: a load creates and changes nothing.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use super::egress::NETWORK_OPEN;
use super::lineage::{parse, toml_files};
use super::paths;
use super::{AgentConfig, Config};
use crate::domain::canonical::toml_digest;
use crate::domain::soe::gates::{cited_problems, CitedRecord};
use crate::domain::soe::opportunity::Opportunity;
use crate::domain::soe::ops::TokenPrices;
use crate::domain::soe::profile::OperatorProfile;
use crate::domain::soe::record::{from_toml, validate, Problems, SoeRecord};
use crate::domain::soe::value::{codes, Minor, ValueError};
use crate::domain::tools::{SIDE_EFFECT_TOOLS, SOE_ALLOWED};

/// The SOE state dir name under `<TENGU_HOME>/state/`.
pub const SOE_STATE: &str = "soe";
/// The profile's file name in it.
pub const PROFILE_FILE: &str = "operator.toml";

/// Module table.
pub fn default_profile_path() -> PathBuf {
    paths::resolve_tengu_home()
        .join("state")
        .join(SOE_STATE)
        .join(PROFILE_FILE)
}

/// A record read from its file, with the file's digest.
#[derive(Debug, Clone)]
pub struct Loaded<R> {
    pub record: R,
    /// `toml_digest` of the file text, 64 hex.
    pub sha256: String,
    pub path: PathBuf,
}

/// `<path>: <code>: <field>: <why>`, one line each.
fn listed(path: &Path, errors: &[ValueError]) -> String {
    errors
        .iter()
        .map(|e| format!("{}: {e}", path.display()))
        .collect::<Vec<_>>()
        .join("\n")
}

fn read_record<R: SoeRecord + DeserializeOwned>(path: &Path) -> Result<Loaded<R>, String> {
    let text = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let record = from_toml::<R>(&text).map_err(|errors| listed(path, &errors))?;
    let sha256 = toml_digest(&text).map_err(|e| format!("{}: {e}", path.display()))?;
    Ok(Loaded {
        record,
        sha256,
        path: path.to_path_buf(),
    })
}

/// The git work tree `path` sits in: the nearest ancestor holding `.git`
/// (a dir, or a file in a linked worktree).
pub fn git_work_tree(path: &Path) -> Option<PathBuf> {
    let abs = paths::absolute_path(path);
    abs.ancestors()
        .find(|a| a.join(".git").exists())
        .map(Path::to_path_buf)
}

/// Group / other permission bits of the file (`None` off unix).
fn loose_mode(path: &Path) -> Result<Option<u32>, String> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(path)
            .map_err(|e| format!("{}: {e}", path.display()))?
            .permissions()
            .mode();
        Ok((mode & 0o077 != 0).then_some(mode & 0o777))
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        Ok(None)
    }
}

/// Module table: the operator profile at `path`, with its digest.
pub fn load_profile(path: &Path, allow_synthetic: bool) -> Result<Loaded<OperatorProfile>, String> {
    if !path.exists() {
        return Err(format!(
            "operator_profile_missing: {}: no operator profile — the operator writes and signs \
             it; no parameter has a built-in default (PRD § 14)",
            path.display()
        ));
    }
    let loaded =
        read_record::<OperatorProfile>(path).map_err(|e| format!("invalid_profile:\n{e}"))?;
    let p = &loaded.record;
    if p.synthetic {
        if !allow_synthetic {
            return Err(format!(
                "synthetic_profile_refused: {}: a synthetic test profile (`synthetic = true`) — \
                 allow synthetic profiles to use it",
                path.display()
            ));
        }
    } else {
        if let Some(tree) = git_work_tree(path) {
            return Err(format!(
                "profile_in_repo: {}: inside the git work tree {} — the operator profile is \
                 private; keep it under <TENGU_HOME>/state/{SOE_STATE}/",
                path.display(),
                tree.display()
            ));
        }
        if let Some(mode) = loose_mode(path)? {
            return Err(format!(
                "profile_mode: {}: mode {mode:o} lets others read it — chmod 600",
                path.display()
            ));
        }
    }
    if !p.is_signed() {
        return Err(format!(
            "operator_profile_unsigned: {}: `signed_by` / `signed_at` not set — the operator \
             reviews the values and signs the profile before anything decides on it",
            path.display()
        ));
    }
    Ok(loaded)
}

/// [`load_profile`] plus the file's text (a cycle keeps it): the text read
/// must hash to the digest loaded (`profile_changed` when the file was
/// replaced in between).
pub fn load_profile_with_text(
    path: &Path,
    allow_synthetic: bool,
) -> Result<(Loaded<OperatorProfile>, String), String> {
    let loaded = load_profile(path, allow_synthetic)?;
    let text = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
    if toml_digest(&text).ok().as_deref() != Some(loaded.sha256.as_str()) {
        return Err(format!(
            "profile_changed: {}: the file changed while it was read — run again",
            path.display()
        ));
    }
    Ok((loaded, text))
}

/// Module table: one opportunity file.
pub fn load_opportunity(path: &Path) -> Result<Loaded<Opportunity>, String> {
    read_record(path)
}

/// A `--cited` file (module table).
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CitedFile {
    #[serde(default)]
    cited: Vec<CitedRecord>,
}

/// Module table: the record views in `path`.
pub fn load_cited(path: &Path) -> Result<Vec<CitedRecord>, String> {
    let text = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let file: CitedFile = toml::from_str(&text).map_err(|e| {
        format!(
            "{}: {}: {}",
            path.display(),
            codes::INVALID_RECORD,
            e.to_string().trim_end()
        )
    })?;
    let mut p = Problems::default();
    cited_problems(&file.cited, &mut p);
    p.into_result().map_err(|errors| listed(path, &errors))?;
    Ok(file.cited)
}

/// Module table: one replay set (the profile's privacy rules).
pub fn load_replay_set(
    path: &Path,
    allow_synthetic: bool,
) -> Result<Loaded<crate::domain::soe::replay::ReplaySet>, String> {
    let loaded = read_record::<crate::domain::soe::replay::ReplaySet>(path)
        .map_err(|e| format!("invalid_replay_set:\n{e}"))?;
    if loaded.record.synthetic {
        if !allow_synthetic {
            return Err(format!(
                "synthetic_replay_set_refused: {}: a synthetic test set (`synthetic = true`) — \
                 allow synthetic records to use it",
                path.display()
            ));
        }
        return Ok(loaded);
    }
    if let Some(tree) = git_work_tree(path) {
        return Err(format!(
            "replay_set_in_repo: {}: inside the git work tree {} — the operator's labels and \
             holdout outcomes are private; keep the set under <TENGU_HOME>/state/{SOE_STATE}/eval/",
            path.display(),
            tree.display()
        ));
    }
    if let Some(mode) = loose_mode(path)? {
        return Err(format!(
            "replay_set_mode: {}: mode {mode:o} lets others read it — chmod 600",
            path.display()
        ));
    }
    Ok(loaded)
}

/// Module table: every `<id>.toml` record under `dir`, sorted by id; `Err`
/// lists every problem, each naming its file.
pub fn load_record_dir<R: SoeRecord + DeserializeOwned>(
    dir: &Path,
) -> Result<Vec<Loaded<R>>, Vec<String>> {
    if !dir.is_dir() {
        return Err(vec![format!("{}: no such directory", dir.display())]);
    }
    let mut errors = Vec::new();
    let mut out = Vec::new();
    for (path, stem) in toml_files(dir, &mut errors) {
        let Some((record, sha256)) = parse::<R>(&path, &stem, |r: &R| r.id(), &mut errors) else {
            continue;
        };
        match validate(&record) {
            Ok(()) => out.push(Loaded {
                record,
                sha256,
                path,
            }),
            Err(problems) => errors.push(listed(&path, &problems)),
        }
    }
    if !errors.is_empty() {
        return Err(errors);
    }
    out.sort_by(|a, b| a.record.id().cmp(b.record.id()));
    Ok(out)
}

/// Most proposals one cycle takes (`[soe] max_proposals`).
pub const MAX_PROPOSALS: usize = 50;
/// Longest forecast horizon in weeks (`[soe] forecast_max_weeks`).
pub const MAX_FORECAST_WEEKS: u32 = 52;

/// `[soe]` (module table: the section).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SoeConfig {
    /// The `[agents.<name>]` that runs the Architect stage (`soe_propose`).
    pub architect: String,
    /// The `[agents.<name>]` that runs the Critic stage (`soe_challenge`).
    pub critic: String,
    /// Proposals one cycle takes (`too_many_proposals` beyond).
    pub max_proposals: usize,
    /// A forecast resolves within this many weeks (`horizon_too_long`).
    pub forecast_max_weeks: u32,
    /// Per-million token prices; `None` ⇒ the cycle's cost is `UNKNOWN`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token_prices: Option<TokenPrices>,
}

impl SoeConfig {
    /// `<state root>/operator.toml` — the profile a cycle decides on.
    pub fn profile_path(state_root: &Path) -> PathBuf {
        state_root.join(PROFILE_FILE)
    }
}

/// Every `[soe]` load rule (module table), with the state root under
/// `<TENGU_HOME>`.
pub(crate) fn validation_errors(cfg: &Config) -> Vec<String> {
    rules_at(cfg, &paths::resolve_tengu_home())
}

fn rules_at(cfg: &Config, tengu_home: &Path) -> Vec<String> {
    let Some(soe) = &cfg.soe else {
        return Vec::new();
    };
    let mut out = Vec::new();
    stage_errors(cfg, soe, &mut out);
    closed_world_errors(cfg, &mut out);
    egress_errors(cfg, &mut out);
    match &cfg.sources {
        None => out.push(
            "soe: needs [sources] — the SOE state root is the [sources] state dir \
             (<TENGU_HOME>/state/<sources.state>/)"
                .into(),
        ),
        Some(s) => state_root_errors(&s.state_dir(tengu_home), &mut out),
    }
    out
}

fn lists(agent: &AgentConfig, tool: &str) -> bool {
    agent
        .tools
        .iter()
        .chain(&agent.workspace_tools)
        .any(|t| t == tool)
}

/// `architect` · `critic` · the limits (module table: the section).
fn stage_errors(cfg: &Config, soe: &SoeConfig, out: &mut Vec<String>) {
    for (key, name, foreign) in [
        ("architect", soe.architect.trim(), "soe_challenge"),
        ("critic", soe.critic.trim(), "soe_propose"),
    ] {
        if name.is_empty() {
            out.push(format!("soe.{key}: empty — name an [agents.<name>] block"));
            continue;
        }
        match cfg.agents.get(name) {
            None => out.push(format!("soe.{key}: no [agents.{name}] block")),
            Some(a)
                if !a
                    .description
                    .as_deref()
                    .is_some_and(|d| !d.trim().is_empty()) =>
            {
                out.push(format!(
                    "soe.{key}: [agents.{name}] has no description — a stage runs as a \
                     `run-agent` step, which refuses an agent without one"
                ))
            }
            Some(a) if lists(a, foreign) => out.push(format!(
                "soe.{key}: [agents.{name}] lists `{foreign}` — the Architect never \
                 challenges and the Critic never proposes"
            )),
            Some(_) => {}
        }
    }
    if !soe.architect.trim().is_empty() && soe.architect.trim() == soe.critic.trim() {
        out.push(format!(
            "soe.critic: `{}` is also soe.architect — the Critic must not judge its own proposals",
            soe.critic.trim()
        ));
    }
    if !(1..=MAX_PROPOSALS).contains(&soe.max_proposals) {
        out.push(format!(
            "soe.max_proposals {} must be 1–{MAX_PROPOSALS}",
            soe.max_proposals
        ));
    }
    if !(1..=MAX_FORECAST_WEEKS).contains(&soe.forecast_max_weeks) {
        out.push(format!(
            "soe.forecast_max_weeks {} must be 1–{MAX_FORECAST_WEEKS}",
            soe.forecast_max_weeks
        ));
    }
    if let Some(p) = &soe.token_prices {
        if p.prompt_per_million < Minor::ZERO || p.completion_per_million < Minor::ZERO {
            out.push("soe.token_prices: a price per million tokens is never negative".into());
        }
    }
}

/// Tools, scopes, loops, feeds, sections (module table).
fn closed_world_errors(cfg: &Config, out: &mut Vec<String>) {
    let allowed = SOE_ALLOWED.join(", ");
    let mut agents: Vec<_> = cfg.agents.iter().collect();
    agents.sort_by(|a, b| a.0.cmp(b.0));
    for (id, a) in agents {
        if a.tools.is_empty() {
            out.push(format!(
                "agents.{id}.tools: empty = every base tool — an SOE agent lists its tools \
                 (from {allowed})"
            ));
        }
        for (key, names) in [("tools", &a.tools), ("workspace_tools", &a.workspace_tools)] {
            // Always on and never listed; harmless when it is.
            for t in names
                .iter()
                .filter(|t| *t != "compress_and_store" && !SOE_ALLOWED.contains(&t.as_str()))
            {
                out.push(format!(
                    "agents.{id}.{key}: `{t}` is outside the SOE closed world ({allowed})"
                ));
            }
        }
        // An agent's own scope replaces the default wholesale.
        for t in SIDE_EFFECT_TOOLS {
            if a.scopes.get(*t).is_some_and(|s| !s.is_deny_all()) {
                out.push(format!(
                    "agents.{id}.scopes.{t}: must stay deny-all (no keys) in an SOE sandbox"
                ));
            }
        }
    }
    for t in SIDE_EFFECT_TOOLS {
        match cfg.default_scopes.get(*t) {
            None => out.push(format!(
                "default_scopes.{t}: an SOE sandbox needs a deny-all [default_scopes.{t}] \
                 (no keys) — defence in depth behind the tool lists"
            )),
            Some(s) if !s.is_deny_all() => out.push(format!(
                "default_scopes.{t}: must be deny-all (no keys) in an SOE sandbox"
            )),
            Some(_) => {}
        }
    }
    if !cfg.decision_loops.is_empty() {
        out.push(
            "decision_loops: an SOE sandbox runs no decision loop (its cycle is the \
             `soe_cycle` job)"
                .into(),
        );
    }
    for (name, f) in &cfg.feeds {
        if f.kind != "job" {
            out.push(format!(
                "feeds.{name}: kind = \"{}\" — an SOE sandbox runs `kind = \"job\"` feeds only",
                f.kind
            ));
        }
    }
    for (section, set) in [
        ("risk", cfg.risk.is_some()),
        ("paper", cfg.paper.is_some()),
        ("xmarket", cfg.xmarket.is_some()),
        ("backtest", cfg.backtest.is_some()),
        (
            "solana] privy_wallet_id",
            cfg.solana.privy_wallet_id.is_some(),
        ),
        (
            "telegram",
            cfg.telegram.enabled || !cfg.telegram.allowed_users.is_empty(),
        ),
        (
            "webhooks",
            cfg.webhooks.enabled || !cfg.webhooks.endpoints.is_empty(),
        ),
    ] {
        if set {
            out.push(format!(
                "[{section}]: not allowed beside [soe] — an SOE sandbox has no trading, \
                 signing or inbound surface"
            ));
        }
    }
}

/// `[egress] allow_hosts` against the registry (module table).
fn egress_errors(cfg: &Config, out: &mut Vec<String>) {
    let listed: BTreeSet<&str> = cfg
        .sources
        .iter()
        .flat_map(|s| s.registry.values())
        .flat_map(|e| e.hosts.iter().map(String::as_str))
        .collect();
    for h in &cfg.egress.allow_hosts {
        if !listed.contains(h.as_str()) {
            out.push(format!(
                "egress.allow_hosts: `{h}` is no host of a [sources.registry.*] row — an SOE \
                 sandbox reaches its listed sources only"
            ));
        }
    }
    if cfg.egress.network.trim() == NETWORK_OPEN && cfg.egress.allow_hosts.is_empty() {
        out.push(
            "egress.allow_hosts: empty under network = \"open\" = every host — set it to the \
             hosts of the [sources.registry.*] rows"
                .into(),
        );
    }
}

/// The private state root (module table).
fn state_root_errors(root: &Path, out: &mut Vec<String>) {
    if let Some(tree) = git_work_tree(root) {
        out.push(format!(
            "soe: the state root {} is inside the git work tree {} — the operator profile, \
             cycles and logs are private; keep [sources] state under a <TENGU_HOME> outside \
             any repo",
            root.display(),
            tree.display()
        ));
    }
    let profile = SoeConfig::profile_path(root);
    if profile.is_file() {
        match loose_mode(&profile) {
            Ok(Some(mode)) => out.push(format!(
                "soe: profile_mode: {}: mode {mode:o} lets others read it — chmod 600",
                profile.display()
            )),
            Ok(None) => {}
            Err(e) => out.push(format!("soe: {e}")),
        }
    }
}

#[cfg(test)]
#[path = "soe_tests.rs"]
mod tests;
