---
name: aura-orchestrator
description: End-to-end DeSci molecule — POI registration, IP-NFT minting, Molecule authentication, project creation, file upload, and announcement. Single-agent sequential execution.
env_vars:
  - MOLECULE_CLIENT_URL
  - MOLECULE_LABS_URL
  - IPNFT_CONTRACT_ADDRESS
  - X402_GATEWAY_URL
  - EVM_WALLET_ADDRESS
  - CHAIN_ID
  - EXPERIMENT_COST_CENTS
  - PRIVY_APP_ID
  - PRIVY_APP_SECRET
  - PRIVY_WALLET_ID
  - POI_API_KEY
  - MOLECULE_API_KEY
---

# Aura Orchestrator

Complete DeSci molecule executed as one continuous sequence of tool calls.
Do NOT stop, report progress, or output text between steps — execute ALL steps as one uninterrupted flow.

**SUPER IMPORTANT RULES:**
- POI registration is an **HTTP API call** (`http_request`), NOT a smart contract call. Do NOT use `abi_encode` or `sign_and_send_transaction` for POI.
- Use `read_file` for PDFs — it has built-in PDF text extraction. NEVER use python, pip, pdftotext, or any shell tools for PDF reading.
- Do NOT `read_file` on image/binary attachments (PNG, JPG, etc.). The skill's upload flow only needs the `file_path` — pass the path directly to `http_request`.
- Use `shared_cache` to persist all critical molecule values (IDs, hashes, tokens). If you need a value from an earlier step, retrieve it from cache.
- Follow every URL, contract address, and function signature in this document EXACTLY. Do NOT guess or fabricate alternatives.
- NEVER use python, pip, pdftotext, or any external tool for PDF reading. Use `read_file` — it supports PDF extraction natively.
- NEVER guess or fabricate URLs, contract addresses, or function signatures. Follow the aura-orchestrator skill EXACTLY.
- Use x402 payment flow for all Molecule mutations, including project creation, file uploads, announcements, and ownership management. Follow the x402 Payment Flow section below (steps P1–P7).
- Phases executed sequentially without stopping or reporting intermediate progress.

## Required Environment Variables if not available terminate with an error and instructions on how to set them. These are needed for wallet management, authentication, and NFT transfer.

| Variable | Description |
|----------|-------------|
| `PRIVY_APP_ID` | Privy app identifier — used by `auth_basic_user_env` for wallet management |
| `PRIVY_APP_SECRET` | Privy secret key — used by `auth_basic_pass_env` for wallet management |
| `PRIVY_WALLET_ID` | Privy wallet ID (auto-detected or set after wallet creation) |
| `EVM_WALLET_ADDRESS` | Owner's personal wallet address for NFT transfer (optional — skip transfer if not set) |

**Note:** Skill examples reference environment variables by name. URLs and public constants (client URL, GraphQL endpoint, IPNFT contract address) are rendered into the prompt at skill-load time from `.env`. API keys and secrets stay as literal `$VAR` placeholders and are expanded by `http_request` at call time. Switching between staging and production is a `.env` edit only — never modify the skill body for environment changes.

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
  headers: {"privy-app-id": "$PRIVY_APP_ID"}
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
  headers: {"privy-app-id": "$PRIVY_APP_ID", "Content-Type": "application/json"}
  body: {"version": "1.0", "name": "DeSci agent policy", "chain_type": "ethereum", "rules": [{"name": "Single chain only", "method": "eth_sendTransaction", "conditions": [{"field_source": "ethereum_transaction", "field": "chain_id", "operator": "eq", "value": "$CHAIN_ID"}], "action": "ALLOW"}, {"name": "Max 0.07 ETH per tx", "method": "eth_sendTransaction", "conditions": [{"field_source": "ethereum_transaction", "field": "value", "operator": "lte", "value": "10000000000000000"}], "action": "ALLOW"}]}
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
  headers: {"privy-app-id": "$PRIVY_APP_ID", "Content-Type": "application/json"}
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
  url: $MOLECULE_CLIENT_URL/api/v1/inventions
  method: POST
  headers: {"Authorization": "Bearer $POI_API_KEY"}
  file_path: <path-to-pdf>
  file_field_name: files
  return_body: true
