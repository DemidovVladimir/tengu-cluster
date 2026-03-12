---
name: aura-orchestrator
description: DeSci lab automation — IPNFT minting, project creation, file uploads, and announcements via Molecule GraphQL API with Privy agentic wallet for on-chain transactions and signing.
homepage: https://staging.graphql.api.molecule.xyz/graphql
headers:
  x-api-key: $MOLECULE_API_KEY
env_vars:
  - PRIVY_APP_ID
  - PRIVY_APP_SECRET
  - PRIVY_WALLET_ID
  - MOLECULE_API_KEY
  - MOLECULE_LABS_URL
  - MOLECULE_CLIENT_URL
  - POI_API_KEY
---

# Aura Orchestrator: DeSci Lab Automation

**Important:** Base URL already points to the GraphQL endpoint. Use `path: ""`. Never append `/graphql`.
**Execution order:** Step 0 (Wallet) → WF1 (Mint) → Service Token → WF2 (Project) → WF3 (Upload) / WF4 (Announcement)

### When to use `aura_orchestrator` tool vs `run_command` with curl

| Use `aura_orchestrator` tool | Use `run_command` with curl |
|------------------------------|----------------------------|
| Step 3a: generateAssignmentAgreement | Step 0: Privy wallet (different URL + auth) |
| Step 3b: generateImageUploadUrl | Step 1: POI registration (different URL + auth) |
| Step 3c: uploadMetadataWithImageKey | Step 2: POI on-chain via Privy |
| Step 3d: getTermsMessage | Step 3b: PUT image to S3 presigned URL |
| Step 3f: signoffMetadata | Step 3e: Sign terms via Privy |
| Query operations (list/get/search) | Step 3g: Mint on-chain via Privy |
| | Service Token Acquisition (Steps A-C) |
| | Workflows 2-4 (need `x-service-token` header) |

**Rule:** Use `aura_orchestrator` tool ONLY for Molecule GraphQL mutations/queries that need just `x-api-key`. Use `run_command` with curl for everything else — Privy calls, POI API, S3 uploads, and any call needing `x-service-token`. Do NOT try to route Privy or POI calls through the `aura_orchestrator` tool — it only knows the Molecule GraphQL endpoint.

## Required User Inputs (Workflow 1 — Mint)

**BEFORE starting any workflow, validate that ALL required fields are provided by the user. If ANY field is missing, ASK the user — do NOT invent, guess, or hallucinate values. Proceeding with fabricated data wastes tokens and produces invalid on-chain state.**

| Field | Rules | Example |
|-------|-------|---------|
| **name** | Non-empty, max 100 chars | `"Novel Protein Folding Method"` |
| **description** | Non-empty | `"Computational method for membrane proteins"` |
| **symbol** | 3-5 alphanumeric UPPERCASE | `"PROT1"` |
| **organization** | Non-empty | `"DeSci Research Lab"` |
| **research_lead.name** | Non-empty | `"Jane Doe"` |
| **research_lead.email** | Valid email format | `"jane@example.com"` |
| **topic** | Non-empty | `"Computational Biology"` |
| **cover image** | Attached file (PNG/JPG) | user attachment |
| **document** | Attached file (PDF/image) for POI | user attachment |

**Fail-fast checklist** — run before Step 1:
1. All 7 text fields present? If not → list missing fields, ask user, STOP.
2. `symbol` matches `^[A-Z0-9]{3,5}$`? If not → tell user the rules, STOP.
3. `research_lead.email` is valid email? If not → ask user to correct, STOP.
4. Cover image attached? If not → ask user, STOP.
5. Document for POI attached? If not → ask user, STOP.

**Never fill in defaults like `"Tengu Research Labs"` or `"agent@tengu.dev"`. These are the user's legal/identity fields.**

## Reference

