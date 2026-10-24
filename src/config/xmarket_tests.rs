//! Tests of `src/config/xmarket.rs`.

use super::*;
use crate::config::runtime::{runtime_db, RUNTIME_DB};
use crate::domain::calendar::Session;
use chrono::{NaiveDate, NaiveDateTime};

/// Tracker fixture: the `[xmarket]` block of `config.example.toml`.
const FIXTURE: &str = include_str!("../../tests/fixtures/xmarket/calendars.toml");

#[derive(Deserialize)]
struct Fixture {
    xmarket: XmarketConfig,
}

fn fixture() -> XmarketConfig {
    toml::from_str::<Fixture>(FIXTURE).unwrap().xmarket
}

fn et(s: &str) -> i64 {
    Zone::NewYork.to_utc_ms(NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M").unwrap())
}

fn utc(s: &str) -> i64 {
    NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M")
        .unwrap()
        .and_utc()
        .timestamp_millis()
}

fn day(s: &str) -> NaiveDate {
    parse_date(s).unwrap()
}

#[test]
fn defaults_and_state_dir() {
    let x: XmarketConfig = toml::from_str("").unwrap();
    assert_eq!(x.state, "xmarket");
    assert_eq!(
        x.state_dir(Path::new("/h")),
        PathBuf::from("/h/state/xmarket")
    );
    assert_eq!(
        x.history_dir(Path::new("/h")),
        PathBuf::from("/h/state/xmarket/history")
    );
    assert!(x.validation_errors().is_empty());
    assert!(x.calendars().is_empty());
}

/// One layout: every store under the state dir, names distinct.
#[test]
fn state_layout_paths() {
    let dir = Path::new("/h/state/xmarket-weekend");
    assert_eq!(
        ledger_db(dir),
        PathBuf::from("/h/state/xmarket-weekend/ledger.db")
    );
    assert_eq!(
        runtime_db(dir),
        PathBuf::from("/h/state/xmarket-weekend/runtime.db")
    );
    assert_eq!(
        history_dir(dir),
        PathBuf::from("/h/state/xmarket-weekend/history")
    );
    let names = [
        LEDGER_DB,
        RUNTIME_DB,
        HISTORY_DIR,
        CATALOG_DB,
        EVENTS_DB,
        AUDIT_DB,
        SPEND_DB,
    ];
    assert_eq!(names.iter().collect::<BTreeSet<_>>().len(), names.len());
}

/// `<TENGU_HOME>` of the rule tests.
const HOME: &str = "/srv/tengu-home";

/// Chat agent (no workspace) + the xmarket agents sharing `/srv/xm-ws`:
/// a loop agent, a feed agent holding `hl_ctx` (the feed: [`FEED`]), an
/// exec agent opting into `paper_order`.
const AGENTS: &str = r#"
[agents.chat]
default = true
engine = "openrouter"
model = "m"

[agents.xm_feeds]
engine = "openrouter"
model = "m"
workspace = "/srv/xm-ws"
tools = ["hl_ctx"]

[agents.xm_loop]
engine = "openrouter"
model = "m"
workspace = "/srv/xm-ws"

[agents.xm_exec]
engine = "openrouter"
model = "m"
workspace = "/srv/xm-ws"
workspace_tools = ["paper_order"]

[decision_loops.xm_main]
goal = "g"
agent = "xm_loop"
[decision_loops.xm_main.actions.hold]
description = "Nothing to do"
"#;

const FEED: &str = r#"
[feeds.hl_ctx]
kind = "tool"
agent = "xm_feeds"
tool = "hl_ctx"
every_secs = 60
"#;

const XM: &str = "[xmarket]\nstate = \"xm-test\"\n";

/// [`AGENTS`] with `agents.<agent>`'s workspace line replaced (`None` =
/// removed).
fn ws(agent: &str, to: Option<&str>) -> String {
    let block = format!("[agents.{agent}]\nengine = \"openrouter\"\nmodel = \"m\"\n");
    let old = format!("{block}workspace = \"/srv/xm-ws\"\n");
    assert!(AGENTS.contains(&old), "{agent}");
    let new = match to {
        Some(w) => format!("{block}workspace = \"{w}\"\n"),
        None => block,
    };
    AGENTS.replacen(&old, &new, 1)
}

/// The xmarket rules of a whole sandbox, `<TENGU_HOME>` = [`HOME`].
fn rules(toml: &str) -> Vec<String> {
    let cfg: Config = toml::from_str(toml).unwrap_or_else(|e| panic!("{e}\n{toml}"));
    rules_at(&cfg, Path::new(HOME))
}

/// Every rule, table-driven: each error matches a `want` substring and
/// each `want` an error (no `want` = the sandbox passes).
#[test]
fn load_rules() {
    use crate::config::risk::tests::RISK_100;
    let user_ws = dirs_next::home_dir().unwrap().join("xm-ws");
    let user_ws = user_ws.display().to_string();
    let all_at = |w: &str| {
        AGENTS.replace(
            "workspace = \"/srv/xm-ws\"",
            &format!("workspace = \"{w}\""),
        )
    };
    let chat_ws = AGENTS.replacen(
        "model = \"m\"\n",
        "model = \"m\"\nworkspace = \"/srv/chat-ws\"\n",
        1,
    );
    let cases: Vec<(&str, String, Vec<&str>)> = vec![
            ("shared workspace", format!("{XM}{AGENTS}{FEED}"), vec![]),
            ("[feeds] alone", format!("{AGENTS}{FEED}"), vec![]),
            (
                "neither [feeds], [risk] nor [xmarket]: not checked",
                ws("xm_loop", Some("/srv/other")),
                vec![],
            ),
            (
                "a feed agent without workspace",
                format!("{XM}{}{FEED}", ws("xm_feeds", None)),
                vec!["agents.xm_feeds (feeds.hl_ctx.agent, holds hl_ctx) has no `workspace` — the agents of feeds, decision loops and xmarket tools share one"],
            ),
            (
                "a loop agent elsewhere",
                format!("{XM}{}{FEED}", ws("xm_loop", Some("/srv/other"))),
                vec!["must share one `workspace` (a loop reads `world` rows from its agent's .tengu/observations.db; feeds and exec tools write them there) — found 2: `/srv/other`: agents.xm_loop (decision_loops.xm_main.agent) · `/srv/xm-ws`: agents.xm_exec (holds paper_order), agents.xm_feeds (feeds.hl_ctx.agent, holds hl_ctx)"],
            ),
            (
                "an exec agent on a relative workspace",
                format!("{XM}{}", ws("xm_exec", Some("xm-ws"))),
                vec!["agents.xm_exec.workspace `xm-ws` must be absolute or start with `~/`"],
            ),
            (
                "`~/` is its absolute spelling",
                format!(
                    "{XM}{}{FEED}",
                    all_at(&user_ws).replacen(
                        &format!("workspace = \"{user_ws}\""),
                        "workspace = \"~/xm-ws\"",
                        1
                    )
                ),
                vec![],
            ),
            (
                "[risk] without [xmarket], a chat agent without workspace",
                format!("{RISK_100}{AGENTS}{FEED}"),
                vec![
                    "[risk] needs [xmarket]: the paper ledger lives in <TENGU_HOME>/state/<xmarket.state>/ledger.db",
                    "agents.chat has no `workspace` — in a [risk] sandbox every agent sets one",
                ],
            ),
            (
                "[risk] + [xmarket], every agent a workspace (the chat agent its own)",
                format!("{XM}{RISK_100}{chat_ws}{FEED}"),
                vec![],
            ),
            (
                "the state dir inside the workspace",
                format!("{XM}{}{FEED}", all_at(HOME)),
                vec![
                    "the [xmarket] state dir `/srv/tengu-home/state/xm-test` is inside agents.xm_exec.workspace `/srv/tengu-home` — keep it outside every fs root and workspace (xmarket: no tool may reach the install-wide stores)",
                    "is inside agents.xm_feeds.workspace",
                    "is inside agents.xm_loop.workspace",
                ],
            ),
            (
                "an fs root inside the state dir",
                format!("{XM}{AGENTS}[default_scopes.read_file]\nfs_roots = [\"{HOME}/state/xm-test/notes\"]\n"),
                vec!["the [xmarket] state dir `/srv/tengu-home/state/xm-test` is around default_scopes.read_file.fs_roots `/srv/tengu-home/state/xm-test/notes`"],
            ),
        ];
    for (label, toml, want) in cases {
        let got = rules(&toml);
        for e in &got {
            assert!(
                want.iter().any(|w| e.contains(w)),
                "{label}: unexpected {e}"
            );
        }
        for w in want {
            assert!(
                got.iter().any(|e| e.contains(w)),
                "{label}: want {w}\ngot {got:#?}"
            );
        }
    }
}

/// A hardened (`[risk]`) sandbox leaves the state dir to the broader
/// `<TENGU_HOME>/state` rule, which reports a workspace holding it
/// (`hardening::tests::state_dir_and_kill_switch_stay_outside_every_root`):
/// not reported twice.
#[test]
fn a_hardened_sandbox_leaves_the_state_dir_to_hardening() {
    use crate::config::risk::tests::RISK_100;
    let all_home = AGENTS
        .replace(
            "workspace = \"/srv/xm-ws\"",
            &format!("workspace = \"{HOME}\""),
        )
        .replacen(
            "model = \"m\"\n",
            &format!("model = \"m\"\nworkspace = \"{HOME}\"\n"),
            1,
        );
    let got = rules(&format!("{XM}{RISK_100}{all_home}"));
    assert!(got.is_empty(), "{got:#?}");
}

/// A weekend-fade sandbox: an exchange calendar, `[xmarket.weekend_fade]`,
/// the recorder, the $100 `[risk]`, a private exec agent holding the tool.
fn fade_sandbox() -> String {
    use crate::config::risk::tests::RISK_100;
    format!(
        r#"{XM}
[xmarket.calendars.us]
kind = "exchange"
tz = "America/New_York"
core = ["09:30", "16:00"]

[xmarket.calendars.crypto]
kind = "24x7"

[xmarket.weekend_fade]
calendar = "us"
universe = ["hyperliquid:xyz:TSLA"]
exclude = []
capped_top_n = 1
min_abs_signal_bps = 50
capped_notional_usd = 25
shadow_account = "xmarket-shadow"
shadow_initial_cash_usd = 10000
shadow_notional_usd = 100
expected_edge_bps = 23
anchor_max_age_secs = 600
entry_max_age_secs = 120
entry_lateness_max_secs = 600
max_slippage_bps = 50

[recorder]
enabled = true
schemas = ["mkt_ctx/1"]
{RISK_100}
[agents.main]
default = true
engine = "openrouter"
model = "m"
workspace = "/srv/xm-ws"

[agents.xm_exec]
engine = "openrouter"
model = "m"
workspace = "/srv/xm-ws"
tools = ["xm_weekend_fade"]
"#
    )
}

/// Every `[xmarket.weekend_fade]` rule, table-driven (each error matches
/// a `want`, each `want` an error).
#[test]
fn weekend_fade_rules() {
    let base = fade_sandbox();
    let with = |from: &str, to: &str| {
        assert!(base.contains(from), "{from}");
        base.replacen(from, to, 1)
    };
    let cases: Vec<(&str, String, Vec<&str>)> = vec![
            ("valid", base.clone(), vec![]),
            (
                "a 24x7 calendar",
                with("calendar = \"us\"", "calendar = \"crypto\""),
                vec!["xmarket.weekend_fade: calendar `crypto` must be kind = \"exchange\""],
            ),
            (
                "an unknown calendar",
                with("calendar = \"us\"", "calendar = \"nyse\""),
                vec!["calendar `nyse` is not an [xmarket.calendars.<id>] row"],
            ),
            (
                "a short id",
                with(
                    "universe = [\"hyperliquid:xyz:TSLA\"]",
                    "universe = [\"xyz:TSLA\"]",
                ),
                vec![
                    "universe id `xyz:TSLA` is not a full `hyperliquid:<coin>` id",
                    "every universe name not excluded must be tradable under [risk]",
                ],
            ),
            (
                "a twice-listed id, an exclusion outside the universe",
                with(
                    "universe = [\"hyperliquid:xyz:TSLA\"]\nexclude = []",
                    "universe = [\"hyperliquid:xyz:TSLA\", \"hyperliquid:xyz:TSLA\"]\n\
                     exclude = [\"hyperliquid:xyz:NVDA\"]",
                ),
                vec![
                    "universe lists `hyperliquid:xyz:TSLA` twice",
                    "exclude id `hyperliquid:xyz:NVDA` is not in universe",
                ],
            ),
            (
                "more capped names than tradable ones",
                with("capped_top_n = 1", "capped_top_n = 2"),
                vec!["capped_top_n 2 exceeds the 1 universe names not excluded"],
            ),
            (
                "under the HL minimum",
                with("capped_notional_usd = 25", "capped_notional_usd = 5"),
                vec!["capped_notional_usd must be ≥ 10"],
            ),
            (
                "over the [risk] order cap",
                with("capped_notional_usd = 25", "capped_notional_usd = 30"),
                vec!["capped_notional_usd 30 exceeds [risk] max_order_notional_usd 25"],
            ),
            (
                "over the gross budget",
                with(
                    "max_gross_exposure_usd = 100",
                    "max_gross_exposure_usd = 20",
                ),
                vec!["capped_top_n × capped_notional_usd = 25 exceeds [risk] max_gross_exposure_usd 20"],
            ),
            (
                "the [risk] account as the shadow",
                with(
                    "shadow_account = \"xmarket-shadow\"",
                    "shadow_account = \"xmarket\"",
                ),
                vec!["shadow_account `xmarket` is the [risk] account"],
            ),
            (
                "a shadow account with a space",
                with(
                    "shadow_account = \"xmarket-shadow\"",
                    "shadow_account = \"x shadow\"",
                ),
                vec!["shadow_account `x shadow` must be a [A-Za-z0-9._-] ledger account"],
            ),
            (
                "an edge under min_edge_bps",
                with("expected_edge_bps = 23", "expected_edge_bps = 5"),
                vec!["expected_edge_bps 5 is below [risk] min_edge_bps 10"],
            ),
            (
                "no lateness",
                with(
                    "entry_lateness_max_secs = 600",
                    "entry_lateness_max_secs = 0",
                ),
                vec!["entry_lateness_max_secs must be > 0 and < 54000"],
            ),
            (
                "an anchor age under the recorder heartbeat",
                with("anchor_max_age_secs = 600", "anchor_max_age_secs = 100"),
                vec!["anchor_max_age_secs 100 is below 300 s"],
            ),
            (
                "mkt_ctx/1 not recorded",
                with(
                    "schemas = [\"mkt_ctx/1\"]",
                    "schemas = [\"hl_book/1\"]",
                ),
                vec!["the anchor price comes from the history: [recorder] enabled = true with `mkt_ctx/1`"],
            ),
            (
                "the fades' strategy needs a hedge",
                with(
                    "require_hedge_for = [\"convergence\"]",
                    "require_hedge_for = [\"overreaction\"]",
                ),
                vec!["[risk] require_hedge_for lists `overreaction`"],
            ),
            (
                "a denied universe name",
                with("instruments_deny = []", "instruments_deny = [\"hyperliquid:xyz:TSLA\"]"),
                vec!["hyperliquid:xyz:TSLA is in instruments_deny"],
            ),
            (
                "excluded, it need not be tradable",
                with(
                    "universe = [\"hyperliquid:xyz:TSLA\"]\nexclude = []\ncapped_top_n = 1",
                    "universe = [\"hyperliquid:xyz:TSLA\", \"hyperliquid:xyz:KIOXIA\"]\n\
                     exclude = [\"hyperliquid:xyz:KIOXIA\"]\ncapped_top_n = 1",
                ),
                vec![],
            ),
            (
                "an agent holds the tool, no section",
                format!(
                    "{XM}{}\n[agents.x]\nengine = \"openrouter\"\nmodel = \"m\"\nworkspace = \"/srv/xm-ws\"\ntools = [\"xm_weekend_fade\"]\n",
                    crate::config::risk::tests::RISK_100
                ),
                vec!["agents.x holds xm_weekend_fade, but the sandbox has no [xmarket.weekend_fade]"],
            ),
        ];
    for (label, toml, want) in cases {
        let got = rules(&toml);
        for e in &got {
            assert!(
                want.iter().any(|w| e.contains(w)),
                "{label}: unexpected {e}"
            );
        }
        for w in want {
            assert!(
                got.iter().any(|e| e.contains(w)),
                "{label}: want {w}\ngot {got:#?}"
            );
        }
    }
    // Without [risk] + [paper].
    let no_risk = base.replacen(crate::config::risk::tests::RISK_100, "", 1);
    let got = rules(&no_risk);
    assert!(
        got.iter().any(|e| e.contains("needs [risk] + [paper]")),
        "{got:#?}"
    );
    // Every key required, unknown keys refused.
    let missing = with("max_slippage_bps = 50\n", "");
    assert!(toml::from_str::<Config>(&missing).is_err());
    let typo = with(
        "max_slippage_bps = 50",
        "max_slippage_bps = 50\nmax_slippage = 50",
    );
    assert!(toml::from_str::<Config>(&typo).is_err());
    // The rules run in `Config::validate`; the section reaches every
    // agent's tools.
    let mut cfg: Config = toml::from_str(&base).unwrap();
    cfg.validate().expect("valid");
    cfg.fold_default_scopes();
    let fade = cfg.agents["xm_exec"].sandbox.weekend_fade.clone().unwrap();
    assert_eq!(fade.universe, ["hyperliquid:xyz:TSLA"]);
    assert_eq!(fade.rule().capped_top_n, 1);
    let bad: Config =
        toml::from_str(&with("expected_edge_bps = 23", "expected_edge_bps = 5")).unwrap();
    assert!(bad.validate().is_err());
}

/// The commented `[xmarket.weekend_fade]` block of `config.example.toml`,
/// uncommented under the calendar fixture, passes the section's rules:
/// the 75-name universe in full ids, NYSE as the clock.
#[test]
fn example_weekend_fade_block_is_valid() {
    let text = include_str!("../../config.example.toml");
    let block: Vec<&str> = text
        .lines()
        .skip_while(|l| *l != "# [xmarket.weekend_fade]")
        .take_while(|l| l.starts_with('#'))
        .map(|l| l.strip_prefix("# ").unwrap_or(l.trim_start_matches('#')))
        .collect();
    assert!(block.len() > 20, "{block:?}");
    let x = toml::from_str::<Fixture>(&format!("{FIXTURE}\n{}", block.join("\n")))
        .unwrap_or_else(|e| panic!("{e}"))
        .xmarket;
    assert_eq!(x.validation_errors(), Vec::<String>::new());
    let fade = x.weekend_fade.unwrap();
    assert_eq!(fade.universe.len(), 75);
    assert!(fade
        .universe
        .iter()
        .all(|id| id.starts_with("hyperliquid:xyz:")));
    assert!(fade.universe.contains(&"hyperliquid:xyz:TSLA".to_string()));
    assert_eq!(
        (
            fade.capped_top_n,
            fade.min_abs_signal_bps,
            fade.capped_notional_usd
        ),
        (4, 50.0, 25.0)
    );
    assert_eq!(fade.calendar, "us_equity");
    assert_eq!(fade.expected_edge_bps, 23.0);
}

/// `sandboxes/xmarket-weekend/config.toml` (`x-weekend-sandbox`) trades
/// rule W as the replay golden computed it: the 75 names with the
/// golden's knobs, books of exactly those names, and the replay of the
/// 2026-09-26 → 09-28 weekend through its own calendar, universe and
/// rule reproduces the golden bit for bit (KIOXIA excluded here, as the
/// fixture's provenance says: split halt). Also what rule W needs from
/// the rest of the file, and its first window = the runbook's.
#[test]
fn weekend_sandbox_replays_the_golden() {
    use crate::domain::xm::weekend_fade::{fade_window, golden, replay};

    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("sandboxes")
        .join("xmarket-weekend")
        .join("config.toml");
    let cfg = Config::load(&path).unwrap_or_else(|e| panic!("{}: {e:#}", path.display()));
    let x = cfg.xmarket.as_ref().unwrap();
    assert_eq!(x.state, "xmarket-weekend");
    let fade = x.weekend_fade.as_ref().unwrap();
    let candles = golden::candles();
    assert_eq!(candles.len(), 75);
    let missing: Vec<&String> = candles
        .keys()
        .filter(|id| !fade.universe.contains(id))
        .collect();
    assert!(missing.is_empty(), "universe lacks {missing:?}");
    assert_eq!(
        (
            fade.capped_top_n,
            fade.min_abs_signal_bps,
            fade.capped_notional_usd
        ),
        (4, 50.0, 25.0)
    );
    let coins: Vec<&str> = fade
        .universe
        .iter()
        .map(|id| id.strip_prefix("hyperliquid:").unwrap())
        .collect();
    let books: Vec<&str> = cfg.feeds["hl_book"].each["coin"]
        .iter()
        .map(|c| c.as_str().unwrap())
        .collect();
    assert_eq!(books, coins);

    let Some(Calendar::Exchange(cal)) = x.calendars().remove(&fade.calendar) else {
        panic!("calendar `{}` is not an exchange row", fade.calendar)
    };
    let w = fade_window(&cal, et("2026-09-25 12:00")).unwrap();
    assert_eq!(
        (w.anchor_ms, w.entry_ms, w.exit_ms),
        (
            utc("2026-09-26 00:00"),
            utc("2026-09-27 22:00"),
            utc("2026-09-28 13:00")
        )
    );
    let universe: BTreeMap<_, _> = candles
        .into_iter()
        .filter(|(id, _)| fade.universe.contains(id))
        .collect();
    let mut exclude: BTreeSet<String> = fade.exclude.iter().cloned().collect();
    exclude.insert("hyperliquid:xyz:KIOXIA".to_string());
    assert_eq!(exclude, golden::excluded());
    let r = replay(&universe, &w, &exclude, golden::cost_rt_bps(), &fade.rule());
    golden::assert_replay(&r);

    // Rule W's needs beyond the load rules (wave D notes).
    let (risk, paper) = (cfg.risk.as_ref().unwrap(), cfg.paper.as_ref().unwrap());
    let capped_usd = fade.capped_top_n as f64 * fade.capped_notional_usd;
    assert!(risk.max_leverage * paper.initial_cash_usd > capped_usd);
    assert!(risk.max_net_exposure_usd >= capped_usd);
    assert!(risk.max_orders_per_min as usize >= 2 * fade.capped_top_n);
    assert!(risk.exits.max_hold_secs > 15 * 3600);
    let every = |f: &str| cfg.feeds[f].every_secs;
    assert_eq!(
        (every("hl_ctx"), every("xm_weekend_fade"), every("xm_exits")),
        (Some(60), Some(60), Some(15))
    );
    let ctx = &cfg.feeds["hl_ctx"];
    assert_eq!(ctx.args["dex"], "xyz");
    let ctx_late_ms = ctx.every_secs.unwrap() * 1000 * (100 + u64::from(ctx.jitter_pct)) / 100;
    assert!(risk.max_data_age_ms.ctx >= ctx_late_ms);
    assert!(fade.entry_max_age_secs * 1000 >= ctx_late_ms);
    for schema in ["mkt_ctx/1", "hl_book/1", "mkt_instrument/1"] {
        assert!(cfg.recorder.records(schema), "{schema}");
    }
    assert!(cfg.recorder.retention_days == 0 || cfg.recorder.retention_days >= 30);

    // This weekend: anchor Fri 2026-10-02 20:00, entry Sun 18:00, exit
    // Mon 09:00 New York (the runbook's timeline).
    let first = fade_window(&cal, et("2026-10-01 12:00")).unwrap();
    assert_eq!(
        (first.anchor_ms, first.entry_ms, first.exit_ms),
        (
            et("2026-10-02 20:00"),
            et("2026-10-04 18:00"),
            et("2026-10-05 09:00")
        )
    );
    assert_eq!(first.entry_ms, utc("2026-10-04 22:00"));
}

/// `sandboxes/xmarket/config.toml` (`ops-sandbox-config`), stage M0:
/// its own state dir, workspace and kill switch (not the weekend run's —
/// both may run at once), one shared workspace, exec tools only on the
/// private `xm_executor`, an explicit scope for every tool an agent holds,
/// the egress ceiling = the scopes' hosts, feeds whose agent holds their
/// tool, the market rows recorded, ctx fresh enough for `xm_exits`.
#[test]
fn xmarket_sandbox_m0_stage() {
    use crate::config::AgentConfig;
    use crate::domain::scope::host_matches;
    use crate::domain::tools::XM_EXEC_TOOLS;

    let load = |name: &str| {
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("sandboxes")
            .join(name)
            .join("config.toml");
        Config::load(&path).unwrap_or_else(|e| panic!("{}: {e:#}", path.display()))
    };
    let held = |a: &AgentConfig| -> BTreeSet<String> {
        a.tools.iter().chain(&a.workspace_tools).cloned().collect()
    };
    let cfg = load("xmarket");
    let weekend = load("xmarket-weekend");

    let x = cfg.xmarket.as_ref().unwrap();
    assert_eq!(x.state, "xmarket");
    assert!(x.weekend_fade.is_none(), "rule W is xmarket-weekend's");
    let home = Path::new(HOME);
    let weekend_x = weekend.xmarket.as_ref().unwrap();
    assert_ne!(x.state_dir(home), weekend_x.state_dir(home));
    let workspaces = |c: &Config| -> BTreeSet<PathBuf> {
        c.agents
            .values()
            .map(|a| expand_tilde(a.workspace.as_ref().expect("every agent sets one")))
            .collect()
    };
    let ws = workspaces(&cfg);
    assert_eq!(ws.len(), 1, "one shared workspace: {ws:?}");
    assert!(ws.is_disjoint(&workspaces(&weekend)));
    let kill = |c: &Config| expand_tilde(&c.risk.as_ref().unwrap().kill_switch_file);
    assert_ne!(kill(&cfg), kill(&weekend));

    // Exec tools only on the private executor; the architect (default,
    // routable) reads only.
    let exec_holders: Vec<&str> = cfg
        .agents
        .iter()
        .filter(|(_, a)| held(a).iter().any(|t| XM_EXEC_TOOLS.contains(&t.as_str())))
        .map(|(id, _)| id.as_str())
        .collect();
    assert_eq!(exec_holders, ["xm_executor"]);
    let exec = &cfg.agents["xm_executor"];
    assert!(exec.description.is_none() && !exec.default);
    let routable: Vec<&str> = cfg
        .agents
        .iter()
        .filter(|(_, a)| a.description.is_some())
        .map(|(id, _)| id.as_str())
        .collect();
    assert_eq!(routable, ["xm_architect"]);
    // No planner (operator 2026-10-02, as in xlab): chat talks to the
    // architect; every agent on the Claude CLI, OpenRouter only for Jev.
    assert!(cfg.agents["xm_architect"].default);
    assert!(cfg.orchestrator.is_none());
    for (id, a) in &cfg.agents {
        assert_eq!(a.engine, "claude_code", "{id}");
    }

    // An explicit scope (folded into `scopes` by `Config::load`) for every
    // tool an agent holds; every scope host inside the egress ceiling,
    // every ceiling host used by a scope.
    for (id, a) in &cfg.agents {
        for t in held(a) {
            assert!(a.scopes.contains_key(&t), "agents.{id}: {t} has no scope");
        }
    }
    let allow = &cfg.egress.allow_hosts;
    let scopes = cfg
        .default_scopes
        .values()
        .chain(cfg.agents.values().flat_map(|a| a.scopes.values()));
    let hosts: BTreeSet<&String> = scopes.flat_map(|s| &s.net_hosts).collect();
    for h in &hosts {
        assert!(
            allow.iter().any(|p| host_matches(p, h)),
            "{h} outside {allow:?}"
        );
    }
    for p in allow {
        assert!(hosts.iter().any(|h| host_matches(p, h)), "{p} unused");
    }
    assert_eq!(cfg.egress.network, "open");

    // Feeds run tools their agent holds; the market rows are recorded;
    // the 15 s ctx feed keeps marks fresh for the gate and xm_exits.
    for (name, f) in cfg.feeds.iter().filter(|(_, f)| f.kind == "tool") {
        let (agent, tool) = (f.agent.as_deref().unwrap(), f.tool.as_deref().unwrap());
        assert!(held(&cfg.agents[agent]).contains(tool), "feeds.{name}");
    }
    for schema in ["mkt_ctx/1", "hl_book/1"] {
        assert!(cfg.recorder.records(schema), "{schema}");
    }
    let ctx = &cfg.feeds["hl_ctx"];
    assert_eq!(ctx.args["dex"], "xyz");
    let ctx_late_ms = ctx.every_secs.unwrap() * 1000 * (100 + u64::from(ctx.jitter_pct)) / 100;
    let risk = cfg.risk.as_ref().unwrap();
    assert!(risk.max_data_age_ms.ctx >= ctx_late_ms);
    assert_eq!(cfg.feeds["xm_exits"].every_secs, Some(15));
}

/// The rules run inside `Config::validate` (and so `Config::load`).
#[test]
fn validate_runs_the_rules() {
    let mut cfg: Config =
        toml::from_str(&format!("{XM}{}{FEED}", ws("xm_loop", Some("/srv/other")))).unwrap();
    let e = cfg.validate().unwrap_err().to_string();
    assert!(e.contains("must share one `workspace`"), "{e}");
    cfg.agents.get_mut("xm_loop").unwrap().workspace = Some("/srv/xm-ws".into());
    cfg.validate().expect("one workspace");
}

#[test]
fn rejects_paths_and_unknown_keys() {
    for bad in ["", "..", "a/b", "flows", "x y"] {
        let x = XmarketConfig {
            state: bad.to_string(),
            ..XmarketConfig::default()
        };
        assert_eq!(x.validation_errors().len(), 1, "{bad:?}");
    }
    assert!(toml::from_str::<XmarketConfig>("stat = \"x\"").is_err());
    let typo = "[calendars.x]\nkind = \"24x7\"\nholiday = [\"2026-01-01\"]\n";
    assert!(toml::from_str::<XmarketConfig>(typo).is_err());
}

/// The fixture (= `config.example.toml`) builds, and its NYSE data drives
/// the weekend clock and sessions end to end.
#[test]
fn fixture_calendars_build_and_evaluate() {
    let x = fixture();
    assert_eq!(x.validation_errors(), Vec::<String>::new());
    let cals = x.calendars();
    assert_eq!(
        cals.keys().map(String::as_str).collect::<Vec<_>>(),
        [
            "crypto",
            "rh_tokenization",
            "us_equity",
            "xyz_fx",
            "xyz_indices",
            "xyz_stocks"
        ]
    );
    let nyse = cals["us_equity"].exchange().unwrap();
    assert_eq!(nyse.holidays.len(), 29);
    assert_eq!(nyse.early_closes.len(), 5);
    // Verified on nyse.com 2026-09-30: Christmas 2027 observed Fri 12-24,
    // no New Year holiday for Sat 2028-01-01.
    assert!(!nyse.is_trading_day(day("2027-12-24")));
    assert!(nyse.is_trading_day(day("2027-12-31")));
    assert!(nyse.is_early_close(day("2028-07-03")));

    let w = nyse.weekend_window(et("2026-09-26 12:00")).unwrap();
    assert_eq!(
        (w.anchor_ms, w.entry_ms, w.exit_ms),
        (
            utc("2026-09-26 00:00"),
            utc("2026-09-27 22:00"),
            utc("2026-09-28 13:00")
        )
    );
    let mlk = nyse.weekend_window(et("2027-01-16 12:00")).unwrap();
    assert_eq!(mlk.entry_ms, utc("2027-01-18 23:00"));
    assert_eq!(mlk.exit_ms, utc("2027-01-19 14:00"));
    for (t, want) in [
        ("2026-11-26 12:00", Session::Closed),
        ("2026-11-27 13:00", Session::Post),
        ("2026-11-27 17:00", Session::Closed),
        ("2026-09-29 03:00", Session::Overnight),
    ] {
        assert_eq!(cals["us_equity"].session(et(t)), want, "{t} ET");
    }
    assert_eq!(
        cals["xyz_indices"].session(et("2026-09-29 17:30")),
        Session::Closed
    );
    assert_eq!(
        cals["rh_tokenization"].session(utc("2026-10-03 00:00")),
        Session::Closed
    );
    assert_eq!(
        cals["crypto"].session(utc("2026-10-03 00:00")),
        Session::Open
    );
}

/// Every agent's tools see the built calendars through `AgentConfig::sandbox`.
#[test]
fn calendars_reach_every_agent_through_sandbox_sections() {
    let mut config: crate::config::Config = toml::from_str(&format!(
        "{FIXTURE}\n[agents.main]\ndefault = true\nengine = \"openrouter\"\nmodel = \"m\"\n"
    ))
    .unwrap();
    config.validate().unwrap();
    config.fold_default_scopes();
    let cals = &config.agents["main"].sandbox.calendars;
    assert_eq!(cals.len(), 6);
    assert_eq!(
        cals["us_equity"].session(et("2026-09-28 10:00")),
        Session::Open
    );

    let mut bad: crate::config::Config = toml::from_str(
        "[xmarket.calendars.us]\nkind = \"exchange\"\ntz = \"UTC\"\n\
             [agents.main]\nengine = \"openrouter\"\nmodel = \"m\"\n",
    )
    .unwrap();
    let e = bad.validate().unwrap_err().to_string();
    assert!(
        e.contains("xmarket.calendars.us: `core` is required"),
        "{e}"
    );
    bad.fold_default_scopes();
    assert!(bad.agents["main"].sandbox.calendars.is_empty());
}

#[test]
fn toml_dates_are_accepted_unquoted() {
    let x: XmarketConfig = toml::from_str(
        "[calendars.us]\nkind = \"exchange\"\ntz = \"America/New_York\"\n\
             core = [\"09:30\", \"16:00\"]\nholidays = [2026-11-26, \"2026-12-25\"]\n",
    )
    .unwrap();
    assert_eq!(x.calendars["us"].holidays, ["2026-11-26", "2026-12-25"]);
    assert!(x.validation_errors().is_empty());
}

#[test]
fn calendar_validation_errors() {
    let errors = |toml_row: &str| {
        let x: XmarketConfig = toml::from_str(&format!("[calendars.c]\n{toml_row}")).unwrap();
        x.validation_errors()
    };
    let base = "kind = \"exchange\"\ntz = \"America/New_York\"\ncore = [\"09:30\", \"16:00\"]\n";
    assert!(errors(base).is_empty());
    for (extra, want) in [
        ("holidays = [\"2026-02-30\"]", "is not a YYYY-MM-DD date"),
        ("holidays = [\"2026-07-04\"]", "is a Sat"),
        (
            "early_closes = [\"2026-11-27\"]",
            "early_closes needs early_close",
        ),
        ("overnight = true", "overnight needs pre and post"),
        ("pre = \"10:00\"", "pre must be before the core open"),
        ("post = \"15:00\"", "post must be after the core close"),
        ("early_close = \"17:00\"", "early_close must be after"),
        (
            "early_close = \"13:00\"\nearly_post = \"17:00\"",
            "early_post needs",
        ),
        (
            "open = \"Sun 20:00\"",
            "`open` is not used by kind = \"exchange\"",
        ),
        (
            "early_close = \"13:00\"\nholidays = [\"2026-11-27\"]\nearly_closes = [\"2026-11-27\"]",
            "both a holiday and an early close",
        ),
    ] {
        let got = errors(&format!("{base}{extra}\n"));
        assert!(
            got.iter()
                .any(|e| e.starts_with("xmarket.calendars.c: ") && e.contains(want)),
            "{extra:?} → {got:?}"
        );
    }
    let e = errors("kind = \"exchange\"\ntz = \"Asia/Almaty\"\ncore = [\"16:00\", \"09:30\"]\n");
    assert!(
        e.iter()
            .any(|m| m.contains("tz `Asia/Almaty` is not supported")),
        "{e:?}"
    );
    assert!(
        e.iter().any(|m| m.contains("core must open before")),
        "{e:?}"
    );
    let e = errors("kind = \"weekly\"\ntz = \"UTC\"\nopen = \"Sun 20:00\"\n");
    assert!(e.iter().any(|m| m.contains("`close` is required")), "{e:?}");
    let e =
        errors("kind = \"weekly\"\ntz = \"UTC\"\nopen = \"Sunday 20:00\"\nclose = \"Fri 20:00\"\n");
    assert!(
        e.iter().any(|m| m.contains("is not \"Ddd HH:MM\"")),
        "{e:?}"
    );
    let e =
        errors("kind = \"weekly\"\ntz = \"UTC\"\nopen = \"Fri 20:00\"\nclose = \"Fri 20:00\"\n");
    assert!(e.iter().any(|m| m.contains("use kind = \"24x7\"")), "{e:?}");
    let e = errors("kind = \"24x7\"\ntz = \"UTC\"\n");
    assert!(
        e.iter()
            .any(|m| m.contains("`tz` is not used by kind = \"24x7\"")),
        "{e:?}"
    );
    let e = errors("kind = \"weekly\"\nopen = \"Sun 18:00\"\nclose = \"Fri 17:00\"\ndaily_break = [\"17:00\"]\n");
    assert!(e.iter().any(|m| m.contains("`tz` is required")), "{e:?}");
    assert!(e.iter().any(|m| m.contains("daily_break is [")), "{e:?}");
    assert!(toml::from_str::<XmarketConfig>("[calendars.c]\nkind = \"24x5\"\n").is_err());
}
