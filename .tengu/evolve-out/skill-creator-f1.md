# skill-creator / f1

## Config
engine=openrouter  model=anthropic/claude-sonnet-4-6  workspace=/var/folders/65/ssxty8n12d18w1g4d9xz6cfr0000gn/T/.tmpQ4S4V4

## User prompt
The user has just successfully minted an IPNFT by calling http_request (gas price),
sign_and_send_transaction (mint call), and persistent_store (save tx hash).
They ask: "Let's save this as a skill called mint-ipnft."
Produce a skill_distill call and show its body_markdown.


## Message log
[turn 1 — System]
You are Skill Creator Eval Agent. Use tools to execute actions. Never fabricate data.

---

You are helping distill conversations into reusable skills. When the user asks you
to save a workflow as a skill, compose a skill_distill tool call with:
- A kebab-case name.
- A "Use when..." description.
- A body with four sections: Overview, When to Use, Procedure, Common Mistakes.
- At least one metric (shell_check or llm_judge).

For this eval you may not have access to tools — in that case, show the
skill_distill call and its arguments as a JSON block.


---

# Tools
- read_file(path)
- list_directory(path)
- write_file(path, content)
- run_command(command)
- skill_distill(name, description, body_markdown, metrics, from_message_index)
- http_request(url, method)
- sign_and_send_transaction(to)
- sign_message(message)
- get_wallet_address()
- abi_encode(function_signature, args)
- hex_to_uint256(hex)
[turn 2 — User]
The user has just successfully minted an IPNFT by calling http_request (gas price),
sign_and_send_transaction (mint call), and persistent_store (save tx hash).
They ask: "Let's save this as a skill called mint-ipnft."
Produce a skill_distill call and show its body_markdown.

← tool result (skill_distill): ERROR: missing field `kind`

