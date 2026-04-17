---
name: molecule-x402
description: Execute paid Molecule Labs mutations via x402 payment protocol. USDC on Base, no API key needed. Supports project creation, file uploads, announcements, and ownership management.
env_vars:
  - X402_GATEWAY_URL
  - PRIVY_APP_ID
  - PRIVY_APP_SECRET
  - PRIVY_WALLET_ID
---

# Molecule x402

Pay-per-call access to Molecule Labs write mutations via the [x402 HTTP payment protocol](https://github.com/coinbase/x402).
Uses USDC on Base -- no Molecule API key or service token required.

---

## When to Use

Use this skill when you do NOT have `MOLECULE_API_KEY`. Each mutation costs USDC.
If you have `MOLECULE_API_KEY`, use the standard molecule skills instead (molecule-auth, molecule-project, molecule-upload, molecule-announcement) -- they are free and simpler.

---

## Environment Variables

| Variable | Required | Description |
|----------|----------|-------------|
| `X402_GATEWAY_URL` | Yes | x402 gateway base URL |
| `PRIVY_APP_ID` | Yes | Privy app identifier |
| `PRIVY_APP_SECRET` | Yes | Privy app secret |
| `PRIVY_WALLET_ID` | Yes | Privy wallet ID -- wallet must hold USDC on the target network |

---

## Network Configuration

| Network (CAIP-2) | Chain ID | USDC Contract |
|-----------------|----------|---------------|
| `eip155:84532` | 84532 | `0x036CbD53842c5426634e7929541eC2318f3dCF7e` |

USDC EIP-712 domain: `name="USDC"`, `version="2"`

---

## Supported Mutations

| Mutation | Description |
|----------|-------------|
| `createProject` | Create molecule project for IP-NFT |
| `initiateCreateOrUpdateFileV2` | Start file upload (get presigned URL) |
| `finishCreateOrUpdateFileV2` | Finalize file upload |
| `createAnnouncementV2` | Post project announcement |
| `updateFileMetadataV2` | Update file metadata |
| `generateServiceToken` | Generate long-lived service token |
| `addProjectOwner` | Add project co-owner |

Prices are set server-side per mutation (default ~1.00 USDC). The exact price is returned in the 402 response.

---

## x402 Payment Flow

Every mutation follows this 7-step flow. Substitute the mutation name, GraphQL query, and variables per mutation (see Mutation Reference below).

### Step 1: Send request -- get 402 challenge

```
run_command:
  command: curl -sS -i -X POST "$X402_GATEWAY_URL/x402/labs/<mutation_name>" -H "Content-Type: application/json" -d '<JSON body with query and variables>'
```

The `-i` flag includes response headers in the output. Look for the `payment-required` header (base64-encoded).

Example output:
```
HTTP/2 402
content-type: application/json
payment-required: eyJ4NDAyVmVyc2lvbiI6Miwi...

{"isSuccess":false,"message":"Payment required"}
```

### Step 2: Decode payment requirements

Extract the `payment-required` header value from step 1 output, then decode:

```
run_command:
  command: echo '<payment-required value>' | base64 -D
```

Result:
```json
{
  "x402Version": 2,
  "error": "Payment required",
  "resource": {
    "url": "https://...",
    "description": "x402 payment for createProject",
    "mimeType": ""
  },
  "accepts": [{
    "scheme": "exact",
    "network": "eip155:84532",
    "amount": "1000000",
    "asset": "0x036CbD53842c5426634e7929541eC2318f3dCF7e",
    "payTo": "0x...",
    "maxTimeoutSeconds": 60,
    "extra": {"name": "USDC", "version": "2", "assetSymbol": "USDC"}
  }]
}
```

Save the entire `accepts[0]` object as `accepted` and the `resource` object as `resource` -- both are needed in step 6.

Extract from `accepts[0]`:
- `network` -- CAIP-2 chain identifier (e.g. `eip155:84532`)
- `amount` -- USDC in smallest unit (6 decimals: `1000000` = $1.00)
- `asset` -- USDC contract address
- `payTo` -- payment recipient address
- `maxTimeoutSeconds` -- deadline offset in seconds
- `extra.name` -- EIP-712 domain name (e.g. `"USDC"`)
- `extra.version` -- EIP-712 domain version (e.g. `"2"`)

### Step 3: Get wallet address

```
get_wallet_address
```

Save as `wallet_address`.

### Step 4: Generate nonce, validAfter, and validBefore

Generate all timing values in one command:
```
run_command:
  command: NOW=$(date +%s) && echo "0x$(openssl rand -hex 32)" && echo $(( NOW - 600 )) && echo $(( NOW + <maxTimeoutSeconds> ))
```
- Line 1: `nonce` (random bytes32)
- Line 2: `valid_after` (10 minutes before now -- matches SDK behavior)
- Line 3: `valid_before` (now + maxTimeoutSeconds from step 2)

### Step 5: Sign EIP-712 TransferWithAuthorization

Extract chain ID from the network string (e.g. `eip155:84532` -> `84532`).

Sign the USDC TransferWithAuthorization via Privy wallet RPC:

**IMPORTANT Privy API field names:**
- Use `primary_type` (snake_case), NOT `primaryType`
- Do NOT include `caip2` -- Privy infers the chain from the `chainId` in the domain

```
http_request:
  url: https://api.privy.io/v1/wallets/$PRIVY_WALLET_ID/rpc
  method: POST
  auth_basic_user_env: PRIVY_APP_ID
  auth_basic_pass_env: PRIVY_APP_SECRET
  headers: {"privy-app-id": "$PRIVY_APP_ID", "Content-Type": "application/json"}
  body: {
    "method": "eth_signTypedData_v4",
    "params": {
      "typed_data": {
        "types": {
          "EIP712Domain": [
            {"name": "name", "type": "string"},
            {"name": "version", "type": "string"},
            {"name": "chainId", "type": "uint256"},
            {"name": "verifyingContract", "type": "address"}
          ],
          "TransferWithAuthorization": [
            {"name": "from", "type": "address"},
            {"name": "to", "type": "address"},
            {"name": "value", "type": "uint256"},
            {"name": "validAfter", "type": "uint256"},
            {"name": "validBefore", "type": "uint256"},
            {"name": "nonce", "type": "bytes32"}
          ]
        },
        "primary_type": "TransferWithAuthorization",
        "domain": {
          "name": "<extra.name from step 2>",
          "version": "<extra.version from step 2>",
          "chainId": <chain_id>,
          "verifyingContract": "<asset>"
        },
        "message": {
          "from": "<wallet_address>",
          "to": "<payTo>",
          "value": "<amount>",
          "validAfter": "<valid_after>",
          "validBefore": "<valid_before>",
          "nonce": "<nonce>"
        }
      }
    }
  }
  return_body: true
```

Extract `data.signature` from the response.

### Step 6: Build and encode payment header

Construct the v2 payment payload JSON.

**IMPORTANT:**
- All fields in `authorization` MUST be **strings** -- decimal for numeric values, 0x-prefixed hex for nonce.
- The `accepted` field MUST be the full `accepts[0]` object from step 2 (including `scheme`, `network`, `amount`, `asset`, `payTo`, `maxTimeoutSeconds`, `extra`).
- The `resource` field MUST be the `resource` object from step 2.

```json
{
  "x402Version": 2,
  "resource": {
    "url": "<resource.url from step 2>",
    "description": "<resource.description from step 2>",
    "mimeType": "<resource.mimeType from step 2>"
  },
  "accepted": {
    "scheme": "exact",
    "network": "<network>",
    "amount": "<amount>",
    "asset": "<asset>",
    "payTo": "<payTo>",
    "maxTimeoutSeconds": <maxTimeoutSeconds>,
    "extra": {"name": "<extra.name>", "version": "<extra.version>"}
  },
  "payload": {
    "signature": "<signature>",
    "authorization": {
      "from": "<wallet_address>",
      "to": "<payTo>",
      "value": "<amount>",
      "validAfter": "<valid_after>",
      "validBefore": "<valid_before>",
      "nonce": "<nonce>"
    }
  }
}
```

Base64 encode (single line, no wrapping):
```
run_command:
  command: printf '%s' '<payment JSON with no whitespace>' | base64 | tr -d '\n'
```

Save as `payment_header`.

### Step 7: Retry with payment -- get result

**CRITICAL: The header MUST be `PAYMENT-SIGNATURE`. Do NOT use `X-PAYMENT` or `Payment` -- the x402 server only reads `PAYMENT-SIGNATURE`.**

```
run_command:
  command: curl -sS -X POST "$X402_GATEWAY_URL/x402/labs/<mutation_name>" -H "Content-Type: application/json" -H "PAYMENT-SIGNATURE: <payment_header>" -d '<same JSON body as step 1>'
```

Response: HTTP 200 with standard Molecule GraphQL response body.

---

## Mutation Reference

### createProject

**URL path:** `/x402/labs/createProject`

**Body:**
```json
{
  "query": "mutation CreateProject($input: CreateProjectInput!) { createProject(input: $input) { isSuccess message error { message code retryable } project { ipnftUid ipnftSymbol ipnftAddress ipnftTokenId } } }",
  "variables": {
    "input": {
      "ipnftSymbol": "<symbol>",
      "ipnftTokenId": "<token_id as decimal string>"
    }
  }
}
```

**Response:** `data.createProject.project` contains `ipnftUid`, `ipnftSymbol`, `ipnftAddress`, `ipnftTokenId`.

---

### File Upload (3-step workflow)

File upload requires TWO paid x402 mutations plus one direct S3 call.

**IMPORTANT:** Wait 30 seconds after `createProject` before starting upload -- data room provisioning is async.

**Step A -- Initiate upload** (x402 paid)

URL path: `/x402/labs/initiateCreateOrUpdateFileV2`

Body:
```json
{
  "query": "mutation InitiateCreateOrUpdateFileV2($ipnftUid: String!, $contentType: String!, $contentLength: Int!) { initiateCreateOrUpdateFileV2(ipnftUid: $ipnftUid, contentType: $contentType, contentLength: $contentLength) { uploadToken uploadUrl uploadUrlExpiry method headers { key value } useMultipart isSuccess error { message code retryable } } }",
  "variables": {
    "ipnftUid": "<ipnft_uid>",
    "contentType": "application/pdf",
    "contentLength": <file_size_in_bytes>
  }
}
```

Response: `data.initiateCreateOrUpdateFileV2` contains `uploadToken`, `uploadUrl`, `method`, `headers`.

**Step B -- Upload to S3** (direct, NO x402 payment)

Use the exact `uploadUrl` and `headers` from step A:

```
http_request:
  url: <uploadUrl>
  method: <method from step A, usually PUT>
  headers: {<all key:value pairs from step A headers>, "Content-Type": "application/pdf"}
  file_path: <path-to-file>
```

**Step C -- Finalize upload** (x402 paid)

URL path: `/x402/labs/finishCreateOrUpdateFileV2`

Body:
```json
{
  "query": "mutation FinishCreateOrUpdateFileV2($ipnftUid: String!, $uploadToken: String!, $path: String, $accessLevel: String!, $changeBy: String!, $description: String, $tags: [String!], $categories: [String!]) { finishCreateOrUpdateFileV2(ipnftUid: $ipnftUid, uploadToken: $uploadToken, path: $path, accessLevel: $accessLevel, changeBy: $changeBy, description: $description, tags: $tags, categories: $categories) { datasetId contentHash version newHead isSuccess message error { message code retryable } } }",
  "variables": {
    "ipnftUid": "<ipnft_uid>",
    "uploadToken": "<from step A>",
    "path": "<filename>",
    "accessLevel": "PUBLIC",
    "changeBy": "<wallet_address>",
    "description": "<file description>"
  }
}
```

Response: `data.finishCreateOrUpdateFileV2` contains `datasetId` (format: `did:odf:...`), `contentHash`.

---

### createAnnouncementV2

**URL path:** `/x402/labs/createAnnouncementV2`

**Body:**
```json
{
  "query": "mutation CreateAnnouncementV2($ipnftUid: String!, $headline: String!, $body: String!, $attachments: [String!]) { createAnnouncementV2(ipnftUid: $ipnftUid, headline: $headline, body: $body, attachments: $attachments) { isSuccess message error { message code retryable } } }",
  "variables": {
    "ipnftUid": "<ipnft_uid>",
    "headline": "<headline>",
    "body": "<markdown body>",
    "attachments": ["<datasetId from upload>"]
  }
}
```

---

### addProjectOwner

**URL path:** `/x402/labs/addProjectOwner`

**Body:**
```json
{
  "query": "mutation AddProjectOwner($ipnftUid: String!, $walletAddress: String!) { addProjectOwner(ipnftUid: $ipnftUid, walletAddress: $walletAddress) { isSuccess message error { message code retryable } } }",
  "variables": {
    "ipnftUid": "<ipnft_uid>",
    "walletAddress": "<new_owner_address>"
  }
}
```

---

### Other Mutations

These follow the same x402 payment flow. GraphQL schemas available via Molecule API introspection.

- `generateServiceToken` -- Generate service token (requires prior wallet signature via `getServiceSignInMessage` query)

---

## Error Handling

| Response | Meaning | Action |
|----------|---------|--------|
| 402 (no payment sent) | Expected first response | Decode `payment-required` header, sign, retry |
| 402 with `invalid_exact_evm_payload_signature` | Wrong EIP-712 domain or field types | Verify domain name matches `extra.name` from 402 response, all authorization fields are strings |
| 402 `"Payment verification failed"` | Payload format wrong or signature invalid | Verify v2 format: `accepted` field present with full payment requirements, `resource` field present, header is `PAYMENT-SIGNATURE` (not `X-PAYMENT`) |
| 402 (payment sent, other error) | Payment verification failed | Check USDC balance, signature correctness, nonce freshness |
| 400 `"Mutation not enabled"` | Not in x402 whitelist | Verify mutation name spelling |
| 400 `"Missing query"` | Body format wrong | Include `query` field with GraphQL mutation string |
| 400 `"Path mutation does not match"` | URL/query mismatch | URL path mutation name must match the GraphQL top-level field |
| 500 | Server error | Retry once |

---

## Guardrails

- Never hardcode wallet secrets in commands -- use environment variables (`$PRIVY_APP_ID`, etc.)
- Verify the wallet has sufficient USDC balance before starting payment flow
- Always validate mutation name is in the supported list before calling
- S3 upload (step B of file upload) is direct -- do NOT send x402 payment for S3 PUT
- The `amount` in 402 response is in USDC smallest unit (6 decimals): `1000000` = $1.00
- Escape user-supplied values in JSON before embedding in curl `-d` arguments -- replace `\` with `\\`, `"` with `\"`, and newlines with `\n`
- Settlement only occurs after successful mutation execution -- no pay-and-fail scenario
- Do not modify or fabricate GraphQL response data -- return results faithfully
- The agent never handles wallet private keys directly -- all signing goes through Privy wallet RPC
