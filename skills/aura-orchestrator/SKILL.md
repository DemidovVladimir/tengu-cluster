---
name: aura-orchestrator
description: DeSci lab automation — IPNFT minting, project creation, file uploads, and announcements via Molecule GraphQL API with Privy agentic wallet for on-chain transactions and signing.
base_url: https://staging.graphql.api.molecule.xyz/graphql
capability: skill.aura_orchestrator
effect_class: external_api
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

**Execution order:** Step 0 (Wallet) → WF1 (Mint) → Service Token → WF2 (Project) → WF3 (Upload) → WF4 (Announcement)

## CRITICAL: URL Formats

**Do NOT fabricate URLs from memory. Construct them ONLY from actual API responses and the constants below.**

| URL | Format | Example |
|-----|--------|---------|
| **IP-NFT page** | `${MOLECULE_CLIENT_URL}/ipnfts/${RESERVATION_ID}` | `https://testnet.molecule.xyz/ipnfts/42` |
| **Etherscan tx** | `https://sepolia.etherscan.io/tx/${TX_HASH}` | `https://sepolia.etherscan.io/tx/0xabc...` |
| **ipnftUid** | `0x152B444e60C526fe4434C721561a077269FcF61a_${RESERVATION_ID}` | `0x152B444e60C526fe4434C721561a077269FcF61a_42` |

**The IPNFT contract is `0x152B444e60C526fe4434C721561a077269FcF61a`. No other contract address is valid.**
**The client URL is `${MOLECULE_CLIENT_URL}` (e.g. `https://testnet.molecule.xyz`). No other domain is valid.**

---

## Tool Routing

Each step below specifies exactly which tool to use. Follow it literally.

| Tool | When to use |
|------|-------------|
| `aura_orchestrator` | **ALL** Molecule GraphQL calls (WF1 steps 3a/3b-url/3c/3d/3f, Service Token A/C, WF2, WF3 steps 1/3, WF4, Queries) |
| `run_command` with curl | Privy wallet calls (Step 0, WF1 steps 2/3e/3g, Service Token B) — needs shell env vars |
| `poi_register_document` | POI document registration (WF1 step 1) |
| `upload_binary_url` | Binary file uploads to presigned URLs (WF1 step 3b-put, WF3 step 2) |

**`aura_orchestrator` details:** `path` is always `""` (base URL IS the GraphQL endpoint). The tool auto-injects `x-api-key`. For calls needing a service token, pass `headers: "{\"x-service-token\":\"TOKEN\"}"`.

---

## Required User Inputs (Workflow 1 — Mint)

**BEFORE starting any workflow, validate that ALL required fields are provided by the user. If ANY field is missing, ASK the user — do NOT invent, guess, or hallucinate values.**

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

## Transaction Communication

**BEFORE every `run_command` that sends an on-chain transaction, send a message to the user explaining:**
1. **What** — plain-language description
2. **Why** — why this step is required
3. **Cost** — any ETH cost (e.g., "gas only" or "0.001 ETH mint fee")
4. **Consequence of denial** — what happens if they press Deny

## Constants

| Parameter | Value |
|-----------|-------|
| Chain | Sepolia testnet |
| Chain ID | `11155111` |
| CAIP-2 | `eip155:11155111` |
| IPNFT Contract | `0x152B444e60C526fe4434C721561a077269FcF61a` |
| Mint Fee | `0.001 ETH` = `1000000000000000` wei |
| `ipnftUid` format | `{IPNFT_CONTRACT}_{RESERVATION_ID}` |
| Project link | `${MOLECULE_CLIENT_URL}/ipnfts/${RESERVATION_ID}` |

**Not required:** `EVM_PRIVATE_KEY`, `EVM_RPC_URL`, `MOLECULE_SERVICE_TOKEN`, `TENGU_RELAY_URL`, `TENGU_WALLET_SESSION`.

---

## Step 0: Resolve Wallet Address

**Tool: `run_command`** (Privy endpoint, needs shell env vars)
```bash
curl -s -X GET "https://api.privy.io/v1/wallets/$PRIVY_WALLET_ID" \
  --user "$PRIVY_APP_ID:$PRIVY_APP_SECRET" \
  -H "privy-app-id: $PRIVY_APP_ID"
```

