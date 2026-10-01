//! Secrets management: encrypted vault storage + runtime redaction.
//!
//! **Vault** — secrets are stored encrypted at rest using AES-256-GCM with a
//! PBKDF2-HMAC-SHA256 derived key.  The master password is read from
//! the `TENGU_MASTER_PASSWORD` env var or prompted interactively.
//!
//! **Redaction** — `SanitizedToolExecutor` (below) redacts tool output through
//! `domain::secrets::SecretRegistry`.

use aes_gcm::aead::{Aead, KeyInit, OsRng};
use aes_gcm::{Aes256Gcm, Nonce};
use anyhow::{bail, Context, Result};
use pbkdf2::pbkdf2_hmac;
use sha2::Sha256;
use std::path::{Path, PathBuf};

use async_trait::async_trait;

use crate::domain::message::ToolCall;
use crate::domain::secrets::SecretRegistry;
use crate::ports::engine::ToolExecutor;

// ── vault constants ─────────────────────────────────────────────

/// Magic header: "TENGU_VAULT" + version byte (0x01).
const MAGIC: &[u8; 12] = b"TENGU_VAULT\x01";
const SALT_LEN: usize = 32;
const NONCE_LEN: usize = 12;
const PBKDF2_ITERATIONS: u32 = 600_000;

// ── vault crypto primitives ─────────────────────────────────────

fn derive_key(password: &str, salt: &[u8]) -> [u8; 32] {
    let mut key = [0u8; 32];
    pbkdf2_hmac::<Sha256>(password.as_bytes(), salt, PBKDF2_ITERATIONS, &mut key);
    key
}

fn encrypt_vault(plaintext: &[u8], password: &str) -> Result<Vec<u8>> {
    use aes_gcm::aead::rand_core::RngCore;

    let mut salt = [0u8; SALT_LEN];
    OsRng.fill_bytes(&mut salt);

    let mut nonce_bytes = [0u8; NONCE_LEN];
    OsRng.fill_bytes(&mut nonce_bytes);

    let key = derive_key(password, &salt);
    let cipher = Aes256Gcm::new_from_slice(&key)
        .map_err(|e| anyhow::anyhow!("cipher init failed: {}", e))?;
    let nonce = Nonce::from_slice(&nonce_bytes);

    let ciphertext = cipher
        .encrypt(nonce, plaintext)
        .map_err(|e| anyhow::anyhow!("encryption failed: {}", e))?;

    let mut out = Vec::with_capacity(MAGIC.len() + SALT_LEN + NONCE_LEN + ciphertext.len());
    out.extend_from_slice(MAGIC);
    out.extend_from_slice(&salt);
    out.extend_from_slice(&nonce_bytes);
    out.extend_from_slice(&ciphertext);
    Ok(out)
}

fn decrypt_vault(data: &[u8], password: &str) -> Result<Vec<u8>> {
    let header_len = MAGIC.len() + SALT_LEN + NONCE_LEN;
    if data.len() < header_len {
        bail!("vault file too short — is this a valid secrets vault?");
    }
    if &data[..MAGIC.len()] != MAGIC.as_slice() {
        bail!("invalid vault header — not a Tengu secrets vault");
    }

    let salt = &data[MAGIC.len()..MAGIC.len() + SALT_LEN];
    let nonce_bytes = &data[MAGIC.len() + SALT_LEN..header_len];
    let ciphertext = &data[header_len..];

    let key = derive_key(password, salt);
    let cipher = Aes256Gcm::new_from_slice(&key)
        .map_err(|e| anyhow::anyhow!("cipher init failed: {}", e))?;
    let nonce = Nonce::from_slice(nonce_bytes);

    cipher
        .decrypt(nonce, ciphertext)
        .map_err(|_| anyhow::anyhow!("decryption failed — wrong password?"))
}

// ── password prompting ──────────────────────────────────────────

fn prompt_password() -> Result<String> {
    if let Ok(pw) = std::env::var("TENGU_MASTER_PASSWORD") {
        if !pw.is_empty() {
            return Ok(pw);
        }
    }
    rpassword::prompt_password("Master password: ").context("failed to read password from terminal")
}

