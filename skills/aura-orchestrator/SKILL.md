---
name: aura-orchestrator
description: DeSci lab automation — IPNFT minting, project creation, file uploads, and announcements via Molecule GraphQL API and on-chain transactions.
homepage: https://staging.graphql.api.molecule.xyz/graphql
headers:
  x-api-key: $MOLECULE_API_KEY
  x-service-token: $MOLECULE_SERVICE_TOKEN
---

# Aura Orchestrator: DeSci Lab Automation

**Important:** The base URL already points to the GraphQL endpoint. For all GraphQL calls use `path: ""` (empty string). Never append `/graphql` or any other path — the URL is complete as-is.

Four workflows for Molecule DeSci infrastructure:

1. **IPNFT Mint** — POI registration, metadata via GraphQL, mint on-chain
2. **Project Creation** — create a data room linked to a minted IP-NFT
3. **File Upload** — three-phase presigned upload
4. **Announcement** — publish updates with file attachments

---

## Prerequisites

### Required Environment Variables

| Variable | Required For | Description |
|----------|-------------|-------------|
| `MOLECULE_API_KEY` | All GraphQL calls | API key. Sent as `x-api-key` header. |
| `MOLECULE_SERVICE_TOKEN` | File uploads, announcements | Service token JWT. Sent as `x-service-token` header. |
| `MOLECULE_LABS_URL` | All GraphQL calls | GraphQL endpoint (e.g. `https://staging.graphql.api.molecule.xyz/graphql`). |
| `MOLECULE_CLIENT_URL` | Link construction | Client URL (e.g. `https://testnet.molecule.xyz`). |
| `EVM_PRIVATE_KEY` | On-chain signing | Wallet private key (hex). **Never sent to any API.** |
| `EVM_RPC_URL` | On-chain transactions | Sepolia RPC endpoint. |
| `POI_API_KEY` | POI registration | API key. Sent as `Authorization: Bearer $POI_API_KEY`. |

### On-Chain Constants

| Parameter | Value |
|-----------|-------|
| **Chain** | Sepolia testnet |
| **Chain ID** | `11155111` |
| **IPNFT Contract** | `0x152B444e60C526fe4434C721561a077269FcF61a` |
| **Mint Fee** | `0.001 ETH` |

---

## Authentication

**Minting (Workflow 1):** `x-api-key` only — no service token needed.

**Workflows 2-4:** `x-api-key` + `x-service-token`

All GraphQL requests go to `${MOLECULE_LABS_URL}` via the `aura_orchestrator` tool.

---

## Workflow 1: Mint an IP-NFT

Three mandatory stages: POI registration, POI on-chain submission, and CLI minting. Every stage depends on the previous one's output — do NOT skip any. **If any step fails, STOP and report the error. Do NOT fall back to the CLI minter without `--reservation-id`, `--poi-tx-hash`, and `--merkle-root` — that produces a wrong sequential ID.**

### Step 1: Register POI

Use `run_command` to call the POI API. **The `-F` flag MUST use `=@` (equals-at) syntax — `files=@path`. Missing `=` causes a curl error.**

```bash
curl -X POST https://testnet.molecule.xyz/api/v1/inventions \
  -H "Authorization: Bearer $POI_API_KEY" \
  -H "Content-Type: multipart/form-data" \
  -F "files=@/absolute/path/to/document.pdf"
```

Use the absolute path to the file. Do NOT omit the `=` sign before `@`.

Save the entire JSON response. Key values:
- `data.transaction.to` — POI contract address (for Step 2 `cast send`)
- `data.transaction.data` — merkle root hash (for Step 2 `cast send` AND used as reservation ID when cast to decimal uint256)
- `data.proof.tree[0]` — same merkle root hash (passed as `--merkle-root` in Step 3)

### Step 2: Submit POI on-chain

Submit the POI transaction on-chain using `data.transaction.to` and `data.transaction.data` from Step 1:

```bash
cast send <transaction.to> <transaction.data> --private-key $EVM_PRIVATE_KEY --rpc-url $EVM_RPC_URL --chain 11155111 --json
```

Save the `TX_HASH` from the receipt (`transactionHash` field).

**The reservation ID is the `transaction.data` from Step 1 cast to a decimal uint256** — it does NOT come from receipt logs (the POI contract emits no events). Compute it:

```bash
cast to-dec <transaction.data>
```

Where `<transaction.data>` is the hex value from the POI API response (`data.transaction.data`), which is the same as `data.proof.tree[0]` (the merkle root).