**Expected response:**
```json
{"id": "wallet-id", "address": "0x4A75...", "chain_type": "ethereum"}
```

**Save:** `address` → `WALLET_ADDRESS`. Used everywhere as `connectedWalletAddress`, `minter`, `to`, `changeBy`.

---

## Workflow 1: Mint an IP-NFT

9 sequential steps. **If any step fails, STOP and report the error. Do NOT skip steps.**

### Step 1: Register POI

**Tool: `poi_register_document`**
```json
{"document_path": ".tengu-attachments/document.pdf"}
```
Find the actual filename first with `list_directory(".tengu-attachments")`.

**Expected response:**
```json
{
  "success": true,
  "data": {
    "proof": {
      "tree": ["0xabc123...", "0xdef456...", "0x789..."]
    },
    "transaction": {
      "data": "0x1234abcd...",
      "to": "0x1DEA29b04a59000b877979339a457d5aBE315b52"
    }
  }
}
```

**Verify:** `success` is `true`. If `false`, read error and STOP.
**Save:** `data.transaction.to` → `POI_CONTRACT`, `data.transaction.data` → `POI_CALLDATA`, `data.proof.tree[0]` → `MERKLE_ROOT`

### Step 2: Submit POI On-Chain

**Tool: `run_command`** (Privy endpoint)
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
        "to": "POI_CONTRACT",
        "data": "POI_CALLDATA",
        "value": "0"
      }
    }
  }'