```

If failed, stop and report the error. 

The field name MUST be `files` (plural). The URL MUST be exactly `$MOLECULE_CLIENT_URL/api/v1/inventions` — no other endpoint exists for POI.

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
3. `ipnft_uid` = `$IPNFT_CONTRACT_ADDRESS_{reservationId}` (contract address + underscore + decimal token ID)
4. The Molecule project URL = `$MOLECULE_CLIENT_URL/ipnfts/{reservationId}`

If `reservationId` is a small number like 1 or 0, something went wrong in Phase 1. Stop and report the error.

Cache it in `shared_cache` immediately and use it for ALL subsequent steps in the molecule.
```shared_cache: { "operation": "put", "namespace": "molecule", "key": "reservation_id", "value": "<reservationId>" }```

Proceed immediately to Phase 2 — the merkle root is already in the POI response, no waiting needed.

## Phase 2: IP-NFT Minting (10 steps)

### Step 1 — Anchor POI on-chain

```
sign_and_send_transaction:
  to: <poi_to>
  data: <poi_data>
  chain_id: $CHAIN_ID
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
    "funding_amount": {"value": $EXPERIMENT_COST_CENTS, "currency": "USD", "currency_type": "ISO4217", "decimals": 2},
    "organization": "<organization>",
    "research_lead": {"name": "<lead_name>", "email": "<lead_email>"},
    "topic": "<topic>"
  },
  "connectedWalletAddress": "<wallet_address>",
  "agreementType": "POI_ASSIGNMENT",
  "chainId": $CHAIN_ID,
  "ipnftId": "<reservationId as decimal string>",
  "poiLocation": {"chainId": $CHAIN_ID, "transactionHash": "<poi_tx_hash>"},
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
      "funding_amount": {"value": $EXPERIMENT_COST_CENTS, "currency": "USD", "currency_type": "ISO4217", "decimals": 2},
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
  body: {"query": "query GetTermsMessage($metadataCid: String!, $minter: String!, $chainId: Int!) { getTermsMessage(metadataCid: $metadataCid, minter: $minter, chainId: $chainId) { message digest isSuccess error { message code retryable } } }", "variables": {"metadataCid": "<metadataCid from step 5>", "minter": "<wallet_address>", "chainId": $CHAIN_ID}}
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
  body: {"query": "mutation SignoffMetadata($ipnftId: String!, $tokenURI: String!, $chainId: Int!, $minter: String!, $to: String!, $termsSignature: String!) { signoffMetadata(ipnftId: $ipnftId, tokenURI: $tokenURI, chainId: $chainId, minter: $minter, to: $to, termsSignature: $termsSignature) { authorization isSuccess error { message code retryable } } }", "variables": {"ipnftId": "<reservationId>", "tokenURI": "ipfs://<metadataCid>", "chainId": $CHAIN_ID, "minter": "<wallet_address>", "to": "<wallet_address>", "termsSignature": "<signature from step 7>"}}
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
  to: $IPNFT_CONTRACT_ADDRESS
  data: <calldata from step 9>
  value: 1000000000000000
  chain_id: $CHAIN_ID
```

The mint fee is 0.001 ETH (1000000000000000 wei).

Save to `mint/metadata/mint_result.json`:
- `reservation_id` (the large decimal from hex_to_uint256 — this IS the token_id)
- `poi_tx_hash`
- `mint_tx_hash`
- `metadata_cid`
- `ipnft_symbol`
- `contract_address`: `$IPNFT_CONTRACT_ADDRESS`
- `ipnft_uid`: `$IPNFT_CONTRACT_ADDRESS_{reservation_id}`

Cache mint results:

```
shared_cache: { "operation": "put", "namespace": "molecule", "key": "mint_tx_hash", "value": "<mint_tx_hash>" }
shared_cache: { "operation": "put", "namespace": "molecule", "key": "ipnft_uid", "value": "<ipnft_uid>" }
shared_cache: { "operation": "put", "namespace": "molecule", "key": "ipnft_symbol", "value": "<symbol>" }
shared_cache: { "operation": "put", "namespace": "molecule", "key": "metadata_cid", "value": "<metadataCid>" }
```

## x402 Payment Flow (used by ALL mutations in Phases 3–6)

Every Molecule mutation below uses x402 payment. No API key or service token needed — USDC on Base pays per call.

**Required env vars:** `X402_GATEWAY_URL`, `PRIVY_APP_ID`, `PRIVY_APP_SECRET`, `PRIVY_WALLET_ID`

Each mutation follows this 7-step flow. Substitute `<mutation_name>`, `<query>`, and `<variables>` per mutation.

**Step P1 — Send request, get 402 challenge:**
```
run_command:
  command: curl -sS -i -X POST "$X402_GATEWAY_URL/x402/labs/<mutation_name>" -H "Content-Type: application/json" -d '<JSON body with query and variables>'
