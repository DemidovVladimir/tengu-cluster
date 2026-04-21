//! Harness-owned memory subsystem.
//!
//! - `provider` — MemoryProvider trait (Hermes-shaped)
//! - `builtin` — BuiltinMemoryProvider (MEMORY.md, identity, daily logs, vector)
//! - `manager` — MemoryManager holding one builtin + at most one external provider
//! - `injector` — pre-turn fenced context injection
//! - `writer` — post-turn spawned non-blocking writes
//! - `fencing` — `<memory-context>` block helpers
//! - `vector` — embeddings + Qdrant/bincode backend
//! - `context_block` — shared types

pub mod builtin;
pub mod context_block;
pub mod fencing;
pub mod injector;
pub mod manager;
pub mod provider;
pub mod vector;
pub mod writer;
