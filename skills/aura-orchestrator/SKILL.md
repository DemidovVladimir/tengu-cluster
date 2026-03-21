---
name: aura-orchestrator
description: End-to-end DeSci pipeline — POI registration, IP-NFT minting, Molecule authentication, project creation, file upload, and announcement. Single-agent sequential execution.
homepage: https://testnet.molecule.xyz
---

# Aura Orchestrator

Complete DeSci pipeline executed as one continuous sequence of tool calls.
Do NOT stop, report progress, or output text between steps — execute ALL steps as one uninterrupted flow.

## Required Environment Variables

| Variable | Description |
|----------|-------------|
| `POI_API_KEY` | Bearer token for POI registration |
| `MOLECULE_LABS_URL` | Molecule GraphQL endpoint |
| `MOLECULE_API_KEY` | x-api-key header for Molecule GraphQL |
| `MOLECULE_CLIENT_URL` | Molecule frontend URL (for links) |

## Input

- A research PDF file in the workspace (e.g. `.tengu-attachments/document.pdf`)
- An optional cover image (PNG/JPG) in `.tengu-attachments/`
- Title, description, symbol, organization, lead name, lead email, topic — derived from the research document

## Phase 1: POI Registration

Register the research PDF as a Proof of Innovation.

```
http_request:
  url: https://testnet.molecule.xyz/api/v1/inventions
  method: POST
  auth_bearer_env: POI_API_KEY
  file_path: <path-to-pdf>
  file_field_name: files
  return_body: true
```

IMPORTANT: The field name MUST be `files` (plural). This is the ONLY url for POI. Do not modify it.

Extract from the response:
- `data.transaction.to` → `poi_to`
- `data.transaction.data` → `poi_data`
- `data.proof.tree[0]` → `merkle_root`

Save the full response to `mint/metadata/poi_result.json`.

## Phase 2: IP-NFT Minting (10 steps)

### Step 1 — Anchor POI on-chain

```
sign_and_send_transaction:
  to: <poi_to>
  data: <poi_data>
  chain_id: 11155111
```

Save `tx_hash` as `poi_tx_hash`.

Derive the `reservationId` from the `merkle_root`:

```
hex_to_uint256:
  hex: <merkle_root>
```

The returned `decimal` is the `reservationId`. Use it as `ipnftId` in all subsequent steps.

### Step 2 — Generate assignment agreement

```
http_request:
  url: $MOLECULE_LABS_URL
  method: POST
  headers: {"x-api-key": "$MOLECULE_API_KEY", "Content-Type": "application/json"}
  body: {"query": "mutation GenerateAssignmentAgreement($projectData: AWSJSON!) { generateAssignmentAgreement(projectData: $projectData) { agreementCid agreementContentHash isSuccess error { message code retryable } } }", "variables": {"projectData": "<JSON-encoded string, see below>"}}
  return_body: true
```

`projectData` is a **JSON-encoded string** containing:
```json
{
  "project": {
    "name": "<title>",
    "description": "<description>",
    "initialSymbol": "<symbol>",
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
  "merkleRootHash": "<merkle_root>"
}
```

Save `agreementCid` and `agreementContentHash`.

### Step 3 — Get image upload URL

```
http_request:
  url: $MOLECULE_LABS_URL
  method: POST
  headers: {"x-api-key": "$MOLECULE_API_KEY", "Content-Type": "application/json"}
  body: {"query": "mutation GenerateImageUploadUrl($filename: String!, $contentType: String!, $ipnftId: String!) { generateImageUploadUrl(filename: $filename, contentType: $contentType, ipnftId: $ipnftId) { uploadUrl key isSuccess error { message code retryable } } }", "variables": {"filename": "cover.png", "contentType": "image/png", "ipnftId": "<reservationId>"}}
  return_body: true
```

Save `uploadUrl` and `key` (image key).

### Step 4 — Upload cover image

If a cover image exists in `.tengu-attachments/`, upload it. Otherwise skip.

```
http_request:
  url: <uploadUrl from step 3>
  method: PUT
  headers: {"Content-Type": "image/png"}
  file_path: <path to image>
```

### Step 5 — Upload metadata

```
http_request:
  url: $MOLECULE_LABS_URL
  method: POST
  headers: {"x-api-key": "$MOLECULE_API_KEY", "Content-Type": "application/json"}
  body: {"query": "mutation UploadMetadataWithImageKey($metadata: AWSJSON!, $imageKey: String!, $ipnftId: String!) { uploadMetadataWithImageKey(metadata: $metadata, imageKey: $imageKey, ipnftId: $ipnftId) { metadataCid metadataUrl isSuccess error { message code retryable } } }", "variables": {"metadata": "<JSON-encoded string, see below>", "imageKey": "<key from step 3>", "ipnftId": "<reservationId>"}}
  return_body: true
```

