//! `tengu risk status | halt | resume [--sandbox <s>] [--account <a>]` — the
//! operator's handle on the paper ledger's risk state
//! (`domain/xm/risk_state.rs`, `<TENGU_HOME>/state/<xmarket.state>/ledger.db`).
//! No LLM, no network.
//!
//! | Command | Rule |
//! |---|---|
//! | `status` | read-only, no TTY needed; never creates the ledger: per account (every ledger account, or `--account`) the halt in force, the UTC day's start equity, cash, open positions (full ids, exit deadlines), orders in the last 60 s, the latest verdicts (rule, full ids, exec tool, call id), and the kill-switch file. Equity at mark: the `risk_status` tool (marks live in the workspace store) |
//! | `halt` | operator only: an `operator` halt (a sticky halt stays as is) — entries deny `halted`, reduce-only exits still pass (`allow_reduce_degraded`). The `[risk]` account is opened first if new |
//! | `resume` | operator only; refused while the kill-switch file exists (checked again inside the ledger transaction); the operator types the account name to confirm; clears any halt. A loss still over its limit halts again at the next valuation |
//! | operator only | refused when stdin or stdout is not a terminal (a piped `y` is never accepted) or `TENGU_AGENT_IPC` / `TENGU_AGENT_NAME` is set (an agent process) |

use std::io::IsTerminal;
use std::path::PathBuf;

use anyhow::{anyhow, bail, Result};
use clap::Subcommand;

use crate::adapters::outbound::paper_store::{kill_switch_state, ledger_path, SqlitePaperLedger};
use crate::config::paths::resolve_tengu_home;
use crate::config::risk::{PaperConfig, RiskConfig};
use crate::config::Config;
use crate::domain::observation::{now_ms, Field};
use crate::ports::paper::PaperLedger;

#[derive(Subcommand)]
pub(super) enum RiskAction {
    /// Risk state, cash and positions of every ledger account (read-only).
    Status {
        /// One account; default: every account in the ledger.
        #[arg(long)]
        account: Option<String>,
    },
    /// Halt new entries (reason `operator`); reduce-only exits still pass.
    /// Operator at a terminal only.
    Halt {
        /// Default: `[risk] account`.
        #[arg(long)]
        account: Option<String>,
    },
    /// Clear a halt: type the account name to confirm. Operator at a
    /// terminal only; refused while the kill-switch file exists.
    Resume {
        /// Default: `[risk] account`.
        #[arg(long)]
        account: Option<String>,
    },
}

/// Env vars that mark an agent process (`run-agent` children, the bridge
/// under one).
const AGENT_ENV: [&str; 2] = ["TENGU_AGENT_IPC", "TENGU_AGENT_NAME"];

/// What the commands act on.
pub(super) struct RiskTarget {
    /// `[risk]`, `kill_switch_file` expanded.
    pub risk: RiskConfig,
    pub paper: PaperConfig,
    /// `<TENGU_HOME>/state/<xmarket.state>` — the ledger's directory.
    pub state_dir: PathBuf,
}

impl RiskTarget {
    fn from_config(config: &Config) -> Result<Self> {
        let risk = config
            .risk
            .as_ref()
            .ok_or_else(|| anyhow!("no [risk] section: `tengu risk` needs a paper-trading sandbox (--sandbox <name>)"))?
            .resolved();
        let paper = config
            .paper
            .clone()
            .ok_or_else(|| anyhow!("no [paper] section next to [risk]"))?;
        let xmarket = config.xmarket.as_ref().ok_or_else(|| {
            anyhow!("no [xmarket] section: the ledger lives in <TENGU_HOME>/state/<xmarket.state>/ledger.db")
        })?;
        Ok(Self {
            risk,
            paper,
            state_dir: xmarket.state_dir(&resolve_tengu_home()),
        })
    }

    fn account(&self, account: Option<String>) -> String {
        account.unwrap_or_else(|| self.risk.account.clone())
    }
}

/// The operator's terminal; a scripted one in tests.
pub(super) trait Console {
    /// stdin and stdout are both terminals.
    fn is_tty(&self) -> bool;
    fn read_line(&mut self) -> std::io::Result<String>;
    fn say(&mut self, line: &str);
}

