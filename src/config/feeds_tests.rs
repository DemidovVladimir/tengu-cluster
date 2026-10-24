//! Tests of `src/config/feeds.rs`.

use super::*;
use serde_json::json;

/// An agent with an allow-list and one decision loop to point at; the
/// xmarket agents share one workspace (`config/xmarket.rs`).
const BASE: &str = r#"
        [agents.xm_feeds]
        engine = "openrouter"
        model = "m"
        workspace = "/srv/xm-ws"
        tools = ["hl_ctx", "hl_book", "risk_status"]

        [agents.xm_exec]
        engine = "openrouter"
        model = "m"
        workspace = "/srv/xm-ws"
        tools = ["xm_exits"]

        [agents.open]
        engine = "openrouter"
        model = "m"

        [decision_loops.xm_main]
        goal = "g"
        agent = "xm_feeds"
        [decision_loops.xm_main.actions.hold]
        description = "Nothing to do"
    "#;

fn load(feeds: &str) -> Config {
    toml::from_str(&format!("{BASE}\n{feeds}")).unwrap_or_else(|e| panic!("{e}"))
}

fn errors(feeds: &str) -> Vec<String> {
    validation_errors(&load(feeds))
}

const WEEKEND_BOOK: &str = r#"
        [feeds.hl_book]
        kind = "tool"
        agent = "xm_feeds"
        tool = "hl_book"
        each = { coin = ["xyz:TSLA", "xyz:NVDA"] }
        every_secs = 300
        tz = "America/New_York"
        windows = [
          { days = ["Sun"], from = "17:00", to = "19:00", every_secs = 60 },
          { days = ["Mon"], from = "08:30", to = "09:30", every_secs = 60 },
        ]
        required = true
    "#;

#[test]
fn weekend_book_feed_builds_its_schedule_and_calls() {
    let cfg = load(WEEKEND_BOOK);
    assert!(validation_errors(&cfg).is_empty());
    cfg.validate().expect("the whole config validates");
    let f = &cfg.feeds["hl_book"];
    assert_eq!(f.kind(), Ok(FeedKind::Tool));
    let s = f.schedule().unwrap();
    assert_eq!((s.zone, s.every_ms), (Zone::NewYork, Some(300_000)));
    let sun = Days::NONE.with(chrono::Weekday::Sun);
    assert_eq!(
        s.windows[0],
        Window {
            days: sun,
            from: 17 * 60,
            to: 19 * 60,
            every_ms: 60_000
        }
    );
    assert_eq!((s.windows[1].from, s.windows[1].to), (510, 570));
    assert_eq!(
        f.calls(),
        vec![json!({"coin": "xyz:TSLA"}), json!({"coin": "xyz:NVDA"})]
    );
    assert_eq!((f.concurrency(), f.stale_after_secs()), (1, 900));
    assert!(f.required && !f.run_on_start && f.jitter_pct == 0);
}

#[test]
fn fan_out_is_every_combination_in_key_order() {
    let f: FeedConfig = toml::from_str(
        r#"
            kind = "tool"
            args = { max_age_secs = 0 }
            each = { interval = ["1m", "5m"], coin = ["ETH", "xyz:TSLA"] }
        "#,
    )
    .unwrap();
    assert_eq!(
        f.calls(),
        vec![
            json!({"max_age_secs": 0, "coin": "ETH", "interval": "1m"}),
            json!({"max_age_secs": 0, "coin": "ETH", "interval": "5m"}),
            json!({"max_age_secs": 0, "coin": "xyz:TSLA", "interval": "1m"}),
            json!({"max_age_secs": 0, "coin": "xyz:TSLA", "interval": "5m"}),
        ]
    );
    let plain: FeedConfig = toml::from_str("kind = \"tool\"").unwrap();
    assert_eq!(plain.calls(), vec![json!({})]);
}