fn prompt_new_password() -> Result<String> {
    if let Ok(pw) = std::env::var("TENGU_MASTER_PASSWORD") {
        if !pw.is_empty() {
            return Ok(pw);
        }
    }
    println!();
    println!("  Choose a strong master password:");
    println!("    - At least 12 characters");
    println!("    - Mix of upper/lowercase, numbers, symbols");
    println!("    - Do NOT reuse a password from another service");
    println!("    - Store it in a password manager if possible");
    println!();
    let p1 =
        rpassword::prompt_password("New master password: ").context("failed to read password")?;
    if p1.is_empty() {
        bail!("password must not be empty");
    }
    if p1.len() < 8 {
        bail!("password too short — use at least 8 characters (12+ recommended)");
    }
    let p2 = rpassword::prompt_password("Confirm master password: ")
        .context("failed to read password")?;
    if p1 != p2 {
        bail!("passwords do not match");
    }
    Ok(p1)
}

// ── vault public API ────────────────────────────────────────────

/// Return the canonical secrets vault path.
pub(crate) fn secrets_file_path(tengu_home: &Path) -> PathBuf {
    tengu_home.join("secrets.vault")
}

/// Create a new empty encrypted vault with `chmod 600`.
pub(crate) fn init_secrets_file(path: &Path) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("Failed to create directory {}", parent.display()))?;
    }

    if path.exists() {
        println!("  secrets vault already exists: {}", path.display());
        return Ok(());
    }

    let password = prompt_new_password()?;
    let encrypted = encrypt_vault(b"", &password)?;

    std::fs::write(path, &encrypted)
        .with_context(|| format!("Failed to write {}", path.display()))?;
    enforce_permissions(path)?;

    println!("  created: {}", path.display());
    println!("  permissions: 600");
    Ok(())
}

/// Set (or overwrite) a secret key=value pair.
pub(crate) fn set_secret(path: &Path, key: &str, value: &str) -> Result<()> {
    validate_key(key)?;

    let password = prompt_password()?;
    let mut lines = decrypt_and_parse(path, &password)?;

    let new_line = format!("{}={}", key, value);
    let mut replaced = false;
    for line in &mut lines {
        if line_matches_key(line, key) {
            *line = new_line.clone();
            replaced = true;
            break;
        }
    }
    if !replaced {
        lines.push(new_line);
    }

    let plaintext = serialize_lines(&lines);
    let encrypted = encrypt_vault(plaintext.as_bytes(), &password)?;
    write_atomic(path, &encrypted)?;
    enforce_permissions(path)?;

    println!("  set: {}", key);
    Ok(())
}

/// Remove a secret by key.
pub(crate) fn remove_secret(path: &Path, key: &str) -> Result<()> {
    let password = prompt_password()?;
    let lines = decrypt_and_parse(path, &password)?;

    let filtered: Vec<String> = lines
        .into_iter()
        .filter(|l| !line_matches_key(l, key))
        .collect();

    let plaintext = serialize_lines(&filtered);
    let encrypted = encrypt_vault(plaintext.as_bytes(), &password)?;
    write_atomic(path, &encrypted)?;
    enforce_permissions(path)?;

    println!("  removed: {}", key);
    Ok(())
}

/// List all secret key names (values hidden).
pub(crate) fn list_secret_keys(path: &Path) -> Result<Vec<String>> {
    if !path.exists() {
        return Ok(vec![]);
    }

    let password = prompt_password()?;
    let lines = decrypt_and_parse(path, &password)?;

    Ok(lines
        .iter()
        .filter(|l| {
            let trimmed = l.trim();
            !trimmed.is_empty() && !trimmed.starts_with('#') && trimmed.contains('=')
        })
        .filter_map(|l| l.split('=').next().map(|k| k.to_string()))
        .collect())
}