```

**Expected response:**
```json
{"data": {"hash": "0xf172ae62..."}}
```

**Verify:** response contains `data.hash`. If error, STOP.
**Save:** `data.hash` → `POI_TX_HASH`
**Compute RESERVATION_ID:** `printf "%d\n" POI_CALLDATA` (converts the hex calldata to decimal uint256)

### Step 3a: Generate Assignment Agreement

**Tool: `aura_orchestrator`** — method: `POST`, path: `""`
```json
{
  "query": "mutation GenerateAssignmentAgreement($projectData: AWSJSON!) { generateAssignmentAgreement(projectData: $projectData) { agreementCid agreementContentHash isSuccess error { message code retryable } } }",
  "variables": {
    "projectData": "{\"project\":{\"name\":\"NAME\",\"description\":\"DESC\",\"initialSymbol\":\"SYM\",\"funding_amount\":{\"value\":0,\"currency\":\"USD\",\"currency_type\":\"ISO4217\",\"decimals\":2},\"organization\":\"ORG\",\"research_lead\":{\"name\":\"LEAD\",\"email\":\"EMAIL\"},\"topic\":\"TOPIC\"},\"connectedWalletAddress\":\"WALLET_ADDRESS\",\"agreementType\":\"POI_ASSIGNMENT\",\"chainId\":11155111,\"ipnftId\":\"RESERVATION_ID\",\"poiLocation\":{\"chainId\":11155111,\"transactionHash\":\"POI_TX_HASH\"},\"merkleRootHash\":\"MERKLE_ROOT\"}"
  }
}
```

**Field rules:** `ipnftId` MUST be decimal (not hex) | `name` max 100 chars | `initialSymbol` 3-5 alphanumeric UPPERCASE | `email` valid format | `merkleRootHash` = `data.proof.tree[0]` from Step 1

**Expected response:**
```json
{
  "data": {
    "generateAssignmentAgreement": {
      "agreementCid": "QmXyz...",
      "agreementContentHash": "0xabc...",
      "isSuccess": true
    }
  }
}
```

**Verify:** `isSuccess` is `true`. If `INTERNAL_ERROR` + `retryable: true`: check field rules above. If all correct, save response to `mint/diagnostics/error.json`, STOP. DO NOT retry with different parameters.
**Save:** `agreementCid` → `AGREEMENT_CID`, `agreementContentHash` → `AGREEMENT_HASH`

### Step 3b: Upload Cover Image

**Tool: `aura_orchestrator`** — method: `POST`, path: `""` (URL generation)
```json
{
  "query": "mutation GenerateImageUploadUrl($filename: String!, $contentType: String!, $ipnftId: String!) { generateImageUploadUrl(filename: $filename, contentType: $contentType, ipnftId: $ipnftId) { uploadUrl key isSuccess error { message code retryable } } }",
  "variables": {"filename": "cover.png", "contentType": "image/png", "ipnftId": "RESERVATION_ID"}
}
```
Use actual filename and matching content type (e.g., `image/jpeg` for `.jpg`).

**Expected response:**
```json
{
  "data": {
    "generateImageUploadUrl": {
      "uploadUrl": "https://s3.amazonaws.com/...",
      "key": "uploads/cover.png",
      "isSuccess": true
    }
  }
}
```

**Verify:** `isSuccess` is `true`.
**Save:** `key` → `IMAGE_KEY`

Then upload the binary:

**Tool: `upload_binary_url`**
```json
{
  "url": "https://s3.amazonaws.com/...(uploadUrl from above)",
  "file_path": ".tengu-attachments/cover.png",
  "content_type": "image/png"
}
```

**Verify:** HTTP 200 response.

### Step 3c: Upload Metadata

**Tool: `aura_orchestrator`** — method: `POST`, path: `""`
```json
{
  "query": "mutation UploadMetadataWithImageKey($metadata: AWSJSON!, $imageKey: String!, $ipnftId: String!) { uploadMetadataWithImageKey(metadata: $metadata, imageKey: $imageKey, ipnftId: $ipnftId) { metadataCid metadataUrl isSuccess error { message code retryable } } }",
  "variables": {
    "metadata": "{\"name\":\"NAME\",\"description\":\"DESC\",\"external_url\":\"MOLECULE_CLIENT_URL/ipnfts/RESERVATION_ID\",\"properties\":{\"symbol\":\"SYM\",\"organization\":\"ORG\",\"research_lead\":{\"name\":\"LEAD\",\"email\":\"EMAIL\"},\"topic\":\"TOPIC\",\"funding_amount\":{\"value\":0,\"currency\":\"USD\",\"currency_type\":\"ISO4217\",\"decimals\":2}},\"terms_signature\":\"AGREEMENT_HASH\",\"agreements\":[{\"type\":\"POI_ASSIGNMENT\",\"cid\":\"AGREEMENT_CID\",\"contentHash\":\"AGREEMENT_HASH\"}]}",
    "imageKey": "IMAGE_KEY",
    "ipnftId": "RESERVATION_ID"
  }
}
```

**Field mapping:**
- `external_url` → `${MOLECULE_CLIENT_URL}/ipnfts/${RESERVATION_ID}` (construct from env + Step 2)
- `properties.symbol` → user-provided (NOT at top level — API rejects top-level `symbol`)
- `terms_signature` → `AGREEMENT_HASH` from Step 3a
- `agreements[0].cid` → `AGREEMENT_CID` from Step 3a
- `agreements[0].contentHash` → `AGREEMENT_HASH` from Step 3a

**Expected response:**
```json
{
  "data": {
    "uploadMetadataWithImageKey": {
      "metadataCid": "QmMetadata...",
      "metadataUrl": "ipfs://QmMetadata...",
      "isSuccess": true
    }
  }
}
```

**Verify:** `isSuccess` is `true`. If `MISSING_PARAMETERS` or `INVALID_PARAMETERS`, save to `mint/diagnostics/metadata_error.json`, STOP. Do NOT guess alternative structures.
**Save:** `metadataCid` → `METADATA_CID`

### Step 3d: Get Terms Message

**Tool: `aura_orchestrator`** — method: `POST`, path: `""`
```json
{
  "query": "query GetTermsMessage($metadataCid: String!, $minter: String!, $chainId: Int!) { getTermsMessage(metadataCid: $metadataCid, minter: $minter, chainId: $chainId) { message digest isSuccess error { message code retryable } } }",
  "variables": {"metadataCid": "METADATA_CID", "minter": "WALLET_ADDRESS", "chainId": 11155111}
}
```

**Expected response:**
```json
{
  "data": {
    "getTermsMessage": {
      "message": "I accept the terms...",
      "digest": "0x...",
      "isSuccess": true
    }
  }
}
```

**Verify:** `isSuccess` is `true`.
**Save:** `message` → `TERMS_MESSAGE`

### Step 3e: Sign Terms Message

**Tool: `run_command`** (Privy endpoint)
```bash
HEX_MSG=$(echo -n 'TERMS_MESSAGE' | xxd -p | tr -d '\n' | sed 's/^/0x/')
curl -s -X POST "https://api.privy.io/v1/wallets/$PRIVY_WALLET_ID/rpc" \
  --user "$PRIVY_APP_ID:$PRIVY_APP_SECRET" \
  -H "privy-app-id: $PRIVY_APP_ID" \
  -H "Content-Type: application/json" \
  -d "{\"method\": \"personal_sign\", \"params\": {\"message\": \"$HEX_MSG\"}}"
