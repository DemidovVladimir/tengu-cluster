---
name: molecule-upload
description: Upload a research file to a Molecule project data room via S3 presigned URL.
homepage: https://staging.graphql.api.molecule.xyz/graphql
---

# Upload File to Molecule Data Room

Three-step upload: initiate → PUT to S3 → finalize.

IMPORTANT: After creating a project, wait 5-10 seconds before starting the upload.
The data room provisioning is asynchronous — if you get `dataRoom: null` in the
finalize step, wait a few seconds and retry the full 3-step upload sequence once.

## Required Environment Variables

| Variable | Description |
|----------|-------------|
| `MOLECULE_API_KEY` | Sent as `x-api-key` header |
| `MOLECULE_LABS_URL` | GraphQL endpoint URL |

## Input

- `ipnft_uid` from `uploads/project_result.json`
- Service token from `uploads/service_token.txt`
- File to upload (e.g. `.tengu-attachments/document.pdf`)
- Wallet address from `get_wallet_address` (used as `changeBy` in finalize)

## Step 1: Initiate upload

```
http_request:
  url: $MOLECULE_LABS_URL
  method: POST
  headers: {"x-api-key": "$MOLECULE_API_KEY", "x-service-token": "<token>", "Content-Type": "application/json"}
  body: {"query": "mutation InitiateCreateOrUpdateFileV2($ipnftUid: String!, $contentType: String!, $contentLength: Int!) { initiateCreateOrUpdateFileV2(ipnftUid: $ipnftUid, contentType: $contentType, contentLength: $contentLength) { uploadToken uploadUrl uploadUrlExpiry method headers { key value } useMultipart isSuccess error { message code retryable } } }", "variables": {"ipnftUid": "<ipnft_uid>", "contentType": "application/pdf", "contentLength": <file_size_in_bytes>}}
```

Extract from `data.initiateCreateOrUpdateFileV2`:
- `uploadToken` — needed for finalize step
- `uploadUrl` — the presigned S3 URL
- `method` — HTTP method for S3 upload (usually "PUT")
- `headers` — array of `{key, value}` pairs to include in the S3 request

## Step 2: Upload to S3

Use the EXACT `uploadUrl` from step 1. Include ALL headers from step 1's `headers` array.

```
http_request:
  url: <uploadUrl from step 1>
  method: <method from step 1, usually PUT>
  headers: {<all key:value pairs from step 1 headers>, "Content-Type": "application/pdf"}
  file_path: <path-to-file>
```

## Step 3: Finalize upload

```
http_request:
  url: $MOLECULE_LABS_URL
  method: POST
  headers: {"x-api-key": "$MOLECULE_API_KEY", "x-service-token": "<token>", "Content-Type": "application/json"}
  body: {"query": "mutation FinishCreateOrUpdateFileV2($ipnftUid: String!, $uploadToken: String!, $path: String, $ref: String, $accessLevel: String!, $changeBy: String!, $description: String, $tags: [String!], $categories: [String!]) { finishCreateOrUpdateFileV2(ipnftUid: $ipnftUid, uploadToken: $uploadToken, path: $path, ref: $ref, accessLevel: $accessLevel, changeBy: $changeBy, description: $description, tags: $tags, categories: $categories) { datasetId contentHash version newHead isSuccess message error { message code retryable } } }", "variables": {"ipnftUid": "<ipnft_uid>", "uploadToken": "<uploadToken from step 1>", "path": "<filename>", "accessLevel": "PUBLIC", "changeBy": "<wallet_address>", "description": "<file description>"}}
```

## Output

Extract from `data.finishCreateOrUpdateFileV2`:
- `datasetId` — the `did:odf:...` dataset ID (needed for announcement attachments)
- `contentHash`

Save to `uploads/upload_result.json`.