/// Set by the first `tengu` that opens the vault (even when unlocking
/// fails): comma-separated names of the env vars it loaded. A `tengu`
/// spawned below it (e.g. `tengu decide` from an agent's `run_command`)
/// inherits those vars and never prompts — the prompt would block on a
/// terminal the parent's TUI owns.
pub(crate) const SECRETS_LOADED_ENV: &str = "TENGU_SECRETS_LOADED";

/// Decrypt the vault and inject every key=value pair into the process
/// environment.  Used at startup so that the rest of the application
/// can read secrets via `std::env::var`.
///
/// Returns the `(key, value)` pairs it set (non-empty values only) so the
/// values can be registered in the `SecretRegistry` for output redaction
/// and the keys published in `SECRETS_LOADED_ENV`.
pub(crate) fn load_secrets_into_env(path: &Path) -> Result<Vec<(String, String)>> {
    let password = prompt_password()?;
    let lines = decrypt_and_parse(path, &password)?;

    let mut loaded = Vec::new();
    for line in &lines {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        if let Some((k, v)) = trimmed.split_once('=') {
            let existing = std::env::var(k).unwrap_or_default();
            if existing.is_empty() {
                std::env::set_var(k, v);
                if !v.is_empty() {
                    loaded.push((k.to_string(), v.to_string()));
                }
            }
        }
    }
    Ok(loaded)
}

/// The process `SecretRegistry` — the values tool output is redacted with.
///
/// | Case | Registered |
/// |---|---|
/// | `TENGU_SECRETS_LOADED` set (an ancestor `tengu` opened the vault) | the env value of each name it lists — never prompts again |
/// | else `vault` given and present (the CLI) | the vault, unlocked (prompt / `TENGU_MASTER_PASSWORD`), loaded into the env; names published in `TENGU_SECRETS_LOADED` |
/// | always | `TENGU_MASTER_PASSWORD` |
/// | always | every env var named `*_API_KEY`, `*_SECRET`, `*_TOKEN`, `*_PASSWORD`, `*_PRIVATE_KEY` whose value passes `domain::secrets::is_env_secret` (≥ 8 chars, no placeholder / number / template, never a public on-chain id) — `.env` (loaded by `main`) and exported keys, inherited by `run-agent` and the bridge, so every surface registers the same values |
///
/// `tengu mcp-bridge` and `run-agent` pass `None`: their stdin is a protocol
/// channel, so they never prompt.
pub(crate) fn process_secret_registry(vault: Option<&Path>) -> SecretRegistry {
    let mut registry = SecretRegistry::new();
    if let Ok(names) = std::env::var(SECRETS_LOADED_ENV) {
        register_named(&mut registry, &names, |k| std::env::var(k).ok());
    } else if let Some(path) = vault.filter(|p| p.exists()) {
        warn_if_group_or_world_readable(path);
        let loaded = load_secrets_into_env(path).unwrap_or_else(|e| {
            eprintln!("WARNING: Failed to load secrets vault: {}", e);
            Vec::new()
        });
        let keys: Vec<&str> = loaded.iter().map(|(k, _)| k.as_str()).collect();
        std::env::set_var(SECRETS_LOADED_ENV, keys.join(","));
        for (_, v) in loaded {
            registry.register(v);
        }
    }
    if let Ok(pw) = std::env::var("TENGU_MASTER_PASSWORD") {
        registry.register(pw);
    }
    register_env_secrets(&mut registry, std::env::vars_os());
    registry
}

/// Register the value of every `name = value` in `vars` that
/// `domain::secrets::is_env_secret` calls a credential (trimmed); a name
/// or value that is not UTF-8 is skipped.
fn register_env_secrets(
    registry: &mut SecretRegistry,
    vars: impl IntoIterator<Item = (std::ffi::OsString, std::ffi::OsString)>,
) {
    for (name, value) in vars {
        let (Some(name), Some(value)) = (name.to_str(), value.to_str()) else {
            continue;
        };
        if crate::domain::secrets::is_env_secret(name, value) {
            registry.register(value.trim().to_string());
        }
    }
}

