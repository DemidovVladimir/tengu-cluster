//! `tengu decide --sandbox <s> --loop <name> [--event file.json]` — run one
//! event through a decision loop and print the outcomes + history (args and
//! reduced result per step). Manual test path for `[decision_loops.*]`, and
//! the architect → executor hand-off in `sandboxes/jev-exec`; the
//! long-running trigger is the webhook listener.
//!
//! `tengu decide --sandbox <s> --map <file|->` runs an execution map instead
//! (`config/execution_map.rs`): the loop it names, narrowed by it, on its
//! event. A refused map lists every problem and runs nothing. The map is
//! kept as `<TENGU_HOME>/logs/maps/<sha256>.json` (canonical JSON) and
//! every audit line of the run carries `trigger = "map:<sha256>"`.
//!
//! Each run that starts is one trace recording (`RunKind::Decide`, no
//! `runtime_id`: it runs outside the runtime lease): its `run_id` is on every
//! audit line and in the output (`run_id`, `trace` = the file;
//! `tengu trace show --run <run_id>`). Stdout is the JSON alone; logs go to
//! stderr (`cli/mod.rs`).

use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{anyhow, bail, Context, Result};
use serde_json::Value;

use crate::config::execution_map::ExecutionMap;
use crate::config::Config;
use crate::domain::secrets::SecretRegistry;
use crate::domain::trace::RunKind;

pub(super) async fn run_decide(
    config: &Config,
    loop_name: Option<&str>,
    event: Option<&Path>,
    map: Option<&Path>,
    secret_registry: Arc<SecretRegistry>,
) -> Result<()> {
    let open_trace = || {
        let trace =
            crate::bootstrap::trace::open_sink(config, None, RunKind::Decide, &secret_registry);
        let ids = crate::bootstrap::trace::run_ids(&*trace, None);
        (trace, ids)
    };
    let (loop_name, event, dl, map_out, trace) = match map {
        Some(p) => {
            let map =
                ExecutionMap::parse(&read_input(p, "execution map")?).map_err(|e| anyhow!(e))?;
            if let Some(l) = loop_name.filter(|l| *l != map.loop_name) {
                bail!(
                    "--loop {l} but the execution map names loop `{}`",
                    map.loop_name
                );
            }
            let base = config.decision_loops.get(&map.loop_name).ok_or_else(|| {
                anyhow!(
                    "execution map: no [decision_loops.{}] block in this config",
                    map.loop_name
                )
            })?;
            let cfg = map
                .apply(base)
                .map_err(|errs| anyhow!("execution map refused:\n- {}", errs.join("\n- ")))?;
            let sha = map.sha256();
            let kept = keep_map(&sha, &map.canonical())?;
            let (trace, ids) = open_trace();
            let dl = crate::bootstrap::decision::build_mapped_loop(
                config,
                &map.loop_name,
                cfg,
                format!("map:{sha}"),
                Arc::clone(&secret_registry),
                ids,
            )?;
            let out = serde_json::json!({"sha256": sha, "path": kept});
            (map.loop_name, map.event, dl, Some(out), trace)
        }
        None => {
            let loop_name = loop_name.ok_or_else(|| anyhow!("--loop or --map is required"))?;
            let event: Value = match event {
                None => Value::Object(Default::default()),
                Some(p) => {
                    let raw = read_input(p, "event")?;
                    serde_json::from_str(&raw)
                        .with_context(|| format!("{} is not JSON", p.display()))?
                }
            };
            if !config.decision_loops.contains_key(loop_name) {
                bail!("no [decision_loops.{loop_name}] block in this config");
            }
            let (trace, ids) = open_trace();
            let dl = crate::bootstrap::decision::build_decision_loop(
                config,
                loop_name,
                None,
                Arc::clone(&secret_registry),
                ids,
            )?;
            (loop_name.to_string(), event, dl, None, trace)
        }
    };
    let session_id = format!("decide-{loop_name}-{}", uuid::Uuid::new_v4());
    let outcomes = dl.handle_event(&event, &session_id).await?;
    let mut out = serde_json::json!({
        "session_id": session_id,
        "outcomes": outcomes,
        "history": dl.history().await,
        "audit": crate::bootstrap::decision::audit_path(),
        "run_id": trace.run_id(),
        "trace": trace.run_id().map(|r| {
            crate::bootstrap::trace::run_path(&crate::bootstrap::runtime::runner_name(config), r)
        }),
    });
    if let (Some(m), Value::Object(o)) = (map_out, &mut out) {
        o.insert("map".into(), m);
    }
    println!("{}", serde_json::to_string_pretty(&out)?);
    Ok(())
}

/// A file's text, or stdin's for `-`.
pub(super) fn read_input(p: &Path, what: &str) -> Result<String> {
    if p.as_os_str() == "-" {
        let mut buf = String::new();
        std::io::stdin()
            .read_to_string(&mut buf)
            .with_context(|| format!("read {what} from stdin"))?;
        return Ok(buf);
    }
    std::fs::read_to_string(p).with_context(|| format!("read {what} {}", p.display()))
}

/// `<TENGU_HOME>/logs/maps/<sha256>.json` — what the Architect asked for,
/// by the identity every audit line of its run carries.
fn keep_map(sha: &str, canonical: &str) -> Result<PathBuf> {
    let dir = crate::config::paths::resolve_tengu_home()
        .join("logs")
        .join("maps");
    std::fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
    let path = dir.join(format!("{sha}.json"));
    std::fs::write(&path, format!("{canonical}\n"))
        .with_context(|| format!("write {}", path.display()))?;
    Ok(path)
}
