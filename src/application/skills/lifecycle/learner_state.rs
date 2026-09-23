//! Per-learner skill state — sidecar JSON at `skills/<name>/state/<learner_id>.json`.
//!
//! Architecture decision A1 (see `docs/skill-research-2026-04-28.md`): each learner
//! gets a small JSON file under the skill's `state/` subdir tracking topics covered,
//! topics weak, mastery scores, and last-session timestamp. Cold-start (file missing)
//! returns the default zero-state — fail-soft, per the doctrine.
//!
//! Atomic writes via temp-then-rename mirror the pattern in
//! `outbound/tools/skill_lifecycle/distill.rs:148`. JSON parse errors at load time
//! are hard errors: a corrupt state file is a plan-shape failure, not a soft miss.
//!
//! Module is landed but not yet wired into the harness. The "adjust yourself"
//! three-step plan in `skills/orchestrator/SKILL.md` references per-learner
//! state, but the Rust runtime doesn't load/save it yet — that's the next
//! follow-up. `#![allow(dead_code)]` keeps the build warning-clean during the
//! interim. Remove once the call sites land.

#![allow(dead_code)]
//!
//! `learner_id` is validated against `^[a-z0-9][a-z0-9_-]{0,63}$` — this guards
//! against path traversal (no `..`, no `/`, no leading dot).

use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub(crate) struct LearnerState {
    pub learner_id: String,
    #[serde(default)]
    pub topics_covered: Vec<String>,
    #[serde(default)]
    pub topics_weak: Vec<String>,
    #[serde(default)]
    pub mastery_scores: BTreeMap<String, f32>,
    #[serde(default = "epoch_zero_iso")]
    pub last_session_ts: String,
}

fn epoch_zero_iso() -> String {
    "1970-01-01T00:00:00Z".to_string()
}

fn validate_learner_id(learner_id: &str) -> Result<()> {
    let len = learner_id.len();
    if len == 0 || len > 64 {
        return Err(anyhow!(
            "invalid learner_id: must match ^[a-z0-9][a-z0-9_-]{{0,63}}$ (length {} out of range)",
            len
        ));
    }
    let bytes = learner_id.as_bytes();
    let first_ok = matches!(bytes[0], b'a'..=b'z' | b'0'..=b'9');
    if !first_ok {
        return Err(anyhow!(
            "invalid learner_id: must match ^[a-z0-9][a-z0-9_-]{{0,63}}$ (must start with [a-z0-9])"
        ));
    }
    for &b in &bytes[1..] {
        let ok = matches!(b, b'a'..=b'z' | b'0'..=b'9' | b'_' | b'-');
        if !ok {
            return Err(anyhow!(
                "invalid learner_id: must match ^[a-z0-9][a-z0-9_-]{{0,63}}$ (illegal byte 0x{:02x})",
                b
            ));
        }
    }
    Ok(())
}

pub(crate) fn state_path(skill_dir: &Path, learner_id: &str) -> Result<PathBuf> {
    validate_learner_id(learner_id)?;
    Ok(skill_dir.join("state").join(format!("{learner_id}.json")))
}

pub(crate) fn load(skill_dir: &Path, learner_id: &str) -> Result<LearnerState> {
    validate_learner_id(learner_id)?;
    let path = state_path(skill_dir, learner_id)?;
    if !path.exists() {
        // Cold-start: return zero-state for a brand-new learner. Fail-soft on
        // missing files — only the parse path is hard.
        return Ok(LearnerState {
            learner_id: learner_id.to_string(),
            topics_covered: Vec::new(),
            topics_weak: Vec::new(),
            mastery_scores: BTreeMap::new(),
            last_session_ts: epoch_zero_iso(),
        });
    }
    let bytes = std::fs::read(&path).with_context(|| format!("read {:?}", path))?;
    let state: LearnerState = serde_json::from_slice(&bytes)
        .with_context(|| format!("parse learner_state json at {:?}", path))?;
    Ok(state)
}

