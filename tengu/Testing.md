---
tags:
  - development
  - testing
---

# Testing

Testing strategy for [[Overview|Tengu Cluster]], following the [[Principles|project principles]].

## Test Strategy

Every task should be followed by tests and doc updates. Tests verify:

1. **Domain logic** — pure business rules
2. **Integration** — adapter behavior with real dependencies

## Integration Tests

### Qdrant Memory Store
- Marked `#[ignore]` — requires running Qdrant instance
- Start Qdrant: `docker run -p 6333:6333 -p 6334:6334 qdrant/qdrant`
- Run: `cargo test --features qdrant -- --ignored`

## Event-Bus Tests

The event-bus orchestration architecture has test coverage across the consolidated builder modules:

| Module | What's Covered |
|--------|---------------|
| `types` | OrchestratorEvent variants, Plan/Task state machine, EventBus channels, buffer scaling |
| `agent_builder` | Task completion, errors, retryability, shutdown, artifact extraction |
| `event_orchestrator` | Context routing, cascade-skip, timeouts, integration (task chains, parallel, retry, failure) |
| `orchestrator` | RunBudget tracking, agent listing, plan generation |

## What to Test When Adding Features

| Change Type | Required Tests |
|-------------|---------------|
| New type | Unit tests for invariants |
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

# Format check
cargo fmt --all -- --check

# Lint
cargo clippy --workspace
```

## Related

- [[Architecture]] — project structure
- [[Principles]] — "every task includes tests"
- [[Deployment]] — CI/CD pipeline