```

**Expected response:**
```json
{"data": {"signature": "0xabc123..."}}
```

**Verify:** response contains `data.signature`.
**Save:** `data.signature` → `TERMS_SIGNATURE`

### Step 3f: Sign Off Metadata

**Tool: `aura_orchestrator`** — method: `POST`, path: `""`
```json
{
  "query": "mutation SignoffMetadata($ipnftId: String!, $tokenURI: String!, $chainId: Int!, $minter: String!, $to: String!, $termsSignature: String!) { signoffMetadata(ipnftId: $ipnftId, tokenURI: $tokenURI, chainId: $chainId, minter: $minter, to: $to, termsSignature: $termsSignature) { authorization isSuccess error { message code retryable } } }",
  "variables": {
    "ipnftId": "RESERVATION_ID",
    "tokenURI": "ipfs://METADATA_CID",
    "chainId": 11155111,
    "minter": "WALLET_ADDRESS",
    "to": "WALLET_ADDRESS",
    "termsSignature": "TERMS_SIGNATURE"
  }
}
```

**Expected response:**
```json
{
  "data": {
    "signoffMetadata": {
      "authorization": "0xlong_hex...",
      "isSuccess": true
    }
  }
}
```

**Verify:** `isSuccess` is `true`. `tokenURI` must be `ipfs://CID` (not bare CID).
**Save:** `authorization` → `AUTHORIZATION`

### Step 3g: Mint On-Chain

**Tool: `run_command`** (Privy endpoint)

ABI-encode the calldata with `cast`:
```bash
cast calldata "mintReservation(address,uint256,string,string,bytes)" \
  WALLET_ADDRESS RESERVATION_ID "ipfs://METADATA_CID" "SYMBOL" AUTHORIZATION
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
        "data": "CALLDATA_FROM_CAST",
        "value": "1000000000000000"
      }
    }
  }'
```

**Expected response:**
```json
{"data": {"hash": "0x5d3df8f5..."}}
```

**Verify:** response contains `data.hash`.
**Save:** `data.hash` → `MINT_TX_HASH`

**WF1 complete.** Construct these URLs from your saved values:
- **IP-NFT page:** `${MOLECULE_CLIENT_URL}/ipnfts/${RESERVATION_ID}` (e.g. `https://testnet.molecule.xyz/ipnfts/42`)
- **Mint tx:** `https://sepolia.etherscan.io/tx/${MINT_TX_HASH}`
- **POI tx:** `https://sepolia.etherscan.io/tx/${POI_TX_HASH}`
- **ipnftUid:** `0x152B444e60C526fe4434C721561a077269FcF61a_${RESERVATION_ID}`

Save all to `mint/metadata/mint_result.md`.

---

## Service Token Acquisition (for Workflows 2-4)

Run this after WF1 and before WF2. The service token is valid for 180 days.

### Step A: Get Sign-In Message

**Tool: `aura_orchestrator`** — method: `POST`, path: `""`
```json
{
  "query": "query GetServiceSignInMessage($walletAddress: String!, $serviceName: String!) { getServiceSignInMessage(walletAddress: $walletAddress, serviceName: $serviceName) }",
  "variables": {"walletAddress": "WALLET_ADDRESS", "serviceName": "tengu-agent"}
}
```

**Expected response:**
```json
{
  "data": {
    "getServiceSignInMessage": "service.molecule.xyz wants you to sign in with your Ethereum account:\n0x4A75..."
  }
}
```

