---
name: poi-register
description: Register a Proof of Innovation (POI) for a research PDF on Molecule testnet.
homepage: https://testnet.molecule.xyz
---

# POI Registration

Register a research PDF as a Proof of Innovation.

## Required Environment Variables

| Variable | Description |
|----------|-------------|
| `POI_API_KEY` | Bearer token for the POI endpoint |

## Input

A research PDF file in the workspace (e.g. `.tengu-attachments/document.pdf`).

## Tool Call

```
http_request:
  url: https://testnet.molecule.xyz/api/v1/inventions
  method: POST
  auth_bearer_env: POI_API_KEY
  file_path: <path-to-pdf>
  file_field_name: files
  return_body: true
```

IMPORTANT: The field name MUST be `files` (plural), not `file`. The API rejects requests with `file`.
This is the ONLY url for this skill. Do not append paths or modify it.

## Output

The response JSON has this structure:
```json
{
  "data": {
    "transaction": {
      "to": "0x...",
      "data": "0x..."
    },
    "proof": {
      "tree": ["0x...merkle_root..."]
    }
  }
}
```

Extract these values:
- `data.transaction.to` — target address for the on-chain POI anchor transaction
- `data.transaction.data` — calldata for the on-chain POI anchor transaction
- `data.proof.tree[0]` — the POI merkle root (first element of the tree array)

Save the full response to `mint/metadata/poi_result.json` for the minting step.
The orchestrator will automatically forward these values to downstream tasks via context.

IMPORTANT: After completing POI registration, immediately continue with the IP-NFT minting pipeline (ipnft-mint). Do NOT stop, report progress, or output text between POI and minting — execute both as one continuous sequence of tool calls.