#[test]
fn tick_feed_with_at_ticks_validates() {
    let cfg = load(
        r#"
            [feeds.weekend_entry]
            kind = "tick"
            target = "xm_main"
            tz = "America/New_York"
            at = ["Sun 18:00", "daily 09:00"]
            event = { phase = "entry" }
        "#,
    );
    assert_eq!(validation_errors(&cfg), Vec::<String>::new());
    let f = &cfg.feeds["weekend_entry"];
    assert_eq!(f.kind(), Ok(FeedKind::Tick));
    assert_eq!(f.schedule().unwrap().at.len(), 2);
    assert_eq!(f.stale_after_secs(), 8 * 86_400);
    // Windows only: idle between them, so not stale within the week.
    let w: FeedConfig = toml::from_str(
            "kind = \"tick\"\nwindows = [{ days = [\"Sun\"], from = \"17:00\", to = \"19:00\", every_secs = 60 }]",
        )
        .unwrap();
    assert_eq!(w.stale_after_secs(), 8 * 86_400);
    let fast: FeedConfig = toml::from_str("kind = \"tick\"\nevery_secs = 5").unwrap();
    assert_eq!(fast.stale_after_secs(), 60);
}

#[test]
fn validation_names_every_problem() {
    for (feeds, want) in [
            (
                "[feeds.news]\nkind = \"poll\"\nevery_secs = 60",
                "feeds.news.kind = \"poll\" is reserved for `info-fetch` (xmarket wave W2)",
            ),
            (
                "[feeds.x]\nkind = \"ws\"\nevery_secs = 60",
                "feeds.x.kind = \"ws\" is reserved for a later xmarket milestone",
            ),
            (
                "[feeds.x]\nkind = \"cron\"\nevery_secs = 60",
                "feeds.x.kind = \"cron\" is unknown",
            ),
            (
                "[feeds.x]\nkind = \"tool\"\nevery_secs = 60",
                "feeds.x.agent is required for kind = \"tool\"",
            ),
            (
                "[feeds.x]\nkind = \"tool\"\nagent = \"xm_feeds\"\nevery_secs = 60",
                "feeds.x.tool is required for kind = \"tool\"",
            ),
            (
                "[feeds.x]\nkind = \"tool\"\nagent = \"nobody\"\ntool = \"hl_ctx\"\nevery_secs = 60",
                "feeds.x.agent: no [agents.nobody] block",
            ),
            (
                "[feeds.x]\nkind = \"tool\"\nagent = \"xm_feeds\"\ntool = \"read_file\"\nevery_secs = 60",
                "feeds.x.tool: `read_file` is not in [agents.xm_feeds].tools",
            ),
            (
                "[feeds.x]\nkind = \"tool\"\nagent = \"open\"\ntool = \"sol_price\"\nevery_secs = 60",
                "feeds.x.tool: `sol_price` is an opt-in tool",
            ),
            (
                "[feeds.x]\nkind = \"tick\"\nevery_secs = 60",
                "feeds.x.target is required for kind = \"tick\"",
            ),
            (
                "[feeds.x]\nkind = \"tick\"\ntarget = \"nope\"\nevery_secs = 60",
                "feeds.x.target: no [decision_loops.nope] block",
            ),
            (
                "[feeds.x]\nkind = \"tick\"\ntarget = \"xm_main\"\nevery_secs = 60\nevent = { ts_ms = 1 }",
                "feeds.x.event.ts_ms is set by the scheduler",
            ),
            (
                "[feeds.x]\nkind = \"tick\"\ntarget = \"xm_main\"\nevery_secs = 60\ntool = \"hl_ctx\"",
                "feeds.x.tool is only for kind = \"tool\"",
            ),
            (
                "[feeds.x]\nkind = \"tool\"\nagent = \"xm_feeds\"\ntool = \"hl_ctx\"\nevery_secs = 60\ntarget = \"xm_main\"",
                "feeds.x.target is only for kind = \"tick\"",
            ),
            (
                "[feeds.x]\nkind = \"tick\"\ntarget = \"xm_main\"",
                "feeds.x.every_secs: set every_secs, windows or at",
            ),
            (
                "[feeds.x]\nkind = \"tick\"\ntarget = \"xm_main\"\nevery_secs = 0",
                "feeds.x.every_secs 0 must be 1–604800",
            ),
            (
                "[feeds.x]\nkind = \"tick\"\ntarget = \"xm_main\"\nevery_secs = 60\ntz = \"Asia/Almaty\"",
                "feeds.x.tz: unknown zone `Asia/Almaty`",
            ),
            (
                "[feeds.x]\nkind = \"tick\"\ntarget = \"xm_main\"\nat = [\"Sunday 18:00\"]",
                "feeds.x.at[0] `Sunday 18:00` is not",
            ),
            (
                "[feeds.x]\nkind = \"tick\"\ntarget = \"xm_main\"\nwindows = [{ from = \"17:00\", to = \"17:00\", every_secs = 60 }]",
                "feeds.x.windows[0]: from and to are both 17:00 (empty)",
            ),
            (
                "[feeds.x]\nkind = \"tick\"\ntarget = \"xm_main\"\nwindows = [{ days = [\"Sun\", \"Funday\"], from = \"25:00\", to = \"19:00\", every_secs = 0 }]",
                "feeds.x.windows[0].from `25:00` is not HH:MM",
            ),
            (
                "[feeds.x]\nkind = \"tick\"\ntarget = \"xm_main\"\nwindows = [{ days = [\"Funday\"], from = \"17:00\", to = \"19:00\", every_secs = 60 }]",
                "feeds.x.windows[0].days: `Funday` is not a day",
            ),
            (
                "[feeds.x]\nkind = \"tick\"\ntarget = \"xm_main\"\nevery_secs = 60\njitter_pct = 60",
                "feeds.x.jitter_pct 60 must be 0–50",
            ),
            (
                "[feeds.x]\nkind = \"tick\"\ntarget = \"xm_main\"\nevery_secs = 60\nstale_after_secs = 0",
                "feeds.x.stale_after_secs must be at least 1",
            ),
            (
                "[feeds.x]\nkind = \"tool\"\nagent = \"xm_feeds\"\ntool = \"hl_book\"\nevery_secs = 60\nargs = { coin = \"ETH\" }\neach = { coin = [\"BTC\"] }",
                "feeds.x.each.coin: also set in args",
            ),
            (
                "[feeds.x]\nkind = \"tool\"\nagent = \"xm_feeds\"\ntool = \"hl_book\"\nevery_secs = 60\neach = { coin = [] }",
                "feeds.x.each.coin: empty list",
            ),
            (
                "[feeds.x]\nkind = \"tool\"\nagent = \"xm_feeds\"\ntool = \"hl_book\"\nevery_secs = 60\nconcurrency = 0",
                "feeds.x.concurrency 0 must be 1–32",
            ),
            (
                "[feeds.\"hl:book\"]\nkind = \"tick\"\ntarget = \"xm_main\"\nevery_secs = 60",
                "feeds.hl:book: name must be [a-z0-9_-]+",
            ),
        ] {
            let errs = errors(feeds);
            assert!(
                errs.iter().any(|e| e.starts_with(want)),
                "{feeds}\nwant: {want}\ngot: {errs:#?}"
            );
        }
    // Opted in via workspace_tools, or no allow-list and not opt-in: fine.
    let mut cfg =
        load("[feeds.x]\nkind = \"tool\"\nagent = \"open\"\ntool = \"read_file\"\nevery_secs = 60");
    assert!(validation_errors(&cfg).is_empty());
    cfg.feeds.get_mut("x").unwrap().tool = Some("sol_price".into());
    cfg.agents.get_mut("open").unwrap().workspace_tools = vec!["sol_price".into()];
    assert!(validation_errors(&cfg).is_empty());
    // The whole config refuses to load with a bad feed.
    let bad = load("[feeds.news]\nkind = \"poll\"\nevery_secs = 60");
    let e = format!("{:#}", bad.validate().unwrap_err());
    assert!(e.contains("info-fetch"), "{e}");
}

