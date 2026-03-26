---
name: x402-hello
description: Call a paid /hello endpoint on an x402-hello-hono server. USDC on Base Sepolia, uses x402.org facilitator for verification and settlement. For testing x402 payment flow.
homepage: https://comm-attribute-zope-imagine.trycloudflare.com
metadata: {"openclaw":{"emoji":"🔑","requires":{"env":["X402_HELLO_URL","PRIVY_APP_ID","PRIVY_APP_SECRET","PRIVY_WALLET_ID"]}}}
---

# x402 Hello (Test)

Pay-per-call access to a `/hello` endpoint via the [x402 HTTP payment protocol](https://github.com/coinbase/x402).
Uses USDC on Base Sepolia. Verification and settlement via x402.org facilitator.

---

## Environment Variables

| Variable | Required | Description |
|----------|----------|-------------|
| `X402_HELLO_URL` | Yes | Base URL of the x402-hello-hono server (e.g. `http://localhost:4021`) |
| `PRIVY_APP_ID` | Yes | Privy app identifier |
| `PRIVY_APP_SECRET` | Yes | Privy app secret |
| `PRIVY_WALLET_ID` | Yes | Privy wallet ID — wallet must hold USDC on Base Sepolia |

---

## Network Configuration

| Network (CAIP-2) | Chain ID | USDC Contract |
|-----------------|----------|---------------|
| `eip155:84532` | 84532 | `0x036CbD53842c5426634e7929541eC2318f3dCF7e` |

USDC EIP-712 domain on Base Sepolia: `name="USDC"`, `version="2"`

---

## x402 Payment Flow (v2)

The `/hello` endpoint requires payment. Follow these 7 steps exactly.

### Step 1: Send request — get 402 challenge

```
run_command:
  command: curl -sS -i -X GET "$X402_HELLO_URL/hello"
```

Look for the `payment-required` header (base64-encoded) in the 402 response.

### Step 2: Decode payment requirements

Extract the `payment-required` header value from step 1 output, then decode:

```
run_command:
  command: echo '<payment-required value>' | base64 -D
```

Result (x402 v2 format):
```json
{
  "x402Version": 2,
  "resource": {"url": "...", "description": "hello world after payment", "mimeType": "application/json"},
  "accepts": [{
    "scheme": "exact",
    "network": "eip155:84532",
    "amount": "10000",
    "asset": "0x036CbD53842c5426634e7929541eC2318f3dCF7e",
    "payTo": "0x...",
    "maxTimeoutSeconds": 60,
    "extra": {"name": "USDC", "version": "2"}
  }]
}
```

Extract from `accepts[0]`:
- `network` — CAIP-2 chain identifier (e.g. `eip155:84532`)
- `amount` — USDC in smallest unit (6 decimals: `10000` = $0.01)
- `asset` — USDC contract address
- `payTo` — payment recipient address
- `maxTimeoutSeconds` — deadline offset in seconds

Also save the full `resource` object and the entire `accepts[0]` entry — they are needed in step 6.

### Step 3: Get wallet address

```
get_wallet_address
```

Save as `wallet_address`.

### Step 4: Generate nonce, validAfter, and validBefore

Generate all timing values in one command:
```
run_command:
  command: NOW=$(date +%s) && echo "0x$(openssl rand -hex 32)" && echo $(( NOW - 600 )) && echo $(( NOW + 60 ))
```
- Line 1: `nonce` (random bytes32)
- Line 2: `valid_after` (10 minutes before now — matches SDK behavior)
- Line 3: `valid_before` (now + maxTimeoutSeconds from step 2)

### Step 5: Sign EIP-712 TransferWithAuthorization

Extract chain ID from the network string (e.g. `eip155:84532` -> `84532`).

Sign the USDC TransferWithAuthorization via Privy wallet RPC:

**IMPORTANT Privy API field names:**
- Use `primary_type` (snake_case), NOT `primaryType`
- Do NOT include `caip2` — Privy infers the chain from the `chainId` in the domain

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
          "name": "USDC",
          "version": "2",
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

Construct the x402 **v2** payment payload. This MUST include `resource` and `accepted` fields from step 2.

**IMPORTANT:** All fields in `authorization` MUST be **strings** (matching the `@x402/core` SDK). The `accepted` field must be the exact `accepts[0]` object from step 2 (deep equality is checked by the server).

```json
{
  "x402Version": 2,
  "resource": <resource object from step 2>,
  "accepted": <the exact accepts[0] entry from step 2>,
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

Build and encode in one command. Use `PAYMENT-SIGNATURE` as the header name (x402 v2 standard):
```
run_command:
  command: PAYMENT=$(printf '%s' '<payment JSON with no whitespace>' | base64 | tr -d '\n') && curl -sS -i -X GET "$X402_HELLO_URL/hello" -H "PAYMENT-SIGNATURE: $PAYMENT"
```

### Step 7: Verify result

Expected response (HTTP 200):
```json
{"ok":true,"message":"hello world"}
```

The `payment-response` header (base64) contains settlement details:
```json
{
  "success": true,
  "payer": "0x...",
  "transaction": "0x...",
  "network": "eip155:84532"
}
```

---

## Error Handling

| Response | Meaning | Action |
|----------|---------|--------|
| 402 (no payment sent) | Expected first response | Decode `payment-required` header, sign, retry |
| 402 with `invalid_exact_evm_payload_signature` | Wrong EIP-712 domain or field types | Verify domain name matches `"USDC"`, all authorization fields are strings |
| 402 with `Facilitator verify failed` | Payload format rejected by facilitator | Verify `accepted` is exact copy of `accepts[0]`, `x402Version` is 2, authorization fields are strings |
| 402 (payment sent, other error) | Payment verification failed | Check USDC balance, signature correctness, nonce freshness |
| 200 with `ok: true` | Success | Payment settled, decode `payment-response` header for tx hash |

---

## Guardrails

- Never hardcode wallet secrets in commands — use environment variables (`$PRIVY_APP_ID`, etc.)
- Verify the wallet has sufficient USDC balance on Base Sepolia before starting payment flow
- The `amount` in 402 response is in USDC smallest unit (6 decimals): `10000` = $0.01)
- The agent never handles wallet private keys directly — all signing goes through Privy wallet RPC
