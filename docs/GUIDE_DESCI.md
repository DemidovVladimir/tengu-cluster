# DeSci Guide: IP-NFT Minting with Tengu + Aura-Orchestrator

End-to-end guide for minting IP-NFTs on Molecule DeSci Labs using Tengu agents.

---

## Prerequisites

| Item | Where to get it |
|------|----------------|
| **OpenRouter API key** (or Anthropic/OpenAI) | https://openrouter.ai |
| **Telegram bot token** | @BotFather on Telegram |
| **Molecule API key** | Molecule DeSci Labs team |
| **Molecule service token** | Molecule DeSci Labs team (JWT for Workflows 2-4) |
| **POI API key** | Molecule DeSci Labs team |
| **EVM private key** | Wallet with Sepolia ETH for on-chain signing |
| **Sepolia RPC URL** | Alchemy, Infura, or public endpoint |
| **Sepolia ETH** (~0.01) | Sepolia faucet |

---

## Step 1: Build Tengu and Tools

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
cargo run -- secret set MOLECULE_SERVICE_TOKEN jwt-...
cargo run -- secret set POI_API_KEY poi-...
cargo run -- secret set EVM_PRIVATE_KEY 0xabcdef...
cargo run -- secret set EVM_RPC_URL https://rpc.sepolia.org
```

Set `TENGU_MASTER_PASSWORD` env var to skip the interactive password prompt on startup.

## Step 3: Fund the Wallet

Your EVM wallet needs at least 0.01 Sepolia ETH:
- ~0.001 ETH for the POI on-chain submission + gas
- 0.001 ETH for the mint fee + gas

Get Sepolia ETH from a faucet (e.g., Alchemy Sepolia faucet, Google Cloud faucet).

## Step 4: Configure Tengu

Create or edit your config file (`~/.tengu/config.toml`):

```toml
[agents.main]
default = true
engine = "openrouter"
model = "nvidia/nemotron-3-super-120b-a12b:free"
skill_packages = ["aura-orchestrator"]

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

## Step 5: Start the Bot

```bash
cargo run -- telegram
```

## Step 6: Mint an IP-NFT

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

1. **Registers POI** — uploads your PDF to the Molecule proof-of-invention API via `poi_register_document`, which returns POI transaction data and a merkle root
2. **Mints the IP-NFT** — the `mint_ipnft` native tool submits the POI on-chain via alloy (using `EVM_PRIVATE_KEY`), then handles the full Molecule GraphQL flow: reservation, assignment agreement, image upload, metadata, terms signing, signoff, and the final mint transaction. Outputs structured JSON with `reservation_id`, `token_id`, `mint_tx`, `metadata_cid`, and `project_url`

All output values come from the native tool results — the agent saves them exactly as returned, never fabricating data.

## After Minting

Once your IP-NFT is minted, you can continue with additional workflows (requires `MOLECULE_SERVICE_TOKEN`):

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
export MOLECULE_API_KEY=mol-...
export MOLECULE_SERVICE_TOKEN=jwt-...
export POI_API_KEY=poi-...
export EVM_PRIVATE_KEY=0xabcdef...
export EVM_RPC_URL=https://rpc.sepolia.org
export MOLECULE_LABS_URL=https://staging.graphql.api.molecule.xyz/graphql
export MOLECULE_CLIENT_URL=https://testnet.molecule.xyz
```

### 2. Use the skills

Paste the contents of `skills/aura-orchestrator/SKILL.md` into your Claude Code session or OpenClaw workspace. The skill provides workflow reference for the native DeSci tools.

---

## Troubleshooting

| Problem | Solution |
|---------|----------|
| Agent says `EVM_PRIVATE_KEY` not set | Store it via `cargo run -- secret set EVM_PRIVATE_KEY 0x...` |
| Agent says `EVM_RPC_URL` not set | Store it via `cargo run -- secret set EVM_RPC_URL https://rpc.sepolia.org` |
| Agent says `MOLECULE_API_KEY` not set | Store it via `cargo run -- secret set MOLECULE_API_KEY ...` |
| Agent says `POI_API_KEY` not set | Store it via `cargo run -- secret set POI_API_KEY ...` |
| Mint transaction reverts | Check you have enough Sepolia ETH (0.001 + gas) |
| `SERVICE_AUTH_FAILED` | Service token expired. Get a new one and update `MOLECULE_SERVICE_TOKEN`. |
| Agent fabricates values | Stale state? Run `tengu prune --sandbox desci --yes` |
| Stale conversation/memory from prior runs | Run `tengu prune --sandbox desci --yes` to reset all ephemeral state |

---

## Cleaning Up State

After debugging or failed runs, use `tengu prune` to wipe all cached/ephemeral state (conversations, memory, task outcomes, attachments, logs) while preserving config and secrets:

```bash
# Preview what will be removed
tengu prune --sandbox desci

# Skip confirmation
tengu prune --sandbox desci --yes
```

In Telegram, use `/purge` to clear conversation state, persistent memory, and workspace artifacts.

---

## Reference

| Resource | Link |
|----------|------|
| Aura-orchestrator skill reference | [skills/aura-orchestrator/SKILL.md](../skills/aura-orchestrator/SKILL.md) |
| Full configuration reference | [docs/CONFIGURATION.md](CONFIGURATION.md) |
| DeSci sandbox (multi-agent team) | [sandboxes/desci/config.toml](../sandboxes/desci/config.toml) |
| Molecule DeSci Labs | https://testnet.molecule.xyz |