**Env vars:** `PRIVY_APP_ID`/`PRIVY_APP_SECRET`/`PRIVY_WALLET_ID` (on-chain ops) | `MOLECULE_API_KEY` (`x-api-key` header) | `MOLECULE_LABS_URL` (GraphQL endpoint) | `MOLECULE_CLIENT_URL` (links) | `POI_API_KEY` (`Authorization: Bearer`)
**Constants:** Sepolia `11155111`, CAIP-2 `eip155:11155111`, IPNFT `0x152B444e60C526fe4434C721561a077269FcF61a`, Mint `0.001 ETH` (`1000000000000000` wei), `ipnftUid` = `{contract}_{tokenId}`, link = `${MOLECULE_CLIENT_URL}/ipnfts/{tokenId}`
**Not required:** `EVM_PRIVATE_KEY`, `EVM_RPC_URL`, `MOLECULE_SERVICE_TOKEN`, `TENGU_RELAY_URL`, `TENGU_WALLET_SESSION`.
**Privy base:** `curl -s -X POST "https://api.privy.io/v1/wallets/$PRIVY_WALLET_ID/rpc" --user "$PRIVY_APP_ID:$PRIVY_APP_SECRET" -H "privy-app-id: $PRIVY_APP_ID" -H "Content-Type: application/json"`
- **Send tx:** `method: "eth_sendTransaction"`, `caip2: "eip155:11155111"`, `params.transaction: {to, data, value}`
- **Sign msg:** `method: "personal_sign"`, `params.message: "0x<hex>"` — hex-encode: `echo -n "text" | xxd -p | tr -d '\n' | sed 's/^/0x/'`

---

## Step 0: Resolve Wallet Address
```bash
curl -s -X GET "https://api.privy.io/v1/wallets/$PRIVY_WALLET_ID" \
  --user "$PRIVY_APP_ID:$PRIVY_APP_SECRET" \
  -H "privy-app-id: $PRIVY_APP_ID"
```
Save `address` as `WALLET_ADDRESS` — used for `connectedWalletAddress`, `minter`, `to`, `changeBy`.

## Service Token Acquisition (for Workflows 2-4)

**Step A** — get sign-in message:
```bash
curl -s -X POST "$MOLECULE_LABS_URL" \
  -H "Content-Type: application/json" \
  -H "x-api-key: $MOLECULE_API_KEY" \
  -d '{
    "query": "query GetServiceSignInMessage($walletAddress: String!, $serviceName: String!) { getServiceSignInMessage(walletAddress: $walletAddress, serviceName: $serviceName) }",
    "variables": { "walletAddress": "WALLET_ADDRESS", "serviceName": "tengu-agent" }
  }'
```
**Step B** — sign with Privy:
```bash
HEX_MSG=$(echo -n 'THE_MESSAGE_FROM_STEP_A' | xxd -p | tr -d '\n' | sed 's/^/0x/')
curl -s -X POST "https://api.privy.io/v1/wallets/$PRIVY_WALLET_ID/rpc" \
  --user "$PRIVY_APP_ID:$PRIVY_APP_SECRET" \
  -H "privy-app-id: $PRIVY_APP_ID" \
  -H "Content-Type: application/json" \
  -d "{\"method\": \"personal_sign\", \"params\": {\"message\": \"$HEX_MSG\"}}"
```
**Step C** — exchange for token (valid 180 days):
```bash
curl -s -X POST "$MOLECULE_LABS_URL" \
  -H "Content-Type: application/json" \
  -H "x-api-key: $MOLECULE_API_KEY" \
  -d '{
    "query": "mutation GenerateServiceToken($serviceName: String!, $walletAddress: String!, $messageSignature: String!) { generateServiceToken(serviceName: $serviceName, walletAddress: $walletAddress, messageSignature: $messageSignature) { token metadata { tokenId expiresAt serviceName } } }",
    "variables": { "serviceName": "tengu-agent", "walletAddress": "WALLET_ADDRESS", "messageSignature": "SIGNATURE_FROM_STEP_B" }
  }'
```
Save `token` as `SERVICE_TOKEN`.

---

## Workflow 1: Mint an IP-NFT

Three mandatory stages. Each depends on the previous. **If any step fails, STOP and report.**

### Step 1: Register POI
**`-F` MUST use `=@` syntax — `files=@path`. Missing `=` causes curl error.**
```bash
curl -X POST https://testnet.molecule.xyz/api/v1/inventions \
  -H "Authorization: Bearer $POI_API_KEY" \
  -H "Content-Type: multipart/form-data" \
  -F "files=@/absolute/path/to/document.pdf"
```
Save: `data.transaction.to` (contract), `data.transaction.data` (calldata), `data.proof.tree[0]` (merkle root).

