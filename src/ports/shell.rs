//! Port for executing shell commands in a workspace directory.

use anyhow::Result;

/// Port for executing shell commands in a workspace directory.
pub(crate) trait ShellExecutionPort: Send + Sync {
    fn execute_shell(&self, command: &str, workspace: &std::path::Path) -> Result<String>;

    /// `command` with `env` set for this run only: names and values reach the
    /// child as variables, never spliced into the command line. A shell that
    /// cannot pass variables refuses a non-empty `env` rather than drop it.
    fn execute_shell_env(
        &self,
        command: &str,
        workspace: &std::path::Path,
        env: &[(&str, &str)],
    ) -> Result<String> {
        if !env.is_empty() {
            anyhow::bail!("this shell cannot pass environment variables");
        }
        self.execute_shell(command, workspace)
    }
}
