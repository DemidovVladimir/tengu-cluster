//! Studio control rules — Play / Stop / send-event (`TENGU_STUDIO_PLAN.md`
//! § 6, ST-30): which action is allowed now, and why not, from what this
//! Studio process owns ([`Phase`]) and what the heartbeat shows
//! ([`Seen`]). Pure; the IO — starting the runtime (`tengu run`'s
//! `start_session`), the stop signal, the loop dispatch, the `studio.control`
//! events — is `adapters/inbound/studio/control.rs`. The page draws the
//! [`ControlView`] it is served and decides nothing.
//!
//! | State ([`ControlState`]) | When | Tone | play | stop | event |
//! |---|---|---|---|---|---|
//! | `idle` | this Studio started nothing; no live heartbeat | plain | ✓ | — | — |
//! | `attached` | not ours: a fresh heartbeat (`running` / `stopping`) of another holder — a CLI `tengu run`, another Studio | plain | — (read-only) | — (stop it where it runs) | — |
//! | `starting` | our Play is taking the lease, building loops and feeds | amber | — | — | — |
//! | `running` | our runtime runs | green | — (second Play) | ✓ | ✓ |
//! | `stopping` | our runtime drains (`shutdown_grace_secs`) | amber | — | — | — |
//! | `stopped` | ours stopped cleanly | plain | ✓ | — | — |
//! | `failed` | ours failed to start, or stopped failed (a lost lease, a task that died) | red | ✓ | — | — |
//!
//! Control off ([`ControlPolicy`], `config/studio.rs`) refuses every action
//! with its reason (403); a state that forbids one is a conflict (409).

use serde::Serialize;
use serde_json::Value;

use crate::config::studio::ControlPolicy;
use crate::domain::runtime::RunState;
use crate::domain::trace::Tone;

/// This Studio's own runtime, as the control use case tracks it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Phase {
    /// Never started (or a Play refused before it took the lease).
    #[default]
    Idle,
    Starting,
    Running,
    Stopping,
    /// It ran and stopped cleanly.
    Stopped,
    /// Its start failed, or it stopped failed.
    Failed,
}

/// What the heartbeat file says now (`run-<sandbox>.json`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct Seen {
    /// The lease holder `<host>:<pid>:<uuid>`.
    pub holder: String,
    pub pid: u32,
    pub state: RunState,
    /// Written within `[runtime] heartbeat_stale_secs`.
    pub fresh: bool,
}

impl Seen {
    /// A runtime that runs now: fresh and not `stopped`.
    fn live(&self) -> bool {
        self.fresh && self.state != RunState::Stopped
    }
}

/// Reads the heartbeat now (`bootstrap::studio::StudioContext::seen_fn`);
/// `None` without a readable one.
pub(crate) type SeenFn = std::sync::Arc<dyn Fn() -> Option<Seen> + Send + Sync>;

/// The control state the page shows (module table).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ControlState {
    Idle,
    Attached,
    Starting,
    Running,
    Stopping,
    Stopped,
    Failed,
}

impl ControlState {
    /// The badge's tone (module table).
    pub(crate) fn tone(self) -> Tone {
        match self {
            ControlState::Running => Tone::Green,
            ControlState::Starting | ControlState::Stopping => Tone::Amber,
            ControlState::Failed => Tone::Red,
            ControlState::Idle | ControlState::Attached | ControlState::Stopped => Tone::Plain,
        }
    }
}

/// `phase` of our runtime (holder `own` while we hold one), `seen` the
/// heartbeat → the state (module table).
pub(crate) fn state(phase: Phase, own: Option<&str>, seen: Option<&Seen>) -> ControlState {
    match phase {
        Phase::Starting => return ControlState::Starting,
        Phase::Running => return ControlState::Running,
        Phase::Stopping => return ControlState::Stopping,
        Phase::Idle | Phase::Stopped | Phase::Failed => {}
    }
    if seen.is_some_and(|s| s.live() && Some(s.holder.as_str()) != own) {
        return ControlState::Attached;
    }
    match phase {
        Phase::Stopped => ControlState::Stopped,
        Phase::Failed => ControlState::Failed,
        _ => ControlState::Idle,
    }
}

/// A control request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Action {
    Play,
    Stop,
    Event,
}

impl Action {
    pub(crate) const ALL: [Action; 3] = [Action::Play, Action::Stop, Action::Event];

    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Action::Play => "play",
            Action::Stop => "stop",
            Action::Event => "event",
        }
    }
}

/// Why an action is refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Refusal {
    /// Control is off for this Studio (HTTP 403).
    Off(String),
    /// Not in this state (HTTP 409).
    Conflict(String),
}

impl Refusal {
    pub(crate) fn why(&self) -> &str {
        match self {
            Refusal::Off(w) | Refusal::Conflict(w) => w,
        }
    }
}

fn other(seen: Option<&Seen>) -> String {
    match seen {
        Some(s) => format!("`{}` (pid {})", s.holder, s.pid),
        None => "another process".into(),
    }
}

