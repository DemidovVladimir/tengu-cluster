---
name: molecule-upload
description: Upload a research file to a Molecule project data room via S3 presigned URL.
homepage: https://staging.graphql.api.molecule.xyz/graphql
---

# Upload File to Molecule Data Room

Three-step upload: initiate → PUT to S3 → finalize.

IMPORTANT: After creating a project, wait 30 seconds before starting the upload.
The data room provisioning is asynchronous and takes time to complete on the backend.
Use `run_command` with `sleep 30` to wait before proceeding.

## Required Environment Variables

| Variable | Description |
|----------|-------------|
| `MOLECULE_API_KEY` | Sent as `x-api-key` header |
| `MOLECULE_LABS_URL` | GraphQL endpoint URL |

## Step 1: Gather Inputs
These values are provided by the orchestrator context from upstream tasks, or from prior steps in this agent's workflow:
- `ipnftUid` — format: `{contract_address}_{token_id}` from the minting step
- `serviceToken` — from the authentication step (molecule-auth)
- File to upload (e.g. `.tengu-attachments/<document.pdf>`)
- Wallet address from `get_wallet_address`
- File metadata: content type (e.g. "application/pdf"), file size in bytes, and an optional description to include in Molecule.

## Step 2: Initiate upload

```
http_request:
  url: $MOLECULE_LABS_URL
  method: POST
  headers: {"x-api-key": "$MOLECULE_API_KEY", "x-service-token": "<serviceToken>", "Content-Type": "application/json"}
  body: {"query": "mutation InitiateCreateOrUpdateFileV2($ipnftUid: String!, $contentType: String!, $contentLength: Int!) { initiateCreateOrUpdateFileV2(ipnftUid: $ipnftUid, contentType: $contentType, contentLength: $contentLength) { uploadToken uploadUrl uploadUrlExpiry method headers { key value } useMultipart isSuccess error { message code retryable } } }", "variables": {"ipnftUid": "<ipnftUid>", "contentType": "application/pdf", "contentLength": <file_size_in_bytes>}}
  return_body: true
```

Extract from `data.initiateCreateOrUpdateFileV2`:
- `uploadToken` — needed for finalize step
- `uploadUrl` — the presigned S3 URL
- `method` — HTTP method for S3 upload (usually "PUT")
- `headers` — array of `{key, value}` pairs to include in the S3 request

## Step 3: Upload to S3

Use the EXACT `uploadUrl` from step 2. Include ALL headers from step 2's `headers` array.

```
http_request:
  url: <uploadUrl from step 2>
  method: <method from step 2, usually PUT>
  headers: {<all key:value pairs from step 2 headers>, "Content-Type": "application/pdf"}
  file_path: <path-to-file>
```

## Step 4: Finalize upload

```
http_request:
  url: $MOLECULE_LABS_URL
  method: POST
  headers: {"x-api-key": "$MOLECULE_API_KEY", "x-service-token": "<serviceToken>", "Content-Type": "application/json"}
  body: {"query": "mutation FinishCreateOrUpdateFileV2($ipnftUid: String!, $uploadToken: String!, $path: String, $ref: String, $accessLevel: String!, $changeBy: String!, $description: String, $tags: [String!], $categories: [String!]) { finishCreateOrUpdateFileV2(ipnftUid: $ipnftUid, uploadToken: $uploadToken, path: $path, ref: $ref, accessLevel: $accessLevel, changeBy: $changeBy, description: $description, tags: $tags, categories: $categories) { datasetId contentHash version newHead isSuccess message error { message code retryable } } }", "variables": {"ipnftUid": "<ipnftUid>", "uploadToken": "<uploadToken from step 2>", "path": "<filename>", "accessLevel": "PUBLIC", "changeBy": "<wallet_address>", "description": "<file description>"}}
  return_body: true
```

## Output

Extract from `data.finishCreateOrUpdateFileV2`:
- `datasetId` — the `did:odf:...` dataset ID (needed for announcement attachments)
- `contentHash`

Save to `uploads/upload_result.json`.
The orchestrator will automatically forward these values to downstream tasks via context.
