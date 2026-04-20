//! Metric kind implementations. One file per kind.

pub(crate) mod shell_check;
// pub(crate) mod tool_assertion; // Task 4
// pub(crate) mod script;         // Task 5
// pub(crate) mod llm_judge;      // Task 6

pub(crate) use shell_check::ShellCheckKind;