struct Terminal;

impl Console for Terminal {
    fn is_tty(&self) -> bool {
        std::io::stdin().is_terminal() && std::io::stdout().is_terminal()
    }
    fn read_line(&mut self) -> std::io::Result<String> {
        let mut line = String::new();
        std::io::stdin().read_line(&mut line)?;
        Ok(line)
    }
    fn say(&mut self, line: &str) {
        println!("{line}");
    }
}

pub(super) async fn run_risk(config: &Config, action: RiskAction) -> Result<()> {
    let target = RiskTarget::from_config(config)?;
    let agent = AGENT_ENV
        .into_iter()
        .find(|k| std::env::var_os(k).is_some());
    run(&target, action, &mut Terminal, agent, now_ms()).await
}

async fn run(
    t: &RiskTarget,
    action: RiskAction,
    console: &mut dyn Console,
    agent_env: Option<&str>,
    now_ms: i64,
) -> Result<()> {
    match action {
        RiskAction::Status { account } => status(t, account, console, now_ms).await,
        RiskAction::Halt { account } => {
            operator_only("halt", console, agent_env)?;
            halt(t, t.account(account), console, now_ms).await
        }
        RiskAction::Resume { account } => {
            operator_only("resume", console, agent_env)?;
            resume(t, t.account(account), console, now_ms).await
        }
    }
}

/// `halt` / `resume` belong to the operator at a terminal (module table).
fn operator_only(what: &str, console: &dyn Console, agent_env: Option<&str>) -> Result<()> {
    if let Some(var) = agent_env {
        bail!(
            "tengu risk {what} refused: {var} is set (an agent process) — only the operator \
             at a terminal may {what}"
        );
    }
    if !console.is_tty() {
        bail!(
            "tengu risk {what} refused: stdin / stdout is not a terminal — piped input is never \
             accepted"
        );
    }
    Ok(())
}

/// RFC 3339 UTC, seconds (`2026-10-03T00:00:00Z`); the raw ms if out of range.
fn iso(ms: i64) -> String {
    chrono::DateTime::from_timestamp_millis(ms).map_or_else(
        || ms.to_string(),
        |t| t.to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
    )
}

fn kill_switch_line(t: &RiskTarget) -> String {
    let path = t.risk.kill_switch_file.display();
    match kill_switch_state(&t.risk.kill_switch_file) {
        Field::Ok { value: true } => {
            format!("kill-switch file {path}: PRESENT — every account is halted")
        }
        Field::Ok { value: false } => format!("kill-switch file {path}: absent"),
        Field::Error { error } => format!(
            "kill-switch file {path}: UNKNOWN ({}) — entries are refused",
            error.message
        ),
        Field::Absent => format!("kill-switch file {path}: unknown"),
    }
}

