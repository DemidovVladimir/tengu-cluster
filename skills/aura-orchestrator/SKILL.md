---
name: aura-orchestrator
description: Deprecated legacy Molecule workflow reference. Prefer the smaller plug-and-play skills (`poi-register`, `ipnft-mint`, `molecule-project`, `molecule-upload`, `molecule-announcement`) over generic platform tools.
homepage: https://staging.graphql.api.molecule.xyz/graphql
headers:
  x-api-key: $MOLECULE_API_KEY
  x-service-token: $MOLECULE_SERVICE_TOKEN
---

# Aura Orchestrator: Deprecated Legacy Reference

This package is kept only as a legacy reference.

Do not use it as the primary execution path.
Use the smaller skill packages and the generic platform tools instead:
- `http_request`
- `get_wallet_address`
- `sign_message`
- `sign_and_send_transaction`
- `abi_encode`

Use it only to understand:
- the workflow order
- required business fields
- the meaning of Molecule entities such as `ipnftUid`, `datasetId`, and announcements

## Canonical Workflow Order

1. Register POI for the hypothesis PDF
2. Mint the IP-NFT
3. Create the Molecule project / data room
4. Upload the research file
5. Create the announcement

Each step depends on real outputs from the prior step. Do not skip or reorder them.

## Preferred Skill Mapping

Prefer these generic skill packages instead of native runtime wrappers:

- `poi-register`
- `ipnft-mint`
- `molecule-auth`
- `molecule-project`
- `molecule-upload`
- `molecule-announcement`

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

- `MOLECULE_API_KEY` and `MOLECULE_SERVICE_TOKEN` may only be used against Molecule hosts
- `EVM_PRIVATE_KEY` and `EVM_RPC_URL` are runtime secrets and must never be echoed into user content
- `MOLECULE_CLIENT_URL` is for user-facing links only
