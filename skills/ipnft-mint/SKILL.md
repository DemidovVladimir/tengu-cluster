---
name: ipnft-mint
description: Full IP-NFT minting pipeline — POI anchor, Molecule metadata flow, terms signing, and on-chain mint on Sepolia testnet.
homepage: https://sepolia.etherscan.io
---

# IP-NFT Minting Pipeline

10-step workflow using `sign_and_send_transaction`, `abi_encode`, `sign_message`, and `http_request`.

IMPORTANT: Execute ALL 10 steps in sequence using tool calls. Do NOT stop mid-pipeline, report progress, or output text until every step is complete.

## Required Environment Variables

| Variable | Usage |
|---|---|
| `MOLECULE_LABS_URL` | Molecule GraphQL endpoint (steps 2-3, 5-6, 8) |
| `MOLECULE_API_KEY` | x-api-key header for Molecule GraphQL |
| `MOLECULE_CLIENT_URL` | Molecule frontend URL (for metadata external_url) |

## Input

These values come from the orchestrator context (provided by the upstream POI registration task) or from `mint/metadata/poi_result.json`:
- `data.transaction.to` → use as `to` in step 1
- `data.transaction.data` → use as `data` in step 1
- `data.proof.tree[0]` → this is the `merkle_root`

Get your wallet address via `get_wallet_address`.

## Step 1 — Anchor POI on-chain

```
sign_and_send_transaction:
  to: <data.transaction.to from poi_result.json>
  data: <data.transaction.data from poi_result.json>
  chain_id: 11155111
```

Save the `tx_hash` from the response as `poi_tx_hash`.

Derive the `reservationId` from `merkle_root` (which is `data.proof.tree[0]`) using `hex_to_uint256`:

```
hex_to_uint256:
  hex: <merkle_root, e.g. "0xe6f7...728c">
```

The returned `decimal` value is the reservation ID. Use it as both the reservation ID and the `ipnftId` in all subsequent steps. Do not cast the entire `transaction_data` blob to uint256.

## Step 2 — Generate assignment agreement

```
http_request:
  url: $MOLECULE_LABS_URL
  method: POST
  headers: {"x-api-key": "$MOLECULE_API_KEY", "Content-Type": "application/json"}
  body: <GraphQL below>
  return_body: true
```

GraphQL mutation:
```graphql
mutation GenerateAssignmentAgreement($projectData: AWSJSON!) {
  generateAssignmentAgreement(projectData: $projectData) {
    agreementCid agreementContentHash isSuccess error { message code retryable }
  }
}
```

Variables — `projectData` is a **JSON-encoded string** containing:
```json
{
  "project": {
    "name": "<title>",
    "description": "<description>",
    "initialSymbol": "<symbol, e.g. VDNA>",
    "funding_amount": {"value": 0, "currency": "USD", "currency_type": "ISO4217", "decimals": 2},
    "organization": "<organization>",
    "research_lead": {"name": "<lead_name>", "email": "<lead_email>"},
    "topic": "<topic>"
  },
  "connectedWalletAddress": "<wallet_address>",
  "agreementType": "POI_ASSIGNMENT",
  "chainId": 11155111,
  "ipnftId": "<reservationId as decimal string>",
  "poiLocation": {"chainId": 11155111, "transactionHash": "<poi_tx_hash>"},
  "merkleRootHash": "<merkle_root from poi_result.json>"
}
```

Save `agreementCid` and `agreementContentHash` from the response.

## Step 3 — Get image upload URL

```
http_request:
  url: $MOLECULE_LABS_URL
  method: POST
  headers: {"x-api-key": "$MOLECULE_API_KEY", "Content-Type": "application/json"}
  body: <GraphQL below>
  return_body: true
```

```graphql
mutation GenerateImageUploadUrl($filename: String!, $contentType: String!, $ipnftId: String!) {
  generateImageUploadUrl(filename: $filename, contentType: $contentType, ipnftId: $ipnftId) {
    uploadUrl key isSuccess error { message code retryable }
  }
}
```

Variables: `filename`: `"cover.png"`, `contentType`: `"image/png"`, `ipnftId`: `"<reservationId>"`.

Save `uploadUrl` and `key` (image key).

## Step 4 — Upload cover image

If a cover image exists in `.tengu-attachments/` (PNG/JPG), upload it. Otherwise skip — the metadata step will work without it.

```
http_request:
  url: <uploadUrl from step 3>
  method: PUT
  headers: {"Content-Type": "image/png"}
  file_path: <path to image, or skip>
```

