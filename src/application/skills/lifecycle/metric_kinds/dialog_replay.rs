//! `dialog_replay` — score a skill against the *current* conversation slice
//! `[from_message_index..end]` instead of pre-authored `evals/prompts.yaml`
//! rows. Synthesizes a single in-memory `Fixture` from the slice's last user
//! message and delegates scoring to a sibling metric on the same skill
//! (`llm_judge` or `tool_assertion`). See `docs/skill-research-2026-04-28.md`
//! Batch 8 actions #23 & #25.
//!
//! Doctrine notes:
//! - Fail-soft. Bad slice indices, missing conversation, or unknown delegate
//!   metrics all return a `MetricOutcome { pass: false, ... }` with notes
//!   rather than `bail!`. The eval runner shouldn't crash because one metric
//!   configuration is broken.
//! - Pure dispatch. This kind never scores on its own — it just rewires
//!   inputs and calls another `MetricKind`.
//!
//! ## Required extension to `MetricRunCtx` (parent agent: paste into
//! `src/adapters/skill_lifecycle/metrics.rs`, ~lines 96–109, mirror the
//! existing optional-pointer pattern):
//!
//! ```ignore
//! pub struct MetricRunCtx<'a> {
//!     // ... existing fields ...
//!     /// Conversation slice the metric may inspect (e.g. `DialogReplay`).
//!     /// `None` outside of in-chat reflective evals; pre-authored fixtures
//!     /// don't need it.
//!     pub conversation: Option<&'a [crate::domain::message::Message]>,
//!     /// Sibling metric specs on the same skill, used by `DialogReplay`
//!     /// to look up the named `delegate_metric`. `None` falls through to
//!     /// "delegate not found".
//!     pub sibling_metrics: Option<&'a [MetricSpec]>,
//! }
//! ```
//!
//! ## Required `MetricSpec::DialogReplay` variant (parent agent: paste into
//! `MetricSpec`, mirror the other variants' field set):
//!
//! ```ignore
//! DialogReplay {
//!     name: String,
//!     /// 0-based message index where the slice starts. End is current
//!     /// end-of-conversation.
//!     from_message_index: usize,
//!     /// Name of an `llm_judge` or `tool_assertion` metric on the same
//!     /// skill that this replay delegates to for actual scoring.
//!     delegate_metric: String,
//!     #[serde(default)]
//!     expected_outcome: Option<String>,
//!     #[serde(default)]
//!     min_pass_rate: Option<f32>,
//! }
//! ```
//!
//! Add the variant to `MetricSpec::name()` and `MetricSpec::min_pass_rate()`
//! match arms, and in `validate_metrics`:
//!
//! ```ignore
//! MetricSpec::DialogReplay { delegate_metric, .. } => {
//!     if delegate_metric.trim().is_empty() {
//!         bail!("metric '{}' delegate_metric empty", name);
//!     }
//! }
//! ```

use anyhow::Result;
use async_trait::async_trait;
use serde_json::json;

use crate::application::skills::lifecycle::metrics::{
    FixtureContext, MetricKind, MetricOutcome, MetricRunCtx, MetricSpec,
};
use crate::domain::message::{Message, Role};

use super::{LlmJudgeKind, ToolAssertionKind};

pub(crate) struct DialogReplayKind;

#[async_trait]
impl MetricKind for DialogReplayKind {
    async fn run(
        &self,
        spec: &MetricSpec,
        _fixture: &FixtureContext<'_>,
        ctx: &MetricRunCtx<'_>,
    ) -> Result<MetricOutcome> {
        let (from_idx, delegate_name, expected) = match spec {
            MetricSpec::DialogReplay {
                from_message_index,
                delegate_metric,
                expected_outcome,
                ..
            } => (
                *from_message_index,
                delegate_metric.clone(),
                expected_outcome.clone(),
            ),
            _ => anyhow::bail!("DialogReplayKind given wrong spec"),
        };

        // Precondition 1: conversation must be present in ctx. Fail-soft.
        let Some(messages) = ctx.conversation else {
            return Ok(fail("conversation slice unavailable in run context"));
        };

        // Precondition 2: from_message_index must be in range and yield a
        // non-empty slice.
        if from_idx > messages.len() {
            return Ok(fail(&format!(
                "from_message_index {} out of range (len {})",
                from_idx,
                messages.len()
            )));
        }
        let slice = &messages[from_idx..];
        if slice.is_empty() {
            return Ok(fail("dialog slice is empty"));
        }

        // Synthesize the fixture: prompt is the LAST user message in the
        // slice (most recent ask); transcript is the full slice rendered
        // role-by-role; expected_outcome is passed through.
        let Some(prompt) = last_user_message(slice) else {
            return Ok(fail("dialog slice contains no user message"));
        };
        let transcript = render_transcript(slice);
        let synth_fixture = FixtureContext {
            prompt: &prompt,
            expected_outcome: expected.as_deref(),
            transcript: &transcript,
        };

        // Precondition 3: delegate metric must exist on the same skill.
        let Some(siblings) = ctx.sibling_metrics else {
            return Ok(fail(
                "sibling_metrics unavailable in run context (cannot resolve delegate)",
            ));
        };
        let Some(delegate_spec) = siblings.iter().find(|s| s.name() == delegate_name) else {
            return Ok(fail(&format!(
                "delegate metric '{}' not found on this skill",
                delegate_name
            )));
        };

        // Dispatch to the delegate. Only `llm_judge` and `tool_assertion`
        // are supported delegates per the design doc; other kinds (Script,
        // ShellCheck, DialogReplay-recursive) fail-soft.
        match delegate_spec {
            MetricSpec::LlmJudge { .. } => {
                LlmJudgeKind.run(delegate_spec, &synth_fixture, ctx).await
            }
            MetricSpec::ToolAssertion { .. } => {
                ToolAssertionKind
                    .run(delegate_spec, &synth_fixture, ctx)
                    .await
            }
            other => Ok(fail(&format!(
                "delegate metric '{}' has unsupported kind '{}' \
                 (only llm_judge / tool_assertion are valid replay delegates)",
                delegate_name,
                spec_kind_label(other),
            ))),
        }
    }
}