### Step 2: Submit POI on-chain (Privy)
```bash
curl -s -X POST "https://api.privy.io/v1/wallets/$PRIVY_WALLET_ID/rpc" \
  --user "$PRIVY_APP_ID:$PRIVY_APP_SECRET" \
  -H "privy-app-id: $PRIVY_APP_ID" \
  -H "Content-Type: application/json" \
  -d '{
    "method": "eth_sendTransaction",
    "caip2": "eip155:11155111",
    "params": {
      "transaction": {
        "to": "<transaction.to from Step 1>",
        "data": "<transaction.data from Step 1>",
        "value": "0"
      }
    }
  }'
```
Save `data.hash` as `TX_HASH`. Compute `RESERVATION_ID`: `printf "%d\n" <transaction.data from Step 1>`. Carry forward `MERKLE_ROOT` = `data.proof.tree[0]` from Step 1.

### Step 3: Mint the IP-NFT

#### 3a: Generate assignment agreement
```graphql
mutation GenerateAssignmentAgreement($projectData: AWSJSON!) {
  generateAssignmentAgreement(projectData: $projectData) {
    agreementCid agreementContentHash isSuccess
    error { message code retryable }
  }
}
```
`projectData` is a JSON string:
```json
{
  "projectData": "{\"project\":{\"name\":\"PROJECT_NAME\",\"description\":\"DESC\",\"initialSymbol\":\"SYM\",\"funding_amount\":{\"value\":0,\"currency\":\"USD\",\"currency_type\":\"ISO4217\",\"decimals\":2},\"organization\":\"ORG\",\"research_lead\":{\"name\":\"LEAD_NAME\",\"email\":\"LEAD_EMAIL\"},\"topic\":\"TOPIC\"},\"connectedWalletAddress\":\"WALLET_ADDRESS\",\"agreementType\":\"POI_ASSIGNMENT\",\"chainId\":11155111,\"ipnftId\":\"RESERVATION_ID\",\"poiLocation\":{\"chainId\":11155111,\"transactionHash\":\"TX_HASH\"},\"merkleRootHash\":\"MERKLE_ROOT\"}"
}
```
**Field rules:** `name` max 100 chars | `initialSymbol` 3-5 alphanumeric uppercase | `research_lead.email` valid email | `funding_amount` needs `value`(int), `currency`(`"USD"`), `currency_type`(`"ISO4217"`), `decimals`(`2`) | `organization`/`research_lead.name`/`topic` non-empty | `connectedWalletAddress` `0x`-prefixed 42-char | `agreementType` = `"POI_ASSIGNMENT"` | `chainId` = `11155111` | `ipnftId` decimal uint256 NOT hex | `poiLocation.transactionHash` `0x`-prefixed 66-char | `merkleRootHash` = `data.proof.tree[0]`

**CRITICAL — generateAssignmentAgreement troubleshooting** (if `INTERNAL_ERROR` + `retryable: true`):
1. `name` ≤ 100 chars, `initialSymbol` 3-5 alphanumeric
2. `research_lead.email` is valid email format
3. `RESERVATION_ID` is decimal uint256 from Step 2 (NOT hex)
4. `TX_HASH` is POI submission tx hash from Step 2
5. `MERKLE_ROOT` is `data.proof.tree[0]` from Step 1
6. If all correct → save response to `mint/diagnostics/error.json`, STOP, report to user. API is down.
DO NOT retry with "different parameters" — either fields are wrong or API is down.

Save `agreementCid` and `agreementContentHash`.

#### 3b: Upload cover image
```graphql
mutation GenerateImageUploadUrl($filename: String!, $contentType: String!, $ipnftId: String!) {
  generateImageUploadUrl(filename: $filename, contentType: $contentType, ipnftId: $ipnftId) {
    uploadUrl key isSuccess
    error { message code retryable }
  }
}
```
Variables: `{"filename": "cover.png", "contentType": "image/png", "ipnftId": "RESERVATION_ID"}`
```bash
curl -X PUT "UPLOAD_URL" -H "Content-Type: image/png" --data-binary @cover.png
```
Save `key` as `IMAGE_KEY`.