`metadata` is a **JSON-encoded string**:
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

Save `metadataCid`.

### Step 6 — Get terms message

```
http_request:
  url: $MOLECULE_LABS_URL
  method: POST
  headers: {"x-api-key": "$MOLECULE_API_KEY", "Content-Type": "application/json"}
  body: {"query": "query GetTermsMessage($metadataCid: String!, $minter: String!, $chainId: Int!) { getTermsMessage(metadataCid: $metadataCid, minter: $minter, chainId: $chainId) { message digest isSuccess error { message code retryable } } }", "variables": {"metadataCid": "<metadataCid from step 5>", "minter": "<wallet_address>", "chainId": 11155111}}
  return_body: true
```

Save `message` from the response.

### Step 7 — Sign terms

```
sign_message:
  message: <message from step 6>
```

Save `signature`.

### Step 8 — Sign off metadata (get authorization)

```
http_request:
  url: $MOLECULE_LABS_URL
  method: POST
  headers: {"x-api-key": "$MOLECULE_API_KEY", "Content-Type": "application/json"}
  body: {"query": "mutation SignoffMetadata($ipnftId: String!, $tokenURI: String!, $chainId: Int!, $minter: String!, $to: String!, $termsSignature: String!) { signoffMetadata(ipnftId: $ipnftId, tokenURI: $tokenURI, chainId: $chainId, minter: $minter, to: $to, termsSignature: $termsSignature) { authorization isSuccess error { message code retryable } } }", "variables": {"ipnftId": "<reservationId>", "tokenURI": "ipfs://<metadataCid>", "chainId": 11155111, "minter": "<wallet_address>", "to": "<wallet_address>", "termsSignature": "<signature from step 7>"}}
  return_body: true
```

Save `authorization`.

### Step 9 — ABI-encode the mint call

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

Save `calldata`.

### Step 10 — Mint IP-NFT on-chain

```
sign_and_send_transaction:
  to: 0x152B444e60C526fe4434C721561a077269FcF61a
  data: <calldata from step 9>
  value: 1000000000000000
  chain_id: 11155111
```

The mint fee is 0.001 ETH (1000000000000000 wei).

Save to `mint/metadata/mint_result.json`:
- `reservation_id` / `token_id` (the reservationId decimal)
- `poi_tx_hash`
- `mint_tx_hash`
- `metadata_cid`
- `ipnft_symbol`
- `contract_address`: `0x152B444e60C526fe4434C721561a077269FcF61a`

Derive: `ipnft_uid` = `{contract_address}_{token_id}`

## Phase 3: Molecule Authentication

Acquire a service token before creating the project.

### Step 11 — Get wallet address

```
get_wallet_address
```

(Use the same wallet address from Phase 2 if already obtained.)

### Step 12 — Get sign-in message

```
http_request:
  url: $MOLECULE_LABS_URL
  method: POST
  headers: {"x-api-key": "$MOLECULE_API_KEY", "Content-Type": "application/json"}
  body: {"query": "query GetServiceSignInMessage($walletAddress: String!, $serviceName: String!) { getServiceSignInMessage(walletAddress: $walletAddress, serviceName: $serviceName) { message } }", "variables": {"walletAddress": "<wallet_address>", "serviceName": "tengu-agent"}}
  return_body: true
```

### Step 13 — Sign the message

```
sign_message:
  message: <message from step 12 response>
```

### Step 14 — Exchange for service token

```
http_request:
  url: $MOLECULE_LABS_URL
  method: POST
  headers: {"x-api-key": "$MOLECULE_API_KEY", "Content-Type": "application/json"}
  body: {"query": "mutation GenerateServiceToken($serviceName: String!, $expiresIn: String!, $walletAddress: String!, $messageSignature: String!) { generateServiceToken(serviceName: $serviceName, expiresIn: $expiresIn, walletAddress: $walletAddress, messageSignature: $messageSignature) { token } }", "variables": {"serviceName": "tengu-agent", "expiresIn": "720h", "walletAddress": "<wallet_address>", "messageSignature": "<signature from step 13>"}}
  return_body: true
```

Extract `data.generateServiceToken.token` as `serviceToken`.
Save to `uploads/service_token.txt`.

## Phase 4: Create Molecule Project

### Step 15 — Create project (data room)

```
http_request:
  url: $MOLECULE_LABS_URL
  method: POST
  headers: {"x-api-key": "$MOLECULE_API_KEY", "x-service-token": "<serviceToken>", "Content-Type": "application/json"}
  body: {"query": "mutation CreateProject($input: CreateProjectInput!) { createProject(input: $input) { isSuccess message error { message code retryable } project { ipnftUid ipnftSymbol ipnftAddress ipnftTokenId } } }", "variables": {"input": {"ipnftSymbol": "<ipnft_symbol>", "ipnftTokenId": "<reservationId as decimal string>", "ipnftUid": "<ipnft_uid>", "ipnftAddress": "0x152B444e60C526fe4434C721561a077269FcF61a"}}}
  return_body: true
```