**Save:** the message string → `SIGN_IN_MESSAGE`

### Step B: Sign Message with Privy

**Tool: `run_command`** (Privy endpoint)
```bash
HEX_MSG=$(echo -n 'SIGN_IN_MESSAGE' | xxd -p | tr -d '\n' | sed 's/^/0x/')
curl -s -X POST "https://api.privy.io/v1/wallets/$PRIVY_WALLET_ID/rpc" \
  --user "$PRIVY_APP_ID:$PRIVY_APP_SECRET" \
  -H "privy-app-id: $PRIVY_APP_ID" \
  -H "Content-Type: application/json" \
  -d "{\"method\": \"personal_sign\", \"params\": {\"message\": \"$HEX_MSG\"}}"
```

**Expected response:**
```json
{"data": {"signature": "0xdef456..."}}
```

**Save:** `data.signature` → `MESSAGE_SIGNATURE`

### Step C: Exchange for Service Token

**Tool: `aura_orchestrator`** — method: `POST`, path: `""`
```json
{
  "query": "mutation GenerateServiceToken($serviceName: String!, $walletAddress: String!, $messageSignature: String!) { generateServiceToken(serviceName: $serviceName, walletAddress: $walletAddress, messageSignature: $messageSignature) { token metadata { tokenId expiresAt serviceName } } }",
  "variables": {"serviceName": "tengu-agent", "walletAddress": "WALLET_ADDRESS", "messageSignature": "MESSAGE_SIGNATURE"}
}
```

**Expected response:**
```json
{
  "data": {
    "generateServiceToken": {
      "token": "eyJhbGciOiJ...",
      "metadata": {"tokenId": "...", "expiresAt": "...", "serviceName": "tengu-agent"}
    }
  }
}
```

**Verify:** response contains `token`.
**Save:** `token` → `SERVICE_TOKEN`

---

## Workflow 2: Create Project (Data Room)

**After WF1. Requires SERVICE_TOKEN.**

**Tool: `aura_orchestrator`** — method: `POST`, path: `""`, headers: `{"x-service-token": "SERVICE_TOKEN"}`
```json
{
  "query": "mutation CreateProject($input: CreateProjectInput!) { createProject(input: $input) { isSuccess message error { message code retryable } project { ipnftUid ipnftSymbol ipnftAddress ipnftTokenId } } }",
  "variables": {"input": {"ipnftSymbol": "SYMBOL", "ipnftTokenId": "RESERVATION_ID"}}
}
```

`ipnftSymbol` must match the symbol used during minting. `ipnftTokenId` is the RESERVATION_ID as a string.

**Expected response:**
```json
{
  "data": {
    "createProject": {
      "isSuccess": true,
      "message": "Project created",
      "project": {
        "ipnftUid": "0x152B444e60C526fe4434C721561a077269FcF61a_42",
        "ipnftSymbol": "SYM",
        "ipnftAddress": "0x152B444e60C526fe4434C721561a077269FcF61a",
        "ipnftTokenId": "42"
      }
    }
  }
}
```

**Verify:** `isSuccess` is `true`. The `ipnftUid` in the response confirms the format `{contractAddress}_{tokenId}`.
**Save:** `project.ipnftUid` → `IPNFT_UID` (use this exact value from the response for WF3 and WF4)

**Project URL** (construct from constants, NOT from memory): `${MOLECULE_CLIENT_URL}/ipnfts/${RESERVATION_ID}`

---

## Workflow 3: File Upload

Three-phase presigned upload. **After WF2. Requires SERVICE_TOKEN.**

### Step 1: Initiate Upload

**Tool: `aura_orchestrator`** — method: `POST`, path: `""`, headers: `{"x-service-token": "SERVICE_TOKEN"}`

First, get the file size:
**Tool: `run_command`** — `wc -c < /path/to/file.pdf` (or `stat -f%z` on macOS)

