//! Shell execution adapter for running skill commands.

use crate::ports::shell::ShellExecutionPort;
use anyhow::Result;
use std::path::Path;
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

const SHELL_TIMEOUT: Duration = Duration::from_secs(300);

pub(crate) struct LocalShellExecutor {
    cancel: Option<Arc<AtomicBool>>,
}

impl LocalShellExecutor {
    pub(crate) fn new() -> Self {
        Self { cancel: None }
    }

    pub(crate) fn with_cancel(mut self, cancel: Arc<AtomicBool>) -> Self {
        self.cancel = Some(cancel);
        self
    }
}

impl ShellExecutionPort for LocalShellExecutor {
    fn execute_shell(&self, command: &str, workspace: &Path) -> Result<String> {
        // `[egress]` decides the wrapper (plain `sh`, or `sandbox-exec` under
        // `shell_network = "isolated"`) and exports the proxy env vars.
        let mut child = crate::adapters::egress::policy()
            .shell_command(command)
            .current_dir(workspace)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| anyhow::anyhow!("Failed to spawn shell: {}", e))?;

        let start = Instant::now();
        loop {
            match child.try_wait() {
                Ok(Some(_status)) => break,
                Ok(None) => {
                    // Check cancel flag — kill child immediately on /stop.
                    if let Some(ref flag) = self.cancel {
                        if flag.load(Ordering::Relaxed) {
                            let _ = child.kill();
                            let _ = child.wait();
                            anyhow::bail!("Command cancelled by /stop");
                        }
                    }
                    if start.elapsed() >= SHELL_TIMEOUT {
                        let _ = child.kill();
                        let _ = child.wait();
                        anyhow::bail!("Command timed out after {}s", SHELL_TIMEOUT.as_secs());
                    }
                    std::thread::sleep(Duration::from_millis(50));
                }
                Err(e) => anyhow::bail!("Failed to wait on child process: {}", e),
            }
        }

        let output = child
            .wait_with_output()
            .map_err(|e| anyhow::anyhow!("Failed to read command output: {}", e))?;

        let stdout = String::from_utf8_lossy(&output.stdout);

        if output.status.success() {
            Ok(stdout.to_string())
        } else {
            let stderr = String::from_utf8_lossy(&output.stderr);
            anyhow::bail!(
                "Command failed (exit {}): {}",
                output.status.code().unwrap_or(-1),
                stderr
            )
        }
    }
}