pub(crate) fn save(skill_dir: &Path, state: &LearnerState) -> Result<()> {
    validate_learner_id(&state.learner_id)?;
    let final_path = state_path(skill_dir, &state.learner_id)?;
    let state_dir = final_path
        .parent()
        .ok_or_else(|| anyhow!("state_path missing parent"))?;
    std::fs::create_dir_all(state_dir)
        .with_context(|| format!("create state dir {:?}", state_dir))?;

    // Unique per call: a clock-based name collided between concurrent saves
    // (macOS ticks in microseconds) and the losing rename failed.
    let tmp_path = state_dir.join(format!(
        ".{}.tmp-{}",
        state.learner_id,
        uuid::Uuid::new_v4()
    ));

    let bytes = serde_json::to_vec_pretty(state).context("serialize learner_state")?;
    std::fs::write(&tmp_path, &bytes).with_context(|| format!("write tmp {:?}", tmp_path))?;
    std::fs::rename(&tmp_path, &final_path)
        .with_context(|| format!("rename {:?} -> {:?}", tmp_path, final_path))?;
    Ok(())
}

pub(crate) fn list_learners(skill_dir: &Path) -> Result<Vec<String>> {
    let dir = skill_dir.join("state");
    if !dir.exists() {
        // Fail-soft: no state dir means no learners yet.
        return Ok(Vec::new());
    }
    let mut out: Vec<String> = Vec::new();
    let entries = std::fs::read_dir(&dir).with_context(|| format!("read_dir {:?}", dir))?;
    for entry in entries {
        let entry = entry.with_context(|| format!("entry under {:?}", dir))?;
        let ft = match entry.file_type() {
            Ok(ft) => ft,
            Err(_) => continue,
        };
        if !ft.is_file() {
            continue;
        }
        let name = entry.file_name();
        let name_str = match name.to_str() {
            Some(s) => s,
            None => continue,
        };
        if name_str.starts_with('.') {
            continue;
        }
        let stem = match name_str.strip_suffix(".json") {
            Some(s) => s,
            None => continue,
        };
        if stem.is_empty() {
            continue;
        }
        out.push(stem.to_string());
    }
    out.sort();
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use tempfile::TempDir;

    #[test]
    fn roundtrip_serde_preserves_btreemap_floats() {
        let dir = TempDir::new().unwrap();
        let mut scores = BTreeMap::new();
        scores.insert("calc.derivatives".to_string(), 0.75_f32);
        scores.insert("calc.integrals".to_string(), 0.5_f32);
        scores.insert("calc.limits".to_string(), 1.0_f32);
        let state = LearnerState {
            learner_id: "alice".into(),
            topics_covered: vec!["calc.limits".into()],
            topics_weak: vec!["calc.integrals".into()],
            mastery_scores: scores,
            last_session_ts: "2026-04-28T10:00:00Z".into(),
        };
        save(dir.path(), &state).unwrap();
        let loaded = load(dir.path(), "alice").unwrap();
        // f32 round-trip via JSON: serde_json emits the f32 promoted to f64 with
        // sufficient precision that the values 0.75, 0.5, 1.0 (all exactly
        // representable in binary) come back bit-identical, so PartialEq holds.
        assert_eq!(state, loaded);
    }

    #[test]
    fn cold_start_load_returns_default_state() {
        let dir = TempDir::new().unwrap();
        let s = load(dir.path(), "newcomer").unwrap();
        assert_eq!(s.learner_id, "newcomer");
        assert!(s.topics_covered.is_empty());
        assert!(s.topics_weak.is_empty());
        assert!(s.mastery_scores.is_empty());
        assert_eq!(s.last_session_ts, "1970-01-01T00:00:00Z");
    }

    #[test]
    fn rejects_bad_learner_id() {
        let dir = TempDir::new().unwrap();
        for bad in &["../etc", "a/b", "", "X", ".hidden", "foo bar", "foo.bar"] {
            let r = load(dir.path(), bad);
            assert!(r.is_err(), "expected err for {:?}", bad);
            let r2 = state_path(dir.path(), bad);
            assert!(r2.is_err(), "expected err for state_path({:?})", bad);
        }
        // Length > 64 also rejected.
        let too_long = "a".repeat(65);
        assert!(state_path(dir.path(), &too_long).is_err());
    }

    #[test]
    fn accepts_valid_learner_ids() {
        let dir = TempDir::new().unwrap();
        for ok in &["alice", "alice_2", "learner-001", "0", "z"] {
            let p = state_path(dir.path(), ok);
            assert!(p.is_ok(), "expected ok for {:?}: {:?}", ok, p.err());
            let l = load(dir.path(), ok);
            assert!(l.is_ok(), "expected ok load for {:?}", ok);
        }
        // Max length boundary: exactly 64 chars.
        let max_id = format!("a{}", "b".repeat(63));
        assert_eq!(max_id.len(), 64);
        assert!(state_path(dir.path(), &max_id).is_ok());
    }

    #[test]
    fn list_learners_returns_all_json_files() {
        let dir = TempDir::new().unwrap();
        let alice = LearnerState {
            learner_id: "alice".into(),
            topics_covered: vec![],
            topics_weak: vec![],
            mastery_scores: BTreeMap::new(),
            last_session_ts: epoch_zero_iso(),
        };
        let bob = LearnerState {
            learner_id: "bob".into(),
            ..alice.clone()
        };
        save(dir.path(), &alice).unwrap();
        save(dir.path(), &bob).unwrap();

        // Decoy: a non-.json file, a dotfile, and a subdirectory should all be skipped.
        let state_dir = dir.path().join("state");
        std::fs::write(state_dir.join("notes.txt"), b"ignore me").unwrap();
        std::fs::write(state_dir.join(".hidden.json"), b"{}").unwrap();
        std::fs::create_dir_all(state_dir.join("subdir.json")).unwrap();

        let learners = list_learners(dir.path()).unwrap();
        assert_eq!(learners, vec!["alice".to_string(), "bob".to_string()]);
    }

    #[test]
    fn list_learners_returns_empty_when_state_dir_missing() {
        let dir = TempDir::new().unwrap();
        let learners = list_learners(dir.path()).unwrap();
        assert!(learners.is_empty());
    }

    #[test]
    fn save_is_atomic_concurrent() {
        let dir = Arc::new(TempDir::new().unwrap());
        let mut handles = Vec::new();
        for marker in ["one", "two"] {
            let dir = Arc::clone(&dir);
            let marker = marker.to_string();
            handles.push(std::thread::spawn(move || {
                let mut scores = BTreeMap::new();
                scores.insert(format!("topic.{marker}"), 0.5_f32);
                let state = LearnerState {
                    learner_id: "alice".into(),
                    topics_covered: vec![marker.clone()],
                    topics_weak: vec![],
                    mastery_scores: scores,
                    last_session_ts: format!("2026-04-28T10:00:0{}Z", &marker[..1]),
                };
                // Hammer save a few times to widen the race window.
                for _ in 0..10 {
                    save(dir.path(), &state).unwrap();
                }
            }));
        }
        for h in handles {
            h.join().unwrap();
        }
        // Final state must parse cleanly and match exactly one of the writers.
        let loaded = load(dir.as_ref().path(), "alice").unwrap();
        assert_eq!(loaded.learner_id, "alice");
        assert_eq!(loaded.topics_covered.len(), 1);
        let marker = &loaded.topics_covered[0];
        assert!(
            marker == "one" || marker == "two",
            "unexpected marker: {marker}"
        );
        // Mastery scores must be consistent with the same marker — i.e. no
        // tearing across the two writers' payloads.
        assert!(loaded
            .mastery_scores
            .contains_key(&format!("topic.{marker}")));
        assert_eq!(loaded.mastery_scores.len(), 1);
    }

    /// Regression: temp names were clock-based and collided between threads
    /// (the losing `rename` failed). 8 writers x 100 saves never error.
    #[test]
    fn concurrent_saves_never_collide() {
        let dir = Arc::new(TempDir::new().unwrap());
        let handles: Vec<_> = (0..8)
            .map(|k| {
                let dir = Arc::clone(&dir);
                std::thread::spawn(move || {
                    let state = LearnerState {
                        learner_id: "alice".into(),
                        topics_covered: vec![format!("t{k}")],
                        topics_weak: vec![],
                        mastery_scores: BTreeMap::new(),
                        last_session_ts: "x".into(),
                    };
                    for _ in 0..100 {
                        save(dir.path(), &state).unwrap();
                    }
                })
            })
            .collect();
        for h in handles {
            h.join().unwrap();
        }
    }
}