async fn status(
    t: &RiskTarget,
    account: Option<String>,
    console: &mut dyn Console,
    now_ms: i64,
) -> Result<()> {
    let path = ledger_path(&t.state_dir);
    console.say(&kill_switch_line(t));
    if !path.exists() {
        console.say(&format!("no ledger yet: {}", path.display()));
        return Ok(());
    }
    console.say(&format!("ledger {}", path.display()));
    let ledger = SqlitePaperLedger::open(&t.state_dir)?;
    let names = match account {
        Some(a) => vec![a],
        None => ledger.accounts().await?,
    };
    if names.is_empty() {
        console.say("no accounts yet");
    }
    for name in names {
        let s = ledger.snapshot(&name, now_ms).await?;
        let a = &s.account;
        console.say(&format!(
            "account {name}: initial {:.2} USD, cash {:.2} USD, orders in the last 60 s {}, resting {}",
            a.initial_cash_usd, a.cash_usd, s.orders_last_min, s.open_orders
        ));
        let halt = match s.risk.effective_halt(now_ms) {
            Some(h) => format!(
                "  halt: {} since {} (clear: tengu risk resume --account {name})",
                h.reason.as_str(),
                iso(h.since_ms)
            ),
            None => "  halt: none".to_string(),
        };
        console.say(&halt);
        match (s.risk.day_utc_ms, s.risk.day_start_equity_usd) {
            (Some(day), Some(eq)) => console.say(&format!(
                "  day {} (UTC): start equity {eq:.2} USD",
                iso(day)
            )),
            (Some(day), None) => console.say(&format!(
                "  day {} (UTC): start equity unknown (no valuation yet)",
                iso(day)
            )),
            _ => console.say("  day: not rolled yet (no gate call or risk_status read)"),
        }
        for p in a.open_positions() {
            let exit = s
                .exit_at_ms
                .get(&p.instrument)
                .map_or(String::new(), |t| format!(", exit by {}", iso(*t)));
            console.say(&format!(
                "  position {} qty {} avg {} opened {}{exit}",
                p.instrument,
                p.qty,
                p.avg_px.map_or("-".to_string(), |x| x.to_string()),
                p.opened_ms.map_or("-".to_string(), iso),
            ));
        }
        for d in ledger.decisions(&name, 5).await? {
            let verdict = if d.verdict.allow { "allow" } else { "deny" };
            console.say(&format!(
                "  verdict {} {verdict} {} {} {} by {} call {}",
                iso(d.ts_ms),
                d.verdict.rule,
                d.instrument,
                d.client_order_id,
                d.tool.as_deref().unwrap_or("-"),
                d.call_id.as_deref().unwrap_or("-"),
            ));
        }
    }
    Ok(())
}

async fn halt(t: &RiskTarget, name: String, console: &mut dyn Console, now_ms: i64) -> Result<()> {
    let ledger = SqlitePaperLedger::open(&t.state_dir)?;
    if name == t.risk.account {
        ledger
            .open_account(&name, t.paper.initial_cash_usd, now_ms)
            .await?;
    }
    let s = ledger
        .update_risk_state(
            &name,
            now_ms,
            Box::new(move |s| Ok(s.risk.halt_operator(now_ms))),
        )
        .await?;
    let h = s
        .risk
        .effective_halt(now_ms)
        .ok_or_else(|| anyhow!("account {name}: the halt was not recorded"))?;
    console.say(&format!(
        "account {name}: halted ({}) since {} — entries deny `halted`, reduce-only exits pass",
        h.reason.as_str(),
        iso(h.since_ms)
    ));
    Ok(())
}

