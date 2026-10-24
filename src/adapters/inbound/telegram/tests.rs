//! Tests of `adapters/inbound/telegram/`.

use super::*;

fn sorted<'a>(keys: impl Iterator<Item = &'a String>) -> Vec<&'a str> {
    let mut keys: Vec<&str> = keys.map(String::as_str).collect();
    keys.sort_unstable();
    keys
}

/// `/stop` mid-turn: the flag set from another task ends the race, and
/// the turn future (here one that never finishes) is dropped.
#[tokio::test]
async fn stop_flag_ends_a_running_turn() {
    let flag = Arc::new(AtomicBool::new(false));
    let setter = Arc::clone(&flag);
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        setter.store(true, Ordering::Relaxed);
    });
    let out = tokio::time::timeout(std::time::Duration::from_secs(2), async {
        tokio::select! {
            () = std::future::pending::<()>() => "finished",
            () = cancel_requested(&flag) => "stopped",
        }
    })
    .await
    .expect("the stop flag ends the turn");
    assert_eq!(out, "stopped");
}

#[test]
fn allow_list_merges_config_and_env_and_drops_blanks() {
    let set = allowed_users(&["848344935".into(), " ".into()], Some(" 1, ,2 "));
    assert_eq!(sorted(set.iter()), ["1", "2", "848344935"]);
    assert!(allowed_users(&[], None).is_empty());
    assert!(allowed_users(&["".into()], Some(" , ")).is_empty());
}

/// W1-gate review: an empty allow-list (config + env) refuses to start
/// and answers no one; a listed sender is answered, anyone else is not.
#[test]
fn empty_allow_list_fails_closed() {
    let none = HashSet::new();
    let err = require_allowed_users(&none).unwrap_err().to_string();
    assert!(
        err.contains("no allowed users — refusing to start")
            && err.contains("TENGU_TELEGRAM_ALLOWED_USERS"),
        "{err}"
    );
    assert!(!is_authorized(&none, "848344935"));
    let one = allowed_users(&["848344935".into()], None);
    require_allowed_users(&one).unwrap();
    assert!(is_authorized(&one, "848344935"));
    assert!(!is_authorized(&one, "1"));
}

fn agents(toml: &str) -> HashMap<String, crate::config::AgentConfig> {
    toml::from_str::<Config>(toml).expect("config").agents
}

fn view(
    agents: &HashMap<String, crate::config::AgentConfig>,
) -> impl Iterator<Item = (&str, &crate::config::AgentConfig)> {
    agents.iter().map(|(id, a)| (id.as_str(), a))
}

/// Private agents (no `description`, not `default` — the exec-tool
/// owners) are never `@`-routable from Telegram nor its default; the
/// default and the described agents are.
#[test]
fn private_agents_are_never_routable_or_default() {
    let xm = agents(
        r#"
            [agents.xm]
            default = true
            engine = "openrouter"
            model = "m"
            [agents.xm_architect]
            engine = "openrouter"
            model = "m"
            role = "Architect"
            description = "research"
            [agents.xm_executor]
            engine = "openrouter"
            model = "m"
            role = "executor"
            "#,
    );
    let routes = telegram_routes(view(&xm));
    assert_eq!(sorted(routes.keys()), ["architect", "xm", "xm_architect"]);
    assert_eq!(routes["architect"], "xm_architect");
    assert_eq!(telegram_default_agent(view(&xm)).as_deref(), Some("xm"));

    // No `default`: the first described agent, never a private one
    // (even one that sorts first).
    let no_default = agents(
        r#"
            [agents.a_exec]
            engine = "openrouter"
            model = "m"
            [agents.b_research]
            engine = "openrouter"
            model = "m"
            description = "research"
            "#,
    );
    assert_eq!(
        telegram_default_agent(view(&no_default)).as_deref(),
        Some("b_research")
    );
    assert_eq!(
        sorted(telegram_routes(view(&no_default)).keys()),
        ["b_research"]
    );

    // Only private agents (sandboxes/xmarket-weekend): nothing to reach.
    let private = agents("[agents.xm_weekend]\nengine = \"openrouter\"\nmodel = \"m\"\n");
    assert_eq!(telegram_default_agent(view(&private)), None);
    assert!(telegram_routes(view(&private)).is_empty());

    // Ids with `-` or capitals: reached by what the user types, which
    // `parse_agent_routing` lowercases and turns `-` into `_`.
    let dashed =
        agents("[agents.Lp-exec]\nengine = \"openrouter\"\nmodel = \"m\"\ndescription = \"lp\"\n");
    let routes = telegram_routes(view(&dashed));
    let (role, msg) = crate::adapters::inbound::channel::parse_agent_routing(
        "@Lp-exec: open 0.5 SOL",
        Some(&routes),
    );
    assert_eq!(routes[role.as_deref().unwrap()], "Lp-exec");
    assert_eq!(msg, "open 0.5 SOL");
    let (implicit, _) =
        crate::adapters::inbound::channel::parse_agent_routing("lp-exec: hi", Some(&routes));
    assert_eq!(implicit.as_deref(), Some("lp_exec"));
}

/// Two senders never share a planner session: an override gets the sender
/// appended, no override gives each sender its own UUID.
#[test]
fn each_sender_gets_its_own_session_id() {
    assert_eq!(session_id_for_sender(Some("ops".into()), "111"), "ops-111");
    assert_ne!(
        session_id_for_sender(Some("ops".into()), "111"),
        session_id_for_sender(Some("ops".into()), "222")
    );
    let (a, b) = (
        session_id_for_sender(None, "111"),
        session_id_for_sender(Some("  ".into()), "222"),
    );
    assert_ne!(a, b);
    assert_eq!(a.len(), 36, "a UUID: {a}");
}