/// Build a fail-soft `MetricOutcome` with explanatory `notes`.
fn fail(note: &str) -> MetricOutcome {
    MetricOutcome {
        pass: false,
        score: 0.0,
        notes: Some(note.to_string()),
        raw: json!({ "kind": "dialog_replay", "error": note }),
    }
}

/// Return the content of the last `Role::User` message in `slice`, or `None`.
fn last_user_message(slice: &[Message]) -> Option<String> {
    slice
        .iter()
        .rev()
        .find(|m| matches!(m.role, Role::User))
        .map(|m| m.content.clone())
}

/// Render the slice as a flat transcript (`role: content` per line). Tool
/// messages are included so the judge can see tool I/O if relevant.
fn render_transcript(slice: &[Message]) -> String {
    let mut out = String::new();
    for m in slice {
        let role = match m.role {
            Role::System => "system",
            Role::User => "user",
            Role::Assistant => "assistant",
            Role::Tool => "tool",
        };
        out.push_str(role);
        out.push_str(": ");
        out.push_str(&m.content);
        out.push('\n');
    }
    out
}

fn spec_kind_label(spec: &MetricSpec) -> &'static str {
    match spec {
        MetricSpec::ShellCheck { .. } => "shell_check",
        MetricSpec::LlmJudge { .. } => "llm_judge",
        MetricSpec::ToolAssertion { .. } => "tool_assertion",
        MetricSpec::Script { .. } => "script",
        MetricSpec::DialogReplay { .. } => "dialog_replay",
        MetricSpec::DescriptionTrigger { .. } => "description_trigger",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::application::skills::lifecycle::metrics::JudgeClient;
    use std::path::Path;
    use std::sync::Arc;
    use tempfile::TempDir;

    // -- fixtures ---------------------------------------------------------

    struct StubJudge(&'static str);
    #[async_trait]
    impl JudgeClient for StubJudge {
        async fn judge(&self, _: &str, _: &str, _: &str, _: Option<&str>) -> Result<String> {
            Ok(self.0.to_string())
        }
    }

    struct NoShell;
    impl crate::ports::shell::ShellExecutionPort for NoShell {
        fn execute_shell(&self, _: &str, _: &Path) -> Result<String> {
            Ok(String::new())
        }
    }

    fn msg(role: Role, content: &str) -> Message {
        Message {
            role,
            content: content.into(),
            tool_call_id: None,
            tool_calls: None,
        }
    }

    /// Build a `MetricRunCtx` populated with the bits `DialogReplay` cares
    /// about (skill_dir, judge, conversation, sibling_metrics). Other
    /// fields default to `None`.
    fn ctx<'a>(
        skill_dir: &'a Path,
        workspace: &'a Path,
        shell: &'a dyn crate::ports::shell::ShellExecutionPort,
        judge: Option<Arc<dyn JudgeClient>>,
        conversation: Option<&'a [Message]>,
        siblings: Option<&'a [MetricSpec]>,
    ) -> MetricRunCtx<'a> {
        MetricRunCtx {
            skill_dir,
            workspace,
            shell,
            tools: None,
            judge,
            http: None,
            memory_manager: None,
            secret_registry: None,
            activity: None,
            tool_scopes: None,
            conversation,
            sibling_metrics: siblings,
        }
    }

    fn dummy_fixture() -> FixtureContext<'static> {
        FixtureContext {
            prompt: "",
            expected_outcome: None,
            transcript: "",
        }
    }

    // -- tests ------------------------------------------------------------

    #[tokio::test]
    async fn scores_via_llm_judge_delegate() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("r.md"), "criterion: replies cleanly\n").unwrap();
        let ws = std::env::temp_dir();
        let shell = NoShell;

        let conversation = vec![
            msg(Role::User, "tell me about doctrine"),
            msg(Role::Assistant, "LLM = heart, RAG = brain, tools = hands."),
            msg(Role::User, "evaluate"),
        ];
        let siblings = vec![MetricSpec::LlmJudge {
            name: "rubric_pass".into(),
            rubric_file: "r.md".into(),
            judge_model: None,
            min_pass_rate: None,
        }];

        let judge: Arc<dyn JudgeClient> = Arc::new(StubJudge(
            r#"{"verdict":"pass","score":0.9,"notes":"clean"}"#,
        ));
        let ctx = ctx(
            dir.path(),
            &ws,
            &shell,
            Some(judge),
            Some(&conversation),
            Some(&siblings),
        );

        let spec = MetricSpec::DialogReplay {
            name: "replay_current".into(),
            from_message_index: 0,
            delegate_metric: "rubric_pass".into(),
            expected_outcome: Some("doctrine summary".into()),
            min_pass_rate: None,
        };
        let out = DialogReplayKind
            .run(&spec, &dummy_fixture(), &ctx)
            .await
            .unwrap();

        assert!(out.pass, "expected pass, got: {out:?}");
        assert!((out.score - 0.9).abs() < 1e-4);
    }

    #[tokio::test]
    async fn rejects_when_slice_empty() {
        let dir = TempDir::new().unwrap();
        let ws = std::env::temp_dir();
        let shell = NoShell;

        let conversation = vec![msg(Role::User, "hi"), msg(Role::Assistant, "hello")];
        let siblings: Vec<MetricSpec> = vec![];
        let ctx = ctx(
            dir.path(),
            &ws,
            &shell,
            None,
            Some(&conversation),
            Some(&siblings),
        );

        let spec = MetricSpec::DialogReplay {
            name: "replay".into(),
            from_message_index: 2, // == len, slice is empty
            delegate_metric: "unused".into(),
            expected_outcome: None,
            min_pass_rate: None,
        };
        let out = DialogReplayKind
            .run(&spec, &dummy_fixture(), &ctx)
            .await
            .unwrap();

        assert!(!out.pass);
        assert_eq!(out.score, 0.0);
        let notes = out.notes.unwrap();
        assert!(
            notes.contains("empty"),
            "expected empty-slice note, got: {notes}"
        );
    }

    #[tokio::test]
    async fn rejects_when_from_index_out_of_range() {
        let dir = TempDir::new().unwrap();
        let ws = std::env::temp_dir();
        let shell = NoShell;

        let conversation = vec![msg(Role::User, "hi")];
        let siblings: Vec<MetricSpec> = vec![];
        let ctx = ctx(
            dir.path(),
            &ws,
            &shell,
            None,
            Some(&conversation),
            Some(&siblings),
        );

        let spec = MetricSpec::DialogReplay {
            name: "replay".into(),
            from_message_index: 99, // > len
            delegate_metric: "unused".into(),
            expected_outcome: None,
            min_pass_rate: None,
        };
        let out = DialogReplayKind
            .run(&spec, &dummy_fixture(), &ctx)
            .await
            .unwrap();

        assert!(!out.pass);
        let notes = out.notes.unwrap();
        assert!(
            notes.contains("out of range"),
            "expected out-of-range note, got: {notes}"
        );
    }

    #[tokio::test]
    async fn rejects_when_delegate_unknown() {
        let dir = TempDir::new().unwrap();
        let ws = std::env::temp_dir();
        let shell = NoShell;

        let conversation = vec![msg(Role::User, "hi"), msg(Role::Assistant, "hello")];
        let siblings: Vec<MetricSpec> = vec![]; // no metrics at all
        let ctx = ctx(
            dir.path(),
            &ws,
            &shell,
            None,
            Some(&conversation),
            Some(&siblings),
        );

        let spec = MetricSpec::DialogReplay {
            name: "replay".into(),
            from_message_index: 0,
            delegate_metric: "nonexistent".into(),
            expected_outcome: None,
            min_pass_rate: None,
        };
        let out = DialogReplayKind
            .run(&spec, &dummy_fixture(), &ctx)
            .await
            .unwrap();

        assert!(!out.pass);
        let notes = out.notes.unwrap();
        assert!(
            notes.contains("nonexistent") && notes.contains("not found"),
            "expected delegate-not-found note, got: {notes}"
        );
    }
}
