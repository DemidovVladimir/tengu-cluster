---
tags:
  - development
  - testing
---

# Testing

Testing strategy for [[Overview|Tengu Cluster]], following the [[Principles|project principles]].

## Test Strategy

Every task should be followed by tests and doc updates. Tests verify:

1. **[[Architecture]] enforcement** — hex layer boundaries
2. **Domain logic** — pure business rules
3. **Integration** — adapter behavior with real dependencies

## Architecture Enforcement Tests

5 tests in `tests/hex_architecture_enforcement.rs`:

| Test | Verifies |
|------|----------|
| Domain purity | No `reqwest`, `cursive`, `std::fs`, `tokio::process` in `src/domain/` |
| Application purity | No `reqwest`, `cursive`, `std::fs`, `tokio::process` in `src/application/` |
| Main entry | Hexagonal module usage, no direct HTTP/process probing |
| Module layout | `domain/`, `application/`, `adapters/` directories exist |
| Port definitions | `src/application/ports.rs` contains trait definitions |

Run with:
```bash
cargo test --workspace
```

## Integration Tests

### Qdrant Memory Store
- Marked `#[ignore]` — requires running Qdrant instance
- Start Qdrant: `docker run -p 6333:6333 -p 6334:6334 qdrant/qdrant`
- Run: `cargo test --features qdrant -- --ignored`

## What to Test When Adding Features

| Change Type | Required Tests |
|-------------|---------------|
| New domain type | Unit tests for invariants |
| New port | Architecture test still passes |
| New adapter | Integration test with real deps |
| New [[Tools|tool]] | Tool execution test |
| New [[Skills|skill]] | Skill parsing + discovery test |
| New [[Channels|channel]] | Pipe + approval adapter test |

## Running Tests

```bash
# All tests
cargo test --workspace

# With Qdrant integration
cargo test --workspace --features qdrant -- --ignored

# Architecture only
cargo test hex_architecture

# Format check
cargo fmt --all -- --check

# Lint
cargo clippy --workspace
```

## Related

- [[Architecture]] — what enforcement tests verify
- [[Principles]] — "every task includes tests"
- [[Deployment]] — CI/CD pipeline
