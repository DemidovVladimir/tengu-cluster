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
//! | digest | `lineage::pins::toml_digest` of the file text (the profile's is `profile_sha256`) |
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
//! | no `[risk]`, `[paper]`, `[xmarket]`, `[backtest]`, `[solana] signer_key_file`, `[telegram]` (enabled or users), `[webhooks]` (enabled or endpoints) | no trading, signing or inbound surface |
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
use crate::domain::lineage::pins::toml_digest;
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
            "solana] signer_key_file",
            cfg.solana.signer_key_file.is_some(),
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
mod tests {
    use std::collections::BTreeSet;

    use super::*;
    use crate::domain::soe::eval::{run_case, CaseClass, EvalCase};
    use crate::domain::soe::opportunity::tests::RECURRING;
    use crate::domain::soe::profile::tests::SYNTHETIC;

    fn write(path: &Path, text: &str, mode: u32) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
        }
        #[cfg(not(unix))]
        let _ = mode;
    }

    fn real() -> String {
        SYNTHETIC.replace("synthetic = true", "synthetic = false")
    }

    fn code(r: Result<Loaded<OperatorProfile>, String>) -> String {
        r.unwrap_err().split(':').next().unwrap().to_string()
    }

    #[test]
    fn missing_profile_is_an_error_not_a_default() {
        let tmp = tempfile::TempDir::new().unwrap();
        let path = tmp.path().join("state/soe/operator.toml");
        let e = load_profile(&path, true).unwrap_err();
        assert!(e.starts_with("operator_profile_missing: "), "{e}");
        assert!(e.contains(&path.display().to_string()), "{e}");
        assert!(default_profile_path().ends_with("state/soe/operator.toml"));
        // A broken one lists its problems, never falls back.
        write(
            &path,
            &SYNTHETIC.replace("version = 1", "version = 0"),
            0o600,
        );
        let e = load_profile(&path, true).unwrap_err();
        assert!(e.starts_with("invalid_profile:\n"), "{e}");
        assert!(e.contains("invalid_version: version: must be ≥ 1"), "{e}");
    }

    #[test]
    fn synthetic_profile_refused_without_flag() {
        let tmp = tempfile::TempDir::new().unwrap();
        let path = tmp.path().join("fixtures/operator.toml");
        write(&path, SYNTHETIC, 0o644);
        assert_eq!(
            code(load_profile(&path, false)),
            "synthetic_profile_refused"
        );
        let loaded = load_profile(&path, true).unwrap();
        assert!(loaded.record.synthetic);
        assert_eq!(loaded.sha256, toml_digest(SYNTHETIC).unwrap());
        assert_eq!(loaded.sha256.len(), 64);
        // Unsigned is refused even for a fixture.
        let unsigned = SYNTHETIC
            .replace("signed_by = \"fixture\"", "signed_by = \"UNSIGNED\"")
            .replace(
                "signed_at = \"2026-10-01T09:00:00Z\"",
                "signed_at = \"UNKNOWN\"",
            );
        write(&path, &unsigned, 0o644);
        assert_eq!(code(load_profile(&path, true)), "operator_profile_unsigned");
    }

    #[test]
    fn real_profile_inside_git_repo_refused() {
        let tmp = tempfile::TempDir::new().unwrap();
        // A repo (`.git` dir) and a linked worktree (`.git` file).
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(repo.join(".git")).unwrap();
        let in_repo = repo.join("private/operator.toml");
        write(&in_repo, &real(), 0o600);
        let e = load_profile(&in_repo, true).unwrap_err();
        assert!(e.starts_with("profile_in_repo: "), "{e}");
        let worktree = tmp.path().join("wt");
        write(&worktree.join(".git"), "gitdir: elsewhere\n", 0o644);
        let in_wt = worktree.join("operator.toml");
        write(&in_wt, &real(), 0o600);
        assert_eq!(code(load_profile(&in_wt, false)), "profile_in_repo");

        // Outside any work tree: 0600 loads, a group / other bit is refused.
        if git_work_tree(tmp.path()).is_some() {
            return; // the temp dir itself sits in a repo here
        }
        let home = tmp.path().join("home/state/soe/operator.toml");
        write(&home, &real(), 0o600);
        let loaded = load_profile(&home, false).unwrap();
        assert!(!loaded.record.synthetic && loaded.record.is_signed());
        #[cfg(unix)]
        {
            write(&home, &real(), 0o640);
            let e = load_profile(&home, false).unwrap_err();
            assert!(e.starts_with("profile_mode: ") && e.contains("640"), "{e}");
        }
    }

    /// A replay set: synthetic only when allowed; the operator's labels
    /// never inside a git work tree or open to other users.
    #[test]
    fn replay_set_privacy() {
        use crate::domain::soe::replay::tests::SET;
        let tmp = tempfile::TempDir::new().unwrap();
        let synthetic = tmp.path().join("set.toml");
        write(&synthetic, SET, 0o644);
        let e = load_replay_set(&synthetic, false).unwrap_err();
        assert!(e.starts_with("synthetic_replay_set_refused: "), "{e}");
        assert_eq!(
            load_replay_set(&synthetic, true)
                .unwrap()
                .record
                .cases
                .len(),
            3
        );
        let labels = SET.replace("synthetic = true", "synthetic = false");
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(repo.join(".git")).unwrap();
        let in_repo = repo.join("eval/set.toml");
        write(&in_repo, &labels, 0o600);
        let e = load_replay_set(&in_repo, false).unwrap_err();
        assert!(e.starts_with("replay_set_in_repo: "), "{e}");
        let broken = tmp.path().join("broken.toml");
        write(
            &broken,
            &SET.replace("label = \"HOLD\"", "label = \"MAYBE\""),
            0o600,
        );
        let e = load_replay_set(&broken, true).unwrap_err();
        assert!(e.starts_with("invalid_replay_set:"), "{e}");
        if git_work_tree(tmp.path()).is_some() {
            return; // the temp dir itself sits in a repo here
        }
        let home = tmp.path().join("home/state/soe/eval/set.toml");
        write(&home, &labels, 0o600);
        assert!(!load_replay_set(&home, false).unwrap().record.synthetic);
        #[cfg(unix)]
        {
            write(&home, &labels, 0o640);
            let e = load_replay_set(&home, false).unwrap_err();
            assert!(e.starts_with("replay_set_mode: "), "{e}");
        }
    }

    #[test]
    fn case_dir_file_stem_must_equal_id() {
        let tmp = tempfile::TempDir::new().unwrap();
        let dir = tmp.path().join("cases");
        let with_id =
            |id: &str| RECURRING.replace("id = \"example-automation\"", &format!("id = \"{id}\""));
        write(&dir.join("b.toml"), &with_id("b"), 0o644);
        write(&dir.join("a-x.toml"), &with_id("a-x"), 0o644);
        write(&dir.join("a.toml"), &with_id("a"), 0o644);
        write(&dir.join("README.md"), "# cases\n", 0o644);
        let ids: Vec<String> = load_record_dir::<Opportunity>(&dir)
            .unwrap()
            .into_iter()
            .map(|l| l.record.id)
            .collect();
        assert_eq!(ids, ["a", "a-x", "b"]);

        // A stem that is not the id, a stray file, a sub-dir, a bad record.
        write(&dir.join("other.toml"), &with_id("c"), 0o644);
        write(&dir.join("notes.txt"), "x", 0o644);
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        let fake = with_id("d").replace(
            "mechanism = \"AUTOMATE\"",
            "mechanism = \"HIGH_TICKET_DELIVERY\"",
        );
        write(&dir.join("d.toml"), &fake, 0o644);
        let errors = load_record_dir::<Opportunity>(&dir).unwrap_err();
        assert_eq!(errors.len(), 4, "{errors:#?}");
        let all = errors.join("\n");
        assert!(
            all.contains("id `c` differs from the file stem `other`"),
            "{all}"
        );
        assert!(all.contains("notes.txt: not a record"), "{all}");
        assert!(all.contains("sub: not a record"), "{all}");
        assert!(
            all.contains("d.toml: fake_recurring: economics.revenue.kind:"),
            "{all}"
        );
        assert!(load_record_dir::<Opportunity>(&tmp.path().join("none")).is_err());

        // One opportunity file loads by any name.
        let one = load_opportunity(&dir.join("other.toml")).unwrap();
        assert_eq!((one.record.id.as_str(), one.sha256.len()), ("c", 64));
        let e = load_opportunity(&dir.join("d.toml")).unwrap_err();
        assert!(e.contains("fake_recurring"), "{e}");
    }

    #[test]
    fn cited_file_lists_views_and_refuses_repeats() {
        let tmp = tempfile::TempDir::new().unwrap();
        let path = tmp.path().join("cited.toml");
        let row = |id: &str| {
            format!(
                "[[cited]]\nrecord_id = \"{id}\"\nevent_key = \"synthetic:e\"\nkind = \"FACT\"\nknowable_at = \"2026-09-10\"\n"
            )
        };
        write(
            &path,
            &format!("{}{}", row("synthetic:a"), row("synthetic:b")),
            0o644,
        );
        let views = load_cited(&path).unwrap();
        assert_eq!(views.len(), 2);
        assert_eq!(views[1].record_id, "synthetic:b");
        write(&path, "", 0o644);
        assert!(load_cited(&path).unwrap().is_empty());
        write(
            &path,
            &format!("{}{}", row("synthetic:a"), row("synthetic:a")),
            0o644,
        );
        let e = load_cited(&path).unwrap_err();
        assert!(e.contains("duplicate: cited.record_id"), "{e}");
        write(&path, &format!("{}extra = 1\n", row("synthetic:a")), 0o644);
        let e = load_cited(&path).unwrap_err();
        assert!(e.contains("invalid_record") && e.contains("extra"), "{e}");
    }

    /// The O0 eval set: every case under `tests/fixtures/soe/cases/` loads and
    /// answers as expected under the synthetic profile.
    #[test]
    fn fixture_eval_cases_pass() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/soe");
        let profile = load_profile(&root.join("profile.synthetic.toml"), true)
            .unwrap()
            .record;
        let cases = load_record_dir::<EvalCase>(&root.join("cases"))
            .unwrap_or_else(|e| panic!("{}", e.join("\n")));
        assert!(cases.len() >= 14, "{} cases", cases.len());
        let classes: BTreeSet<CaseClass> = cases.iter().map(|c| c.record.class).collect();
        assert_eq!(classes, CaseClass::ALL.into_iter().collect());
        assert!(cases.iter().all(|c| c.record.synthetic));
        let failed: Vec<String> = cases
            .iter()
            .map(|c| {
                run_case(&c.record, &profile).unwrap_or_else(|e| panic!("{}: {e}", c.record.id))
            })
            .filter(|r| !r.ok)
            .map(|r| format!("{}: {:#?}\n{:#?}", r.id, r.diffs, r.answer))
            .collect();
        assert!(failed.is_empty(), "{}", failed.join("\n"));
    }

    // --- `[soe]` section ----------------------------------------------------

    /// An SOE sandbox that passes every `[soe]` rule (open network, the two
    /// registry rows, two stage agents, deny-all side-effect scopes, the
    /// weekly job).
    const SOE: &str = r#"
        [egress]
        network = "open"
        allow_hosts = ["www.sec.gov", "data.sec.gov", "api.ted.europa.eu"]

        [rate_limits.sec]
        per_minute = 300
        [rate_limits.ted]
        per_minute = 60

        [sources]
        state = "soe-test"

        [sources.registry.sec_edgar]
        kind = "sec_edgar"
        class = "company_primary"
        trust = "primary"
        revision = "immutable"
        enabled = false
        hosts = ["www.sec.gov", "data.sec.gov"]
        auth = "user_agent_env:SEC_USER_AGENT"
        rate_limit = "sec"
        store_raw = true
        jurisdiction = "US"
        language = "en"

        [sources.registry.ted_search]
        kind = "ted_search"
        class = "law_regulator"
        trust = "primary"
        revision = "immutable"
        enabled = false
        hosts = ["api.ted.europa.eu"]
        auth = "none"
        rate_limit = "ted"
        store_raw = true
        jurisdiction = "EU"
        language = "en"
        query = "publication-date >= {from} AND publication-date <= {to}"

        [soe]
        architect = "soe_architect"
        critic = "soe_critic"
        max_proposals = 12
        forecast_max_weeks = 12

        [agents.soe_architect]
        engine = "claude_code"
        model = "claude-opus-5-5"
        description = "Proposes mechanisms as data"
        tools = ["soe_view", "soe_propose", "source_evidence"]
        [agents.soe_architect.claude_code]
        builtin_tools_profile = "none"

        [agents.soe_critic]
        engine = "claude_code"
        model = "claude-sonnet-5-5"
        description = "Challenges the week's proposals"
        tools = ["soe_view", "soe_challenge", "source_evidence"]
        [agents.soe_critic.claude_code]
        builtin_tools_profile = "none"

        [default_scopes.http_request]
        [default_scopes.write_file]
        [default_scopes.run_command]
        [default_scopes.sign_and_send_transaction]
        [default_scopes.sign_message]

        [feeds.soe_week]
        kind = "job"
        job = "soe_cycle"
        tz = "Europe/Paris"
        at = ["Mon 07:00"]
    "#;

    fn soe(text: &str) -> Config {
        toml::from_str(text).unwrap_or_else(|e| panic!("{e}\n{text}"))
    }

    /// A home outside any git work tree (`None` when the temp dir is in one).
    fn home() -> Option<tempfile::TempDir> {
        let tmp = tempfile::TempDir::new().unwrap();
        git_work_tree(tmp.path()).is_none().then_some(tmp)
    }

    fn has(errors: &[String], want: &str) -> bool {
        errors.iter().any(|e| e.contains(want))
    }

    /// The `soe` sandbox with the cycle added (`[soe]`, two stage agents,
    /// deny-all side-effect scopes, the weekly job) loads like any sandbox:
    /// every load rule passes and the section reaches every agent.
    #[test]
    fn soe_sandbox_loads() {
        let repo = Path::new(env!("CARGO_MANIFEST_DIR"));
        let base = std::fs::read_to_string(repo.join("sandboxes/soe/config.toml")).unwrap();
        let overlay = SOE
            .split("[soe]")
            .nth(1)
            .map(|rest| format!("[soe]{rest}"))
            .unwrap();
        let tmp = tempfile::TempDir::new().unwrap();
        let file = tmp.path().join("config.toml");
        std::fs::write(&file, format!("{base}\n{overlay}")).unwrap();
        let cfg = Config::load(&file).unwrap_or_else(|e| panic!("{e:#}"));
        let s = cfg.soe.as_ref().unwrap();
        assert_eq!(
            (s.architect.as_str(), s.critic.as_str()),
            ("soe_architect", "soe_critic")
        );
        assert!(cfg
            .agents
            .values()
            .all(|a| a.sandbox.soe.as_deref() == Some(s)));
        if let Some(h) = home() {
            assert_eq!(rules_at(&cfg, h.path()), Vec::<String>::new());
        }
        // The shipped sandbox (no [soe] yet) has no SOE rule to pass.
        let shipped: Config = toml::from_str(&base).unwrap();
        assert!(shipped.soe.is_none() && validation_errors(&shipped).is_empty());
    }

    /// An empty `tools` list is every base tool: refused for every agent of
    /// an SOE sandbox. The stage agents' own rules too.
    #[test]
    fn empty_tools_refused() {
        let Some(h) = home() else { return };
        assert_eq!(rules_at(&soe(SOE), h.path()), Vec::<String>::new());
        let mut cfg = soe(SOE);
        cfg.agents.get_mut("soe_critic").unwrap().tools.clear();
        assert_eq!(
            rules_at(&cfg, h.path()),
            [
                "agents.soe_critic.tools: empty = every base tool — an SOE agent lists its tools \
              (from soe_view, soe_propose, soe_challenge, source_evidence, read_file, \
              list_directory, view_skill, skill_resource)"
            ]
        );
        // Stage agents: named, with a description, distinct, each in its role.
        let mut cfg = soe(SOE);
        cfg.soe.as_mut().unwrap().critic = "soe_architect".into();
        cfg.agents.get_mut("soe_architect").unwrap().description = None;
        cfg.soe.as_mut().unwrap().max_proposals = 0;
        let errs = rules_at(&cfg, h.path());
        for want in [
            "soe.architect: [agents.soe_architect] has no description",
            "soe.critic: `soe_architect` is also soe.architect",
            "soe.max_proposals 0 must be 1–50",
        ] {
            assert!(has(&errs, want), "{want}: {errs:#?}");
        }
    }

    /// Closed world: a tool outside `SOE_ALLOWED` (a write, contact, spend,
    /// publish or shell tool) is refused in `tools` and `workspace_tools`;
    /// the Architect never challenges, the Critic never proposes.
    #[test]
    fn side_effect_tool_refused() {
        let Some(h) = home() else { return };
        let mut cfg = soe(SOE);
        let a = cfg.agents.get_mut("soe_architect").unwrap();
        a.tools.push("http_request".into());
        a.tools.push("soe_challenge".into());
        a.workspace_tools.push("persistent_store".into());
        cfg.agents
            .get_mut("soe_critic")
            .unwrap()
            .tools
            .push("run_command".into());
        let errs = rules_at(&cfg, h.path());
        for want in [
            "agents.soe_architect.tools: `http_request` is outside the SOE closed world",
            "agents.soe_architect.workspace_tools: `persistent_store` is outside the SOE closed world",
            "agents.soe_critic.tools: `run_command` is outside the SOE closed world",
            "soe.architect: [agents.soe_architect] lists `soe_challenge`",
        ] {
            assert!(has(&errs, want), "{want}: {errs:#?}");
        }
        assert_eq!(errs.len(), 4, "{errs:#?}");
        // `compress_and_store` is always on: listing it is harmless.
        let mut cfg = soe(SOE);
        cfg.agents
            .get_mut("soe_critic")
            .unwrap()
            .tools
            .push("compress_and_store".into());
        assert!(rules_at(&cfg, h.path()).is_empty());
    }

    /// Every side-effect tool needs a deny-all default scope; an agent's own
    /// scope for one (it replaces the default wholesale) stays deny-all.
    #[test]
    fn http_request_scope_must_be_deny_all() {
        let Some(h) = home() else { return };
        let granted = SOE.replace(
            "[default_scopes.http_request]\n",
            "[default_scopes.http_request]\n        net_hosts = [\"www.sec.gov\"]\n",
        );
        assert_eq!(
            rules_at(&soe(&granted), h.path()),
            ["default_scopes.http_request: must be deny-all (no keys) in an SOE sandbox"]
        );
        let missing = SOE.replace("        [default_scopes.sign_message]\n", "");
        assert!(has(
            &rules_at(&soe(&missing), h.path()),
            "default_scopes.sign_message: an SOE sandbox needs a deny-all [default_scopes.sign_message]"
        ));
        let own = format!(
            "{SOE}\n[agents.soe_critic.scopes.http_request]\nnet_hosts = [\"api.ted.europa.eu\"]\n"
        );
        assert_eq!(
            rules_at(&soe(&own), h.path()),
            ["agents.soe_critic.scopes.http_request: must stay deny-all (no keys) in an SOE sandbox"]
        );
    }

    /// `[soe]` hardens the sandbox: no `[[mcp_servers]]` (foreign processes).
    #[test]
    fn mcp_servers_refused() {
        let text = format!(
            "{SOE}\n[[mcp_servers]]\nname = \"x\"\ntransport = \"stdio\"\ncommand = [\"x\"]\n"
        );
        let errs = soe(&text).validation_errors();
        assert!(
            has(
                &errs,
                "[[mcp_servers]] are not allowed in a hardened sandbox (Solana signer, [risk] or [soe])"
            ),
            "{errs:#?}"
        );
    }

    /// No trading, signing or inbound surface beside `[soe]`; no decision
    /// loop; feeds of `kind = "job"` only.
    #[test]
    fn trading_sections_refused() {
        let Some(h) = home() else { return };
        let mut cfg = soe(SOE);
        cfg.xmarket = Some(toml::from_str("state = \"xm\"").unwrap());
        cfg.solana.signer_key_file = Some(h.path().join("key.json").display().to_string());
        cfg.telegram.enabled = true;
        cfg.webhooks.enabled = true;
        let mut tick = soe(
            "[feeds.t]\nkind = \"tick\"\ntarget = \"l\"\nevery_secs = 60\n[decision_loops.l]\ngoal = \"g\"\nagent = \"soe_architect\"\n[decision_loops.l.actions.hold]\ndescription = \"x\"\n",
        );
        cfg.feeds.append(&mut tick.feeds);
        cfg.decision_loops = std::mem::take(&mut tick.decision_loops);
        let errs = rules_at(&cfg, h.path());
        for want in [
            "[xmarket]: not allowed beside [soe]",
            "[solana] signer_key_file]: not allowed beside [soe]",
            "[telegram]: not allowed beside [soe]",
            "[webhooks]: not allowed beside [soe]",
            "decision_loops: an SOE sandbox runs no decision loop",
            "feeds.t: kind = \"tick\" — an SOE sandbox runs `kind = \"job\"` feeds only",
        ] {
            assert!(has(&errs, want), "{want}: {errs:#?}");
        }
        // And `[soe]` needs `[sources]`: its state root.
        let mut cfg = soe(SOE);
        cfg.sources = None;
        assert!(has(&rules_at(&cfg, h.path()), "soe: needs [sources]"));
    }

    /// The SOE state root (`operator.toml`, cycles, logs) never sits in a
    /// git work tree.
    #[test]
    fn profile_in_repo_refused() {
        let tmp = tempfile::TempDir::new().unwrap();
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(repo.join(".git")).unwrap();
        let errs = rules_at(&soe(SOE), &repo.join("home"));
        assert_eq!(errs.len(), 1, "{errs:#?}");
        assert!(
            errs[0].starts_with("soe: the state root ")
                && errs[0].contains(&format!("inside the git work tree {}", repo.display())),
            "{errs:#?}"
        );
    }

    /// An `operator.toml` in the state root with a group / other bit fails
    /// the load (stricter than a warning, critic C10); 0600 passes.
    #[cfg(unix)]
    #[test]
    fn profile_mode_not_0600_refused() {
        let Some(h) = home() else { return };
        let profile = h.path().join("state/soe-test").join(PROFILE_FILE);
        write(&profile, &real(), 0o644);
        let errs = rules_at(&soe(SOE), h.path());
        assert_eq!(errs.len(), 1, "{errs:#?}");
        assert!(
            errs[0].starts_with("soe: profile_mode: ") && errs[0].contains("mode 644"),
            "{errs:#?}"
        );
        write(&profile, &real(), 0o600);
        assert!(rules_at(&soe(SOE), h.path()).is_empty());
    }

    /// The state root sits under `<TENGU_HOME>/state`, which a hardened
    /// sandbox keeps out of every fs root and workspace.
    #[test]
    fn state_dir_in_fs_root_refused() {
        let home = crate::config::paths::resolve_tengu_home();
        let text = format!(
            "{SOE}\n[default_scopes.source_evidence]\nfs_roots = [\"{}\"]\n",
            home.display()
        );
        let errs = soe(&text).validation_errors();
        assert!(
            errs.iter()
                .any(|e| e.contains("default_scopes.source_evidence.fs_roots")
                    && e.contains("hardened sandbox: Solana signer, [risk] or [soe]")),
            "{errs:#?}"
        );
    }

    /// `[egress] allow_hosts` equals the registry's hosts: one outside it is
    /// refused here, a row host missing from it by `[sources]`; an open
    /// network with no list (= every host) too.
    #[test]
    fn egress_host_outside_sources_refused() {
        let Some(h) = home() else { return };
        let wider = SOE.replace(
            "\"api.ted.europa.eu\"]\n\n        [rate_limits.sec]",
            "\"api.ted.europa.eu\", \"evil.example.com\"]\n\n        [rate_limits.sec]",
        );
        assert_eq!(
            rules_at(&soe(&wider), h.path()),
            ["egress.allow_hosts: `evil.example.com` is no host of a [sources.registry.*] row — \
              an SOE sandbox reaches its listed sources only"]
        );
        let narrower = SOE.replace(
            "allow_hosts = [\"www.sec.gov\", \"data.sec.gov\", \"api.ted.europa.eu\"]",
            "allow_hosts = [\"www.sec.gov\", \"data.sec.gov\"]",
        );
        let errs = soe(&narrower).validation_errors();
        assert!(
            errs.iter()
                .any(|e| e.starts_with("sources.registry.ted_search.")
                    && e.contains("api.ted.europa.eu")),
            "{errs:#?}"
        );
        let mut open = soe(SOE);
        open.egress.allow_hosts.clear();
        assert!(has(
            &rules_at(&open, h.path()),
            "egress.allow_hosts: empty under network = \"open\" = every host"
        ));
    }

    /// `[soe]` hardens the sandbox: a `claude_code` agent runs with the
    /// built-in tools off.
    #[test]
    fn claude_code_needs_profile_none() {
        let text = SOE.replacen(
            "        [agents.soe_architect.claude_code]\n        builtin_tools_profile = \"none\"\n",
            "",
            1,
        );
        let errs = soe(&text).validation_errors();
        assert!(
            has(
                &errs,
                "agents.soe_architect: engine = \"claude_code\" in a hardened sandbox (Solana signer, \
                 [risk] or [soe]) needs [agents.soe_architect.claude_code] builtin_tools_profile = \"none\""
            ),
            "{errs:#?}"
        );
    }

    /// The commented `[soe]` block of `config.example.toml`, uncommented
    /// over the SOE sandbox above (without its own `[soe]` and feed), is
    /// valid.
    #[test]
    fn example_block_uncommented_is_valid() {
        let Some(h) = home() else { return };
        let text = include_str!("../../config.example.toml");
        let block: Vec<&str> = text
            .lines()
            .skip_while(|l| *l != "# [soe]")
            .take_while(|l| l.starts_with('#'))
            .map(|l| l.strip_prefix("# ").unwrap_or(l.trim_start_matches('#')))
            .collect();
        assert!(block.len() > 8, "{block:?}");
        let before = SOE.split("[soe]").next().unwrap();
        let agents = SOE
            .split("[agents.soe_architect]")
            .nth(1)
            .and_then(|r| r.split("[feeds.soe_week]").next())
            .unwrap();
        let cfg = soe(&format!(
            "{before}\n[agents.soe_architect]{agents}\n{}",
            block.join("\n")
        ));
        assert_eq!(rules_at(&cfg, h.path()), Vec::<String>::new());
        assert_eq!(
            crate::config::feeds::validation_errors(&cfg),
            Vec::<String>::new()
        );
        let s = cfg.soe.as_ref().unwrap();
        assert!(s.token_prices.is_some());
        assert_eq!(cfg.feeds["soe_week"].job.as_deref(), Some("soe_cycle"));
    }
}
