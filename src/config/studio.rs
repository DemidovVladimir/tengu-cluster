//! `[studio]` — Tengu Studio knobs (`tengu studio`, `adapters/inbound/studio`;
//! `TENGU_STUDIO_PLAN.md` § 6). `deny_unknown_fields`; absent = every
//! default.
//!
//! | Key | Default | Effect |
//! |---|---|---|
//! | `control` | `false` | `tengu studio` may Play (start this sandbox's runtime in its own process — the `tengu run` composition), Stop it (the graceful drain) and send a scenario event to it. `false`: read-only, unless `tengu studio --allow-control` |
//!
//! [`control_policy`] decides, once per `tengu studio` process:
//!
//! | Sandbox | Control |
//! |---|---|
//! | `[generation]`-bound (a frozen design: view-only, plan § 7) | refused — always, `--allow-control` too; `[studio] control = true` there is a load error |
//! | hardened (`[risk]`, `[soe]` or a `[solana]` signer: `hardening::requires_hardened_claude_code`) | refused — always, the same way |
//! | `[studio] control = true` (the `control-loop-lab` sandbox) | on |
//! | `--allow-control` | on |
//! | otherwise | off (read-only Studio) |

use serde::{Deserialize, Serialize};

use super::Config;

/// `[studio]` section.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StudioConfig {
    /// Play / Stop / send-event in `tengu studio` for this sandbox (module
    /// table). Refused in a `[generation]`-bound or hardened sandbox.
    #[serde(default)]
    pub control: bool,
}

/// Whether this `tengu studio` process may control the runtime, and why
/// (served in `/api/v1/control`; the page shows the controls only when
/// `enabled`).
#[cfg_attr(not(feature = "studio"), allow(dead_code))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct ControlPolicy {
    pub enabled: bool,
    pub why: String,
}

#[cfg_attr(not(feature = "studio"), allow(dead_code))]
impl ControlPolicy {
    /// Read-only (no control at all), with the reason.
    pub(crate) fn off(why: impl Into<String>) -> Self {
        Self {
            enabled: false,
            why: why.into(),
        }
    }
}

/// Why `cfg` never allows control (`None`: it may).
fn never(cfg: &Config) -> Option<String> {
    if let Some(g) = &cfg.generation {
        return Some(format!(
            "refused: this sandbox is bound to generation `{}` ([generation]) — a frozen design \
             is view-only; run a new, unbound sandbox to control one",
            g.id
        ));
    }
    if super::hardening::requires_hardened_claude_code(cfg) {
        return Some(
            "refused: hardened sandbox ([risk], [soe] or a [solana] signer) — Studio never starts \
             or stops a hardened runtime (money, signing, the SOE cycle); use `tengu run` and \
             its signals"
                .into(),
        );
    }
    None
}

/// The control policy of `cfg` for a `tengu studio` started with or
/// without `--allow-control` (module table).
#[cfg_attr(not(feature = "studio"), allow(dead_code))]
pub(crate) fn control_policy(cfg: &Config, allow_control: bool) -> ControlPolicy {
    if let Some(why) = never(cfg) {
        return ControlPolicy::off(why);
    }
    if cfg.studio.control {
        return ControlPolicy {
            enabled: true,
            why: "on: [studio] control = true in this sandbox".into(),
        };
    }
    if allow_control {
        return ControlPolicy {
            enabled: true,
            why: "on: tengu studio --allow-control".into(),
        };
    }
    ControlPolicy::off(
        "off: read-only — set [studio] control = true in the sandbox or start tengu studio with \
         --allow-control",
    )
}

/// `[studio] control = true` where control is always refused.
pub(crate) fn validation_errors(cfg: &Config) -> Vec<String> {
    match (cfg.studio.control, never(cfg)) {
        (true, Some(why)) => vec![format!("studio.control = true: {why}")],
        _ => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(extra: &str) -> Config {
        let text = format!("[agents.a]\nengine = \"openrouter\"\nmodel = \"m\"\n\n{extra}\n");
        toml::from_str(&text).unwrap()
    }

    /// No `[studio]` = read-only; `control = true` or `--allow-control` = on.
    #[test]
    fn control_off_by_default() {
        let plain = cfg("");
        assert_eq!(plain.studio, StudioConfig::default());
        let p = control_policy(&plain, false);
        assert!(!p.enabled && p.why.contains("--allow-control"), "{p:?}");
        assert!(control_policy(&plain, true).enabled);
        let on = cfg("[studio]\ncontrol = true\n");
        assert!(control_policy(&on, false).enabled);
        assert!(validation_errors(&on).is_empty());
        assert!(toml::from_str::<Config>("[studio]\ncontrl = true\n").is_err());
    }

    /// A `[generation]`-bound or hardened sandbox is never controlled —
    /// not with `--allow-control`, and `[studio] control = true` there is
    /// a load error.
    #[test]
    fn refused_for_generation_bound_and_hardened() {
        let bound = cfg("[generation]\nid = \"W1\"\nregistry = \"../../lineage\"\n");
        let p = control_policy(&bound, true);
        assert!(!p.enabled && p.why.contains("generation `W1`"), "{p:?}");
        let mut risky = cfg("");
        risky.solana.signer_key_file = Some("/k.json".into());
        let p = control_policy(&risky, true);
        assert!(!p.enabled && p.why.contains("hardened"), "{p:?}");
        let mut both =
            cfg("[studio]\ncontrol = true\n[generation]\nid = \"W1\"\nregistry = \"x\"\n");
        assert_eq!(validation_errors(&both).len(), 1, "{both:?}");
        both.generation = None;
        both.solana.signer_key_file = Some("/k.json".into());
        let e = validation_errors(&both);
        assert!(e.len() == 1 && e[0].starts_with("studio.control = true: refused: hardened"));
    }
}
