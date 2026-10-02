//! Engine smoke turn (`x-engine-matrix-smoke`): the fixed turn
//! `tengu doctor --engines` runs on every agent's own engine + model, and its
//! verdict. Pure — the doctor makes the temp workspace and the token, runs
//! the turn, and hands the activity here. The live `#[ignore]` matrix
//! (`tests/engine_matrix.rs`) applies the same rules to a larger tool set.
//!
//! | Check | Passes when |
//! |---|---|
//! | called | every expected tool ran at least once |
//! | no error | no run of an expected tool returned an error |
//! | read | the final text holds the token — only `read_file`'s result has it, and only `list_directory`'s result names its file |

use crate::domain::message::ToolRun;

/// The doctor's smoke tools, in the order the prompt uses them.
pub const SMOKE_TOOLS: [&str; 2] = ["list_directory", "read_file"];

/// File-name prefix of the token file in the smoke workspace.
pub const TOKEN_FILE_PREFIX: &str = "smoke-token-";

/// Round cap of the smoke turn (it needs 3: list, read, answer); an agent's
/// lower `limits.max_tool_rounds` wins.
pub const SMOKE_MAX_ROUNDS: u32 = 6;

/// System prompt of the smoke turn.
pub const SMOKE_SYSTEM: &str = "You are a tool smoke test. Follow the user's steps exactly, \
using the named tools. Do not ask questions.";

/// User prompt of the smoke turn. Tool names are given in both forms a model
/// may see: bare (in-process) and bridged (`mcp__tengu-tools__<name>`).
pub fn smoke_prompt() -> String {
    format!(
        "1. Call the list_directory tool (mcp__tengu-tools__list_directory) with path \".\". \
         One file in the listing starts with \"{TOKEN_FILE_PREFIX}\".\n\
         2. Call the read_file tool (mcp__tengu-tools__read_file) on that file.\n\
         3. Reply with the file's content exactly, on one line, and nothing else."
    )
}

/// Outcome of one smoke turn.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SmokeVerdict {
    /// Expected tools that never ran.
    pub missing: Vec<String>,
    /// Expected tools with at least one run that returned an error.
    pub failed: Vec<String>,
    /// The final text holds the token.
    pub token_read: bool,
}

impl SmokeVerdict {
    pub fn ok(&self) -> bool {
        self.missing.is_empty() && self.failed.is_empty() && self.token_read
    }

    /// One line naming every failed check; empty when `ok`.
    pub fn problems(&self) -> String {
        let mut p = Vec::new();
        if !self.missing.is_empty() {
            p.push(format!("not called: {}", self.missing.join(", ")));
        }
        if !self.failed.is_empty() {
            p.push(format!("returned an error: {}", self.failed.join(", ")));
        }
        if !self.token_read {
            p.push("token not in the answer (result not read)".to_string());
        }
        p.join("; ")
    }
}

/// Judge a turn: `runs` is its activity (`EngineResponse::tool_runs` / IPC
/// `tools`), `text` its final answer.
pub fn smoke_verdict(expected: &[&str], runs: &[ToolRun], text: &str, token: &str) -> SmokeVerdict {
    let names = |pred: &dyn Fn(&str) -> bool| -> Vec<String> {
        expected
            .iter()
            .filter(|t| pred(t))
            .map(|t| t.to_string())
            .collect()
    };
    SmokeVerdict {
        missing: names(&|t| !runs.iter().any(|r| r.name == t)),
        failed: names(&|t| runs.iter().any(|r| r.name == t && !r.ok)),
        token_read: !token.is_empty() && text.contains(token),
    }
}

/// The "tools called" cell: every run in call order, a failed one as
/// `<name>:error`; `-` for none.
pub fn runs_summary(runs: &[ToolRun]) -> String {
    if runs.is_empty() {
        return "-".to_string();
    }
    runs.iter()
        .map(|r| {
            if r.ok {
                r.name.clone()
            } else {
                format!("{}:error", r.name)
            }
        })
        .collect::<Vec<_>>()
        .join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(name: &str, ok: bool) -> ToolRun {
        ToolRun {
            name: name.to_string(),
            ok,
        }
    }

    const TOKEN: &str = "smoke-5f0c3a9e1b7d4c2a8e6f0b1d3c5a7e9f";

    /// Table-driven: (runs, answer) → (missing, failed, token_read, ok).
    #[test]
    fn verdict_vectors() {
        let answer = format!("The content is {TOKEN}.");
        let cases: Vec<(Vec<ToolRun>, &str, &[&str], &[&str], bool, bool)> = vec![
            (
                vec![run("list_directory", true), run("read_file", true)],
                &answer,
                &[],
                &[],
                true,
                true,
            ),
            // Extra tools (compress_and_store, built-ins) do not matter.
            (
                vec![
                    run("list_directory", true),
                    run("Read", false),
                    run("read_file", true),
                    run("compress_and_store", false),
                ],
                &answer,
                &[],
                &[],
                true,
                true,
            ),
            // A retry after an error still fails the tool.
            (
                vec![
                    run("list_directory", true),
                    run("read_file", false),
                    run("read_file", true),
                ],
                &answer,
                &[],
                &["read_file"],
                true,
                false,
            ),
            (
                vec![run("read_file", true)],
                &answer,
                &["list_directory"],
                &[],
                true,
                false,
            ),
            (
                vec![run("list_directory", true), run("read_file", true)],
                "I read the file.",
                &[],
                &[],
                false,
                false,
            ),
            (
                vec![],
                "",
                &["list_directory", "read_file"],
                &[],
                false,
                false,
            ),
        ];
        for (i, (runs, text, missing, failed, token_read, ok)) in cases.into_iter().enumerate() {
            let v = smoke_verdict(&SMOKE_TOOLS, &runs, text, TOKEN);
            assert_eq!(v.missing, missing, "case {i}");
            assert_eq!(v.failed, failed, "case {i}");
            assert_eq!(v.token_read, token_read, "case {i}");
            assert_eq!(v.ok(), ok, "case {i}");
            assert_eq!(v.problems().is_empty(), ok, "case {i}: {}", v.problems());
        }
    }

    #[test]
    fn empty_token_is_never_read() {
        let runs = [run("list_directory", true), run("read_file", true)];
        assert!(!smoke_verdict(&SMOKE_TOOLS, &runs, "anything", "").token_read);
    }

    #[test]
    fn problems_name_every_failed_check() {
        let v = smoke_verdict(&SMOKE_TOOLS, &[run("read_file", false)], "", TOKEN);
        assert_eq!(
            v.problems(),
            "not called: list_directory; returned an error: read_file; token not in the answer (result not read)"
        );
    }

    #[test]
    fn summary_lists_runs_in_order() {
        assert_eq!(runs_summary(&[]), "-");
        assert_eq!(
            runs_summary(&[run("list_directory", true), run("read_file", false)]),
            "list_directory, read_file:error"
        );
    }

    #[test]
    fn prompt_names_both_tool_forms_and_the_prefix() {
        let p = smoke_prompt();
        for t in SMOKE_TOOLS {
            assert!(p.contains(t), "{t}");
            assert!(p.contains(&format!("mcp__tengu-tools__{t}")), "{t}");
        }
        assert!(p.contains(TOKEN_FILE_PREFIX));
    }
}