/// Register the value of each var named in the comma-separated `names`
/// (unset or empty values are skipped).
fn register_named(
    registry: &mut SecretRegistry,
    names: &str,
    lookup: impl Fn(&str) -> Option<String>,
) {
    for value in names
        .split(',')
        .filter(|k| !k.is_empty())
        .filter_map(lookup)
    {
        registry.register(value);
    }
}

fn warn_if_group_or_world_readable(path: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Ok(meta) = std::fs::metadata(path) {
            let mode = meta.permissions().mode() & 0o777;
            if mode & 0o077 != 0 {
                eprintln!(
                    "WARNING: {} has permissions {:o} — should be 600. \
                     Run: chmod 600 {}",
                    path.display(),
                    mode,
                    path.display()
                );
            }
        }
    }
    let _ = path; // suppress unused warning on non-unix
}

/// Change the master password on an existing vault.
pub(crate) fn change_password(path: &Path) -> Result<()> {
    if !path.exists() {
        bail!(
            "No secrets vault found at {}. Run `tengu secret init` first.",
            path.display()
        );
    }

    println!("  Enter your current master password to unlock the vault.");
    let old_password = prompt_password()?;
    let lines = decrypt_and_parse(path, &old_password)?;

    println!("  Vault unlocked. Now choose a new master password.");
    let new_password = prompt_new_password()?;

    let plaintext = serialize_lines(&lines);
    let encrypted = encrypt_vault(plaintext.as_bytes(), &new_password)?;
    write_atomic(path, &encrypted)?;
    enforce_permissions(path)?;

    println!("  Master password changed successfully.");
    Ok(())
}

/// Enforce `chmod 600` on Unix.
pub(crate) fn enforce_permissions(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let perms = std::fs::Permissions::from_mode(0o600);
        std::fs::set_permissions(path, perms)
            .with_context(|| format!("Failed to chmod 600 {}", path.display()))?;
    }
    let _ = path; // suppress unused warning on non-unix
    Ok(())
}

// ── internal helpers ────────────────────────────────────────────

fn validate_key(key: &str) -> Result<()> {
    let valid = !key.is_empty()
        && key
            .chars()
            .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
        && key
            .chars()
            .next()
            .map_or(false, |c| c.is_ascii_uppercase() || c == '_');
    if !valid {
        bail!(
            "Invalid key '{}': must match [A-Z_][A-Z0-9_]* (e.g. OPENROUTER_API_KEY)",
            key
        );
    }
    Ok(())
}

fn line_matches_key(line: &str, key: &str) -> bool {
    let trimmed = line.trim();
    if trimmed.starts_with('#') {
        return false;
    }
    match trimmed.split_once('=') {
        Some((k, _)) => k == key,
        None => false,
    }
}

/// Decrypt the vault and return lines.  An empty vault yields an
/// empty vec.
fn decrypt_and_parse(path: &Path, password: &str) -> Result<Vec<String>> {
    if !path.exists() {
        return Ok(vec![]);
    }
    let data = std::fs::read(path).with_context(|| format!("Failed to read {}", path.display()))?;
    let plaintext = decrypt_vault(&data, password)?;
    if plaintext.is_empty() {
        return Ok(vec![]);
    }
    let content = String::from_utf8(plaintext).context("vault plaintext is not valid UTF-8")?;
    Ok(content.lines().map(|l| l.to_string()).collect())
}

fn serialize_lines(lines: &[String]) -> String {
    let mut content = lines.join("\n");
    if !content.is_empty() && !content.ends_with('\n') {
        content.push('\n');
    }
    content
}

fn write_atomic(path: &Path, data: &[u8]) -> Result<()> {
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, data)
        .with_context(|| format!("Failed to write temp file {}", tmp.display()))?;
    std::fs::rename(&tmp, path)
        .with_context(|| format!("Failed to rename {} -> {}", tmp.display(), path.display()))?;
    Ok(())
}

