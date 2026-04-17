//! Golden tool-surface regression test.
//!
//! The tengu-cluster crate ships as a binary (no `lib.rs`), so this integration
//! test cannot reach `pub(crate)` items such as `build_tool_executor` or
//! `ToolRegistry`. The real golden test lives inline:
//!
//! - `src/adapters/channel_runtime.rs` — `golden_tests::tool_names_match_pre_migration_surface`
//!
//! That test asserts the set of registered tool names equals the pre-migration
//! set and runs on every `cargo test` invocation alongside the plugin unit tests.

#[test]
fn tool_registry_golden_test_lives_inline_see_module_docs() {
    // Marker test — keeps this file from appearing empty to cargo/test runners.
}