/// Whether `action` may run now (module table).
pub(crate) fn check(
    action: Action,
    policy: &ControlPolicy,
    state: ControlState,
    seen: Option<&Seen>,
) -> Result<(), Refusal> {
    use ControlState as S;
    if !policy.enabled {
        return Err(Refusal::Off(format!("control {}", policy.why)));
    }
    let conflict = |s: String| Err(Refusal::Conflict(s));
    match (action, state) {
        (Action::Play, S::Idle | S::Stopped | S::Failed) => Ok(()),
        (Action::Stop | Action::Event, S::Running) => Ok(()),
        (Action::Play, S::Attached) => conflict(format!(
            "attached read-only: {} holds this sandbox's runtime lease — stop it where it runs, \
             then Play",
            other(seen)
        )),
        (Action::Stop, S::Attached) => conflict(format!(
            "attached read-only: this Studio did not start {} — stop it where it runs (Ctrl-C, \
             or SIGTERM to its pid)",
            other(seen)
        )),
        (Action::Event, S::Attached) => conflict(format!(
            "attached read-only: events go to a runtime this Studio started; {} is another \
             process's",
            other(seen)
        )),
        (Action::Play, S::Running) => {
            conflict("already running: this Studio started it — Stop it first".into())
        }
        (_, S::Starting) => conflict("starting: wait until the runtime runs".into()),
        (Action::Play, S::Stopping) => conflict("stopping: wait until it has stopped".into()),
        (Action::Stop, S::Stopping) => conflict("already stopping".into()),
        (Action::Event, S::Stopping) => conflict("stopping: no new events".into()),
        (Action::Stop, S::Idle | S::Stopped | S::Failed) => {
            conflict("nothing to stop: this Studio runs no runtime".into())
        }
        (Action::Event, S::Idle | S::Stopped | S::Failed) => {
            conflict("no runtime: Play first".into())
        }
    }
}

/// Most scenarios listed; a scenario file's largest size.
pub(crate) const MAX_SCENARIOS: usize = 64;
pub(crate) const MAX_SCENARIO_BYTES: u64 = 64 * 1024;

/// A named event the page may send to the runtime this Studio runs: one
/// `<sandbox dir>/scenarios/<name>.json` — the files `tengu decide --event`
/// takes (`bootstrap::studio` reads them once). Only a name crosses the
/// HTTP boundary: the page never sends an event body.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub(crate) struct Scenario {
    pub name: String,
    /// Where it was read (`~/…`).
    pub file: String,
    /// The event as shown (redacted like a trace payload).
    pub event: Value,
    /// The event as sent to the loop — the file as is, as `tengu decide
    /// --event <file>` sends it; never served.
    #[serde(skip)]
    pub raw: Value,
}

/// The scenario name of a file in `scenarios/`: `<name>.json` with `name`
/// 1–64 of `A-Z a-z 0-9 _ -`; a `*.map.json` is an execution map (`tengu
/// decide --map`), never a loop event.
pub(crate) fn scenario_name(file_name: &str) -> Option<&str> {
    let name = file_name.strip_suffix(".json")?;
    let ok = (1..=64).contains(&name.len())
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-');
    ok.then_some(name)
}

/// The loop a scenario goes to: the one named (it must be configured), else
/// the only loop.
pub(crate) fn pick_loop<'a>(
    named: Option<&'a str>,
    loops: &'a [String],
) -> Result<&'a str, String> {
    match (named, loops) {
        (Some(l), _) if loops.iter().any(|x| x == l) => Ok(l),
        (Some(l), _) => Err(format!(
            "no decision loop `{l}` in this sandbox (loops: {})",
            loops.join(", ")
        )),
        (None, [only]) => Ok(only),
        (None, []) => Err("this sandbox has no [decision_loops.*]".into()),
        (None, _) => Err(format!(
            "name the loop: \"loop\" = one of {}",
            loops.join(", ")
        )),
    }
}

/// Each action: allowed now, or why not.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct Allowed {
    pub action: Action,
    pub ok: bool,
    /// `None` when `ok`.
    pub why_not: Option<String>,
}

/// `/api/v1/control`'s rule part: state, tone, what each button may do.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct ControlView {
    pub enabled: bool,
    pub why: String,
    pub state: ControlState,
    pub tone: Tone,
    pub actions: Vec<Allowed>,
}

