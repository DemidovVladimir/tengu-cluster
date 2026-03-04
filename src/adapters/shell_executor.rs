//! Shell execution adapter for running skill commands.

use crate::application::ports::ShellExecutionPort;
use anyhow::Result;
use regex::Regex;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

const SHELL_TIMEOUT: Duration = Duration::from_secs(300);
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
            Ok(sanitize_output(&stdout))
        } else {
            let stderr = String::from_utf8_lossy(&output.stderr);
            let sanitized = sanitize_output(&stderr);
            anyhow::bail!(
                "Command failed (exit {}): {}",
                output.status.code().unwrap_or(-1),
                sanitized
            )
        }
    }
}

/// Strip secrets and API key patterns from shell output (both stdout and stderr).
///
/// This is a belt-and-suspenders layer — the `SanitizedToolExecutor` also
/// redacts known vault secrets, but this function catches common key formats
/// even if the value didn't come from the vault.
fn sanitize_output(text: &str) -> String {
    static RE: OnceLock<Regex> = OnceLock::new();
    let re = RE.get_or_init(|| {
        // Order matters — longer prefixes first to avoid partial matches.
        // Patterns:
        //   Bearer tokens:       Bearer sk-abc...
        //   OpenRouter keys:     sk-or-v1-...
        //   Anthropic keys:      sk-ant-api03-...
        //   OpenAI keys:         sk-proj-... or sk-<20+ chars>
        //   HuggingFace tokens:  hf_...
        //   Env dump lines:      KEY=sk-or-v1-...  (value part)
        Regex::new(
            r"(?x)
              Bearer\s+\S+
            | sk-or-v1-[A-Za-z0-9_-]{10,}
            | sk-ant-[A-Za-z0-9_-]{10,}
            | sk-proj-[A-Za-z0-9_-]{10,}
            | sk-[A-Za-z0-9_-]{20,}
            | hf_[A-Za-z0-9]{10,}
            ",
        )
        .expect("invalid regex")
    });
    re.replace_all(text, "[REDACTED]").to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_strips_bearer_tokens() {
        let input = "curl: (22) 401 Authorization: Bearer sk-abc123-secret failed";
        let result = sanitize_output(input);
        assert!(result.contains("[REDACTED]"));
        assert!(!result.contains("sk-abc123-secret"));
    }

    #[test]
    fn sanitize_preserves_safe_output() {
        let input = "error: file not found";
        assert_eq!(sanitize_output(input), input);
    }

    #[test]
    fn sanitize_strips_openrouter_key() {
        let input = "OPENROUTER_API_KEY=sk-or-v1-abc123def456xyz";
        let result = sanitize_output(input);
        assert!(!result.contains("sk-or-v1-abc123def456xyz"));
        assert!(result.contains("[REDACTED]"));
    }

    #[test]
    fn sanitize_strips_anthropic_key() {
        let input = "key is sk-ant-api03-abcdef1234567890";
        let result = sanitize_output(input);
        assert!(!result.contains("sk-ant-api03"));
        assert!(result.contains("[REDACTED]"));
    }

    #[test]
    fn sanitize_strips_openai_key() {
        let input = "export OPENAI_API_KEY=sk-proj-abcdefghij1234567890";
        let result = sanitize_output(input);
        assert!(!result.contains("sk-proj-abcdefghij1234567890"));
        assert!(result.contains("[REDACTED]"));
    }

    #[test]
    fn sanitize_strips_huggingface_token() {
        let input = "HF_TOKEN=hf_abcdefghijklmnop";
        let result = sanitize_output(input);
        assert!(!result.contains("hf_abcdefghijklmnop"));
        assert!(result.contains("[REDACTED]"));
    }

    #[test]
    fn sanitize_strips_long_sk_key() {
        let input = "sk-1234567890abcdefghijklmnop";
        let result = sanitize_output(input);
        assert!(result.contains("[REDACTED]"));
    }

    #[test]
    fn sanitize_preserves_short_sk_prefix() {
        // Short "sk-" prefixes that aren't real keys should be preserved.
        let input = "sk-short";
        assert_eq!(sanitize_output(input), input);
    }
}
