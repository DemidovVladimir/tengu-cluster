//! Tests of `src/config/soe.rs`.

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
        .map(|c| run_case(&c.record, &profile).unwrap_or_else(|e| panic!("{}: {e}", c.record.id)))
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

/// The shipped `soe` sandbox (`[soe]`, the two stage agents, deny-all
/// side-effect scopes, the weekly job, bound to `SOE-G0`) loads like any
/// sandbox, from its repo path: every load rule passes — the `[soe]`
/// rules, the generation binding — and the section and the generation
/// reach every agent; the stage agents hold only their stage's tools.
#[test]
fn soe_sandbox_loads() {
    let repo = Path::new(env!("CARGO_MANIFEST_DIR"));
    let file = repo.join("sandboxes/soe/config.toml");
    let cfg = Config::load(&file).unwrap_or_else(|e| panic!("{e:#}"));
    let s = cfg.soe.as_ref().unwrap();
    assert_eq!(
        (s.architect.as_str(), s.critic.as_str()),
        ("soe_architect", "soe_critic")
    );
    assert!(cfg
        .agents
        .values()
        .all(|a| a.sandbox.soe.as_deref() == Some(s)
            && a.sandbox.generation.as_ref().map(|g| g.id.as_str()) == Some("SOE-G0")));
    assert_eq!(
        cfg.agents["soe_architect"].tools,
        ["soe_view", "soe_propose", "skill_resource"]
    );
    assert_eq!(
        cfg.agents["soe_critic"].tools,
        ["soe_view", "soe_challenge"]
    );
    assert_eq!(cfg.feeds["soe_week"].job.as_deref(), Some("soe_cycle"));
    if let Some(h) = home() {
        assert_eq!(rules_at(&cfg, h.path()), Vec::<String>::new());
    }
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
    let text =
        format!("{SOE}\n[[mcp_servers]]\nname = \"x\"\ntransport = \"stdio\"\ncommand = [\"x\"]\n");
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
    cfg.solana.privy_wallet_id = Some("w1".into());
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
        "[solana] privy_wallet_id]: not allowed beside [soe]",
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
        [
            "egress.allow_hosts: `evil.example.com` is no host of a [sources.registry.*] row — \
              an SOE sandbox reaches its listed sources only"
        ]
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
            "        [agents.soe_architect.claude_code]\n        builtin_tools_profile = \"editor_shell\"\n",
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