async fn resume(
    t: &RiskTarget,
    name: String,
    console: &mut dyn Console,
    now_ms: i64,
) -> Result<()> {
    let kill = t.risk.kill_switch_file.clone();
    match kill_switch_state(&kill) {
        Field::Ok { value: false } => {}
        Field::Ok { value: true } => bail!(
            "tengu risk resume refused: the kill-switch file {} is present — remove it first",
            kill.display()
        ),
        other => bail!(
            "tengu risk resume refused: cannot tell whether the kill-switch file {} exists ({:?})",
            kill.display(),
            other.error().map(|e| e.message.as_str())
        ),
    }
    let path = ledger_path(&t.state_dir);
    if !path.exists() {
        bail!("no ledger yet ({}): nothing to resume", path.display());
    }
    let ledger = SqlitePaperLedger::open(&t.state_dir)?;
    let s = ledger.snapshot(&name, now_ms).await?;
    let Some(h) = s.risk.effective_halt(now_ms).cloned() else {
        console.say(&format!("account {name} is not halted: nothing to resume"));
        return Ok(());
    };
    console.say(&format!(
        "account {name} is halted ({}) since {}. Type the account name to resume:",
        h.reason.as_str(),
        iso(h.since_ms)
    ));
    let typed = console.read_line()?;
    if typed.trim() != name {
        bail!(
            "tengu risk resume refused: `{}` is not the account name `{name}` — nothing changed",
            typed.trim()
        );
    }
    ledger
        .update_risk_state(
            &name,
            now_ms,
            Box::new(move |s| {
                let present = kill_switch_state(&kill).value() != Some(&false);
                s.risk.resume(now_ms, present)
            }),
        )
        .await?;
    console.say(&format!(
        "account {name} resumed — every entry is gated again; a loss still over its limit halts at the next valuation"
    ));
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;
    use crate::domain::xm::risk::{Halt, HaltReason};

    /// 2026-10-03 12:00 UTC.
    const NOW: i64 = 1_791_028_800_000;

    struct Scripted {
        tty: bool,
        input: Vec<String>,
        out: Vec<String>,
    }

    impl Console for Scripted {
        fn is_tty(&self) -> bool {
            self.tty
        }
        fn read_line(&mut self) -> std::io::Result<String> {
            Ok(if self.input.is_empty() {
                String::new()
            } else {
                self.input.remove(0)
            })
        }
        fn say(&mut self, line: &str) {
            self.out.push(line.to_string());
        }
    }

    fn console(tty: bool, input: &[&str]) -> Scripted {
        Scripted {
            tty,
            input: input.iter().map(|s| format!("{s}\n")).collect(),
            out: Vec::new(),
        }
    }

    fn target(dir: &Path) -> RiskTarget {
        let risk = format!(
            r#"
account = "xmarket"
mode = "paper"
venues = ["hyperliquid"]
min_lifecycle = "paper_tradable"
instruments_allow = ["hyperliquid:xyz:TSLA"]
instruments_deny = []
max_order_notional_usd = 25
max_position_notional_usd = 50
max_asset_exposure_usd = 50
max_venue_exposure_usd = 100
max_gross_exposure_usd = 100
max_net_exposure_usd = 100
max_leverage = 1
daily_loss_limit_usd = 10
total_loss_limit_usd = 25
min_edge_bps = 10
max_slippage_bps = 30
min_depth_usd = 250
require_hedge_for = []
max_skew_ms = 5000
max_orders_per_min = 6
max_open_orders = 4
kill_switch_file = "{}"
allow_reduce_degraded = true
max_data_age_ms = {{ book = 5000, ctx = 20000, reference = 60000, quote = 20000 }}
"#,
            dir.join("KILL").display()
        );
        RiskTarget {
            risk: toml::from_str(&risk).unwrap(),
            paper: toml::from_str(
                "initial_cash_usd = 100\nlatency_ms = 250\nlatency_jitter_ms = 100\n\
                 fee_tier = 0\nstaking_discount_pct = 0\norder_types = [\"market\"]\n",
            )
            .unwrap(),
            state_dir: dir.join("state"),
        }
    }

    async fn halt_of(t: &RiskTarget) -> Option<Halt> {
        let l = SqlitePaperLedger::open(&t.state_dir).unwrap();
        l.snapshot("xmarket", NOW).await.unwrap().risk.halt
    }

    fn halt_now() -> RiskAction {
        RiskAction::Halt { account: None }
    }

    fn resume_now() -> RiskAction {
        RiskAction::Resume { account: None }
    }

    /// No TTY (piped input) or an agent's env: refused before anything is
    /// opened — a piped `xmarket` never resumes.
    #[tokio::test]
    async fn halt_and_resume_refuse_without_a_tty_or_from_an_agent() {
        let dir = tempfile::tempdir().unwrap();
        let t = target(dir.path());
        for (tty, agent, want) in [
            (false, None, "not a terminal"),
            (true, Some("TENGU_AGENT_IPC"), "TENGU_AGENT_IPC is set"),
            (true, Some("TENGU_AGENT_NAME"), "TENGU_AGENT_NAME is set"),
            (false, Some("TENGU_AGENT_IPC"), "TENGU_AGENT_IPC is set"),
        ] {
            for (what, action) in [("halt", halt_now()), ("resume", resume_now())] {
                let mut c = console(tty, &["xmarket"]);
                let e = run(&t, action, &mut c, agent, NOW).await.unwrap_err();
                let e = e.to_string();
                assert!(e.contains(&format!("tengu risk {what} refused")), "{e}");
                assert!(e.contains(want), "{e}");
                assert_eq!(c.input.len(), 1, "nothing was read");
            }
        }
        assert!(!ledger_path(&t.state_dir).exists(), "nothing was opened");
        assert_eq!(AGENT_ENV, ["TENGU_AGENT_IPC", "TENGU_AGENT_NAME"]);
    }

    #[tokio::test]
    async fn halt_then_resume_with_the_typed_account_name() {
        let dir = tempfile::tempdir().unwrap();
        let t = target(dir.path());
        let mut c = console(true, &[]);
        run(&t, halt_now(), &mut c, None, NOW).await.unwrap();
        let operator = Some(Halt {
            reason: HaltReason::Operator,
            since_ms: NOW,
        });
        assert_eq!(halt_of(&t).await, operator);
        assert!(c.out[0].contains("account xmarket: halted (operator) since 2026-10-03T12:00:00Z"));
        // A wrong name changes nothing.
        let mut c = console(true, &["xmarkets"]);
        let e = run(&t, resume_now(), &mut c, None, NOW + 1)
            .await
            .unwrap_err();
        assert!(
            e.to_string().contains("`xmarkets` is not the account name"),
            "{e}"
        );
        assert_eq!(halt_of(&t).await, operator);
        // The right one clears it.
        let mut c = console(true, &["xmarket"]);
        run(&t, resume_now(), &mut c, None, NOW + 2).await.unwrap();
        assert_eq!(halt_of(&t).await, None);
        assert!(c.out.last().unwrap().contains("account xmarket resumed"));
        // Nothing left to resume: no prompt.
        let mut c = console(true, &[]);
        run(&t, resume_now(), &mut c, None, NOW + 3).await.unwrap();
        assert_eq!(c.out, ["account xmarket is not halted: nothing to resume"]);
    }

    #[tokio::test]
    async fn resume_is_refused_while_the_kill_switch_file_exists() {
        let dir = tempfile::tempdir().unwrap();
        let t = target(dir.path());
        run(&t, halt_now(), &mut console(true, &[]), None, NOW)
            .await
            .unwrap();
        std::fs::write(&t.risk.kill_switch_file, "").unwrap();
        let mut c = console(true, &["xmarket"]);
        let e = run(&t, resume_now(), &mut c, None, NOW + 1)
            .await
            .unwrap_err();
        assert!(
            e.to_string().contains("is present — remove it first"),
            "{e}"
        );
        assert!(halt_of(&t).await.is_some());
        assert_eq!(c.input.len(), 1, "refused before the prompt");
    }

    /// `status` needs no TTY, never creates the ledger, never writes.
    #[tokio::test]
    async fn status_is_read_only() {
        let dir = tempfile::tempdir().unwrap();
        let t = target(dir.path());
        let mut c = console(false, &[]);
        run(
            &t,
            RiskAction::Status { account: None },
            &mut c,
            Some("TENGU_AGENT_IPC"),
            NOW,
        )
        .await
        .unwrap();
        assert!(
            c.out.iter().any(|l| l.starts_with("no ledger yet")),
            "{:?}",
            c.out
        );
        assert!(c.out[0].ends_with("KILL: absent"), "{:?}", c.out);
        assert!(!ledger_path(&t.state_dir).exists());
        run(&t, halt_now(), &mut console(true, &[]), None, NOW)
            .await
            .unwrap();
        let before = SqlitePaperLedger::open(&t.state_dir)
            .unwrap()
            .snapshot("xmarket", NOW)
            .await
            .unwrap();
        let mut c = console(false, &[]);
        run(
            &t,
            RiskAction::Status { account: None },
            &mut c,
            None,
            NOW + 5_000,
        )
        .await
        .unwrap();
        let text = c.out.join("\n");
        for want in [
            "account xmarket: initial 100.00 USD, cash 100.00 USD",
            "halt: operator since 2026-10-03T12:00:00Z",
            "day: not rolled yet",
        ] {
            assert!(text.contains(want), "{want}: {text}");
        }
        let after = SqlitePaperLedger::open(&t.state_dir)
            .unwrap()
            .snapshot("xmarket", NOW)
            .await
            .unwrap();
        assert_eq!(after, before, "status wrote nothing");
        let e = run(
            &t,
            RiskAction::Status {
                account: Some("nobody".into()),
            },
            &mut console(false, &[]),
            None,
            NOW,
        )
        .await
        .unwrap_err();
        assert!(
            e.to_string().contains("unknown paper account `nobody`"),
            "{e}"
        );
    }
}
