# Wallet & On-Chain Signing

Tengu supports two on-chain signing methods:

- **Direct signing** — `EVM_PRIVATE_KEY` via alloy (used by the DeSci minting pipeline's native tools)
- **Privy agentic wallets** — server-side wallet controlled by the agent with policy-based guardrails (used by the `/wallet` command and available for custom workflows)

## Architecture

```
┌──────────────┐     ┌──────────────────┐
│  Tengu Agent │────▶│  Privy API       │
│  (skills)    │     │  api.privy.io    │
│              │◀────│                  │
└──────────────┘     └──────────────────┘
                            │
                     ┌──────┴──────┐
                     │  Blockchain │
                     │  (Sepolia)  │
                     └─────────────┘
```

- **Privy** — manages wallet keys server-side. The agent authenticates via `PRIVY_APP_ID` + `PRIVY_APP_SECRET` and calls the Privy RPC API to sign messages and send transactions.
- **Policy engine** — enforces spending limits, chain restrictions, and contract allowlists before any transaction is signed.
- **No browser wallet needed** — the agent signs autonomously. No MetaMask, no wallet page, no relay for signing.

## Setup

### 1. Create a Privy account

Go to [dashboard.privy.io](https://dashboard.privy.io) and create an app. Get your **App ID** and **App Secret** from Configuration > App settings > Basics.

### 2. Store credentials

```bash
cargo run -- secret set PRIVY_APP_ID clz...
cargo run -- secret set PRIVY_APP_SECRET your-secret
```

### 3. Create a policy

Always create a policy before creating a wallet. Policies prevent the agent from spending beyond limits.

```bash
curl -X POST "https://api.privy.io/v1/policies" \
  --user "$PRIVY_APP_ID:$PRIVY_APP_SECRET" \
  -H "privy-app-id: $PRIVY_APP_ID" \
  -H "Content-Type: application/json" \
  -d '{
    "version": "1.0",
    "name": "DeSci agent policy",
    "chain_type": "ethereum",
    "rules": [
      {
        "name": "Max 0.01 ETH per tx",
        "method": "eth_sendTransaction",
        "conditions": [{
          "field_source": "ethereum_transaction",
          "field": "value",
          "operator": "lte",
          "value": "10000000000000000"
        }],
        "action": "ALLOW"
      },
      {
        "name": "Sepolia only",
        "method": "eth_sendTransaction",
        "conditions": [{
          "field_source": "ethereum_transaction",
          "field": "chain_id",
          "operator": "eq",
          "value": "11155111"
        }],
        "action": "ALLOW"
      }
    ]
  }'
```

### 4. Create a wallet

```bash
curl -X POST "https://api.privy.io/v1/wallets" \
  --user "$PRIVY_APP_ID:$PRIVY_APP_SECRET" \
  -H "privy-app-id: $PRIVY_APP_ID" \
  -H "Content-Type: application/json" \
  -d '{"chain_type": "ethereum", "policy_ids": ["<policy_id>"]}'
```

Store the wallet ID:

```bash
cargo run -- secret set PRIVY_WALLET_ID <wallet_id>
```

### 5. Fund the wallet

Send Sepolia ETH to the wallet address (from the response above). For IP-NFT minting, you need ~0.01 ETH (mint fee + gas).

## How Signing Works

When a skill (e.g. aura-orchestrator) needs to sign a message or send a transaction, it calls the Privy RPC API:

**Send transaction:**
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
        "to": "0xContractAddress",
        "data": "0xCalldata",
        "value": "0"
      }
    }
  }'
```

**Sign message (personal_sign):**
```bash
curl -s -X POST "https://api.privy.io/v1/wallets/$PRIVY_WALLET_ID/rpc" \
  --user "$PRIVY_APP_ID:$PRIVY_APP_SECRET" \
  -H "privy-app-id: $PRIVY_APP_ID" \
  -H "Content-Type: application/json" \
  -d '{
    "method": "personal_sign",
    "params": {
      "message": "0x<hex-encoded-message>"
    }
  }'
```

The message must be hex-encoded. Convert text to hex:
```bash
echo -n "Message text" | xxd -p | tr -d '\n' | sed 's/^/0x/'
```

## Security

- **Policy enforcement** — Privy validates every transaction against the wallet's policy before signing. Transactions that exceed limits or target unauthorized chains/contracts are rejected.
- **Server-side key management** — wallet keys are managed by Privy's infrastructure. The agent never has access to raw private keys.
- **Credential isolation** — `PRIVY_APP_SECRET` is stored in the encrypted secrets vault. The agent uses it for API auth but never exposes it.
- **Spending limits** — configure per-transaction limits, daily limits, chain restrictions, and contract allowlists via policies.

## Using Without Tengu (Claude Code, OpenClaw, etc.)

The Privy skill works on any platform. Set the environment variables and paste the skill instructions into your agent:

```bash
export PRIVY_APP_ID=clz...
export PRIVY_APP_SECRET=your-secret
export PRIVY_WALLET_ID=your-wallet-id
```

Use `skills/aura-orchestrator/SKILL.md` for DeSci-specific workflows. The wallet API calls can be made via curl with the Privy credentials.

## Troubleshooting

| Problem | Solution |
|---------|----------|
| `401 Unauthorized` | Check PRIVY_APP_ID and PRIVY_APP_SECRET are correct |
| `POLICY_VIOLATION` | Transaction exceeds policy limits. Adjust policy or reduce amount. |
| `INSUFFICIENT_FUNDS` | Fund the wallet with more ETH |
| Wallet not found | Check PRIVY_WALLET_ID is correct. Run `GET /v1/wallets` to list wallets. |
| Wrong chain | Ensure `caip2` matches the policy's allowed chain_id |