Then initiate:
```json
{
  "query": "mutation InitiateCreateOrUpdateFileV2($ipnftUid: String!, $contentType: String!, $contentLength: Int!) { initiateCreateOrUpdateFileV2(ipnftUid: $ipnftUid, contentType: $contentType, contentLength: $contentLength) { uploadToken uploadUrl uploadUrlExpiry method headers { key value } useMultipart isSuccess error { message code retryable } } }",
  "variables": {"ipnftUid": "IPNFT_UID", "contentType": "application/pdf", "contentLength": 381846}
}
```

`ipnftUid` is the value from WF2 response (e.g. `0x152B444e60C526fe4434C721561a077269FcF61a_42`). `contentLength` must be the exact byte count.

**Expected response:**
```json
{
  "data": {
    "initiateCreateOrUpdateFileV2": {
      "uploadToken": "tok_abc123...",
      "uploadUrl": "https://s3.amazonaws.com/...",
      "uploadUrlExpiry": "2024-01-15T12:30:00Z",
      "method": "PUT",
      "headers": [{"key": "Content-Type", "value": "application/pdf"}],
      "useMultipart": false,
      "isSuccess": true
    }
  }
}
```

**Verify:** `isSuccess` is `true`.
**Save:** `uploadToken` → `UPLOAD_TOKEN`, `uploadUrl` → `UPLOAD_URL`, `headers` → `UPLOAD_HEADERS`

### Step 2: Upload File Bytes

**Tool: `upload_binary_url`**
```json
{
  "url": "UPLOAD_URL",
  "file_path": ".tengu-attachments/document.pdf",
  "content_type": "application/pdf"
}
```

**Verify:** HTTP 200 response. If the presigned URL expired, go back to Step 1 for a fresh URL.

### Step 3: Finalize Upload

**Tool: `aura_orchestrator`** — method: `POST`, path: `""`, headers: `{"x-service-token": "SERVICE_TOKEN"}`
```json
{
  "query": "mutation FinishCreateOrUpdateFileV2($ipnftUid: String!, $uploadToken: String!, $path: String, $ref: String, $accessLevel: String!, $changeBy: String!, $description: String, $tags: [String!], $categories: [String!]) { finishCreateOrUpdateFileV2(ipnftUid: $ipnftUid, uploadToken: $uploadToken, path: $path, ref: $ref, accessLevel: $accessLevel, changeBy: $changeBy, description: $description, tags: $tags, categories: $categories) { datasetId contentHash version newHead isSuccess message error { message code retryable } } }",
  "variables": {
    "ipnftUid": "IPNFT_UID",
    "uploadToken": "UPLOAD_TOKEN",
    "path": "hypothesis.pdf",
    "accessLevel": "PUBLIC",
    "changeBy": "WALLET_ADDRESS",
    "description": "Research hypothesis document",
    "tags": ["research", "hypothesis"],
    "categories": ["research"]
  }
}
```

Use `path` for new files. Use `ref` (existing `datasetId`) for new versions. Access levels: `PUBLIC` | `HOLDERS` | `ADMIN`.

**Expected response:**
```json
{
  "data": {
    "finishCreateOrUpdateFileV2": {
      "datasetId": "ds_abc123",
      "contentHash": "0xabc...",
      "version": 1,
      "newHead": "head_xyz",
      "isSuccess": true,
      "message": "File created"
    }
  }
}
```

**Verify:** `isSuccess` is `true`.
**Save:** `datasetId` → `DATASET_ID` (needed for WF4 attachments)

Save results to `uploads/molecule_result.md`.

---

## Workflow 4: Create Announcement

**After WF2. Requires SERVICE_TOKEN.** If attaching files, complete WF3 first.

**Tool: `aura_orchestrator`** — method: `POST`, path: `""`, headers: `{"x-service-token": "SERVICE_TOKEN"}`
```json
{
  "query": "mutation CreateAnnouncementV2($ipnftUid: String!, $headline: String!, $body: String!, $attachments: [String!]) { createAnnouncementV2(ipnftUid: $ipnftUid, headline: $headline, body: $body, attachments: $attachments) { isSuccess message error { message code retryable } } }",
  "variables": {
    "ipnftUid": "IPNFT_UID",
    "headline": "Research Published: Hypothesis Title",
    "body": "## Summary\nResearch findings...\n\n## Links\n- [IP-NFT](https://testnet.molecule.xyz/ipnfts/42)\n- [Mint Transaction](https://sepolia.etherscan.io/tx/0x...)",
    "attachments": ["DATASET_ID"]
  }
}
```