```
Look for the `payment-required` header (base64-encoded) in the response.

**Step P2 — Decode payment requirements:**
```
run_command:
  command: echo '<payment-required header value>' | base64 -D
```
Extract from `accepts[0]`: `network`, `amount`, `asset`, `payTo`, `maxTimeoutSeconds`, `extra.name`, `extra.version`.

**Step P3 — Get wallet address** (reuse from Phase 0 if cached).

**Step P4 — Generate nonce, validAfter, validBefore:**
```
run_command:
  command: NOW=$(date +%s) && echo "0x$(openssl rand -hex 32)" && echo $(( NOW - 600 )) && echo $(( NOW + <maxTimeoutSeconds> ))
```
Line 1: `nonce`, Line 2: `valid_after`, Line 3: `valid_before`.

**Step P5 — Sign EIP-712 TransferWithAuthorization:**

Extract chain ID from network string (e.g. `eip155:84532` → `84532`). Use `primary_type` (snake_case), do NOT include `caip2`.

```
http_request:
  url: https://api.privy.io/v1/wallets/$PRIVY_WALLET_ID/rpc
  method: POST
  auth_basic_user_env: PRIVY_APP_ID
  auth_basic_pass_env: PRIVY_APP_SECRET
  headers: {"privy-app-id": "$PRIVY_APP_ID", "Content-Type": "application/json"}
  body: {"method": "eth_signTypedData_v4", "params": {"typed_data": {"types": {"EIP712Domain": [{"name": "name", "type": "string"}, {"name": "version", "type": "string"}, {"name": "chainId", "type": "uint256"}, {"name": "verifyingContract", "type": "address"}], "TransferWithAuthorization": [{"name": "from", "type": "address"}, {"name": "to", "type": "address"}, {"name": "value", "type": "uint256"}, {"name": "validAfter", "type": "uint256"}, {"name": "validBefore", "type": "uint256"}, {"name": "nonce", "type": "bytes32"}]}, "primary_type": "TransferWithAuthorization", "domain": {"name": "<extra.name>", "version": "<extra.version>", "chainId": <chain_id>, "verifyingContract": "<asset>"}, "message": {"from": "<wallet_address>", "to": "<payTo>", "value": "<amount>", "validAfter": "<valid_after>", "validBefore": "<valid_before>", "nonce": "<nonce>"}}}}
  return_body: true
```
Extract `data.signature`.

**Step P6 — Build and encode payment header:**

All `authorization` fields MUST be strings. The `accepted` field MUST be the full `accepts[0]` object from step P2 (including `scheme`, `network`, `amount`, `asset`, `payTo`, `maxTimeoutSeconds`, `extra`). The `resource` field MUST be the `resource` object from step P2. Construct JSON:
```json
{"x402Version":2,"resource":{"url":"<resource.url from P2>","description":"<resource.description from P2>","mimeType":"<resource.mimeType from P2>"},"accepted":{"scheme":"exact","network":"<network>","amount":"<amount>","asset":"<asset>","payTo":"<payTo>","maxTimeoutSeconds":<maxTimeoutSeconds>,"extra":{"name":"<extra.name>","version":"<extra.version>"}},"payload":{"signature":"<signature>","authorization":{"from":"<wallet_address>","to":"<payTo>","value":"<amount>","validAfter":"<valid_after>","validBefore":"<valid_before>","nonce":"<nonce>"}}}
```
Base64 encode:
```
run_command:
  command: printf '%s' '<payment JSON no whitespace>' | base64 | tr -d '\n'
```

**Step P7 — Retry with payment:**

**CRITICAL: The header MUST be `PAYMENT-SIGNATURE`. Do NOT use `X-PAYMENT` — the x402 server only reads `PAYMENT-SIGNATURE`.**
```
run_command:
  command: curl -sS -X POST "$X402_GATEWAY_URL/x402/labs/<mutation_name>" -H "Content-Type: application/json" -H "PAYMENT-SIGNATURE: <payment_header>" -d '<same JSON body as P1>'
```

---

## Phase 3: Create Molecule Project (via x402)

**Wait 90 seconds** after minting — on-chain ownership needs time to propagate to the AccessResolver:
```
run_command:
  command: sleep 90
