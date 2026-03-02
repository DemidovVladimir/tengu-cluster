//! Shell execution adapter for running skill commands.

use crate::application::ports::ShellExecutionPort;
use anyhow::Result;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

const SHELL_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_OUTPUT_BYTES: usize = 1024 * 1024; // 1 MB

pub(crate) struct LocalShellExecutor;

impl ShellExecutionPort for LocalShellExecutor {
    fn execute_shell(&self, command: &str, workspace: &Path) -> Result<String> {
        let mut child = Command::new("sh")
            .arg("-c")
            .arg(command)
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
                    if start.elapsed() >= SHELL_TIMEOUT {
                        let _ = child.kill();
                        let _ = child.wait();
                        anyhow::bail!(
                            "Command timed out after {}s",
                            SHELL_TIMEOUT.as_secs()
                        );
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
        let stdout = if stdout.len() > MAX_OUTPUT_BYTES {
            format!("{}...[truncated]", &stdout[..MAX_OUTPUT_BYTES])
        } else {
            stdout.to_string()
        };

        if output.status.success() {
            Ok(stdout)
        } else {
            let stderr = String::from_utf8_lossy(&output.stderr);
            let sanitized = sanitize_stderr(&stderr);
            anyhow::bail!(
                "Command failed (exit {}): {}",
                output.status.code().unwrap_or(-1),
                sanitized
            )
        }
    }
}

/// Strip secrets from stderr before surfacing errors to the user/model.
fn sanitize_stderr(stderr: &str) -> String {
    static RE_BEARER: OnceLock<regex::Regex> = OnceLock::new();
    let re = RE_BEARER.get_or_init(|| {
        regex::Regex::new(r"Bearer\s+\S+").expect("invalid regex")
    });
    re.replace_all(stderr, "Bearer [REDACTED]").to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_strips_bearer_tokens() {
        let input = "curl: (22) 401 Authorization: Bearer sk-abc123-secret failed";
        let result = sanitize_stderr(input);
        assert!(result.contains("[REDACTED]"));
        assert!(!result.contains("sk-abc123-secret"));
    }

    #[test]
    fn sanitize_preserves_safe_stderr() {
        let input = "error: file not found";
        assert_eq!(sanitize_stderr(input), input);
    }
}