/// `sandboxes/xlab-w2`'s strategy-ranking feeds (SR-6): they load and
/// validate on the private ranker; the refresh fans out over exactly the
/// ranked universes (a name left out would go STALE); each ranking feed
/// runs a listed contract after its cutoff and after its refresh, at the
/// same New York times across both DST changes.
#[test]
fn xlab_w2_feeds_validate() {
    use crate::domain::schedule::next_fire;
    let path =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("sandboxes/xlab-w2/config.toml");
    let cfg = Config::load(&path).unwrap_or_else(|e| panic!("{e:#}"));
    assert_eq!(validation_errors(&cfg), Vec::<String>::new());
    let names: Vec<&str> = cfg.feeds.keys().map(String::as_str).collect();
    assert_eq!(
        names,
        [
            "history_refresh",
            "strategy_ranking_daily",
            "strategy_ranking_weekend"
        ]
    );
    let ranker = &cfg.agents["xl_ranker"];
    assert!(ranker.description.is_none() && !ranker.default);
    for f in cfg.feeds.values() {
        assert_eq!(f.agent.as_deref(), Some("xl_ranker"));
        assert!(f.required);
    }
    // The refresh: every ranked name, once, in the universes' order.
    let refresh = &cfg.feeds["history_refresh"];
    let bt = cfg.backtest.as_ref().unwrap();
    let want: Vec<Value> = ["xyz_stocks", "crypto"]
        .iter()
        .flat_map(|u| bt.universes[*u].iter())
        .map(|id| json!({"interval": "1h", "fetch": true, "instrument": id}))
        .collect();
    assert_eq!(want.len(), 79);
    assert_eq!(refresh.calls(), want);
    assert_eq!(refresh.concurrency(), 1);
    // Each contract a feed runs is listed in [strategy_ranking].
    let section = cfg.ranking_section.as_ref().unwrap();
    for (feed, contract) in [
        ("strategy_ranking_daily", "rank.xlab-w2.daily.v1"),
        ("strategy_ranking_weekend", "rank.xlab-w2.weekend.v1"),
    ] {
        let f = &cfg.feeds[feed];
        assert_eq!(f.tool.as_deref(), Some("strategy_ranking"));
        assert_eq!(
            f.calls(),
            vec![json!({"action": "run", "contract": contract})]
        );
        assert!(section.contracts.iter().any(|c| c == contract), "{feed}");
    }
    // Fire times (UTC) in summer and winter time: the refresh after each
    // cutoff (daily 00:00, Mon 12:00 New York), each ranking after it.
    let ms = |s: &str| {
        chrono::DateTime::parse_from_rfc3339(s)
            .unwrap()
            .timestamp_millis()
    };
    let fire = |feed: &str, after: &str| {
        let s = cfg.feeds[feed].schedule().unwrap();
        let at = next_fire(&s, ms(after), None).unwrap().at_ms;
        chrono::DateTime::from_timestamp_millis(at)
            .unwrap()
            .to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
    };
    for (after, refresh, daily, weekend) in [
        // EDT: New York = UTC − 4.
        (
            "2026-10-11T12:00:00Z",
            "2026-10-12T09:00:00Z",
            "2026-10-12T10:00:00Z",
            "2026-10-12T17:00:00Z",
        ),
        // EST from Sun 2026-11-01: UTC − 5.
        (
            "2026-11-01T12:00:00Z",
            "2026-11-02T10:00:00Z",
            "2026-11-02T11:00:00Z",
            "2026-11-02T18:00:00Z",
        ),
        // EDT again from Sun 2027-03-14.
        (
            "2027-03-14T12:00:00Z",
            "2027-03-15T09:00:00Z",
            "2027-03-15T10:00:00Z",
            "2027-03-15T17:00:00Z",
        ),
    ] {
        assert_eq!(fire("history_refresh", after), refresh, "{after}");
        assert_eq!(fire("strategy_ranking_daily", after), daily, "{after}");
        assert_eq!(fire("strategy_ranking_weekend", after), weekend, "{after}");
    }
    // The Monday refresh after the weekend cutoff, before its ranking.
    assert_eq!(
        fire("history_refresh", "2026-10-12T09:30:00Z"),
        "2026-10-12T16:05:00Z"
    );
}

