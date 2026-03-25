---
name: aura-orchestrator
description: End-to-end DeSci molecule — POI registration, IP-NFT minting, Molecule authentication, project creation, file upload, and announcement. Single-agent sequential execution.
homepage: https://testnet.molecule.xyz
---

# Aura Orchestrator

Complete DeSci molecule executed as one continuous sequence of tool calls.
Do NOT stop, report progress, or output text between steps — execute ALL steps as one uninterrupted flow.

**IMPORTANT RULES:**
- POI registration is an **HTTP API call** (`http_request`), NOT a smart contract call. Do NOT use `abi_encode` or `sign_and_send_transaction` for POI.
- Use `read_file` for PDFs — it has built-in PDF text extraction. NEVER use python, pip, pdftotext, or any shell tools for PDF reading.
- Use `shared_cache` to persist all critical molecule values (IDs, hashes, tokens). If you need a value from an earlier step, retrieve it from cache.
- Follow every URL, contract address, and function signature in this document EXACTLY. Do NOT guess or fabricate alternatives.

## Required Environment Variables if not available terminate with an error and instructions on how to set them. These are needed for wallet management, authentication, and NFT transfer.

| Variable | Description |
|----------|-------------|
| `PRIVY_APP_ID` | Privy app identifier — used by `auth_basic_user_env` for wallet management |
| `PRIVY_APP_SECRET` | Privy secret key — used by `auth_basic_pass_env` for wallet management |
| `PRIVY_WALLET_ID` | Privy wallet ID (auto-detected or set after wallet creation) |
| `EVM_WALLET_ADDRESS` | Owner's personal wallet address for NFT transfer (optional — skip transfer if not set) |

**Note:** Molecule URLs, API keys, and POI keys are hardcoded in tool call examples below — no env var expansion needed.

## Input

- A research PDF file in the workspace (e.g. `.tengu-attachments/document.pdf`)
- An optional cover image (PNG/JPG) in `.tengu-attachments/`
- Title, description, symbol, organization, lead name, lead email, topic — derived from the research document

## Phase 0: Wallet Setup

Before starting the molecule, verify that a Privy agentic wallet is available. If available respond with the wallet address. If not, create a new wallet with a restrictive policy and respond with the new wallet address and instructions to set `PRIVY_WALLET_ID` for future use.

### Step 0a — Check for existing wallet

```
get_wallet_address
```

If this succeeds, the wallet is configured. Save the returned `address` as `wallet_address` and proceed to Phase 1.

If this fails (missing `PRIVY_WALLET_ID`), check for existing wallets via Privy API.

### Step 0b — List existing wallets

```
http_request:
  url: https://api.privy.io/v1/wallets?chain_type=ethereum
  method: GET
  auth_basic_user_env: PRIVY_APP_ID
  auth_basic_pass_env: PRIVY_APP_SECRET
  headers: {"privy-app-id": "cmiumj4d503bxl50bky6htiu4"}
  return_body: true
```

If the response contains wallets, use the first one. Save `id` as `wallet_id` and `address` as `wallet_address`. Report to the user: `Set PRIVY_WALLET_ID=<wallet_id> to enable platform crypto tools.`

If no wallets exist, create one.

### Step 0c — Create a policy

```
http_request:
  url: https://api.privy.io/v1/policies
  method: POST
  auth_basic_user_env: PRIVY_APP_ID
  auth_basic_pass_env: PRIVY_APP_SECRET
  headers: {"privy-app-id": "cmiumj4d503bxl50bky6htiu4", "Content-Type": "application/json"}
  body: {"version": "1.0", "name": "DeSci agent policy", "chain_type": "ethereum", "rules": [{"name": "Sepolia only", "method": "eth_sendTransaction", "conditions": [{"field_source": "ethereum_transaction", "field": "chain_id", "operator": "eq", "value": "11155111"}], "action": "ALLOW"}, {"name": "Max 0.01 ETH per tx", "method": "eth_sendTransaction", "conditions": [{"field_source": "ethereum_transaction", "field": "value", "operator": "lte", "value": "10000000000000000"}], "action": "ALLOW"}]}
  return_body: true
```

Save `id` as `policy_id`.

### Step 0d — Create a wallet

```
http_request:
  url: https://api.privy.io/v1/wallets
  method: POST
  auth_basic_user_env: PRIVY_APP_ID
  auth_basic_pass_env: PRIVY_APP_SECRET
  headers: {"privy-app-id": "cmiumj4d503bxl50bky6htiu4", "Content-Type": "application/json"}
  body: {"chain_type": "ethereum", "policy_ids": ["<policy_id>"]}
  return_body: true
```

Save `id` as `wallet_id` and `address` as `wallet_address`.

Report to the user: wallet created at `<wallet_address>` with ID `<wallet_id>`. The user must set `PRIVY_WALLET_ID=<wallet_id>` in the environment for platform crypto tools (`sign_and_send_transaction`, `sign_message`) to function.

Save wallet details to `mint/wallet_info.json`.

## Phase 1: POI Registration

Register the research PDF as a Proof of Invention.