```

Retrieve `reservationId` from cache if not in context:
```
shared_cache: { "operation": "get", "namespace": "molecule", "key": "reservation_id" }
```

**Mutation name:** `createProject`
**URL path:** `/x402/labs/createProject`
**Body:**
```json
{"query": "mutation CreateProject($input: CreateProjectInput!) { createProject(input: $input) { isSuccess message error { message code retryable } project { ipnftUid ipnftSymbol ipnftAddress ipnftTokenId } } }", "variables": {"input": {"ipnftSymbol": "<symbol>", "ipnftTokenId": "<reservationId as decimal string>"}}}
```

Run the full x402 payment flow (steps P1–P7) with this mutation. Extract project URL: `$MOLECULE_CLIENT_URL/ipnfts/{reservationId}`

Cache:
```
shared_cache: { "operation": "put", "namespace": "molecule", "key": "project_url", "value": "<project_url>" }
```

## Phase 4: Upload File to Data Room

**Wait 90 seconds** after project creation — data room provisioning is async:
```
run_command:
  command: sleep 90
```

Get file size:
```
run_command:
  command: wc -c < <path-to-pdf>
```

### Step A — Initiate upload (x402 paid)

**Mutation name:** `initiateCreateOrUpdateFileV2`
**URL path:** `/x402/labs/initiateCreateOrUpdateFileV2`
**Body:**
```json
{"query": "mutation InitiateCreateOrUpdateFileV2($ipnftUid: String!, $contentType: String!, $contentLength: Int!) { initiateCreateOrUpdateFileV2(ipnftUid: $ipnftUid, contentType: $contentType, contentLength: $contentLength) { uploadToken uploadUrl uploadUrlExpiry method headers { key value } useMultipart isSuccess error { message code retryable } } }", "variables": {"ipnftUid": "<ipnft_uid>", "contentType": "application/pdf", "contentLength": <file_size_in_bytes>}}
```

Run full x402 payment flow (P1–P7). Extract: `uploadToken`, `uploadUrl`, `method`, `headers`.

### Step B — Upload to S3 (direct, NO x402 payment)

Use the EXACT `uploadUrl` and ALL `headers` from Step A:
```
http_request:
  url: <uploadUrl from step A>
  method: <method from step A, usually PUT>
  headers: {<all key:value pairs from step A headers>, "Content-Type": "application/pdf"}
  file_path: <path-to-file>
```

### Step C — Finalize upload (x402 paid)

**Mutation name:** `finishCreateOrUpdateFileV2`
**URL path:** `/x402/labs/finishCreateOrUpdateFileV2`

**Categories and tags** (REQUIRED — pick exactly one category and one or more correlated tags from the lists below; do NOT invent values):

Allowed categories:
```
['Science', 'Business', 'Governance', 'Media']
```

Correlated tags (each tag belongs to exactly one category — only pick tags whose category matches the chosen category):
```
Business:
  'Ecosystem Partnership',
  'Funding',
  'University Partnership',
  'Important Meeting',
  'Market Opportunity',
  'Regulatory filing',
  'Biotech Partnership'
Governance:
  'Proposal Failed',
  'Proposal Approved',
  'Proposal Open for Feedback'
Media:
  'Promotional material',
  'Blog',
  'News coverage',
  'Academic article',
  'Pitch deck'
Science:
  'Discovery',
  'Clinical Trial',
  'Provisional Patent Application',
  'Validation',
  'Milestone Achieved',
  'Manufacturing',
  'Lab Life',
  'In vivo data',
  'Patent licensed',
  'Non-Provisional Patent Application',
  'Optimization',
  'Patent granted'
