//! `tengu paths [--sandbox <s>] [--json]` — where one sandbox reads and
//! writes, each path with its size on disk (`missing` when absent):
//!
//! | Group | Rows |
//! |---|---|
//! | `home` | `TENGU_HOME` + where it came from (exported · the `.env` file that set it · default), the config file |
//! | `state` | the `runtime.db` / heartbeat dir, `[xmarket]` state (+ recorder day files), `[sources]` state (+ the SOE profile), evidence vaults, `solana-writes.db` |
//! | `logs` | `tengu.log`, the egress audit, `decisions.jsonl`, `risk.jsonl`, this sandbox's trace dir, `maps/` |
//! | `skills` · `memory` | the managed skill tier, the vectors of an agent with no workspace |
//! | `workspace` | each agent's workspace, then the harness files in it: every `.tengu/` entry, `memory/`, `.tengu-attachments/` |
//!
//! Every path comes from the code that uses it (`config::paths::Layout`, the
//! config sections, `trace_root`, `resolve_memory_store_path`). Read-only: no
//! secrets, egress or `[keys]` session; never creates a dir.

use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use serde::Serialize;

use crate::adapters::outbound::solana::writes_store::WRITES_DB;
use crate::adapters::outbound::trace_store::trace_root;
use crate::bootstrap::memory::resolve_memory_store_path;
use crate::bootstrap::runtime::runner_name;
use crate::config::builder::sandbox_name_error;
use crate::config::paths::{
    absolute_path, default_config_path, expand_tilde, home_source, HomeSource, Layout,
};
use crate::config::soe::SoeConfig;
use crate::config::Config;

/// One path (module table).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct PathRow {
    pub group: &'static str,
    pub name: String,
    pub path: PathBuf,
    /// Bytes on disk (a dir: every file under it, symlinks not followed);
    /// `None` when the path does not exist.
    pub bytes: Option<u64>,
}

/// The harness files a workspace may hold (module table).
const WORKSPACE_FILES: [&str; 2] = ["memory", ".tengu-attachments"];

/// `tengu paths`.
pub(crate) fn run_paths(
    config: Option<PathBuf>,
    sandbox: Option<String>,
    json: bool,
) -> Result<()> {
    let config = load(config, sandbox)?;
    let rows = path_rows(&config, &Layout::current(), &home_source());
    if json {
        println!("{}", serde_json::to_string_pretty(&rows)?);
    } else {
        print!("{}", render(&rows));
    }
    Ok(())
}

/// The config as the sandbox's commands load it — without installing its
/// egress or signing a `[keys]` session.
fn load(config: Option<PathBuf>, sandbox: Option<String>) -> Result<Config> {
    let Some(name) = sandbox else {
        let path = config.unwrap_or_else(default_config_path);
        return if path.exists() {
            Config::load(&path).with_context(|| format!("loading {}", path.display()))
        } else {
            Ok(Config::default())
        };
    };
    if let Some(why) = sandbox_name_error(&name) {
        bail!("--sandbox {why}");
    }
    let path = PathBuf::from("sandboxes").join(&name).join("config.toml");
    let mut cfg = Config::load(&path)
        .with_context(|| format!("loading sandbox '{name}' from {}", path.display()))?;
    cfg.sandbox_name = Some(name);
    Ok(cfg)
}

/// Every row of `config` under `layout`, sized (module table).
pub(crate) fn path_rows(config: &Config, layout: &Layout, source: &HomeSource) -> Vec<PathRow> {
    let home = layout.home();
    let mut rows: Vec<(&'static str, String, PathBuf)> = Vec::new();
    let mut add = |group, name: &str, path: PathBuf| rows.push((group, name.to_string(), path));

    match source {
        HomeSource::Env => add("home", "TENGU_HOME (exported)", home.to_path_buf()),
        HomeSource::DotEnv(file) => {
            add("home", "TENGU_HOME (from .env)", home.to_path_buf());
            add("home", ".env that set it", file.clone());
        }
        HomeSource::Default => add("home", "TENGU_HOME (default)", home.to_path_buf()),
    }
    add(
        "home",
        "config",
        config
            .loaded_from
            .as_deref()
            .map_or_else(|| layout.config(), absolute_path),
    );

    let xm = config.xmarket.as_ref().map(|x| x.state_dir(home));
    let sources = config.sources.as_ref().map(|s| s.state_dir(home));
    add(
        "state",
        "runtime.db + heartbeat",
        crate::config::runtime::state_dir(xm.as_deref(), sources.as_deref(), home),
    );
    if let Some(x) = &config.xmarket {
        add("state", "[xmarket] state", x.state_dir(home));
        if config.recorder.enabled {
            add("state", "recorder day files", x.history_dir(home));
        }
    }
    if let Some(dir) = &sources {
        add("state", "[sources] state", dir.clone());
        if config.soe.is_some() {
            add("state", "SOE profile", SoeConfig::profile_path(dir));
        }
    }
    add("state", "evidence vaults", layout.state_dir("evidence"));
    add("state", WRITES_DB, layout.state().join(WRITES_DB));

    add("logs", "tengu.log", layout.tengu_log());
    if let Some(file) = config.egress.audit_file(layout) {
        add("logs", "egress audit", file);
    }
    add("logs", "decisions.jsonl", layout.decisions_log());
    add("logs", "risk.jsonl", layout.risk_log());
    let runner = runner_name(config);
    add("logs", "trace", trace_root(home).join(&runner));
    add("logs", "maps", layout.maps());

    add("skills", "managed tier", layout.skills());
    add(
        "memory",
        "agents without a workspace",
        resolve_memory_store_path(&config.memory, None),
    );

    let mut seen: Vec<PathBuf> = Vec::new();
    let mut agents: Vec<_> = config.agents.iter().collect();
    agents.sort_by(|a, b| a.0.cmp(b.0));
    for (agent, cfg) in agents {
        let Some(ws) = cfg.workspace.as_deref().map(expand_tilde) else {
            continue;
        };
        add("workspace", agent, ws.clone());
        if seen.contains(&ws) {
            continue;
        }
        let mut files: Vec<PathBuf> = std::fs::read_dir(ws.join(".tengu"))
            .into_iter()
            .flatten()
            .filter_map(|e| e.ok().map(|e| e.path()))
            .collect();
        files.sort();
        files.extend(WORKSPACE_FILES.iter().map(|f| ws.join(f)));
        for file in files.into_iter().filter(|f| f.exists()) {
            let rel = file
                .strip_prefix(&ws)
                .unwrap_or(&file)
                .display()
                .to_string();
            add("workspace", &format!("{agent} · {rel}"), file);
        }
        seen.push(ws);
    }

    rows.into_iter()
        .map(|(group, name, path)| PathRow {
            bytes: size(&path),
            group,
            name,
            path,
        })
        .collect()
}

/// Bytes under `path` (symlinks not followed); `None` when it is missing.
fn size(path: &Path) -> Option<u64> {
    let meta = std::fs::symlink_metadata(path).ok()?;
    if !meta.is_dir() {
        return Some(meta.len());
    }
    let mut total = 0;
    let mut stack = vec![path.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir).into_iter().flatten().flatten() {
            match entry.metadata() {
                Ok(m) if m.is_dir() => stack.push(entry.path()),
                Ok(m) => total += m.len(),
                Err(_) => {}
            }
        }
    }
    Some(total)
}

