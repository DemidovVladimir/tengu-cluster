//! `tengu decide --sandbox <s> --loop <name> [--event file.json]` — run one
//! event through a decision loop and print the outcomes. Manual test path for
//! `[decision_loops.*]`; the long-running trigger is the webhook listener.

use std::io::Read;
use std::path::Path;

use anyhow::{Context, Result};
use serde_json::Value;

use crate::config::Config;

pub(super) async fn run_decide(
    config: &Config,
    loop_name: &str,
    event: Option<&Path>,
) -> Result<()> {
    let event: Value = match event {
        None => Value::Object(Default::default()),
        Some(p) if p.as_os_str() == "-" => {
            let mut buf = String::new();
            std::io::stdin()
                .read_to_string(&mut buf)
                .context("read event from stdin")?;
            serde_json::from_str(&buf).context("event on stdin is not JSON")?
        }
        Some(p) => {
            let raw =
                std::fs::read_to_string(p).with_context(|| format!("read {}", p.display()))?;
            serde_json::from_str(&raw).with_context(|| format!("{} is not JSON", p.display()))?
        }
    };
    let dl = crate::bootstrap::decision::build_decision_loop(config, loop_name, None)?;
    let session_id = format!("decide-{loop_name}-{}", uuid::Uuid::new_v4());
    let outcomes = dl.handle_event(&event, &session_id).await?;
    println!(
        "{}",
        serde_json::to_string_pretty(&serde_json::json!({
            "session_id": session_id,
            "outcomes": outcomes,
            "audit": crate::bootstrap::decision::audit_path(),
        }))?
    );
    Ok(())
}