```

Derive the category and tags from the research document content. For a typical research-PDF upload, default to category `Science` with tag(s) like `Discovery` or `Validation` unless the document clearly fits another category.

**Body:**
```json
{"query": "mutation FinishCreateOrUpdateFileV2($ipnftUid: String!, $uploadToken: String!, $path: String, $accessLevel: String!, $changeBy: String!, $description: String, $tags: [String!], $categories: [String!]) { finishCreateOrUpdateFileV2(ipnftUid: $ipnftUid, uploadToken: $uploadToken, path: $path, accessLevel: $accessLevel, changeBy: $changeBy, description: $description, tags: $tags, categories: $categories) { datasetId contentHash version newHead isSuccess message error { message code retryable } } }", "variables": {"ipnftUid": "<ipnft_uid>", "uploadToken": "<from step A>", "path": "<filename>", "accessLevel": "PUBLIC", "changeBy": "<wallet_address>", "description": "<file description>", "categories": ["<one of: Science | Business | Governance | Media>"], "tags": ["<one or more correlated tags from the list above>"]}}
```

Run full x402 payment flow (P1–P7). Extract: `datasetId` (format: `did:odf:...`), `contentHash`.

Cache:
```
shared_cache: { "operation": "put", "namespace": "molecule", "key": "dataset_id", "value": "<datasetId>" }
```

## Phase 5: Create Announcement (via x402)

**Mutation name:** `createAnnouncementV2`
**URL path:** `/x402/labs/createAnnouncementV2`
**Body:**
```json
{"query": "mutation CreateAnnouncementV2($ipnftUid: String!, $headline: String!, $body: String!, $attachments: [String!]) { createAnnouncementV2(ipnftUid: $ipnftUid, headline: $headline, body: $body, attachments: $attachments) { isSuccess message error { message code retryable } } }", "variables": {"ipnftUid": "<ipnft_uid>", "headline": "<title>", "body": "<markdown body>", "attachments": ["<datasetId from upload>"]}}
```

Run full x402 payment flow (P1–P7).

### External Posting Copy Rules (Phase 5 body + any Beach.science post)

When composing any user-facing markdown that describes the registration (the `body` field above, or a Beach.science post body), obey the rules below. The active chain id for this run is **$CHAIN_ID** (resolved from env at skill-load time); use it directly wherever a chain id is needed.

- **Project URL:** use `$MOLECULE_CLIENT_URL/ipnfts/{reservationId}` verbatim — never substitute `testnet.molecule.xyz`, `staging.molecule.xyz`, or any other domain.
- **Chain name:** if the active chain id is `1`, call it "Ethereum mainnet". If it is `11155111`, call it "Sepolia". For any other chain id, name it explicitly (e.g. "Base mainnet (8453)"). Do NOT label the registration as "Sepolia staging", "testnet", or "staging" when the active chain id is `1`.
- **TX explorer links:** chain id `1` → `https://etherscan.io/tx/<hash>`; chain id `11155111` → `https://sepolia.etherscan.io/tx/<hash>`; chain id `8453` → `https://basescan.org/tx/<hash>`.
- **Update slugs:** any `/updates/<slug>` link MUST be lowercase, hyphen-separated, and have NO file extension. Example: `/updates/kiss1r-pipeline-update-gen2` — NOT `/updates/KISS1R_Pipeline_Update_Gen2.md`, `/updates/KISS1R_Pipeline_Update_Gen2`, or `/updates/kiss1r-pipeline-update-gen2.md`. Lowercase the title, replace spaces and underscores with hyphens, and drop any trailing `.md`/`.html`.
- Do not invent URLs, symbols, or transaction hashes — use the values actually saved to `shared_cache` during this run.

## Phase 6: NFT Transfer and Co-Ownership

Transfer the minted IP-NFT to the owner's personal wallet and add them as a project co-owner.

**Skip this phase entirely** if `EVM_WALLET_ADDRESS` is not set or equals the agent's `wallet_address`.

### Step A — Check owner wallet

The owner wallet address is: `$EVM_WALLET_ADDRESS`

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
  to: $IPNFT_CONTRACT_ADDRESS
  data: <calldata from step B>
  chain_id: $CHAIN_ID
```

Save `transfer_tx_hash`.

### Step D — Add owner as project co-owner (via x402)

**Mutation name:** `addProjectOwner`
**URL path:** `/x402/labs/addProjectOwner`
**Body:**
```json
{"query": "mutation AddProjectOwner($ipnftUid: String!, $walletAddress: String!) { addProjectOwner(ipnftUid: $ipnftUid, walletAddress: $walletAddress) { isSuccess message error { message code retryable } } }", "variables": {"ipnftUid": "<ipnft_uid>", "walletAddress": "<owner_wallet>"}}
```

Run full x402 payment flow (P1–P7).

## Output

Final results to report:
- `ipnft_uid`: `{contract_address}_{token_id}`
- `poi_tx_hash`
- `mint_tx_hash`
- `project_url`: `$MOLECULE_CLIENT_URL/ipnfts/{ipnftTokenId}`
- `datasetId` from upload
- Announcement success status
- `transfer_tx_hash` (if transfer was performed)
- Co-owner addition status (if transfer was performed)
