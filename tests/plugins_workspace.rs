//! Workspace-plugin unit tests.
//!
//! The tengu-cluster crate ships as a binary (no `lib.rs`), so integration
//! tests cannot reach `pub(crate)` items such as `ReadFileTool`, `ToolCtx`,
//! `PluginToolExecutor`, or `WorkspacePlugin`. To keep the tests close to the
//! types they exercise, the real workspace-plugin tests live as `#[cfg(test)]`
//! modules inside each tool file:
//!
//! - `src/adapters/plugins/workspace/read_file.rs` — `tests::read_file_*`
//! - `src/adapters/plugins/workspace/list_directory.rs` — `tests::list_directory_*`
//! - `src/adapters/plugins/workspace/write_file.rs` — `tests::write_file_*`
//! - `src/adapters/plugins/workspace/run_command.rs` — `tests::run_command_*`
//!
//! Each tool has a happy-path test and a scope-denied test; all eight run
//! under `cargo test` alongside the rest of the binary's unit tests.

#[test]
fn plugins_workspace_tests_live_inline_see_module_docs() {
    // Marker test — keeps this file from appearing empty to cargo/test runners.
}