Save three values for Step 3:
- `RESERVATION_ID` — the decimal uint256 from `cast to-dec`
- `TX_HASH` — the transaction hash from the `cast send` receipt
- `MERKLE_ROOT` — `data.proof.tree[0]` from Step 1 (same hex as `data.transaction.data`)

### Step 3: Run the CLI minter

Pass the reservation ID and POI transaction hash from Step 2:

```bash
/Users/vladimirdemidov/development/tengu-cluster/tools/ipnft-minter/target/release/ipnft-minter \
  --reservation-id "RESERVATION_ID" \
  --poi-tx-hash "TX_HASH" \
  --merkle-root "MERKLE_ROOT" \
  --name "PROJECT_NAME" \
  --description "PROJECT_DESCRIPTION" \
  --symbol "SYMBOL" \
  --organization "ORG_NAME" \
  --lead-name "LEAD_FULL_NAME" \
  --lead-email "LEAD_EMAIL" \
  --topic "RESEARCH_TOPIC"
```

Optional: `--image /path/to/cover.png` (uses a placeholder if omitted).

**All three POI flags are required:** `--reservation-id`, `--poi-tx-hash`, and `--merkle-root`. If any is omitted, the mint will fail or produce a wrong sequential token ID. Do NOT run this step without completing Steps 1 and 2 first.

Output on success:
```json
{
  "success": true,
  "reservation_id": "89012345678901234567890...",
  "mint_tx": "0x...",
  "metadata_cid": "Qm...",
  "project_url": "https://testnet.molecule.xyz/ipnfts/..."
}
```

Progress logs go to stderr. If it fails, the error message explains which step failed.

---

## Workflow 2: Create Project (Data Room)

**Must be executed after Workflow 1.** Requires `x-api-key` + `x-service-token`.

Use `aura_orchestrator` (include `x-service-token: $MOLECULE_SERVICE_TOKEN` header):

```graphql
mutation CreateProject($input: CreateProjectInput!) {
  createProject(input: $input) {
    isSuccess message
    error { message code retryable }
    project { ipnftUid ipnftSymbol ipnftAddress ipnftTokenId }
  }
}
```

Variables:
```json
{
  "input": {
    "ipnftSymbol": "SYM1",
    "ipnftTokenId": "RESERVATION_ID_AS_STRING"
  }
}
```

Save `ipnftUid` — needed for file uploads and announcements.

---

## Workflow 3: File Upload

Three-phase presigned upload. **Must be executed after Workflow 2.** Requires `x-api-key` + `x-service-token`.

### Step 1: Initiate upload (GraphQL)

Use `aura_orchestrator`:

```graphql
mutation InitiateCreateOrUpdateFileV2($ipnftUid: String!, $contentType: String!, $contentLength: Int!) {
  initiateCreateOrUpdateFileV2(ipnftUid: $ipnftUid, contentType: $contentType, contentLength: $contentLength) {
    uploadToken uploadUrl uploadUrlExpiry method
    headers { key value }
    useMultipart isSuccess
    error { message code retryable }
  }
}
```

Variables:
```json
{
  "ipnftUid": "0x152B444e60C526fe4434C721561a077269FcF61a_42",
  "contentType": "application/pdf",
  "contentLength": 381846
}
```

Save `uploadToken`, `uploadUrl`, `headers`.

### Step 2: Upload file bytes (HTTP PUT)

Use `run_command`:
```bash
curl -X PUT "UPLOAD_URL" -H "HEADER_KEY: HEADER_VALUE" --data-binary @file.pdf
```

Include all headers from step 1.

### Step 3: Finalize upload (GraphQL)

Use `aura_orchestrator`:

```graphql
mutation FinishCreateOrUpdateFileV2($ipnftUid: String!, $uploadToken: String!, $path: String, $ref: String, $accessLevel: String!, $changeBy: String!, $description: String, $tags: [String!], $categories: [String!]) {
  finishCreateOrUpdateFileV2(ipnftUid: $ipnftUid, uploadToken: $uploadToken, path: $path, ref: $ref, accessLevel: $accessLevel, changeBy: $changeBy, description: $description, tags: $tags, categories: $categories) {
    datasetId contentHash version newHead isSuccess message
    error { message code retryable }
  }
}
```

New file variables:
```json
{
  "ipnftUid": "0x152B444e60C526fe4434C721561a077269FcF61a_42",
  "uploadToken": "TOKEN_FROM_STEP_1",
  "path": "research-data.pdf",
  "accessLevel": "PUBLIC",
  "changeBy": "0xWallet",
  "description": "Initial research dataset",
  "tags": ["research", "data"],
  "categories": ["research"]
}
```

New version: use `ref` (existing `datasetId`) instead of `path`.