#[test]
fn unknown_keys_are_parse_errors() {
    let e = toml::from_str::<Config>(&format!(
        "{BASE}\n[feeds.x]\nkind = \"tick\"\ntarget = \"xm_main\"\nevery_sec = 60\n"
    ))
    .unwrap_err();
    assert!(e.to_string().contains("every_sec"), "{e}");
    let e = toml::from_str::<Config>(&format!(
        "{BASE}\n[feeds.x]\nkind = \"tick\"\ntarget = \"xm_main\"\n\
             windows = [{{ from = \"17:00\", to = \"19:00\", every = 60 }}]\n"
    ))
    .unwrap_err();
    assert!(e.to_string().contains("every"), "{e}");
}

const WEEKLY_JOB: &str = r#"
        [feeds.soe_week]
        kind = "job"
        job = "soe_cycle"
        tz = "Europe/Paris"
        at = ["Mon 07:00"]
        required = true
    "#;

fn soe_section() -> crate::config::soe::SoeConfig {
    toml::from_str("architect = \"a\"\ncritic = \"c\"\nmax_proposals = 12\nforecast_max_weeks = 12")
        .unwrap()
}

#[test]
fn job_kind_needs_known_job() {
    let mut cfg = load(WEEKLY_JOB);
    cfg.soe = Some(soe_section());
    assert_eq!(validation_errors(&cfg), Vec::<String>::new());
    let f = &cfg.feeds["soe_week"];
    assert_eq!(f.kind(), Ok(FeedKind::Job));
    assert_eq!(f.job.as_deref(), Some(JOB_SOE_CYCLE));
    let s = f.schedule().unwrap();
    assert_eq!((s.zone, s.every_ms, s.at.len()), (Zone::Paris, None, 1));
    // Idle between weekly ticks: stale only after 8 days.
    assert_eq!(f.stale_after_secs(), 8 * 86_400);
    for (feed, want) in [
            (
                "[feeds.x]\nkind = \"job\"\nat = [\"Mon 07:00\"]",
                "feeds.x.job is required for kind = \"job\"",
            ),
            (
                "[feeds.x]\nkind = \"job\"\njob = \"rm -rf\"\nat = [\"Mon 07:00\"]",
                "feeds.x.job: `rm -rf` is not a known job (soe_cycle)",
            ),
            (
                "[feeds.x]\nkind = \"job\"\njob = \"soe_cycle\"\nat = [\"Mon 07:00\"]\nagent = \"open\"\ntool = \"read_file\"",
                "feeds.x.agent is not for kind = \"job\"",
            ),
            (
                "[feeds.x]\nkind = \"job\"\njob = \"soe_cycle\"\nat = [\"Mon 07:00\"]\ntarget = \"xm_main\"",
                "feeds.x.target is not for kind = \"job\"",
            ),
            (
                "[feeds.x]\nkind = \"tool\"\nagent = \"open\"\ntool = \"read_file\"\nevery_secs = 60\njob = \"soe_cycle\"",
                "feeds.x.job is only for kind = \"job\"",
            ),
            (
                "[feeds.x]\nkind = \"tick\"\ntarget = \"xm_main\"\nevery_secs = 60\njob = \"soe_cycle\"",
                "feeds.x.job is only for kind = \"job\"",
            ),
            (
                "[feeds.x]\nkind = \"job\"\njob = \"soe_cycle\"",
                "feeds.x.every_secs: set every_secs, windows or at",
            ),
        ] {
            let mut cfg = load(feed);
            cfg.soe = Some(soe_section());
            let errs = validation_errors(&cfg);
            assert!(
                errs.iter().any(|e| e.starts_with(want)),
                "{feed}\nwant: {want}\ngot: {errs:#?}"
            );
        }
    // An unknown kind names the three built ones.
    let errs = errors("[feeds.x]\nkind = \"cron\"\nevery_secs = 60");
    assert!(
        errs.iter()
            .any(|e| e.contains("use \"tool\", \"tick\" or \"job\"")),
        "{errs:?}"
    );
}

