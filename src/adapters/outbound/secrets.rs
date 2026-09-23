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

/// Decrypt the vault and inject every key=value pair into the process
/// environment.  Used at startup so that the rest of the application
/// can read secrets via `std::env::var`.
///
/// Returns the secret **values** (not keys) so they can be registered
/// in the `SecretRegistry` for output redaction.
pub(crate) fn load_secrets_into_env(path: &Path) -> Result<Vec<String>> {
    let password = prompt_password()?;
    let lines = decrypt_and_parse(path, &password)?;

    let mut secret_values = Vec::new();
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
                    secret_values.push(v.to_string());
                }
            }
        }
    }
    Ok(secret_values)
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

#[async_trait]
impl ToolExecutor for SanitizedToolExecutor {
    async fn execute(
        &self,
        call: &ToolCall,
        messages: &[crate::domain::message::Message],
    ) -> Result<String> {
        let result = self.inner.execute(call, messages).await?;
        Ok(self.registry.redact(&result))
    }
}
