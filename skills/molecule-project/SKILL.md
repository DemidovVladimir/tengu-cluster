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

## Input

- `ipnft_symbol` and `ipnft_token_id` from `mint/metadata/mint_result.json`
- Service token from `uploads/service_token.txt`

## Tool Call

```
http_request:
  url: $MOLECULE_LABS_URL
  method: POST
  headers: {"x-api-key": "$MOLECULE_API_KEY", "x-service-token": "<token>", "Content-Type": "application/json"}
  body: {"query": "mutation CreateProject($input: CreateProjectInput!) { createProject(input: $input) { isSuccess message error { message code retryable } project { ipnftUid ipnftSymbol ipnftAddress ipnftTokenId } } }", "variables": {"input": {"ipnftSymbol": "<symbol>", "ipnftTokenId": "<token_id>"}}}
```

## Output

Extract from `data.createProject.project`:
- `ipnftUid` (format: `{contractAddress}_{tokenId}`)
- `ipnftSymbol`
- `ipnftTokenId`
- `ipnftAddress`
- Project URL: `$MOLECULE_CLIENT_URL/ipnfts/{ipnftTokenId}` (note: `/ipnfts/` plural, NOT `/ipnft/`)

Save ALL of these to `uploads/project_result.json`, including the full project URL.
The project URL MUST be included — downstream agents use it for links.
