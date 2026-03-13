# DeSci Guide: IP-NFT Minting with Tengu + Aura-Orchestrator

End-to-end guide for minting IP-NFTs on Molecule DeSci Labs using Tengu agents with Privy agentic wallets.

---

## Prerequisites

| Item | Where to get it |
|------|----------------|
| **OpenRouter API key** (or Anthropic/OpenAI) | https://openrouter.ai |
| **Telegram bot token** | @BotFather on Telegram |
| **Molecule API key** | Molecule DeSci Labs team |
| **POI API key** | Molecule DeSci Labs team |
| **Privy app ID + secret** | https://dashboard.privy.io |
| **Sepolia ETH** (~0.01) | Sepolia faucet |

---

## Step 1: Build Tengu

```bash
git clone https://github.com/user/tengu-cluster.git
cd tengu-cluster
cargo build --features telegram
```

## Step 2: Store Secrets

Use the encrypted vault — secrets are AES-256-GCM encrypted at `~/.tengu/secrets.vault`:

```bash
cargo run -- secret init
cargo run -- secret set OPENROUTER_API_KEY sk-or-...
cargo run -- secret set TELEGRAM_BOT_TOKEN 123456:ABC-DEF...
cargo run -- secret set MOLECULE_API_KEY mol-...
cargo run -- secret set POI_API_KEY poi-...
cargo run -- secret set PRIVY_APP_ID clz...
cargo run -- secret set PRIVY_APP_SECRET your-secret
```

Set `TENGU_MASTER_PASSWORD` env var to skip the interactive password prompt on startup.

## Step 3: Create a Privy Wallet

Create an agentic wallet with a safety policy. You can do this from the command line:

```bash
# Create a policy (Sepolia only, max 0.01 ETH per tx)
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

Save the `id` from the response, then create the wallet:

```bash
curl -X POST "https://api.privy.io/v1/wallets" \
  --user "$PRIVY_APP_ID:$PRIVY_APP_SECRET" \
  -H "privy-app-id: $PRIVY_APP_ID" \
  -H "Content-Type: application/json" \
  -d '{
    "chain_type": "ethereum",
    "policy_ids": ["<policy_id>"]
  }'
```

Save the `id` as your wallet ID and `address` as the wallet address:

```bash
cargo run -- secret set PRIVY_WALLET_ID <wallet_id>
```

## Step 4: Fund the Wallet

Send at least 0.01 Sepolia ETH to your wallet address. You'll need:
- ~0.001 ETH for the POI on-chain submission + gas
- 0.001 ETH for the mint fee + gas

Get Sepolia ETH from a faucet (e.g., Alchemy Sepolia faucet, Google Cloud faucet).

## Step 5: Configure Tengu

Create or edit your config file (`~/.tengu/config.toml`):

```toml
[agents.main]
default = true
engine = "openrouter"
model = "nvidia/nemotron-3-super-120b-a12b:free"
skills = ["aura-orchestrator", "privy"]

[telegram]
enabled = true
allowed_users = ["YOUR_TELEGRAM_USER_ID"]
```

To get your Telegram user ID, message @userinfobot on Telegram.

Add to your `.env` in the project root:

```bash
MOLECULE_LABS_URL=https://staging.graphql.api.molecule.xyz/graphql
MOLECULE_CLIENT_URL=https://testnet.molecule.xyz
```

Alternatively, use the pre-built DeSci sandbox with a multi-agent team:

```bash
cargo run -- telegram --sandbox desci
```

The DeSci sandbox uses per-workspace memory isolation — memories from DeSci runs do not pollute other sandbox recall results. When the orchestrator completes a multi-agent task, it auto-summarizes results into a topic overview stored in memory, so future runs can recall prior work context automatically.

## Step 6: Start the Bot

```bash
cargo run -- telegram
```

## Step 7: Mint an IP-NFT

Attach a PDF (for POI) and a cover image (PNG/JPG) to your Telegram message and provide **all required fields**:

```
Mint an IP-NFT with:
- Name: "Novel Approach to Protein Folding"
- Description: "Computational method for predicting membrane protein structures"
- Symbol: PROT1
- Organization: "DeSci Research Lab"
- Lead: Jane Doe, jane@example.com
- Topic: Computational Biology
```

**All 7 fields are required.** If any are missing, the agent will ask you to provide them before proceeding. It will not fabricate values like organization names or email addresses.

The agent follows the aura-orchestrator workflow automatically:

1. **Resolves wallet address** from Privy
2. **Registers POI** — uploads your PDF to the Molecule proof-of-invention API
3. **Submits POI on-chain** — sends transaction via Privy wallet (automatic, no approval needed)
4. **Generates assignment agreement** — via Molecule GraphQL
5. **Uploads cover image and metadata** — via Molecule GraphQL
6. **Signs terms** — signs the terms message via Privy `personal_sign` (automatic)
7. **Gets authorization** — via Molecule GraphQL
8. **Mints the IP-NFT** — sends mint transaction via Privy wallet (0.001 ETH, automatic)
9. **Returns the project URL** — e.g. `https://testnet.molecule.xyz/ipnfts/42`

