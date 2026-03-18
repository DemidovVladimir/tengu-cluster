---
tags:
  - core
  - tools
---

# Tools

**Tools** are general-purpose primitives that [[Agents]] use to interact with the world. They are intentionally few, stable, and reusable across all [[Skills]].

## Design Philosophy

Tools are the **syscall layer** of Tengu. Rather than creating a tool for every specific task, the system provides a small set of powerful primitives. Domain-specific behavior lives in [[Skills]], which compose these primitives.

This means:
- [[Skills]] are plug-and-play (no code changes, no platform-specific frontmatter)
- [[Capabilities]] gate tool access per agent
- Any external skill (beach.science, privy, booking SDK) works by composing these primitives

## Workspace Primitives

| Primitive | Risk | Approval | Description |
|-----------|------|----------|-------------|
| `read_file` | Low | No | Read file contents (text and PDF) |
| `list_directory` | Low | No | List files and directories |
| `write_file` | Medium | Yes | Write content to file |
| `run_command` | High | Yes | Execute shell command in workspace |

Defined in `src/application/workspace_tools_catalog.rs`.

## Platform Primitives

| Primitive | Risk | Approval | Description |
|-----------|------|----------|-------------|
| `http_request` | Medium | Yes | Generic HTTP client (JSON, multipart, bearer/basic auth) |
| `sign_and_send_transaction` | High | Yes | EVM transaction via Privy wallet, waits for receipt |
| `sign_message` | High | Yes | Message signing via Privy wallet |
| `get_wallet_address` | Low | No | Read configured wallet address |
| `abi_encode` | Low | No | ABI-encode EVM function calls into calldata hex |

Defined in `src/application/platform_tools_catalog.rs`.

### `http_request` — the skill enabler

This is the keystone tool that makes external skills cross-platform compatible. Any skill that documents REST API endpoints (beach.science, privy, molecule, booking APIs) works through `http_request`.

Features:
- `$ENV_VAR` expansion in headers (secrets never appear in LLM output)
- `auth_bearer_env` / `auth_basic_user_env` + `auth_basic_pass_env` for auth
- `file_path` + `file_field_name` for multipart/form-data uploads
- Response truncation for large payloads

### Crypto tools

`sign_and_send_transaction`, `sign_message`, and `get_wallet_address` extract Privy agentic wallet operations into reusable primitives. Any skill that needs on-chain operations (DeSci minting, NFT transfers, token operations) composes these.

### `abi_encode` — EVM calldata builder

Pure computation tool (no side effects, no approval required). Takes a Solidity function signature and arguments, returns `0x`-prefixed hex calldata for use with `sign_and_send_transaction`.

```
abi_encode:
  function_signature: "mintReservation(address,uint256,string,string,bytes)"
  args: ["0xWallet...", "42", "ipfs://Qm...", "VDNA", "0xauth..."]
```

Supports: `address`, `uint256`/`uint128`/etc, `int256`, `string`, `bytes`, `bytesN`, `bool`, arrays, tuples. Uses `alloy::dyn_abi` for encoding. Defined in `src/adapters/crypto_tool_executor.rs`.

## Memory Tools

When [[Memory]] is enabled, the memory subsystem registers its own tools:
- **`remember`** — store content with optional metadata tags
- **`recall`** — retrieve similar entries via vector search

Defined in `src/adapters/memory_tool_executor.rs`.

## Tool Approval

Tools with `requires_approval: true` go through the `ToolApprovalPort`:
- **TUI**: interactive dialog
- **Telegram**: inline keyboard (Approve/Deny, 60s timeout)

Approval is **metadata-driven** (risk level + description), not hardcoded per tool name. Shared UI logic in `src/adapters/tool_ui.rs`.

## Tool Execution Flow

```
Agent request -> Engine emits ToolCall
  -> ToolUseService checks approval (via ToolApprovalPort)
  -> CompositeToolExecutor routes to correct executor
  -> Result sanitized (secrets redacted)
  -> Returned to engine for next round (up to 15 rounds)
```

## Related

- [[Agents]] — who uses tools
- [[Skills]] — domain knowledge that guides tool usage
- [[Capabilities]] — permissions that gate tool access
- [[Architecture]] — where tools sit in the hex layers