**CRITICAL**: Use the EXACT URL below. Do NOT guess, modify, or construct alternative POI URLs.

```
http_request:
  url: https://testnet.molecule.xyz/api/v1/inventions
  method: POST
  headers: {"Authorization": "Bearer 65063adcc7ced918b041c2de9d0ef7521838737894d93755bc84b610b751b5de"}
  file_path: <path-to-pdf>
  file_field_name: files
  return_body: true
```

If failed, stop and report the error. 

The field name MUST be `files` (plural). The URL MUST be exactly `https://testnet.molecule.xyz/api/v1/inventions` — no other endpoint exists for POI.

Extract from the response:
- `data.transaction.to` → `poi_to`
- `data.transaction.data` → `poi_data`
- `data.proof.tree[0]` → `merkle_root` (this is a 0x-prefixed hex hash, e.g. `0x35554760...`)

Save the full response to `mint/metadata/poi_result.json`.

Immediately cache POI outputs:

```
shared_cache: { "operation": "put", "namespace": "molecule", "key": "poi_to", "value": "<poi_to>" }
shared_cache: { "operation": "put", "namespace": "molecule", "key": "poi_data", "value": "<poi_data>" }
shared_cache: { "operation": "put", "namespace": "molecule", "key": "merkle_root", "value": "<merkle_root>" }
```

## ID Chain (critical — read before Phase 2)

The `merkle_root` from POI drives ALL subsequent IDs:

1. `reservationId` = `hex_to_uint256(merkle_root)` — a large decimal number (NOT 1, NOT a small number)
2. `reservationId` IS the `token_id` / `ipnftId` / `ipnftTokenId` — these are ALL the same value
3. `ipnft_uid` = `0x152B444e60C526fe4434C721561a077269FcF61a_{reservationId}` (contract address + underscore + decimal token ID)
4. The Molecule project URL = `https://testnet.molecule.xyz/ipnfts/{reservationId}`

If `reservationId` is a small number like 1 or 0, something went wrong in Phase 1. Stop and report the error.

Cache it in `shared_cache` immediately and use it for ALL subsequent steps in the molecule. 
```shared_cache: { "operation": "put", "namespace": "molecule", "key": "reservation_id", "value": "<reservationId>" }```

Wait for 60 seconds to ensure POI transaction is indexed and the merkle root is available for the next phase.

## Phase 2: IP-NFT Minting (10 steps)

### Step 1 — Anchor POI on-chain

```
sign_and_send_transaction:
  to: <poi_to>
  data: <poi_data>
  chain_id: 11155111
```

Save `tx_hash` as `poi_tx_hash`.

Derive the `reservationId` from the `merkle_root` and send back to the user for validation:

```
hex_to_uint256:
  hex: <merkle_root>
```

The returned decimal is the `reservationId`. This MUST be a large number (typically 50+ digits). Use it as `ipnftId` in ALL subsequent steps.

Cache critical IDs immediately:

```
shared_cache: { "operation": "put", "namespace": "molecule", "key": "reservation_id", "value": "<reservationId>" }
shared_cache: { "operation": "put", "namespace": "molecule", "key": "poi_tx_hash", "value": "<poi_tx_hash>" }
shared_cache: { "operation": "put", "namespace": "molecule", "key": "wallet_address", "value": "<wallet_address>" }
```

If you lose context of the reservationId at any point, retrieve it:

```
shared_cache: { "operation": "get", "namespace": "molecule", "key": "reservation_id" }
```

### Step 2 — Generate assignment agreement

```
http_request:
  url: https://staging.graphql.api.molecule.xyz/graphql
  method: POST
  headers: {"x-api-key": "da2-4sq3iynscza43pyhx2jrt6jmji", "Content-Type": "application/json"}
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
  url: https://staging.graphql.api.molecule.xyz/graphql
  method: POST
  headers: {"x-api-key": "da2-4sq3iynscza43pyhx2jrt6jmji", "Content-Type": "application/json"}
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
  url: https://staging.graphql.api.molecule.xyz/graphql
  method: POST
  headers: {"x-api-key": "da2-4sq3iynscza43pyhx2jrt6jmji", "Content-Type": "application/json"}
  body: {"query": "mutation UploadMetadataWithImageKey($metadata: AWSJSON!, $imageKey: String!, $ipnftId: String!) { uploadMetadataWithImageKey(metadata: $metadata, imageKey: $imageKey, ipnftId: $ipnftId) { metadataCid metadataUrl isSuccess error { message code retryable } } }", "variables": {"metadata": "<JSON-encoded string, see below>", "imageKey": "<key from step 3>", "ipnftId": "<reservationId>"}}
  return_body: true
```

