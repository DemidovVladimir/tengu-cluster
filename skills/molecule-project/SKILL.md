---
name: molecule-project
description: Create a Molecule project (data room) for a minted IP-NFT.
homepage: https://staging.graphql.api.molecule.xyz/graphql
---

# Create Molecule Project

Create a data room linked to a minted IP-NFT.

## Required Environment Variables

| Variable | Description |
|----------|-------------|
| `MOLECULE_API_KEY` | Sent as `x-api-key` header |
| `MOLECULE_LABS_URL` | GraphQL endpoint URL |
| `MOLECULE_CLIENT_URL` | Frontend URL (for building project links) |

## Step 1: Gather Inputs
These values are provided by the orchestrator context from upstream tasks. If running standalone, read from `mint/metadata/mint_result.json`:
- `ipnftSymbol` — symbol from the minting step
- `ipnftTokenId` — reservation_id (decimal) from the minting step
- `serviceToken` — from the authentication step (molecule-auth)
- `ipnftUid` — format: `{contract_address}_{token_id}`
- `ipnftAddress` — the IP-NFT contract address (`0x152B444e60C526fe4434C721561a077269FcF61a`)

## Step 2: Create Project

### Tool Call

```
http_request:
  url: $MOLECULE_LABS_URL
  method: POST
  headers: {"x-api-key": "$MOLECULE_API_KEY", "x-service-token": "<serviceToken>", "Content-Type": "application/json"}
  body: {"query": "mutation CreateProject($input: CreateProjectInput!) { createProject(input: $input) { isSuccess message error { message code retryable } project { ipnftUid ipnftSymbol ipnftAddress ipnftTokenId } } }", "variables": {"input": {"ipnftSymbol": "<ipnftSymbol>", "ipnftTokenId": "<ipnftTokenId>", "ipnftUid": "<ipnftUid>", "ipnftAddress": "<ipnftAddress>"}}}
  return_body: true
```

## Output

Extract from `data.createProject.project`:
- `ipnftUid` (format: `{contractAddress}_{tokenId}`)
- `ipnftSymbol`
- `ipnftTokenId`
- `ipnftAddress`
- Project URL: `$MOLECULE_CLIENT_URL/ipnfts/{ipnftTokenId}`

Save ALL of these to `uploads/project_result.json`, including the full project URL.
The orchestrator will automatically forward these values to downstream tasks via context.