Access levels: `PUBLIC` (anyone), `HOLDERS` (token holders), `ADMIN` (admins only).

Save `datasetId` for use in announcements.

---

## Workflow 4: Create Announcement

**Must be executed after Workflow 2.** Requires `x-api-key` + `x-service-token`.

Use `aura_orchestrator`:

```graphql
mutation CreateAnnouncementV2($ipnftUid: String!, $headline: String!, $body: String!, $attachments: [String!]) {
  createAnnouncementV2(ipnftUid: $ipnftUid, headline: $headline, body: $body, attachments: $attachments) {
    isSuccess message
    error { message code retryable }
  }
}
```

Variables:
```json
{
  "ipnftUid": "0x152B444e60C526fe4434C721561a077269FcF61a_42",
  "headline": "Research Milestone: Phase 1 Complete",
  "body": "Phase 1 complete.\n\n## Key Results\n- Dataset validated\n- Hypothesis supported",
  "attachments": ["DATASET_ID_FROM_FILE_UPLOAD"]
}
```

Body supports Markdown.

---

## Query Operations

### List projects

```graphql
query ProjectsV2 {
  projectsV2 {
    projects { ipnftUid ipnftSymbol ipnftAddress ipnftTokenId }
    isSuccess
    error { message code }
  }
}
```

### Get project with data room

```graphql
query ProjectWithDataRoomAndFilesV2($ipnftUid: String!) {
  projectWithDataRoomAndFilesV2(ipnftUid: $ipnftUid) {
    isSuccess
    project { ipnftUid ipnftSymbol }
    dataRoom {
      files { datasetId name contentType accessLevel description tags categories
        versions { version contentHash createdAt }
      }
    }
  }
}
```

### Get project activity

```graphql
query ProjectActivityV2($ipnftUid: String!) {
  projectActivityV2(ipnftUid: $ipnftUid) {
    isSuccess
    activities { type timestamp headline body attachments }
  }
}
```

### Search projects

```graphql
query SearchLabs($query: String!) {
  searchLabs(query: $query) {
    isSuccess
    results { ipnftUid ipnftSymbol }
  }
}
```

---

## Execution Order

```
Workflow 1 (Mint IPNFT)
    |
    v
Workflow 2 (Create Project)
    |
    +-------+-------+
    |               |
    v               v
Workflow 3       Workflow 4
(File Upload)    (Announcement)
```

- Workflow 1 must complete before Workflow 2
- Workflow 2 must complete before Workflows 3 or 4
- Workflows 3 and 4 can run in parallel unless announcements need file attachments

---

## Error Handling

All GraphQL responses include `isSuccess`. On failure:
```json
{"error": {"message": "...", "code": "...", "retryable": true|false}}
```

Only retry if `retryable` is `true`.

| Code | Meaning | Action |
|------|---------|--------|
| `AUTH_FAILED` | Invalid credentials | Do not retry. Check `MOLECULE_API_KEY`. |
| `SERVICE_AUTH_FAILED` | Expired service token | Regenerate token, then retry. |
| `MISSING_PARAMETERS` | Bad payload | Fix request. Do not retry. |
| `INVALID_IPNFT_UID` | Wrong format | Must be `{contractAddress}_{tokenId}`. |
| `NOT_FOUND` | Resource missing | Verify prior step completed. |
| `INTERNAL_ERROR` | Server error | Retry up to 3 times with backoff. |

### On-chain errors

- **Gas estimation failure** — check wallet balance; if sufficient, parameters are invalid
- **Transaction reverted** — do not retry same tx; check parameters
- **`reserve()` is NOT idempotent** — each call creates a new reservation (wastes gas)

### Presigned URL expiry

If upload URL expired, re-execute the initiate/generate step for a fresh URL. Do not retry the PUT.

---

## Quick Reference

| What | Value |
|------|-------|
| GraphQL endpoint | `${MOLECULE_LABS_URL}` |
| API key header | `x-api-key: ${MOLECULE_API_KEY}` |
| Service token header | `x-service-token: ${MOLECULE_SERVICE_TOKEN}` |
| IPNFT contract (Sepolia) | `0x152B444e60C526fe4434C721561a077269FcF61a` |
| Chain ID | `11155111` |
| Mint fee | `0.001 ETH` |
| `ipnftUid` format | `{contractAddress}_{tokenId}` |
| Project link | `${MOLECULE_CLIENT_URL}/ipnfts/{tokenId}` |
| Execution order | Mint -> Project -> File Upload / Announcement |
| CLI tool path | `tools/ipnft-minter/target/release/ipnft-minter` |
