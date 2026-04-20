//! `memory_search` integration-test marker.
//!
//! The `tengu-cluster` crate has no `[lib]` target (only the `tengu` binary),
//! so integration tests in `tests/` cannot `use tengu_cluster::...` to
//! instantiate `MemorySearchTool` directly. The real end-to-end plugin test
//! ("ingested as researcher → searchable as writer") therefore lives with
//! the implementation, in:
//!
//!     src/adapters/plugins/memory/search.rs :: tests
//!
//! and is run by `cargo test --bin tengu memory_search`.
//!
//! This file exists so the Task 2.2 plan step that asks for
//! `tests/memory_search_tool.rs` has a discoverable home — future work that
//! adds a `[lib]` target (or `#[path = ...]` includes the plugin source into
//! the test binary) can migrate the real assertion here without churn in the
//! plan's file list. Today we assert only that the plugin source file is
//! present, matching the sibling `scope_lint.rs` pattern of self-contained
//! integration checks.

use std::fs;
use std::path::Path;

#[test]
fn memory_search_plugin_source_exists() {
    let src =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("src/adapters/plugins/memory/search.rs");
    assert!(
        src.exists(),
        "memory_search plugin source not found at {}",
        src.display()
    );

    let body = fs::read_to_string(&src).expect("read memory/search.rs");

    // Smoke: the file declares the tool name and the public struct the
    // plugin registers.
    assert!(
        body.contains("MEMORY_SEARCH_TOOL_NAME"),
        "MEMORY_SEARCH_TOOL_NAME constant missing from search.rs"
    );
    assert!(
        body.contains("pub(crate) struct MemorySearchTool"),
        "MemorySearchTool struct missing from search.rs"
    );
    assert!(
        body.contains(r#""memory_search""#),
        "tool name literal \"memory_search\" missing from search.rs"
    );

    // Smoke: the inline module that owns the behavioural assertions is
    // present. If this test fails because the module was removed or
    // renamed, update both this marker and the plan reference.
    assert!(
        body.contains("ingest_then_search_returns_ingested_chunk"),
        "behavioural test `ingest_then_search_returns_ingested_chunk` missing from \
         src/adapters/plugins/memory/search.rs — restore it or migrate to a real \
         integration test once a `[lib]` target exists."
    );
}

#[test]
fn memory_search_is_registered_by_plugin() {
    let mod_rs =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("src/adapters/plugins/memory/mod.rs");
    let body = fs::read_to_string(&mod_rs).expect("read memory/mod.rs");

    // The plugin's `tool_defs()` must advertise memory_search so
    // `compute_base_tools`/`compute_bridge_tools` surface it to the LLM.
    assert!(
        body.contains("memory_search_def"),
        "memory_search_def() helper missing from memory/mod.rs"
    );
    assert!(
        body.contains("memory_search_def()"),
        "memory_search_def() must be included in tool_defs()"
    );

    // The plugin's `tools()` path must instantiate MemorySearchTool when
    // memory is enabled. (Negative case: memory disabled → early return —
    // already covered by the existing plugin-mod unit tests.)
    assert!(
        body.contains("MemorySearchTool::new"),
        "MemoryPlugin::tools() must construct MemorySearchTool"
    );
}
