---
tags:
  - skill
  - wallet
  - blockchain
  - privy
aliases:
  - privy
  - privy-agentic-wallets
---

# Skill: Privy Agentic Wallets

Create and manage server-side agentic wallets with policy-based security via the Privy API. Supports Ethereum, Solana, and 15+ chains.

| Field | Value |
|-------|-------|
| Skill file | `skills/privy-agentic-wallets-skill/SKILL.md` |
| Type | Documentation (frontmatter) |
| Base URL | `https://api.privy.io` |
| Auth | Basic auth (`PRIVY_APP_ID` / `PRIVY_APP_SECRET`) |

## Tools Used

- `http_request` — Privy REST API calls (wallets, policies, RPC)
- `sign_and_send_transaction` — shortcut for EVM transactions
- `sign_message` — shortcut for message signing
- `get_wallet_address` — shortcut for address lookup

## Key Endpoints

| Method | Path | Description |
|--------|------|-------------|
| POST | `/v1/wallets` | Create wallet (requires policy) |
| GET | `/v1/wallets` | List wallets |
| GET | `/v1/wallets/{id}` | Get wallet details |
| POST | `/v1/policies` | Create spending policy |
| DELETE | `/v1/policies/{id}` | Delete policy (requires verbal confirmation) |
| POST | `/v1/wallets/{id}/rpc` | Execute RPC (eth_sendTransaction, personal_sign) |

> [!danger] Security rules
> - NEVER create a wallet without a policy attached
> - NEVER expose `PRIVY_APP_SECRET` in output or files
> - Policy deletion requires explicit verbal confirmation from the user

## Supported Chains

Ethereum, Base, Polygon, Arbitrum, Optimism, Solana, Cosmos, Stellar, Sui, Aptos, Tron, Bitcoin-Segwit, Near, TON, Starknet.

Uses CAIP-2 format for chain identification (e.g., `eip155:11155111` for Sepolia).

## Environment Variables

| Variable | Usage |
|----------|-------|
| `PRIVY_APP_ID` | Basic auth username + privy-app-id header |
| `PRIVY_APP_SECRET` | Basic auth password |
| `PRIVY_WALLET_ID` | Target wallet for signing operations |

## Relations

- **Foundation for** [[Skill - IP-NFT Mint]], [[Skill - Molecule Auth]] (signing operations)
- **Used by agent** `wallet_manager` in [[DeSci]] sandbox
- **Cross-platform** — works on Claude Code, OpenClaw, Windsurf, Cursor

## Related

- [[Wallet]] — setup guide
- [[Tools]] — `sign_and_send_transaction`, `sign_message`, `get_wallet_address`
- [[DeSci]] — Privy wallets power the minting pipeline