#[test]
fn job_needs_soe_section() {
    let errs = errors(WEEKLY_JOB);
    assert_eq!(
        errs,
        ["feeds.soe_week.job: `soe_cycle` needs a [soe] section (config/soe.rs)"]
    );
    let mut cfg = load(WEEKLY_JOB);
    cfg.soe = Some(soe_section());
    assert!(validation_errors(&cfg).is_empty());
}

/// The commented `[feeds.*]` block of `config.example.toml`, uncommented
/// (with its agent and loop), is valid.
#[test]
fn example_block_uncommented_is_valid() {
    let text = include_str!("../../config.example.toml");
    let block: Vec<&str> = text
        .lines()
        .skip_while(|l| *l != "# [feeds.hl_ctx]")
        .take_while(|l| l.starts_with('#'))
        .map(|l| l.strip_prefix("# ").unwrap_or(l.trim_start_matches('#')))
        .collect();
    assert!(block.len() > 20, "{block:?}");
    // The exec agent holds the weekend fade too (as in the example).
    let base = BASE.replace(
        "tools = [\"xm_exits\"]",
        "tools = [\"xm_exits\", \"xm_weekend_fade\"]",
    );
    let cfg: Config =
        toml::from_str(&format!("{base}\n{}", block.join("\n"))).unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(validation_errors(&cfg), Vec::<String>::new());
    let names: Vec<&str> = cfg.feeds.keys().map(String::as_str).collect();
    assert_eq!(
        names,
        [
            "hl_book",
            "hl_ctx",
            "risk_day",
            "weekend_review",
            "xm_exits",
            "xm_weekend_fade"
        ]
    );
    assert_eq!(
        cfg.feeds["hl_book"].schedule().unwrap(),
        load(WEEKEND_BOOK).feeds["hl_book"].schedule().unwrap()
    );
    // risk_status once a day at 00:01 UTC, no arguments.
    let day = &cfg.feeds["risk_day"];
    let s = day.schedule().unwrap();
    assert_eq!((s.zone, s.every_ms, s.at.len()), (Zone::Utc, None, 1));
    assert_eq!((s.at[0].days, s.at[0].minute), (Days::ALL, 1));
    assert_eq!(day.calls(), vec![json!({})]);
    assert_eq!(day.stale_after_secs(), 90_000);
    let review = &cfg.feeds["weekend_review"];
    assert_eq!(review.kind(), Ok(FeedKind::Tick));
    assert_eq!(review.schedule().unwrap().at.len(), 2);
    // The exit rules every 15 s on the private exec agent, no arguments.
    let exits = &cfg.feeds["xm_exits"];
    assert_eq!(
        (
            exits.kind(),
            exits.schedule().unwrap().every_ms,
            exits.required
        ),
        (Ok(FeedKind::Tool), Some(15_000), true)
    );
    assert_eq!(exits.calls(), vec![json!({})]);
    // The weekend fade every 60 s on the same agent, no arguments.
    let fade = &cfg.feeds["xm_weekend_fade"];
    assert_eq!(
        (
            fade.kind(),
            fade.schedule().unwrap().every_ms,
            fade.required,
            fade.agent.as_deref()
        ),
        (Ok(FeedKind::Tool), Some(60_000), true, Some("xm_exec"))
    );
    assert_eq!(fade.calls(), vec![json!({})]);
}