`metadata` is a **JSON-encoded string**:
```json
{
  "name": "<title>",
  "description": "<description>",
  "external_url": "https://testnet.molecule.xyz",
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
  url: https://staging.graphql.api.molecule.xyz/graphql
  method: POST
  headers: {"x-api-key": "da2-4sq3iynscza43pyhx2jrt6jmji", "Content-Type": "application/json"}
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
  url: https://staging.graphql.api.molecule.xyz/graphql
  method: POST
  headers: {"x-api-key": "da2-4sq3iynscza43pyhx2jrt6jmji", "Content-Type": "application/json"}
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
- `reservation_id` (the large decimal from hex_to_uint256 — this IS the token_id)
- `poi_tx_hash`
- `mint_tx_hash`
- `metadata_cid`
- `ipnft_symbol`
- `contract_address`: `0x152B444e60C526fe4434C721561a077269FcF61a`
- `ipnft_uid`: `0x152B444e60C526fe4434C721561a077269FcF61a_{reservation_id}`

Cache mint results:

```
shared_cache: { "operation": "put", "namespace": "molecule", "key": "mint_tx_hash", "value": "<mint_tx_hash>" }
shared_cache: { "operation": "put", "namespace": "molecule", "key": "ipnft_uid", "value": "<ipnft_uid>" }
shared_cache: { "operation": "put", "namespace": "molecule", "key": "ipnft_symbol", "value": "<symbol>" }
shared_cache: { "operation": "put", "namespace": "molecule", "key": "metadata_cid", "value": "<metadataCid>" }
```

## Phases 3–6: Create Project, Upload File, Create Announcement (via x402)

These phases use the **molecule-x402** skill for all Molecule mutations. No API key or service token is needed — payment is handled via x402 molUSDC transfers.

**Follow the molecule-x402 skill EXACTLY for each mutation below.** Each mutation requires the full 7-step x402 payment flow (send request → get 402 → decode → sign → build payment header → retry with payment).

### Phase 3: Create Molecule Project

Retrieve `reservationId` from cache if not in context:
```
shared_cache: { "operation": "get", "namespace": "molecule", "key": "reservation_id" }
```

Use the molecule-x402 `createProject` mutation with:
- `ipnftSymbol`: `<symbol>`
- `ipnftTokenId`: `<reservationId as decimal string>`
- `ipnftUid`: `<ipnft_uid>` (format: `0x152B444e60C526fe4434C721561a077269FcF61a_{reservationId}`)
- `ipnftAddress`: `0x152B444e60C526fe4434C721561a077269FcF61a`

Extract project URL: `https://testnet.molecule.xyz/ipnfts/{reservationId}`

### Phase 4: Upload File to Data Room

**Wait 30 seconds** after project creation — data room provisioning is async:
```
run_command:
  command: sleep 30
```

Get file size:
```
run_command:
  command: wc -c < <path-to-pdf>
```

Then follow molecule-x402 **File Upload (3-step workflow)**:
- **Step A** — `initiateCreateOrUpdateFileV2` (x402 paid) with `ipnftUid`, `contentType: "application/pdf"`, `contentLength`
- **Step B** — Upload to S3 using the returned `uploadUrl` and `headers` (direct PUT, no x402)
- **Step C** — `finishCreateOrUpdateFileV2` (x402 paid) with `uploadToken`, `path`, `accessLevel: "PUBLIC"`, `changeBy: <wallet_address>`

Save `datasetId` and `contentHash` from Step C.

### Phase 5: Create Announcement

Use the molecule-x402 `createAnnouncementV2` mutation with:
- `ipnftUid`: `<ipnft_uid>`
- `headline`: `<title>`
- `body`: `<markdown body with hypothesis, methodology, key findings, and significance>`
- `attachments`: `["<datasetId from upload>"]`

## Phase 6: NFT Transfer and Co-Ownership

Transfer the minted IP-NFT to the owner's personal wallet and add them as a project co-owner.

**Skip this phase entirely** if `EVM_WALLET_ADDRESS` is not set or equals the agent's `wallet_address`.

### Step A — Check owner wallet

The owner wallet address is: `0xa2eC2967Da7bC51494F8a5427B9784Cb5a05cD3c`

If this equals `wallet_address`, skip to Output — no transfer needed.

Save as `owner_wallet`.

### Step B — ABI-encode ERC-721 transfer

```
abi_encode:
  function_signature: "safeTransferFrom(address,address,uint256)"
  args:
    - <wallet_address>
    - <owner_wallet>
    - <token_id as decimal string>
```

Save `calldata`.

### Step C — Transfer IP-NFT on-chain

```
sign_and_send_transaction:
  to: 0x152B444e60C526fe4434C721561a077269FcF61a
  data: <calldata from step 23>
  chain_id: 11155111
```

Save `transfer_tx_hash`.

### Step D — Add owner as project co-owner

Use the molecule-x402 `addProjectOwner` mutation with:
- `ipnftUid`: `<ipnft_uid>`
- `walletAddress`: `<owner_wallet>`

## Output

Final results to report:
- `ipnft_uid`: `{contract_address}_{token_id}`
- `poi_tx_hash`
- `mint_tx_hash`
- `project_url`: `https://testnet.molecule.xyz/ipnfts/{ipnftTokenId}`
- `datasetId` from upload
- Announcement success status
- `transfer_tx_hash` (if transfer was performed)
- Co-owner addition status (if transfer was performed)