#### 3c: Upload metadata
```graphql
mutation UploadMetadataWithImageKey($metadata: AWSJSON!, $imageKey: String!, $ipnftId: String!) {
  uploadMetadataWithImageKey(metadata: $metadata, imageKey: $imageKey, ipnftId: $ipnftId) {
    metadataCid metadataUrl isSuccess
    error { message code retryable }
  }
}
```
`metadata` is a **JSON string** with the exact structure below. Do NOT add or remove top-level keys. Do NOT put `symbol` at the top level — it goes inside `properties`.
```json
{
  "metadata": "{\"name\":\"PROJECT_NAME\",\"description\":\"DESC\",\"external_url\":\"MOLECULE_CLIENT_URL/ipnfts/RESERVATION_ID\",\"properties\":{\"symbol\":\"SYM\",\"organization\":\"ORG\",\"research_lead\":{\"name\":\"LEAD_NAME\",\"email\":\"LEAD_EMAIL\"},\"topic\":\"TOPIC\",\"funding_amount\":{\"value\":0,\"currency\":\"USD\",\"currency_type\":\"ISO4217\",\"decimals\":2}},\"terms_signature\":\"AGREEMENT_CONTENT_HASH_FROM_3A\",\"agreements\":[{\"type\":\"POI_ASSIGNMENT\",\"cid\":\"AGREEMENT_CID_FROM_3A\",\"contentHash\":\"AGREEMENT_CONTENT_HASH_FROM_3A\"}]}",
  "imageKey": "IMAGE_KEY_FROM_3B",
  "ipnftId": "RESERVATION_ID"
}
```
**Field mapping:**
- `name`, `description` → user-provided (from Required Inputs)
- `properties.symbol` → user-provided `symbol` (NOT top-level — API rejects top-level `symbol`)
- `properties.organization`, `properties.research_lead`, `properties.topic` → user-provided
- `properties.funding_amount` → `{value: 0, currency: "USD", currency_type: "ISO4217", decimals: 2}`
- `terms_signature` → `agreementContentHash` from Step 3a
- `agreements[0].cid` → `agreementCid` from Step 3a
- `agreements[0].contentHash` → `agreementContentHash` from Step 3a
- `external_url` → `${MOLECULE_CLIENT_URL}/ipnfts/${RESERVATION_ID}`
- `imageKey` → `key` from Step 3b
- `ipnftId` → `RESERVATION_ID` (decimal string)

**If this mutation fails with `MISSING_PARAMETERS` or `INVALID_PARAMETERS`, save the full error to `mint/diagnostics/metadata_error.json` and STOP. Do NOT guess alternative structures — report the exact error to the user.**

Save `metadataCid`.

#### 3d: Get terms message
```graphql
query GetTermsMessage($metadataCid: String!, $minter: String!, $chainId: Int!) {
  getTermsMessage(metadataCid: $metadataCid, minter: $minter, chainId: $chainId) {
    message digest isSuccess
    error { message code retryable }
  }
}
```
Variables: `{"metadataCid": "METADATA_CID", "minter": "WALLET_ADDRESS", "chainId": 11155111}`. Save `message`.

#### 3e: Sign terms (Privy)
```bash
HEX_MSG=$(echo -n 'TERMS_MESSAGE_FROM_3d' | xxd -p | tr -d '\n' | sed 's/^/0x/')
curl -s -X POST "https://api.privy.io/v1/wallets/$PRIVY_WALLET_ID/rpc" \
  --user "$PRIVY_APP_ID:$PRIVY_APP_SECRET" \
  -H "privy-app-id: $PRIVY_APP_ID" \
  -H "Content-Type: application/json" \
  -d "{\"method\": \"personal_sign\", \"params\": {\"message\": \"$HEX_MSG\"}}"
```
Save `data.signature`.

#### 3f: Sign off metadata
```graphql
mutation SignoffMetadata($ipnftId: String!, $tokenURI: String!, $chainId: Int!, $minter: String!, $to: String!, $termsSignature: String!) {
  signoffMetadata(ipnftId: $ipnftId, tokenURI: $tokenURI, chainId: $chainId, minter: $minter, to: $to, termsSignature: $termsSignature) {
    authorization isSuccess
    error { message code retryable }
  }
}
```
Variables: `{"ipnftId": "RESERVATION_ID", "tokenURI": "ipfs://METADATA_CID", "chainId": 11155111, "minter": "WALLET_ADDRESS", "to": "WALLET_ADDRESS", "termsSignature": "SIGNATURE_FROM_3e"}`. Save `authorization`.

