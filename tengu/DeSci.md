---
tags:
  - subsystem
  - desci
  - blockchain
  - minting
---

# DeSci Guide: IP-NFT Minting with Tengu + Aura-Orchestrator

End-to-end guide for minting IP-NFTs on Molecule DeSci Labs using Tengu agents. All operations use **platform primitives** (`http_request`, `sign_and_send_transaction`, `sign_message`) guided by the **aura-orchestrator skill**.

---

## Prerequisites

| Item | Where to get it |
|------|----------------|
| **OpenRouter API key** | https://openrouter.ai |
| **Telegram bot token** | @BotFather on Telegram |
| **Molecule API key** | Molecule DeSci Labs team |
| **POI API key** | Molecule DeSci Labs team |
| **Privy App ID + Secret** | dashboard.privy.io |
| **Privy Wallet ID** | Privy agentic wallet with Sepolia ETH |
| **Sepolia ETH** (~0.01) | Sepolia faucet (fund the Privy wallet address) |

See [[Wallet]] for complete Privy setup instructions.

---

## How It Works

DeSci workflows use the same [[Tools|platform primitives]] as any other integration:

- `http_request` — for Molecule GraphQL API, POI registration, Beach.science posting
- `sign_and_send_transaction` — for on-chain POI anchoring and NFT minting
- `sign_message` — for Molecule terms signing and service token acquisition
- `get_wallet_address` — for wallet address lookup
- `abi_encode` — for building EVM calldata (e.g., `mintReservation` function call)

Each workflow step is documented in a [[Skills|skill]] file. See the individual skill pages: [[Skill - POI Register]], [[Skill - IP-NFT Mint]], [[Skill - Molecule Auth]], [[Skill - Molecule Project]], [[Skill - Molecule Upload]], [[Skill - Molecule Announcement]], [[Skill - Beach Science]].

## Quick Start

```bash
# Build with Telegram support
cargo build --features telegram

# Store secrets
cargo run -- secret init
cargo run -- secret set OPENROUTER_API_KEY sk-or-...
cargo run -- secret set MOLECULE_API_KEY mol-...
cargo run -- secret set POI_API_KEY poi-...
cargo run -- secret set PRIVY_APP_ID clz...
cargo run -- secret set PRIVY_APP_SECRET your-secret
cargo run -- secret set PRIVY_WALLET_ID your-wallet-id

# Run with DeSci sandbox (multi-agent team)
cargo run -- telegram --sandbox desci
```

Set env vars:
```bash
MOLECULE_LABS_URL=https://staging.graphql.api.molecule.xyz/graphql
MOLECULE_CLIENT_URL=https://testnet.molecule.xyz
```

## Workflow

The DeSci pipeline is a multi-agent workflow with two major phases:

### Phase 1: POI + IP-NFT Minting (10-step pipeline)

Handled by the **onchain_minter** agent using [[Skill - POI Register]] and [[Skill - IP-NFT Mint]]:

1. **Register POI** — `http_request` multipart POST to Molecule POI API
2. **Anchor POI on-chain** — `sign_and_send_transaction`
3. **Generate assignment agreement** — `http_request` Molecule GraphQL
4. **Upload cover image** — `http_request` to presigned S3 URL
5. **Upload metadata** — `http_request` Molecule GraphQL → `metadataCid`
6. **Get terms message** — `http_request` Molecule GraphQL
7. **Sign terms** — `sign_message` via Privy
8. **Sign off metadata** — `http_request` Molecule GraphQL → `authorization` bytes
9. **ABI-encode mint call** — `abi_encode` for `mintReservation`
10. **Mint IP-NFT on-chain** — `sign_and_send_transaction` (0.001 ETH fee)

### Phase 2: Molecule Project + Publishing

Handled by **mol_labs** ([[Skill - Molecule Auth]], [[Skill - Molecule Project]], [[Skill - Molecule Upload]], [[Skill - Molecule Announcement]]) and **beach_scientist** ([[Skill - Beach Science]]):

1. **Authenticate** — `sign_message` + GraphQL token exchange
2. **Create project** — GraphQL mutation with token_id + symbol
3. **Upload files** — 3-step S3 flow (initiate → PUT → finalize)
4. **Announce** — GraphQL mutation with headline + body
5. **Publish** — Beach.science API POST

## DeSci [[Sandboxes|Sandbox]]

`sandboxes/desci/config.toml` defines a multi-agent team:

1. **hypothesis_researcher** — PDF analysis, hypothesis extraction (no API access)
2. **wallet_manager** — [[Skill - Privy Wallets|Privy wallet]] lifecycle (create policies, wallets, check balances)
3. **onchain_minter** — 10-step [[Skill - IP-NFT Mint|IP-NFT minting]] pipeline (`requires = ["hypothesis_researcher"]`)
4. **mol_labs** — [[Skill - Molecule Project|Molecule project]], uploads, announcements (`requires = ["onchain_minter"]`)
5. **beach_scientist** — [[Skill - Beach Science|Beach.science]] publishing (`requires = ["hypothesis_researcher", "onchain_minter", "mol_labs"]`)
6. **custodian** — NFT transfer to owner wallet

The [[Orchestrator]] handles dependency resolution and parallel execution.

## Cross-Platform Usage

The aura-orchestrator skill works on any platform (Claude Code, OpenClaw, etc.) that provides `http_request` and crypto signing primitives. Just set the env vars and load the skill.

---

## Troubleshooting

| Problem | Solution |
|---------|----------|
| Missing env var errors | Store via `cargo run -- secret set VAR_NAME value` |
| Mint transaction reverts | Check Sepolia ETH balance (0.001 ETH + gas). Ensure steps 2-8 of ipnft-mint completed (authorization bytes required). |
| Service token expired | Re-authenticate via sign_message flow |
| Stale state from prior runs | `tengu prune --sandbox desci --yes` |

## Related

- [[Wallet]] — Privy agentic wallet setup
- [[Orchestrator]] — dependency-based execution
- [[Sandboxes]] — DeSci sandbox configuration
- [[Skills]] — aura-orchestrator skill
- [[Tools]] — platform primitives used by DeSci
