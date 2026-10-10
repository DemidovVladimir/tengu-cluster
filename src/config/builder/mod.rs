//! The Studio builder's config side (`docs/studio-builder-2026-10-10.md`):
//! the palette the page draws from, the compiler from a blueprint
//! (`domain/blueprint.rs`) to `config.toml`, the starting templates and the
//! load report. Pure but for [`load_report`], which runs the real loader
//! (`Config::load`) on a file the caller wrote.
//!
//! | Module | Does |
//! |---|---|
//! | `palette` | card kinds + fields, the wire table, engines, secret stores (`GET /api/v1/builder`) |
//! | `compile` | blueprint → TOML + per-card / per-wire status + the secrets checklist |
//! | `template` | `blank` · `team` · `telegram` |
//!
//! | Rule | Where |
//! |---|---|
//! | Owned sections (`egress`, `memory`, `studio`, `keys`, `orchestrator`, `telegram`, `webhooks`, `agents`) are rewritten; every other top-level key is kept | `compile::OWNED` |
//! | Keys never enter TOML: a secret card names an env var and its store | `compile` |
//! | Never offered: `run_command`, wallet signing, Solana sends, exec tools | `palette::offered` |
//! | A `[generation]`-bound or hardened sandbox is view-only | [`editable`] |
//! | The Cloudflare store needs a build with `[keys]` (PR #50) — probed, not compiled in | [`keys_supported`] |

pub(crate) mod compile;
pub(crate) mod palette;
pub(crate) mod template;

use std::path::Path;

use serde::Serialize;
use serde_json::Value;

use super::Config;

/// A catalog tool the palette lists (filled from the tool catalog by
/// `bootstrap/builder.rs`).
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct OfferedTool {
    pub name: String,
    pub description: String,
    pub parameters: Value,
    /// An opt-in tool (`domain::tools::WORKSPACE_TOOLS`).
    pub opt_in: bool,
    /// Advertised only with `[memory] enabled`.
    pub needs_memory: bool,
    pub group: &'static str,
}

/// What the palette and the compiler know besides the blueprint.
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct Facts {
    pub tools: Vec<OfferedTool>,
    /// (name, description) of every skill found.
    pub skills: Vec<(String, String)>,
    /// This build knows `[keys]` (the Cloudflare seal proxy).
    pub keys_supported: bool,
}

/// Whether this build's `Config` accepts a `[keys]` section — the seal
/// proxy (PR #50). Probed so the builder lights the Cloudflare store up by
/// itself once that lands.
pub(crate) fn keys_supported() -> bool {
    toml::from_str::<Config>("[keys]\n").is_ok()
}

/// Whether `name` may be a new sandbox: `[a-z0-9][a-z0-9_-]{0,39}`.
pub(crate) fn sandbox_name_error(name: &str) -> Option<String> {
    if compile::valid_name(name) {
        None
    } else {
        Some(format!(
            "'{name}': use lower-case letters, digits, - and _ (start with a letter or digit, at most 40)"
        ))
    }
}

/// Why the builder may not rewrite a sandbox whose `config.toml` is
/// `current` (`None`: it may).
pub(crate) fn editable(current: &str) -> Option<String> {
    let Ok(t) = current.parse::<toml::Table>() else {
        return None;
    };
    if t.contains_key("generation") {
        return Some(
            "bound to a frozen generation ([generation]): view-only — build a new sandbox instead"
                .into(),
        );
    }
    let signer = t
        .get("solana")
        .and_then(|s| s.as_table())
        .is_some_and(|s| s.contains_key("signer_key_file") || s.contains_key("privy_wallet_id"));
    for (key, why) in [
        ("risk", "[risk]"),
        ("paper", "[paper]"),
        ("soe", "[soe]"),
        ("xmarket", "[xmarket]"),
    ] {
        if t.contains_key(key) {
            return Some(format!(
                "a hardened sandbox ({why}): its TOML stays hand-edited"
            ));
        }
    }
    signer.then(|| "a hardened sandbox (a Solana signer): its TOML stays hand-edited".into())
}

/// What the real loader said about a config file.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub(crate) struct LoadReport {
    pub ok: bool,
    pub errors: Vec<String>,
    pub warnings: Vec<String>,
}

/// `Config::load(path)` — the parse, validation, hardening and generation
/// checks every `tengu` command runs — as a report: each validation issue
/// one error line; warnings from `validation_warnings`.
pub(crate) fn load_report(path: &Path) -> LoadReport {
    match Config::load(path) {
        Ok(cfg) => LoadReport {
            ok: true,
            errors: Vec::new(),
            warnings: cfg.validation_warnings(),
        },
        Err(e) => {
            let text = format!("{e:#}");
            let lines: Vec<String> = text
                .lines()
                .filter_map(|l| l.strip_prefix("- "))
                .map(String::from)
                .collect();
            LoadReport {
                ok: false,
                errors: if lines.is_empty() { vec![text] } else { lines },
                warnings: Vec::new(),
            }
        }
    }
}

#[cfg(test)]
mod tests;