/// Decorator that redacts registered secret values from all tool output.
///
/// Owns its dependencies behind `Arc` so the decorator is `'static` —
/// required by the orchestrator chat-factory path. Per-turn borrowed
/// callers wrap their inner executor + registry in `Arc` at the call
/// site.
pub(crate) struct SanitizedToolExecutor {
    inner: std::sync::Arc<dyn ToolExecutor>,
    registry: std::sync::Arc<crate::domain::secrets::SecretRegistry>,
}

impl SanitizedToolExecutor {
    pub fn new(
        inner: std::sync::Arc<dyn ToolExecutor>,
        registry: std::sync::Arc<crate::domain::secrets::SecretRegistry>,
    ) -> Self {
        Self { inner, registry }
    }
}

impl SanitizedToolExecutor {
    /// A failed call's error, redacted: its message reaches the model on
    /// every surface (`ERROR: …` in chat, `tool error: …` in `run-agent`, the
    /// bridge's `isError` text) — a request URL a `$VAR` expanded into, an
    /// upstream body echoing a key. Keeps the displayed (outermost) message.
    fn redact_error(&self, e: anyhow::Error) -> anyhow::Error {
        anyhow::anyhow!(self.registry.redact(&e.to_string()))
    }
}

#[async_trait]
impl ToolExecutor for SanitizedToolExecutor {
    async fn execute(
        &self,
        call: &ToolCall,
        messages: &[crate::domain::message::Message],
    ) -> Result<String> {
        let result = self
            .inner
            .execute(call, messages)
            .await
            .map_err(|e| self.redact_error(e))?;
        Ok(self.registry.redact(&result))
    }

