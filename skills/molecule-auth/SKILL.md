---
name: molecule-auth
description: Authenticate with Molecule GraphQL API to obtain a service token.
homepage: https://staging.graphql.api.molecule.xyz/graphql
---

# Molecule Authentication

Acquire a service token for Molecule GraphQL API. Required before creating projects, uploading files, or creating announcements.

## Required Environment Variables

| Variable | Description |
|----------|-------------|
| `MOLECULE_API_KEY` | Sent as `x-api-key` header |
| `MOLECULE_LABS_URL` | GraphQL endpoint URL |


## Step 1: Get wallet address

```
get_wallet_address
```

## Step 2: Get sign-in message

```
http_request:
  url: $MOLECULE_LABS_URL
  method: POST
  headers: {"x-api-key": "$MOLECULE_API_KEY", "Content-Type": "application/json"}
  body: {"query": "query GetServiceSignInMessage($walletAddress: String!, $serviceName: String!) { getServiceSignInMessage(walletAddress: $walletAddress, serviceName: $serviceName) { message } }", "variables": {"walletAddress": "<wallet_address from step 1>", "serviceName": "tengu-agent"}}
  return_body: true
```

## Step 3: Sign the message

```
sign_message:
  message: <message string from step 2 response>
```

## Step 4: Exchange for service token

```
http_request:
  url: $MOLECULE_LABS_URL
  method: POST
  headers: {"x-api-key": "$MOLECULE_API_KEY", "Content-Type": "application/json"}
  body: {"query": "mutation GenerateServiceToken($serviceName: String!, $expiresIn: String!, $walletAddress: String!, $messageSignature: String!) { generateServiceToken(serviceName: $serviceName, expiresIn: $expiresIn, walletAddress: $walletAddress, messageSignature: $messageSignature) { token } }", "variables": {"serviceName": "tengu-agent", "expiresIn": "720h", "walletAddress": "<address from step 1>", "messageSignature": "<signature from step 3>"}}
  return_body: true
```

## Output

The response contains: `data.generateServiceToken.token` — this is the service token string.

Save to `uploads/service_token.txt` for reference.
Use it as `x-service-token` header in all subsequent Molecule GraphQL calls within this agent's workflow.