fn human(bytes: Option<u64>) -> String {
    let Some(b) = bytes else {
        return "missing".to_string();
    };
    const UNITS: [&str; 4] = ["KB", "MB", "GB", "TB"];
    if b < 1024 {
        return format!("{b} B");
    }
    let mut v = b as f64 / 1024.0;
    let mut unit = 0;
    while v >= 1024.0 && unit < UNITS.len() - 1 {
        v /= 1024.0;
        unit += 1;
    }
    format!("{v:.1} {}", UNITS[unit])
}

/// One line per row: group, name, size, full path (never shortened).
fn render(rows: &[PathRow]) -> String {
    let name_w = rows
        .iter()
        .map(|r| r.name.chars().count())
        .max()
        .unwrap_or(0);
    let mut out = String::new();
    for r in rows {
        out.push_str(&format!(
            "{:<9}  {:<name_w$}  {:>9}  {}\n",
            r.group,
            r.name,
            human(r.bytes),
            r.path.display()
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A sandbox's state, logs and workspace files come out sized, from the
    /// config and the home alone; what is absent says so.
    #[test]
    fn a_sandbox_lists_its_paths_with_sizes() {
        let tmp = tempfile::TempDir::new().unwrap();
        let home = tmp.path().join("home");
        let ws = tmp.path().join("ws");
        std::fs::create_dir_all(home.join("state/xm-paths")).unwrap();
        std::fs::write(home.join("state/xm-paths/ledger.db"), [0u8; 10]).unwrap();
        std::fs::create_dir_all(ws.join(".tengu")).unwrap();
        std::fs::write(ws.join(".tengu/observations.db"), [0u8; 5]).unwrap();
        let mut config: Config = toml::from_str(&format!(
            "[xmarket]\nstate = \"xm-paths\"\n\
             [agents.main]\nengine = \"openrouter\"\nmodel = \"m\"\nworkspace = \"{}\"\n\
             [agents.helper]\nengine = \"openrouter\"\nmodel = \"m\"\nworkspace = \"{}\"\n",
            ws.display(),
            ws.display()
        ))
        .unwrap();
        config.sandbox_name = Some("paths-lab".into());
        let rows = path_rows(
            &config,
            &Layout::at(&home),
            &HomeSource::DotEnv(tmp.path().join(".env")),
        );
        let row = |name: &str| {
            rows.iter()
                .find(|r| r.name == name)
                .unwrap_or_else(|| panic!("{name}: {rows:#?}"))
        };
        assert_eq!(row("TENGU_HOME (from .env)").path, home);
        assert_eq!(row(".env that set it").path, tmp.path().join(".env"));
        let xm = row("[xmarket] state");
        assert_eq!(
            (xm.path.clone(), xm.bytes),
            (home.join("state/xm-paths"), Some(10))
        );
        assert_eq!(
            row("runtime.db + heartbeat").path,
            home.join("state/xm-paths")
        );
        assert_eq!(row("trace").path, home.join("logs/trace/paths-lab"));
        assert_eq!(row("tengu.log").bytes, None);
        assert_eq!(row("egress audit").path, home.join("logs/egress.jsonl"));
        let obs = row("helper · .tengu/observations.db");
        assert_eq!(
            (obs.path.clone(), obs.bytes),
            (ws.join(".tengu/observations.db"), Some(5))
        );
        // A shared workspace lists its files once, under the first agent by name.
        assert!(rows
            .iter()
            .all(|r| r.name != "main · .tengu/observations.db"));
        assert_eq!(row("main").bytes, Some(5));
        assert!(render(&rows).contains(&format!("{}\n", home.join("state/xm-paths").display())));
        assert_eq!(human(Some(1536)), "1.5 KB");
    }
}
