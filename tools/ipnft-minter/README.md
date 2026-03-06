# ipnft-minter

Standalone CLI tool for minting IP-NFTs on Molecule DeSci Labs (Sepolia testnet).

This is **not** an MCP server — it is a plain CLI binary that Tengu agents invoke via `run_command`. It lives outside the main workspace in `tools/`.

## What It Does

Executes the full 9-step IP-NFT minting flow:

1. **Reserve** — call `reserve()` on the IPNFT contract to get a reservation ID
2. **Create agreement** — register POI assignment via Molecule GraphQL
3. **Upload image** — upload cover image (or a default 1x1 transparent PNG)
4. **Claim** — associate the reservation with the agreement
5. **Upload metadata** — create and upload IPNFT metadata JSON to IPFS
6. **Fetch terms** — get the terms-of-service text from Molecule
7. **Sign terms** — produce an EIP-191 personal_sign signature
8. **Authorize** — post the signature to get an authorization token
9. **Mint** — call `mintReservation()` on-chain with correct ABI encoding via alloy `sol!` macro

## Usage

```bash
ipnft-minter \
  --name "My Research" \
  --description "Description of the IP" \
  --symbol SYM1 \
  --organization "My Org" \
  --lead-name "Jane Doe" \
  --lead-email "jane@example.com" \
  --topic "DeSci"
```

Optional: `--image /path/to/cover.png`

## Required Environment Variables

| Variable | Description |
|----------|-------------|
| `EVM_PRIVATE_KEY` | Hex private key (0x-prefixed) with Sepolia ETH for the 0.001 ETH mint fee |
| `EVM_RPC_URL` | Sepolia RPC endpoint (e.g., `https://rpc.sepolia.org`) |
| `MOLECULE_API_KEY` | DeSci Labs API key for GraphQL and uploads |

## Build

```bash
cd tools/ipnft-minter
cargo build --release
```

This is a separate Cargo package, not part of the main tengu-cluster workspace.

## Network

- **Chain:** Sepolia (11155111)
- **Contract:** `0x152B444e60C526fe4434C721561a077269FcF61a`
- **Mint fee:** 0.001 ETH
- **GraphQL:** `https://staging.graphql.api.molecule.xyz/graphql`
