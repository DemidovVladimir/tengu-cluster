---
name: aura-orchestrator
description: Molecule DeSci workflow reference for IP-NFT minting, project creation, uploads, and announcements.
homepage: https://staging.graphql.api.molecule.xyz/graphql
headers:
  x-api-key: $MOLECULE_API_KEY
---

# Aura Orchestrator: DeSci Workflow Reference

This package is **documentation only** for production DeSci agents.

Use it to understand:
- the correct workflow order
- required business fields
- the meaning of Molecule entities such as `ipnftUid`, `datasetId`, and announcements

Do **not** use this package as an execution path for minting, uploads, or announcements when native `desci.*` tools are available.

## Prerequisites

### Required Environment Variables

| Variable | Required For | Description |
|----------|-------------|-------------|
| `PRIVY_APP_ID` | On-chain operations, signing | Privy app identifier |
| `PRIVY_APP_SECRET` | On-chain operations, signing | Privy secret key |
| `PRIVY_WALLET_ID` | On-chain operations, signing | Privy agentic wallet ID |
| `MOLECULE_API_KEY` | All GraphQL calls | Sent as `x-api-key` header |
| `MOLECULE_LABS_URL` | All GraphQL calls | GraphQL endpoint |
| `MOLECULE_CLIENT_URL` | Link construction | Client URL for project links |
| `POI_API_KEY` | POI registration | Bearer token for POI endpoint |

**Not required:** `EVM_PRIVATE_KEY`, `EVM_RPC_URL`, `MOLECULE_SERVICE_TOKEN`. All signing and transactions use Privy agentic wallets. The service token is acquired autonomously via Privy wallet signing.

## Canonical Workflow Order

1. Register POI for the hypothesis PDF
2. Mint the IP-NFT (on-chain via Privy wallet)
3. Create the Molecule project / data room
4. Upload the research file
5. Create the announcement

Each step depends on real outputs from the prior step. Do not skip or reorder them.

## Native Tool Mapping

Use these runtime tools instead of raw shell commands or generic GraphQL calls:

- `poi_register_document`
  - returns POI transaction target/data and the merkle root
- `mint_ipnft`
  - submits POI on-chain and mints via Privy agentic wallet
  - returns reservation ID, token ID, mint tx, metadata CID, and project URL
- `create_molecule_project`
  - service token auto-acquired via Privy signing
  - returns `ipnft_uid`, `ipnft_symbol`, `ipnft_token_id`, and project URL
- `upload_molecule_file`
  - returns `dataset_id` and upload/content hashes
- `create_molecule_announcement`
  - returns the announcement result payload

## Required Inputs

### Minting

- title / project name
- description grounded in the research hypothesis
- symbol
- organization
- lead name
- lead email
- topic
- optional logo image

### Project creation

- `ipnft_symbol`
- `ipnft_token_id`

### File upload

- `ipnft_uid`
- workspace-relative file path
- content type

### Announcement

- `ipnft_uid`
- headline
- markdown body
- optional dataset attachments

## Identifiers

- `reservation_id` and `token_id` are the same logical minted identifier in the current Molecule testnet flow
- `ipnft_uid` format: `{contractAddress}_{tokenId}`
- `dataset_id` comes from the upload finalization step

## Output Discipline

- Treat tool outputs as source of truth
- If a native tool did not return an ID, URL, or hash, do not invent it
- Audit files must be written from structured tool results, not handcrafted summaries

## Public Content Guidelines

Beach.science posts and Molecule announcements are **public-facing scientific content**. They must contain:
- The research hypothesis, methodology, key findings, and significance
- Links to the IP-NFT project page (from `mint.project_url`)

They must **NOT** contain internal pipeline data:
- Merkle roots, transaction hashes, metadata CIDs
- Reservation IDs, token IDs, wallet addresses
- Raw tool output or JSON fragments

These are infrastructure artifacts for audit trails, not for readers.

## Security

- `MOLECULE_API_KEY` may only be used against Molecule hosts
- `PRIVY_APP_ID`, `PRIVY_APP_SECRET`, `PRIVY_WALLET_ID` are runtime secrets and must never be echoed into user content
- `MOLECULE_CLIENT_URL` is for user-facing links only