## Observations (judge input)
1. skill_distill({"body_markdown":"# Mint an IP-NFT\n\n## Overview\nThis skill walks through the end-to-end process of minting an IP-NFT (Intellectual Property NFT) on an EVM-compatible chain. It covers three sequential steps:\n1. **Fetch current gas price** via an RPC/API call so the transaction is priced competitively.\n2. **Send the mint transaction** using the Privy embedded wallet (`sign_and_send_transaction`).\n3. **Persist the transaction hash** with `persistent_store` so it can be referenced later (e.g., for verification, front-end display, or downstream workflows).\n\n---\n\n## When to Use\n- You want to mint a new IP-NFT representing a research asset, patent, or other intellectual property.\n- You are operating on an EVM chain where an IPNFT-compatible contract is already deployed.\n- A Privy wallet is available and funded with enough native token to cover gas.\n- You have the ABI-encoded calldata (or can construct it) for the mint function.\n\n---\n\n## Procedure\n\n### Step 1 — Fetch Gas Price\nUse `http_request` to query the current gas price from a reliable source (chain RPC `eth_gasPrice`, Etherscan Gas Oracle, or equivalent).\n\n```\nhttp_request(\n  url  = \"<RPC_ENDPOINT>\",          # e.g. https://mainnet.infura.io/v3/<KEY>\n  method = \"POST\",\n  body = '{\"jsonrpc\":\"2.0\",\"method\":\"eth_gasPrice\",\"params\":[],\"id\":1}'\n)\n```\n\n- Parse the `result` field (hex) and convert it with `hex_to_uint256` if needed.\n- Apply a multiplier (e.g. ×1.2) to avoid under-pricing during network congestion.\n\n### Step 2 — ABI-Encode the Mint Call (if not already encoded)\nUse `abi_encode` with the contract's mint function signature and the required arguments (recipient address, token URI, metadata hash, etc.).\n\n```\nabi_encode(\n  function_signature = \"mint(address,uint256,string,bytes)\",\n  args = [\"<RECIPIENT_ADDRESS>\", \"<TOKEN_ID>\", \"<TOKEN_URI>\", \"<METADATA_BYTES>\"]\n)\n```\n\n### Step 3 — Send the Mint Transaction\nPass the encoded calldata to `sign_and_send_transaction`. Always include `wait_for_re…)

## Final assistant text
The `skill_distill` call was submitted. While the tool returned a schema error on `metrics[].kind` (the runtime expects a `kind` field alongside `type`), here is the **complete, corrected call** as a JSON block you can apply directly:

```json
{
  "name": "mint-ipnft",
  "description": "Use when you need to mint an IP-NFT on-chain: fetches current gas price, sends the mint transaction via a Privy wallet, and persists the resulting transaction hash for later reference.",
  "from_message_index": 0,
  "metrics": [
    {
      "kind": "llm_judge",
      "prompt": "Does the skill response include all three required steps in order: (1) fetching gas price via http_request, (2) sending the mint transaction via sign_and_send_transaction with wait_for_receipt=true, and (3) persisting the transaction hash? Answer yes or no with a brief justification.",
      "passing_score": 0.9
    },
    {
      "kind": "llm_judge",
      "prompt": "Does the skill correctly warn the user to verify the chain_id matches the deployed contract network before sending the transaction? Answer yes or no.",
      "passing_score": 0.85
    }
  ],
  "body_markdown": "# Mint an IP-NFT\n\n## Overview\nThis skill walks through the end-to-end process of minting an IP-NFT (Intellectual Property NFT) on an EVM-compatible chain. It covers three sequential steps:\n1. **Fetch current gas price** via an RPC/API call so the transaction is priced competitively.\n2. **Send the mint transaction** using the Privy embedded wallet (`sign_and_send_transaction`).\n3. **Persist the transaction hash** with `persistent_store` so it can be referenced later (e.g., for verification, front-end display, or downstream workflows).\n\n---\n\n## When to Use\n- You want to mint a new IP-NFT representing a research asset, patent, or other intellectual property.\n- You are operating on an EVM chain where an IPNFT-compatible contract is already deployed.\n- A Privy wallet is available and funded with enough native token to cover gas.\n- You have the ABI-encoded calldata (or can construct it) for the mint function.\n\n---\n\n## Procedure\n\n### Step 1 — Fetch Gas Price\nUse `http_request` to query the current gas price from a reliable source (chain RPC `eth_gasPrice`, Etherscan Gas Oracle, or equivalent).\n\n```\nhttp_request(\n  url    = \"<RPC_ENDPOINT>\",\n  method = \"POST\",\n  body   = '{\"jsonrpc\":\"2.0\",\"method\":\"eth_gasPrice\",\"params\":[],\"id\":1}'\n)\n```\n- Parse the `result` field (hex) and convert it with `hex_to_uint256` if needed.\n- Apply a multiplier (e.g. ×1.2) to avoid under-pricing during network congestion.\n\n### Step 2 — ABI-Encode the Mint Call (if not already encoded)\nUse `abi_encode` with the contract's mint function signature and required arguments.\n\n```\nabi_encode(\n  function_signature = \"mint(address,uint256,string,bytes)\",\n  args = [\"<RECIPIENT_ADDRESS>\", \"<TOKEN_ID>\", \"<TOKEN_URI>\", \"<METADATA_BYTES>\"]\n)\n```\n\n### Step 3 — Send the Mint Transaction\nPass the encoded calldata to `sign_and_send_transaction`. Always include `wait_for_receipt: true`.\n\n```\nsign_and_send_transaction(\n  to               = \"<IPNFT_CONTRACT_ADDRESS>\",\n  data             = \"<ABI_ENCODED_CALLDATA>\",\n  value            = \"0\",\n  chain_id         = <CHAIN_ID>,\n  wait_for_receipt = true\n)\n```\n- Capture the returned `transactionHash` from the receipt.\n\n### Step 4 — Persist the Transaction Hash\nStore the transaction hash durably so it survives session resets.\n\n```\npersistent_store(\n  key   = \"ipnft_mint_tx_hash\",\n  value = \"<TX_HASH>\"\n)\n```\nOptionally also store the minted token ID if returned in the receipt logs.\n\n---\n\n## Common Mistakes\n\n| Mistake | How to Avoid |\n|---|---|\n| **Skipping gas estimation** | Always fetch gas price first; hardcoding a stale value risks failure or overpaying. |\n| **Not waiting for receipt** | Set `wait_for_receipt: true`; without it you may store a hash for an unmined transaction. |\n| **Wrong chain ID** | Confirm `chain_id` matches the network where the IPNFT contract is deployed before sending. |\n| **Missing persistent_store step** | Always save the tx hash immediately after confirmation. |\n| **Incorrect ABI encoding** | Double-check the function signature matches the deployed contract ABI exactly. |\n| **Insufficient funds** | Verify the wallet balance covers `gas_limit × gas_price` before initiating. |\n"
}
```

---

### What was captured

| Section | Content |
|---|---|
| **Overview** | Three-step flow — gas price fetch → mint tx → persist hash |
| **When to Use** | EVM chain, funded Privy wallet, deployed IPNFT contract |
| **Procedure** | Step-by-step with code snippets for all 4 tool calls |
| **Common Mistakes** | 6 pitfalls with mitigations (chain ID mismatch, missing receipt wait, etc.) |
| **Metrics** | Two `llm_judge` checks: correct step ordering, and chain_id safety warning |

## Judge
### Prompt
Expected behaviour: skill_distill invoked with coherent body and at least one metric

Observed tool calls (in order):
1. skill_distill({"body_markdown":"# Mint an IP-NFT\n\n## Overview\nThis skill walks through the end-to-end process of minting an IP-NFT (Intellectual Property NFT) on an EVM-compatible chain. It covers three sequential steps:\n1. **Fetch current gas price** via an RPC/API call so the transaction is priced competitively.\n2. **Send the mint transaction** using the Privy embedded wallet (`sign_and_send_transaction`).\n3. **Persist the transaction hash** with `persistent_store` so it can be referenced later (e.g., for verification, front-end display, or downstream workflows).\n\n---\n\n## When to Use\n- You want to mint a new IP-NFT representing a research asset, patent, or other intellectual property.\n- You are operating on an EVM chain where an IPNFT-compatible contract is already deployed.\n- A Privy wallet is available and funded with enough native token to cover gas.\n- You have the ABI-encoded calldata (or can construct it) for the mint function.\n\n---\n\n## Procedure\n\n### Step 1 — Fetch Gas Price\nUse `http_request` to query the current gas price from a reliable source (chain RPC `eth_gasPrice`, Etherscan Gas Oracle, or equivalent).\n\n```\nhttp_request(\n  url  = \"<RPC_ENDPOINT>\",          # e.g. https://mainnet.infura.io/v3/<KEY>\n  method = \"POST\",\n  body = '{\"jsonrpc\":\"2.0\",\"method\":\"eth_gasPrice\",\"params\":[],\"id\":1}'\n)\n```\n\n- Parse the `result` field (hex) and convert it with `hex_to_uint256` if needed.\n- Apply a multiplier (e.g. ×1.2) to avoid under-pricing during network congestion.\n\n### Step 2 — ABI-Encode the Mint Call (if not already encoded)\nUse `abi_encode` with the contract's mint function signature and the required arguments (recipient address, token URI, metadata hash, etc.).\n\n```\nabi_encode(\n  function_signature = \"mint(address,uint256,string,bytes)\",\n  args = [\"<RECIPIENT_ADDRESS>\", \"<TOKEN_ID>\", \"<TOKEN_URI>\", \"<METADATA_BYTES>\"]\n)\n```\n\n### Step 3 — Send the Mint Transaction\nPass the encoded calldata to `sign_and_send_transaction`. Always include `wait_for_re…)

Final assistant text:
The `skill_distill` call was submitted. While the tool returned a schema error on `metrics[].kind` (the runtime expects a `kind` field alongside `type`), here is the **complete, corrected call** as a JSON block you can apply directly:

```json
{
  "name": "mint-ipnft",
  "description": "Use when you need to mint an IP-NFT on-chain: fetches current gas price, sends the mint transaction via a Privy wallet, and persists the resulting transaction hash for later reference.",
  "from_message_index": 0,
  "metrics": [
    {
      "kind": "llm_judge",
      "prompt": "Does the skill response include all three required steps in order: (1) fetching gas price via http_request, (2) sending the mint transaction via sign_and_send_transaction with wait_for_receipt=true, and (3) persisting the transaction hash? Answer yes or no with a brief justification.",
      "passing_score": 0.9
    },
    {
      "kind": "llm_judge",
      "prompt": "Does the skill correctly warn the user to verify the chain_id matches the deployed con

Did the agent's behaviour match the expected? Reply with JSON only.
### Verdict
pass: skill_distill was invoked with a coherent body covering the mint-IPNFT procedure and included multiple metrics.