    /// Redacts the text and the observation (headline, error messages,
    /// string features, `data`), and a failed call's error.
    async fn execute_typed(
        &self,
        call: &ToolCall,
        messages: &[crate::domain::message::Message],
    ) -> Result<crate::ports::tool::ToolOutput> {
        let mut out = self
            .inner
            .execute_typed(call, messages)
            .await
            .map_err(|e| self.redact_error(e))?;
        out.text = self.registry.redact(&out.text);
        if let Some(obs) = out.observation.as_mut() {
            self.registry.redact_observation(obs);
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::message::Message;
    use crate::domain::observation::{ObsSource, ObsStatus, Observation};
    use crate::domain::secrets::SecretRegistry;
    use crate::ports::tool::ToolOutput;
    use serde_json::json;
    use std::sync::Arc;

    const KEY: &str = "api-key-0123456789abcdef";

    struct Leaky;

    #[async_trait]
    impl ToolExecutor for Leaky {
        async fn execute(&self, call: &ToolCall, m: &[Message]) -> Result<String> {
            Ok(self.execute_typed(call, m).await?.text)
        }
        async fn execute_typed(&self, _call: &ToolCall, _m: &[Message]) -> Result<ToolOutput> {
            let obs = Observation {
                key: "leak/1:s".into(),
                schema: "leak/1".into(),
                tool: "leak".into(),
                observed_at_ms: 0,
                slot: None,
                ttl_ms: 1_000,
                source: ObsSource::Live,
                status: ObsStatus::Ok,
                errors: vec![],
                headline: format!("rpc {KEY}"),
                features: Default::default(),
                data: json!({"rpc_url": format!("https://rpc.example/?api-key={KEY}"), "n": 1}),
            };
            Ok(ToolOutput::observed(obs, 0))
        }
    }

    /// Inherited vault names → their values are registered; unset and
    /// empty ones are skipped.
    #[test]
    fn inherited_secret_names_register_their_values() {
        let env =
            std::collections::HashMap::from([("VAULT_A", "alpha-0123456789"), ("VAULT_EMPTY", "")]);
        let mut reg = SecretRegistry::new();
        register_named(&mut reg, "VAULT_A,,VAULT_EMPTY,VAULT_UNSET", |k| {
            env.get(k).map(|v| v.to_string())
        });
        assert_eq!(reg.redact("k=alpha-0123456789;"), "k=[REDACTED];");
        assert_eq!(reg.redact("nothing else"), "nothing else");
    }

    /// `.env` / exported credentials by name (`domain::secrets::is_env_secret`):
    /// keys redacted, a token mint and a short value left alone, non-UTF-8
    /// skipped.
    #[test]
    fn env_credentials_register_by_name() {
        use std::ffi::OsString;
        const MINT: &str = "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v";
        let mut vars: Vec<(OsString, OsString)> = [
            ("OPENROUTER_API_KEY", "sk-or-v1-0123456789abcdef"),
            ("PRIVY_APP_SECRET", " privy-app-secret-0123 "),
            ("USDC_TOKEN", MINT),
            ("POSTGRES_PASSWORD", "tengu"),
            ("PATH", "/usr/bin:/bin"),
        ]
        .into_iter()
        .map(|(k, v)| (k.into(), v.into()))
        .collect();
        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStringExt;
            vars.push((
                "BINARY_SECRET".into(),
                OsString::from_vec(vec![0xff, 0xfe, b'a', b'b', b'c', b'd', b'e', b'f', b'g']),
            ));
        }
        let mut reg = SecretRegistry::new();
        register_env_secrets(&mut reg, vars);
        let out = reg.redact(&format!(
            "k=sk-or-v1-0123456789abcdef p=privy-app-secret-0123 mint={MINT} db=tengu path=/usr/bin:/bin"
        ));
        assert_eq!(
            out,
            format!("k=[REDACTED] p=[REDACTED] mint={MINT} db=tengu path=/usr/bin:/bin")
        );
    }

    /// The process registry picks credentials up from the live env, so
    /// every surface (CLI, `run-agent`, the bridge, webhooks, eval) redacts
    /// a `.env` key without a vault.
    #[test]
    fn process_registry_reads_env_credentials() {
        let name = format!(
            "TENGU_REDACT_PROBE_{}_API_KEY",
            uuid::Uuid::new_v4().simple()
        );
        let value = format!("probe-{}", uuid::Uuid::new_v4().simple());
        std::env::set_var(&name, &value);
        let reg = process_secret_registry(None);
        std::env::remove_var(&name);
        assert_eq!(reg.redact(&format!("key {value}")), "key [REDACTED]");
    }

    #[tokio::test]
    async fn sanitized_executor_redacts_observation_data() {
        let mut reg = SecretRegistry::new();
        reg.register(KEY.to_string());
        let exec = SanitizedToolExecutor::new(Arc::new(Leaky), Arc::new(reg));
        let call = ToolCall {
            id: "1".into(),
            name: "leak".into(),
            arguments: json!({}),
        };
        let out = exec.execute_typed(&call, &[]).await.unwrap();
        assert!(!out.text.contains(KEY), "{}", out.text);
        let obs = out.observation.unwrap();
        assert_eq!(
            obs.data,
            json!({"rpc_url": "https://rpc.example/?api-key=[REDACTED]", "n": 1})
        );
        assert_eq!(obs.headline, "rpc [REDACTED]");
    }

    /// A failed call's error reaches the model redacted on every path
    /// (typed and text), like the bridge's error text.
    #[tokio::test]
    async fn sanitized_executor_redacts_errors() {
        struct Failing;
        #[async_trait]
        impl ToolExecutor for Failing {
            async fn execute(&self, _call: &ToolCall, _m: &[Message]) -> Result<String> {
                anyhow::bail!("error sending request for url (https://api.example/?key={KEY})")
            }
        }
        let mut reg = SecretRegistry::new();
        reg.register(KEY.to_string());
        let exec = SanitizedToolExecutor::new(Arc::new(Failing), Arc::new(reg));
        let call = ToolCall {
            id: "c".into(),
            name: "http_request".into(),
            arguments: json!({}),
        };
        let typed = exec
            .execute_typed(&call, &[])
            .await
            .unwrap_err()
            .to_string();
        let text = exec.execute(&call, &[]).await.unwrap_err().to_string();
        for e in [typed, text] {
            assert!(!e.contains(KEY), "{e}");
            assert!(e.contains("?key=[REDACTED]"), "{e}");
        }
    }
}