## Step 5 — Upload metadata

```
http_request:
  url: $MOLECULE_LABS_URL
  method: POST
  headers: {"x-api-key": "$MOLECULE_API_KEY", "Content-Type": "application/json"}
  body: <GraphQL below>
  return_body: true
```

```graphql
mutation UploadMetadataWithImageKey($metadata: AWSJSON!, $imageKey: String!, $ipnftId: String!) {
  uploadMetadataWithImageKey(metadata: $metadata, imageKey: $imageKey, ipnftId: $ipnftId) {
    metadataCid metadataUrl isSuccess error { message code retryable }
  }
}
```

Variables — `metadata` is a **JSON-encoded string**:
```json
{
  "name": "<title>",
  "description": "<description>",
  "external_url": "$MOLECULE_CLIENT_URL",
  "terms_signature": "placeholder",
  "properties": {
    "agreements": [{"content_hash": "<agreementContentHash>", "mime_type": "application/json", "type": "POI_ASSIGNMENT", "url": "ipfs://<agreementCid>"}],
    "initial_symbol": "<symbol>",
    "project_details": {
      "funding_amount": {"value": 0, "currency": "USD", "currency_type": "ISO4217", "decimals": 2},
      "organization": "<organization>",
      "research_lead": {"name": "<lead_name>", "email": "<lead_email>"},
      "topic": "<topic>"
    }
  }
}
```

`imageKey`: from step 3, `ipnftId`: `"<reservationId>"`.

Save `metadataCid` from the response.

## Step 6 — Get terms message

```
http_request:
  url: $MOLECULE_LABS_URL
  method: POST
  headers: {"x-api-key": "$MOLECULE_API_KEY", "Content-Type": "application/json"}
  body: <GraphQL below>
  return_body: true
```

```graphql
query GetTermsMessage($metadataCid: String!, $minter: String!, $chainId: Int!) {
  getTermsMessage(metadataCid: $metadataCid, minter: $minter, chainId: $chainId) {
    message digest isSuccess error { message code retryable }
  }
}
```

Variables: `metadataCid`: from step 5, `minter`: `<wallet_address>`, `chainId`: `11155111`.

Save `message` from the response.

## Step 7 — Sign terms

```
sign_message:
  message: <message from step 6>
```

Save the `signature` from the response.

## Step 8 — Sign off metadata (get authorization)

```
http_request:
  url: $MOLECULE_LABS_URL
  method: POST
  headers: {"x-api-key": "$MOLECULE_API_KEY", "Content-Type": "application/json"}
  body: <GraphQL below>
  return_body: true
```

```graphql
mutation SignoffMetadata($ipnftId: String!, $tokenURI: String!, $chainId: Int!, $minter: String!, $to: String!, $termsSignature: String!) {
  signoffMetadata(ipnftId: $ipnftId, tokenURI: $tokenURI, chainId: $chainId, minter: $minter, to: $to, termsSignature: $termsSignature) {
    authorization isSuccess error { message code retryable }
  }
}
```

Variables:
- `ipnftId`: `"<reservationId>"`
- `tokenURI`: `"ipfs://<metadataCid>"`
- `chainId`: `11155111`
- `minter`: `<wallet_address>`
- `to`: `<wallet_address>`
- `termsSignature`: `<signature from step 7>`

Save `authorization` from the response. This is the critical bytes parameter for the mint call.

## Step 9 — ABI-encode the mint call

```
abi_encode:
  function_signature: "mintReservation(address,uint256,string,string,bytes)"
  args:
    - <wallet_address>
    - <reservationId as decimal string>
    - ipfs://<metadataCid>
    - <symbol>
    - <authorization from step 8>
```

Save the `calldata` from the response.

## Step 10 — Mint IP-NFT on-chain

```
sign_and_send_transaction:
  to: 0x152B444e60C526fe4434C721561a077269FcF61a
  data: <calldata from step 9>
  value: 1000000000000000
  chain_id: 11155111
```

The mint fee is 0.001 ETH (1000000000000000 wei).

## Output

Save to `mint/metadata/mint_result.json`:
- `reservation_id` / `token_id` (same value — the reservationId decimal)
- `poi_tx_hash`
- `mint_tx_hash`
- `metadata_cid`
- `ipnft_symbol`
- `contract_address`: `0x152B444e60C526fe4434C721561a077269FcF61a`

The `ipnft_uid` for downstream steps is: `{contract_address}_{token_id}`

The orchestrator will automatically forward these values to downstream tasks via context.