Body supports Markdown. `attachments` is optional — use `datasetId` values from WF3.

**Expected response:**
```json
{
  "data": {
    "createAnnouncementV2": {
      "isSuccess": true,
      "message": "Announcement created"
    }
  }
}
```

**Verify:** `isSuccess` is `true`.

---

## Query Operations

**Tool: `aura_orchestrator`** (only `x-api-key` needed, no service token)

**List projects:**
```json
{"query": "query ProjectsV2 { projectsV2 { projects { ipnftUid ipnftSymbol ipnftAddress ipnftTokenId } isSuccess error { message code } } }"}
```

**Get project + data room:**
```json
{"query": "query ProjectWithDataRoomAndFilesV2($ipnftUid: String!) { projectWithDataRoomAndFilesV2(ipnftUid: $ipnftUid) { isSuccess project { ipnftUid ipnftSymbol } dataRoom { files { datasetId name contentType accessLevel description tags categories versions { version contentHash createdAt } } } } }", "variables": {"ipnftUid": "IPNFT_UID"}}
```

**Get activity:**
```json
{"query": "query ProjectActivityV2($ipnftUid: String!) { projectActivityV2(ipnftUid: $ipnftUid) { isSuccess activities { type timestamp headline body attachments } } }", "variables": {"ipnftUid": "IPNFT_UID"}}
```

**Search:**
```json
{"query": "query SearchLabs($query: String!) { searchLabs(query: $query) { isSuccess results { ipnftUid ipnftSymbol } } }", "variables": {"query": "search term"}}
```

---

## Error Handling

All responses include `isSuccess`. On failure: `{"error": {"message": "...", "code": "...", "retryable": true|false}}`

**Decision tree:**
1. `AUTH_FAILED` → `MOLECULE_API_KEY` wrong. STOP.
2. `SERVICE_AUTH_FAILED` → Re-run Service Token Acquisition (A-C), retry the failed call.
3. `MISSING_PARAMETERS` → Fix payload. Do NOT retry same request.
4. `INVALID_IPNFT_UID` → Must be `{contractAddress}_{tokenId}` with underscore. Fix.
5. `NOT_FOUND` → Prior step incomplete. Verify.
6. `INTERNAL_ERROR` + `retryable: false` → Save to `mint/diagnostics/error.json`. STOP.
7. `INTERNAL_ERROR` + `retryable: true` → Check per-mutation diagnostics below.

**Per-mutation diagnostics (`INTERNAL_ERROR` retryable):**
- **`generateAssignmentAgreement`**: (1) `name` ≤ 100 chars, `initialSymbol` 3-5 alphanumeric (2) `email` valid (3) `RESERVATION_ID` decimal NOT hex (4) `TX_HASH` is POI tx (5) `MERKLE_ROOT` is `tree[0]`. If all correct → API down. Save, STOP.
- **`uploadMetadataWithImageKey`**: (1) `imageKey` from `generateImageUploadUrl` (2) `symbol` inside `properties` NOT top-level (3) `terms_signature` = `agreementContentHash` (4) `properties` must have all required fields. Save error to `mint/diagnostics/metadata_error.json`, STOP.
- **`signoffMetadata`**: `tokenURI` = `ipfs://CID` (not bare CID), `termsSignature` `0x`-prefixed, `minter`/`to` = `WALLET_ADDRESS`.
- **`initiateCreateOrUpdateFileV2`**: `contentLength` exact byte count. `ipnftUid` uses underscore.
- **`finishCreateOrUpdateFileV2`**: `uploadToken` from same session. If URL expired, re-initiate.

**On-chain (Privy):** `POLICY_VIOLATION` → check limits, STOP | `INSUFFICIENT_FUNDS` → fund wallet, STOP | `INVALID_TRANSACTION` → re-check ABI encoding | `Transaction reverted` → wrong params, no retry.
**Presigned URL expiry:** Re-run initiate/generate step. Do NOT retry PUT with old URL.