Extract project URL: `$MOLECULE_CLIENT_URL/ipnfts/{ipnftTokenId}`

Save to `uploads/project_result.json`.

## Phase 5: Upload File to Data Room

### Step 16 — Wait for data room provisioning

```
run_command:
  command: sleep 30
```

Data room provisioning is asynchronous — 30 seconds is required.

### Step 17 — Get file size

```
run_command:
  command: wc -c < <path-to-pdf>
```

Save the output as `file_size_bytes` (trim whitespace).

### Step 18 — Initiate upload

```
http_request:
  url: $MOLECULE_LABS_URL
  method: POST
  headers: {"x-api-key": "$MOLECULE_API_KEY", "x-service-token": "<serviceToken>", "Content-Type": "application/json"}
  body: {"query": "mutation InitiateCreateOrUpdateFileV2($ipnftUid: String!, $contentType: String!, $contentLength: Int!) { initiateCreateOrUpdateFileV2(ipnftUid: $ipnftUid, contentType: $contentType, contentLength: $contentLength) { uploadToken uploadUrl uploadUrlExpiry method headers { key value } useMultipart isSuccess error { message code retryable } } }", "variables": {"ipnftUid": "<ipnft_uid>", "contentType": "application/pdf", "contentLength": <file_size_bytes as integer>}}
  return_body: true
```

Extract from `data.initiateCreateOrUpdateFileV2`:
- `uploadToken`
- `uploadUrl`
- `method` (usually "PUT")
- `headers` array of `{key, value}` pairs

### Step 19 — Upload to S3

Use the EXACT `uploadUrl` from step 18. Include ALL headers from step 18.

```
http_request:
  url: <uploadUrl from step 18>
  method: <method from step 18, usually PUT>
  headers: {<all key:value pairs from step 18 headers>, "Content-Type": "application/pdf"}
  file_path: <path-to-pdf>
```

### Step 20 — Finalize upload

```
http_request:
  url: $MOLECULE_LABS_URL
  method: POST
  headers: {"x-api-key": "$MOLECULE_API_KEY", "x-service-token": "<serviceToken>", "Content-Type": "application/json"}
  body: {"query": "mutation FinishCreateOrUpdateFileV2($ipnftUid: String!, $uploadToken: String!, $path: String, $ref: String, $accessLevel: String!, $changeBy: String!, $description: String, $tags: [String!], $categories: [String!]) { finishCreateOrUpdateFileV2(ipnftUid: $ipnftUid, uploadToken: $uploadToken, path: $path, ref: $ref, accessLevel: $accessLevel, changeBy: $changeBy, description: $description, tags: $tags, categories: $categories) { datasetId contentHash version newHead isSuccess message error { message code retryable } } }", "variables": {"ipnftUid": "<ipnft_uid>", "uploadToken": "<uploadToken from step 18>", "path": "<filename>", "accessLevel": "PUBLIC", "changeBy": "<wallet_address>", "description": "<file description>"}}
  return_body: true
```

Extract:
- `datasetId` — the `did:odf:...` dataset ID
- `contentHash`

Save to `uploads/upload_result.json`.

## Phase 6: Create Announcement

### Step 21 — Post announcement

```
http_request:
  url: $MOLECULE_LABS_URL
  method: POST
  headers: {"x-api-key": "$MOLECULE_API_KEY", "x-service-token": "<serviceToken>", "Content-Type": "application/json"}
  body: {"query": "mutation CreateAnnouncementV2($ipnftUid: String!, $headline: String!, $body: String!, $attachments: [String!]) { createAnnouncementV2(ipnftUid: $ipnftUid, headline: $headline, body: $body, attachments: $attachments) { isSuccess message error { message code retryable } } }", "variables": {"ipnftUid": "<ipnft_uid>", "headline": "<title>", "body": "<markdown body with hypothesis, methodology, key findings, and significance>", "attachments": ["<datasetId from step 20>"]}}
  return_body: true
```

Announcements are **public-facing scientific content**. Include the research hypothesis, methodology, key findings, and significance. Do NOT include internal pipeline data (merkle roots, tx hashes, CIDs, wallet addresses).

## Output

Final results to report:
- `ipnft_uid`: `{contract_address}_{token_id}`
- `poi_tx_hash`
- `mint_tx_hash`
- `project_url`: `$MOLECULE_CLIENT_URL/ipnfts/{ipnftTokenId}`
- `datasetId` from upload
- Announcement success status