All on-chain operations are autonomous — no manual approval needed. The Privy policy ensures spending limits are enforced.

## After Minting

Once your IP-NFT is minted, you can continue with additional workflows. The agent will automatically obtain a Molecule service token (via wallet signature) for these operations:

**Create a project data room:**
```
Create a project data room for IP-NFT with reservation ID <id>
```

**Upload research files:**
```
Upload the attached PDF to the project data room
```

**Publish an announcement:**
```
Create an announcement: "Phase 1 results published" with a summary of the key findings
```

---

## Using Without Telegram (Claude Code, OpenClaw)

You don't need Tengu or Telegram to use the aura-orchestrator skill. The Privy wallet works from any platform.

### 1. Set up env vars

```bash
export PRIVY_APP_ID=clz...
export PRIVY_APP_SECRET=your-secret
export PRIVY_WALLET_ID=your-wallet-id
export MOLECULE_API_KEY=mol-...
export POI_API_KEY=poi-...
export MOLECULE_LABS_URL=https://staging.graphql.api.molecule.xyz/graphql
export MOLECULE_CLIENT_URL=https://testnet.molecule.xyz
```

### 2. Use the skills

Paste the contents of `skills/aura-orchestrator/SKILL.md` and `skills/privy/SKILL.md` into your Claude Code session or OpenClaw workspace. The skill instructions use Privy API calls directly — no relay or wallet page needed.

---

## Troubleshooting

| Problem | Solution |
|---------|----------|
| "PRIVY_APP_ID not set" | Store it via `cargo run -- secret set PRIVY_APP_ID ...` |
| "PRIVY_WALLET_ID not set" | Create a wallet (Step 3), then store the ID |
| `POLICY_VIOLATION` from Privy | Transaction exceeds policy limits. Check spending limits and chain restrictions. |
| `INSUFFICIENT_FUNDS` from Privy | Fund the wallet with more Sepolia ETH (Step 4) |
| Agent says `MOLECULE_API_KEY` not set | Store it via `cargo run -- secret set MOLECULE_API_KEY ...` |
| Agent says `POI_API_KEY` not set | Store it via `cargo run -- secret set POI_API_KEY ...` |
| `cast` command not found (for ABI encoding) | Install Foundry: `curl -L https://foundry.paradigm.xyz \| bash && foundryup` |
| Mint transaction reverts | Check you have enough Sepolia ETH (0.001 + gas) |
| `SERVICE_AUTH_FAILED` | Service token expired. Agent will re-acquire automatically. |
| `401 Unauthorized` from Privy | Check PRIVY_APP_ID and PRIVY_APP_SECRET are correct |

---

## Reference

| Resource | Link |
|----------|------|
| Privy agentic wallets skill | [skills/privy/SKILL.md](../skills/privy/SKILL.md) |
| Aura-orchestrator skill reference | [skills/aura-orchestrator/SKILL.md](../skills/aura-orchestrator/SKILL.md) |
| Full configuration reference | [docs/CONFIGURATION.md](CONFIGURATION.md) |
| DeSci sandbox (multi-agent team) | [sandboxes/desci/config.toml](../sandboxes/desci/config.toml) |
| Privy dashboard | https://dashboard.privy.io |
| Molecule DeSci Labs | https://testnet.molecule.xyz |