#### 3g: Mint on-chain (Privy)
ABI-encode with `cast`:
```bash
cast calldata "mintReservation(address,uint256,string,string,bytes)" \
  WALLET_ADDRESS RESERVATION_ID "ipfs://METADATA_CID" "SYMBOL" AUTHORIZATION_HEX
```
Send via Privy:
```bash
curl -s -X POST "https://api.privy.io/v1/wallets/$PRIVY_WALLET_ID/rpc" \
  --user "$PRIVY_APP_ID:$PRIVY_APP_SECRET" \
  -H "privy-app-id: $PRIVY_APP_ID" \
  -H "Content-Type: application/json" \
  -d '{
    "method": "eth_sendTransaction",
    "caip2": "eip155:11155111",
    "params": {
      "transaction": {
        "to": "0x152B444e60C526fe4434C721561a077269FcF61a",
        "data": "<ABI-encoded calldata>",
        "value": "1000000000000000"
      }
    }
  }'
```
Save `data.hash` as `MINT_TX`. **Done!** URL: `${MOLECULE_CLIENT_URL}/ipnfts/RESERVATION_ID`

---

## Workflow 2: Create Project (Data Room)

**After WF1. Requires service token.**
```bash
curl -s -X POST "$MOLECULE_LABS_URL" \
  -H "Content-Type: application/json" \
  -H "x-api-key: $MOLECULE_API_KEY" \
  -H "x-service-token: SERVICE_TOKEN" \
  -d '{
    "query": "mutation CreateProject($input: CreateProjectInput!) { createProject(input: $input) { isSuccess message error { message code retryable } project { ipnftUid ipnftSymbol ipnftAddress ipnftTokenId } } }",
    "variables": { "input": { "ipnftSymbol": "SYM1", "ipnftTokenId": "RESERVATION_ID_AS_STRING" } }
  }'
```
Save `ipnftUid` — needed for file uploads and announcements.

---

## Workflow 3: File Upload

Three-phase presigned upload. **After WF2. Requires service token.**

**Step 1 — Initiate:**
```bash
curl -s -X POST "$MOLECULE_LABS_URL" \
  -H "Content-Type: application/json" \
  -H "x-api-key: $MOLECULE_API_KEY" \
  -H "x-service-token: SERVICE_TOKEN" \
  -d '{
    "query": "mutation InitiateCreateOrUpdateFileV2($ipnftUid: String!, $contentType: String!, $contentLength: Int!) { initiateCreateOrUpdateFileV2(ipnftUid: $ipnftUid, contentType: $contentType, contentLength: $contentLength) { uploadToken uploadUrl uploadUrlExpiry method headers { key value } useMultipart isSuccess error { message code retryable } } }",
    "variables": { "ipnftUid": "0x152B444e60C526fe4434C721561a077269FcF61a_42", "contentType": "application/pdf", "contentLength": 381846 }
  }'
```
Save `uploadToken`, `uploadUrl`, `headers`.

**Step 2 — Upload bytes:** `curl -X PUT "UPLOAD_URL" -H "HEADER_KEY: HEADER_VALUE" --data-binary @file.pdf` (include all headers from step 1)

**Step 3 — Finalize:**
```bash
curl -s -X POST "$MOLECULE_LABS_URL" \
  -H "Content-Type: application/json" \
  -H "x-api-key: $MOLECULE_API_KEY" \
  -H "x-service-token: SERVICE_TOKEN" \
  -d '{
    "query": "mutation FinishCreateOrUpdateFileV2($ipnftUid: String!, $uploadToken: String!, $path: String, $ref: String, $accessLevel: String!, $changeBy: String!, $description: String, $tags: [String!], $categories: [String!]) { finishCreateOrUpdateFileV2(ipnftUid: $ipnftUid, uploadToken: $uploadToken, path: $path, ref: $ref, accessLevel: $accessLevel, changeBy: $changeBy, description: $description, tags: $tags, categories: $categories) { datasetId contentHash version newHead isSuccess message error { message code retryable } } }",
    "variables": { "ipnftUid": "0x152B444e60C526fe4434C721561a077269FcF61a_42", "uploadToken": "TOKEN_FROM_STEP_1", "path": "research-data.pdf", "accessLevel": "PUBLIC", "changeBy": "0xWallet", "description": "Initial research dataset", "tags": ["research", "data"], "categories": ["research"] }
  }'
```
New version: use `ref` (existing `datasetId`) instead of `path`. Access levels: `PUBLIC` | `HOLDERS` | `ADMIN`. Save `datasetId`.

---

## Workflow 4: Create Announcement

