//! `SandboxDrafts` — where the Studio builder reads and writes a sandbox
//! (`application/builder.rs`; adapter `adapters/outbound/builder_store.rs`:
//! `sandboxes/<name>/` under the working directory, like `--sandbox`).

use std::path::PathBuf;

use anyhow::Result;

use crate::config::builder::LoadReport;

/// What the store knows about a key by name (never its value).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SecretState {
    /// Set in this process's env (the vault and `.env` load there).
    Present,
    Missing,
    /// Kept by the Cloudflare Worker, not on this machine.
    Remote,
}

impl SecretState {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            SecretState::Present => "present",
            SecretState::Missing => "missing",
            SecretState::Remote => "remote",
        }
    }
}

pub(crate) trait SandboxDrafts: Send + Sync {
    /// `sandboxes/<sandbox>/config.toml` (shown, never read through).
    fn config_path(&self, sandbox: &str) -> PathBuf;
    /// `builder.json`, if the builder made this sandbox.
    fn read_blueprint(&self, sandbox: &str) -> Result<Option<String>>;
    /// Atomic: a temp file renamed over `builder.json`.
    fn write_blueprint(&self, sandbox: &str, json: &str) -> Result<()>;
    fn read_config(&self, sandbox: &str) -> Result<Option<String>>;
    /// `Config::load` on `toml` written to a temp
    /// `<tmp>/sandboxes/<sandbox>/config.toml` (the path matters: hardening
    /// and generation checks read it).
    fn check_config(&self, sandbox: &str, toml: &str) -> LoadReport;
    /// Atomic write of `config.toml`; the old file is kept as
    /// `config.toml.prev` (returned).
    fn write_config(&self, sandbox: &str, toml: &str) -> Result<Option<PathBuf>>;
    /// A new sandbox dir with both files; refused when the dir exists.
    fn create(&self, sandbox: &str, blueprint_json: &str, toml: &str) -> Result<PathBuf>;
    /// Why the sandbox may not be rewritten now (its runtime is running).
    fn busy(&self, sandbox: &str) -> Option<String>;
    fn secret_state(&self, env: &str, backend: &str) -> SecretState;
}