pub(crate) fn view(
    policy: &ControlPolicy,
    phase: Phase,
    own: Option<&str>,
    seen: Option<&Seen>,
) -> ControlView {
    let st = state(phase, own, seen);
    let actions = Action::ALL
        .iter()
        .map(|&action| {
            let verdict = check(action, policy, st, seen);
            Allowed {
                action,
                ok: verdict.is_ok(),
                why_not: verdict.err().map(|r| r.why().to_string()),
            }
        })
        .collect();
    ControlView {
        enabled: policy.enabled,
        why: policy.why.clone(),
        state: st,
        tone: st.tone(),
        actions,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn on() -> ControlPolicy {
        ControlPolicy {
            enabled: true,
            why: "on: test".into(),
        }
    }

    fn seen(holder: &str, state: RunState, fresh: bool) -> Seen {
        Seen {
            holder: holder.into(),
            pid: 7,
            state,
            fresh,
        }
    }

    fn can(v: &ControlView) -> Vec<bool> {
        v.actions.iter().map(|a| a.ok).collect()
    }

    /// The module table, row by row: state, tone, play · stop · event.
    #[test]
    fn rule_table() {
        let p = on();
        let cli = seen("h:1:cli", RunState::Running, true);
        let rows = [
            (
                Phase::Idle,
                None,
                None,
                ControlState::Idle,
                [true, false, false],
            ),
            (
                Phase::Idle,
                None,
                Some(&cli),
                ControlState::Attached,
                [false, false, false],
            ),
            (
                Phase::Starting,
                None,
                None,
                ControlState::Starting,
                [false, false, false],
            ),
            (
                Phase::Running,
                Some("h:2:me"),
                None,
                ControlState::Running,
                [false, true, true],
            ),
            (
                Phase::Stopping,
                Some("h:2:me"),
                None,
                ControlState::Stopping,
                [false, false, false],
            ),
            (
                Phase::Stopped,
                None,
                None,
                ControlState::Stopped,
                [true, false, false],
            ),
            (
                Phase::Failed,
                None,
                None,
                ControlState::Failed,
                [true, false, false],
            ),
            // Ours stopped; the CLI took over: attached.
            (
                Phase::Stopped,
                None,
                Some(&cli),
                ControlState::Attached,
                [false, false, false],
            ),
        ];
        for (phase, own, s, want, allowed) in rows {
            let v = view(&p, phase, own, s);
            assert_eq!(v.state, want, "{phase:?}");
            assert_eq!(can(&v), allowed, "{phase:?}");
            for a in &v.actions {
                assert_eq!(a.ok, a.why_not.is_none(), "{a:?}");
            }
        }
        assert_eq!(ControlState::Running.tone(), Tone::Green);
        assert_eq!(ControlState::Failed.tone(), Tone::Red);
        assert_eq!(ControlState::Stopping.tone(), Tone::Amber);
    }

    /// A stale or `stopped` heartbeat, or our own, never attaches.
    #[test]
    fn only_a_live_foreign_heartbeat_attaches() {
        let stale = seen("h:1:cli", RunState::Running, false);
        let stopped = seen("h:1:cli", RunState::Stopped, true);
        let mine = seen("h:2:me", RunState::Running, true);
        assert_eq!(state(Phase::Idle, None, Some(&stale)), ControlState::Idle);
        assert_eq!(state(Phase::Idle, None, Some(&stopped)), ControlState::Idle);
        assert_eq!(
            state(Phase::Running, Some("h:2:me"), Some(&mine)),
            ControlState::Running
        );
        let draining = seen("h:1:cli", RunState::Stopping, true);
        assert_eq!(
            state(Phase::Failed, None, Some(&draining)),
            ControlState::Attached
        );
    }

    /// Event files are scenarios; maps and odd names are not. A loop is
    /// named, or the only one.
    #[test]
    fn scenarios_and_their_loop() {
        assert_eq!(scenario_name("act.json"), Some("act"));
        assert_eq!(scenario_name("tool-error.json"), Some("tool-error"));
        for no in [
            "uncertain.map.json",
            "act.toml",
            ".json",
            "a b.json",
            "../x.json",
        ] {
            assert_eq!(scenario_name(no), None, "{no}");
        }
        let one = vec!["demo".to_string()];
        assert_eq!(pick_loop(None, &one), Ok("demo"));
        assert_eq!(pick_loop(Some("demo"), &one), Ok("demo"));
        assert!(pick_loop(Some("x"), &one).unwrap_err().contains("`x`"));
        let two = vec!["a".to_string(), "b".to_string()];
        assert!(pick_loop(None, &two).unwrap_err().contains("one of a, b"));
        assert!(pick_loop(None, &[]).is_err());
    }

    /// Off = 403 for every action, with the policy's reason; attached
    /// refusals name the holder in full.
    #[test]
    fn refusals_say_why() {
        let off = ControlPolicy::off("off: read-only — pass --allow-control");
        for a in Action::ALL {
            let r = check(a, &off, ControlState::Running, None).unwrap_err();
            assert!(
                matches!(&r, Refusal::Off(w) if w.contains("--allow-control")),
                "{r:?}"
            );
        }
        let cli = seen(
            "host:4242:0f0e0d0c-0b0a-4908-8706-050403020100",
            RunState::Running,
            true,
        );
        let r = check(Action::Stop, &on(), ControlState::Attached, Some(&cli)).unwrap_err();
        assert!(
            matches!(&r, Refusal::Conflict(w)
                if w.contains("`host:4242:0f0e0d0c-0b0a-4908-8706-050403020100` (pid 7)")),
            "{r:?}"
        );
        let r = check(Action::Play, &on(), ControlState::Running, None).unwrap_err();
        assert!(r.why().contains("already running"), "{r:?}");
    }
}