**After WF2. Requires service token.**
```bash
curl -s -X POST "$MOLECULE_LABS_URL" \
  -H "Content-Type: application/json" \
  -H "x-api-key: $MOLECULE_API_KEY" \
  -H "x-service-token: SERVICE_TOKEN" \
  -d '{
    "query": "mutation CreateAnnouncementV2($ipnftUid: String!, $headline: String!, $body: String!, $attachments: [String!]) { createAnnouncementV2(ipnftUid: $ipnftUid, headline: $headline, body: $body, attachments: $attachments) { isSuccess message error { message code retryable } } }",
    "variables": { "ipnftUid": "0x152B444e60C526fe4434C721561a077269FcF61a_42", "headline": "Research Milestone: Phase 1 Complete", "body": "Phase 1 complete.\n\n## Key Results\n- Dataset validated\n- Hypothesis supported", "attachments": ["DATASET_ID_FROM_FILE_UPLOAD"] }
  }'
```
Body supports Markdown.

---

## Query Operations

Use `aura_orchestrator` tool (only `x-api-key` needed).

**List projects:** `query ProjectsV2 { projectsV2 { projects { ipnftUid ipnftSymbol ipnftAddress ipnftTokenId } isSuccess error { message code } } }`
**Get project + data room:** `query ProjectWithDataRoomAndFilesV2($ipnftUid: String!) { projectWithDataRoomAndFilesV2(ipnftUid: $ipnftUid) { isSuccess project { ipnftUid ipnftSymbol } dataRoom { files { datasetId name contentType accessLevel description tags categories versions { version contentHash createdAt } } } } }`
**Get activity:** `query ProjectActivityV2($ipnftUid: String!) { projectActivityV2(ipnftUid: $ipnftUid) { isSuccess activities { type timestamp headline body attachments } } }`
**Search:** `query SearchLabs($query: String!) { searchLabs(query: $query) { isSuccess results { ipnftUid ipnftSymbol } } }`

---

## Error Handling

All responses include `isSuccess`. On failure: `{"error": {"message": "...", "code": "...", "retryable": true|false}}`

**Decision tree:**
1. `AUTH_FAILED` → `MOLECULE_API_KEY` wrong. STOP.
2. `SERVICE_AUTH_FAILED` → Re-run Service Token Acquisition (A-C), retry.
3. `MISSING_PARAMETERS` → Fix payload. Do NOT retry same request.
4. `INVALID_IPNFT_UID` → Must be `{contractAddress}_{tokenId}`. Fix.
5. `NOT_FOUND` → Prior step incomplete. Verify.
6. `INTERNAL_ERROR` + `retryable: false` → Save to `mint/diagnostics/error.json`. STOP.
7. `INTERNAL_ERROR` + `retryable: true` → Per-mutation diagnostics below.

**Per-mutation diagnostics (`INTERNAL_ERROR` retryable):**
- **`generateAssignmentAgreement`**: (1) `name` ≤ 100 chars, `initialSymbol` 3-5 alphanumeric (2) `email` valid format (3) `RESERVATION_ID` decimal uint256 NOT hex (4) `TX_HASH` is POI tx hash (5) `MERKLE_ROOT` is `tree[0]`. If all correct → API down. Save response, STOP. DO NOT retry with "different parameters".
- **`uploadMetadataWithImageKey`**: (1) `imageKey` must match `key` from `generateImageUploadUrl`, PUT must have returned 200 (2) `symbol` must be inside `properties`, NOT at top level (3) `terms_signature` = `agreementContentHash` from 3a (4) `properties` object must exist with `symbol`, `organization`, `research_lead`, `topic`, `funding_amount` (5) Do NOT guess alternative structures — save error to `mint/diagnostics/metadata_error.json`, STOP, report to user.
- **`signoffMetadata`**: `tokenURI` = `ipfs://CID` (not bare CID), `termsSignature` full `0x`-prefixed, `minter`/`to` = `WALLET_ADDRESS`.
- **`initiateCreateOrUpdateFileV2`**: `contentLength` exact byte count. `ipnftUid` uses underscore.
- **`finishCreateOrUpdateFileV2`**: `uploadToken` from same session. If URL expired, re-run from initiate.

**On-chain (Privy):** `POLICY_VIOLATION` → check limits, STOP | `INSUFFICIENT_FUNDS` → fund wallet, STOP | `INVALID_TRANSACTION` → re-check ABI | `Transaction reverted` → wrong params, no retry | `reserve()` NOT idempotent — wastes gas.
**Presigned URL expiry:** Re-run initiate/generate step. Do NOT retry PUT with old URL.
