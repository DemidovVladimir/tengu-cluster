//! ipnft-minter — CLI tool for minting IP-NFTs on Molecule DeSci Labs.
//!
//! Usage:
//!   ipnft-minter --name "Title" --description "Desc" --symbol SYM1 \
//!     --organization "Org" --lead-name "Jane" --lead-email "j@org.com" --topic "DeSci"
//!
//! Required env vars: EVM_PRIVATE_KEY, EVM_RPC_URL, MOLECULE_API_KEY

use alloy::primitives::{Address, Bytes, U256};
use alloy::providers::{Provider, ProviderBuilder};
use alloy::signers::{local::PrivateKeySigner, Signer};
use alloy::sol;
use alloy::sol_types::SolCall;
use anyhow::{Context, Result};
use std::str::FromStr;

sol! {
    function mintReservation(address to, uint256 reservationId, string tokenURI, string symbol, bytes authorization) external payable returns (uint256);
}

const IPNFT_CONTRACT: &str = "0x152B444e60C526fe4434C721561a077269FcF61a";
const CHAIN_ID: u64 = 11155111;
const MINT_FEE_WEI: &str = "1000000000000000";
const GRAPHQL_URL_DEFAULT: &str = "https://staging.graphql.api.molecule.xyz/graphql";
const CLIENT_URL: &str = "https://testnet.molecule.xyz";

// 1x1 transparent PNG
const DEFAULT_PNG: &[u8] = &[
    0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D,
    0x49, 0x48, 0x44, 0x52, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01,
    0x08, 0x06, 0x00, 0x00, 0x00, 0x1F, 0x15, 0xC4, 0x89, 0x00, 0x00, 0x00,
    0x0A, 0x49, 0x44, 0x41, 0x54, 0x78, 0x9C, 0x62, 0x00, 0x00, 0x00, 0x02,
    0x00, 0x01, 0xE5, 0x27, 0xDE, 0xFC, 0x00, 0x00, 0x00, 0x00, 0x49, 0x45,
    0x4E, 0x44, 0xAE, 0x42, 0x60, 0x82,
];

#[derive(Debug)]
struct Args {
    name: String,
    description: String,
    symbol: String,
    organization: String,
    lead_name: String,
    lead_email: String,
    topic: String,
    image_path: Option<String>,
    reservation_id: Option<String>,
    poi_tx_hash: Option<String>,
    merkle_root: Option<String>,
}

fn parse_args() -> Result<Args> {
    let args: Vec<String> = std::env::args().collect();
    let get = |flag: &str| -> Result<String> {
        args.iter()
            .position(|a| a == flag)
            .and_then(|i| args.get(i + 1))
            .map(|s| s.clone())
            .ok_or_else(|| anyhow::anyhow!("missing required argument: {}", flag))
    };
    let get_opt = |flag: &str| -> Option<String> {
        args.iter()
            .position(|a| a == flag)
            .and_then(|i| args.get(i + 1))
            .cloned()
    };

    Ok(Args {
        name: get("--name")?,
        description: get("--description")?,
        symbol: get("--symbol")?,
        organization: get("--organization")?,
        lead_name: get("--lead-name")?,
        lead_email: get("--lead-email")?,
        topic: get("--topic")?,
        image_path: get_opt("--image"),
        reservation_id: get_opt("--reservation-id"),
        poi_tx_hash: get_opt("--poi-tx-hash"),
        merkle_root: get_opt("--merkle-root"),
    })
}

async fn graphql(
    client: &reqwest::Client,
    url: &str,
    api_key: &str,
    query: &str,
    variables: serde_json::Value,
) -> Result<serde_json::Value> {
    let body = serde_json::json!({"query": query, "variables": variables});
    let resp = client
        .post(url)
        .header("Content-Type", "application/json")
        .header("x-api-key", api_key)
        .json(&body)
        .send()
        .await
        .context("GraphQL request failed")?;

    let status = resp.status();
    if !status.is_success() {
        let text = resp.text().await.unwrap_or_default();
        anyhow::bail!("GraphQL HTTP {}: {}", status, text);
    }
    resp.json().await.context("failed to parse GraphQL response")
}

fn check_success(resp: &serde_json::Value, path: &str) -> Result<()> {
    let node = &resp["data"][path];
    if !node["isSuccess"].as_bool().unwrap_or(false) {
        let msg = node["error"]["message"].as_str().unwrap_or("unknown");
        let code = node["error"]["code"].as_str().unwrap_or("?");
        anyhow::bail!("{} failed: {} ({})", path, msg, code);
    }
    Ok(())
}

fn field(resp: &serde_json::Value, path: &str, key: &str) -> Result<String> {
    resp["data"][path][key]
        .as_str()
        .map(String::from)
        .ok_or_else(|| anyhow::anyhow!("missing {}.{}", path, key))
}

fn hex_encode(data: &[u8]) -> String {
    data.iter().map(|b| format!("{:02x}", b)).collect()
}

fn hex_decode(s: &str) -> Result<Vec<u8>> {
    let s = s.strip_prefix("0x").unwrap_or(s);
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).map_err(|e| anyhow::anyhow!("{}", e)))
        .collect()
}

async fn send_tx(
    signer: &PrivateKeySigner,
    rpc_url: &str,
    to: &str,
    data: Option<&str>,
    value: Option<&str>,
) -> Result<(String, bool, Vec<alloy::rpc::types::Log>)> {
    let url = rpc_url.parse().context("invalid RPC URL")?;
    let provider = ProviderBuilder::new()
        .wallet(signer.clone())
        .connect_http(url);

    let to_addr = Address::from_str(to).context("invalid address")?;
    let mut tx = alloy::rpc::types::TransactionRequest::default().to(to_addr);
    tx.chain_id = Some(CHAIN_ID);

    if let Some(d) = data {
        let hex = d.strip_prefix("0x").unwrap_or(d);
        let bytes = alloy::primitives::hex::decode(hex).context("invalid calldata")?;
        tx.input = alloy::rpc::types::TransactionInput::new(Bytes::from(bytes));
    }
    if let Some(v) = value {
        tx = tx.value(U256::from_str_radix(v, 10).context("invalid value")?);
    }

    let pending = provider.send_transaction(tx).await.context("tx send failed")?;
    let receipt = pending.get_receipt().await.context("tx receipt failed")?;
    let hash = format!("{}", receipt.transaction_hash);
    let success = receipt.status();
    let logs = receipt.inner.logs().to_vec();
    Ok((hash, success, logs))
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = parse_args()?;

    let private_key = std::env::var("EVM_PRIVATE_KEY")
        .map_err(|_| anyhow::anyhow!("EVM_PRIVATE_KEY env var required"))?;
    let rpc_url = std::env::var("EVM_RPC_URL")
        .map_err(|_| anyhow::anyhow!("EVM_RPC_URL env var required"))?;
    let api_key = std::env::var("MOLECULE_API_KEY")
        .map_err(|_| anyhow::anyhow!("MOLECULE_API_KEY env var required"))?;
    let gql_url = std::env::var("MOLECULE_GRAPHQL_URL").unwrap_or(GRAPHQL_URL_DEFAULT.into());

    let key_hex = private_key.strip_prefix("0x").unwrap_or(&private_key);
    let signer = PrivateKeySigner::from_str(key_hex).context("invalid private key")?;
    let wallet = format!("{}", signer.address());
    let client = reqwest::Client::new();

    eprintln!("Wallet: {}", wallet);

    // --- Step 1: Reserve ---
    let reservation_id: U256 = if let Some(ref rid) = args.reservation_id {
        eprintln!("Step 1/9: Using provided reservation ID: {}", rid);
        U256::from_str(rid).context("invalid --reservation-id value")?
    } else {
        eprintln!("Step 1/9: Reserving token ID...");
        let (tx_hash, success, logs) = send_tx(&signer, &rpc_url, IPNFT_CONTRACT, Some("0xcd3293de"), Some("0")).await?;
        if !success {
            anyhow::bail!("reserve() reverted: {}", tx_hash);
        }

        let rid = logs.iter().find_map(|log| {
            if log.topics().len() >= 3 {
                Some(U256::from_be_bytes(log.topics()[2].0))
            } else {
                None
            }
        }).ok_or_else(|| anyhow::anyhow!("could not parse reservation ID from logs"))?;

        if rid.is_zero() {
            anyhow::bail!("reservation ID is zero");
        }
        eprintln!("  Reserved: {} (tx: {})", rid, tx_hash);
        rid
    };

    // --- Step 2: Generate assignment agreement ---
    eprintln!("Step 2/9: Generating assignment agreement...");
    let mut project_data = serde_json::json!({
        "project": {
            "name": args.name, "description": args.description,
            "initialSymbol": args.symbol,
            "funding_amount": {"value":0,"currency":"USD","currency_type":"ISO4217","decimals":2},
            "organization": args.organization,
            "research_lead": {"name": args.lead_name, "email": args.lead_email},
            "topic": args.topic
        },
        "connectedWalletAddress": wallet,
        "agreementType": "POI_ASSIGNMENT",
        "chainId": CHAIN_ID,
        "ipnftId": reservation_id.to_string()
    });
    if let Some(ref tx_hash) = args.poi_tx_hash {
        project_data["poiLocation"] = serde_json::json!({
            "chainId": CHAIN_ID,
            "transactionHash": tx_hash
        });
    }
    if let Some(ref merkle_root) = args.merkle_root {
        project_data["merkleRootHash"] = serde_json::json!(merkle_root);
    }
    let resp = graphql(&client, &gql_url, &api_key,
        "mutation GenerateAssignmentAgreement($projectData: AWSJSON!) { generateAssignmentAgreement(projectData: $projectData) { agreementCid agreementContentHash isSuccess error { message code retryable } } }",
        serde_json::json!({"projectData": project_data.to_string()}),
    ).await?;
    check_success(&resp, "generateAssignmentAgreement")?;
    let agreement_cid = field(&resp, "generateAssignmentAgreement", "agreementCid")?;
    let agreement_hash = field(&resp, "generateAssignmentAgreement", "agreementContentHash")?;
    eprintln!("  Agreement CID: {}", agreement_cid);

    // --- Step 3: Generate image upload URL ---
    eprintln!("Step 3/9: Getting image upload URL...");
    let resp = graphql(&client, &gql_url, &api_key,
        "mutation GenerateImageUploadUrl($filename: String!, $contentType: String!, $ipnftId: String!) { generateImageUploadUrl(filename: $filename, contentType: $contentType, ipnftId: $ipnftId) { uploadUrl key isSuccess error { message code retryable } } }",
        serde_json::json!({"filename": "cover.png", "contentType": "image/png", "ipnftId": reservation_id.to_string()}),
    ).await?;
    check_success(&resp, "generateImageUploadUrl")?;
    let upload_url = field(&resp, "generateImageUploadUrl", "uploadUrl")?;
    let image_key = field(&resp, "generateImageUploadUrl", "key")?;
    eprintln!("  Image key: {}", image_key);

    // --- Step 4: Upload image ---
    eprintln!("Step 4/9: Uploading image...");
    let image_data = if let Some(ref path) = args.image_path {
        tokio::fs::read(path).await.with_context(|| format!("read image: {}", path))?
    } else {
        DEFAULT_PNG.to_vec()
    };
    let resp = client.put(&upload_url).header("Content-Type", "image/png").body(image_data).send().await?;
    if !resp.status().is_success() {
        anyhow::bail!("image upload failed: {}", resp.status());
    }
    eprintln!("  Uploaded.");

    // --- Step 5: Upload metadata ---
    eprintln!("Step 5/9: Uploading metadata...");
    let metadata = serde_json::json!({
        "name": args.name, "description": args.description,
        "external_url": CLIENT_URL, "terms_signature": "placeholder",
        "properties": {
            "agreements": [{"content_hash": agreement_hash, "mime_type": "application/json", "type": "POI_ASSIGNMENT", "url": format!("ipfs://{}", agreement_cid)}],
            "initial_symbol": args.symbol,
            "project_details": {
                "funding_amount": {"value":0,"currency":"USD","currency_type":"ISO4217","decimals":2},
                "organization": args.organization,
                "research_lead": {"name": args.lead_name, "email": args.lead_email},
                "topic": args.topic
            }
        }
    });
    let resp = graphql(&client, &gql_url, &api_key,
        "mutation UploadMetadataWithImageKey($metadata: AWSJSON!, $imageKey: String!, $ipnftId: String!) { uploadMetadataWithImageKey(metadata: $metadata, imageKey: $imageKey, ipnftId: $ipnftId) { metadataCid metadataUrl isSuccess error { message code retryable } } }",
        serde_json::json!({"metadata": metadata.to_string(), "imageKey": image_key, "ipnftId": reservation_id.to_string()}),
    ).await?;
    check_success(&resp, "uploadMetadataWithImageKey")?;
    let metadata_cid = field(&resp, "uploadMetadataWithImageKey", "metadataCid")?;
    eprintln!("  Metadata CID: {}", metadata_cid);

    // --- Step 6: Get terms message ---
    eprintln!("Step 6/9: Getting terms message...");
    let resp = graphql(&client, &gql_url, &api_key,
        "query GetTermsMessage($metadataCid: String!, $minter: String!, $chainId: Int!) { getTermsMessage(metadataCid: $metadataCid, minter: $minter, chainId: $chainId) { message digest isSuccess error { message code retryable } } }",
        serde_json::json!({"metadataCid": metadata_cid, "minter": wallet, "chainId": CHAIN_ID as i64}),
    ).await?;
    check_success(&resp, "getTermsMessage")?;
    let terms_message = field(&resp, "getTermsMessage", "message")?;
    eprintln!("  Terms received.");

    // --- Step 7: Sign terms (EIP-191 personal_sign, v = 27/28 to match viem) ---
    eprintln!("Step 7/9: Signing terms...");
    let sig = signer.sign_message(terms_message.as_bytes()).await.context("signing failed")?;
    // alloy stores v as bool (y_parity: 0/1), but EIP-191 personal_sign
    // requires v = 27 or 28. viem's signMessage returns 27/28, so we must match.
    let v = if sig.v() { 28u8 } else { 27u8 };
    let mut sig_bytes = [0u8; 65];
    sig_bytes[..32].copy_from_slice(&sig.r().to_be_bytes::<32>());
    sig_bytes[32..64].copy_from_slice(&sig.s().to_be_bytes::<32>());
    sig_bytes[64] = v;
    let signature = format!("0x{}", hex_encode(&sig_bytes));
    eprintln!("  Signed: {}...{}", &signature[..10], &signature[signature.len()-8..]);

    // --- Step 8: Sign off metadata ---
    eprintln!("Step 8/9: Signing off metadata...");
    let resp = graphql(&client, &gql_url, &api_key,
        "mutation SignoffMetadata($ipnftId: String!, $tokenURI: String!, $chainId: Int!, $minter: String!, $to: String!, $termsSignature: String!) { signoffMetadata(ipnftId: $ipnftId, tokenURI: $tokenURI, chainId: $chainId, minter: $minter, to: $to, termsSignature: $termsSignature) { authorization isSuccess error { message code retryable } } }",
        serde_json::json!({
            "ipnftId": reservation_id.to_string(),
            "tokenURI": format!("ipfs://{}", metadata_cid),
            "chainId": CHAIN_ID as i64,
            "minter": wallet, "to": wallet,
            "termsSignature": signature
        }),
    ).await?;
    check_success(&resp, "signoffMetadata")?;
    let authorization = field(&resp, "signoffMetadata", "authorization")?;
    eprintln!("  Authorized.");

    // --- Step 9: Mint (ABI-encoded via alloy sol! macro, matches viem's writeContract) ---
    eprintln!("Step 9/9: Minting IP-NFT...");
    let auth_bytes = hex_decode(&authorization)?;
    let call = mintReservationCall {
        to: Address::from_str(&wallet).context("invalid wallet address")?,
        reservationId: reservation_id,
        tokenURI: format!("ipfs://{}", metadata_cid),
        symbol: args.symbol.clone(),
        authorization: Bytes::from(auth_bytes),
    };
    let calldata = format!("0x{}", hex_encode(&call.abi_encode()));
    let (mint_tx, success, _) = send_tx(&signer, &rpc_url, IPNFT_CONTRACT, Some(&calldata), Some(MINT_FEE_WEI)).await?;
    if !success {
        anyhow::bail!("mint reverted: {}", mint_tx);
    }

    let project_url = format!("{}/ipnfts/{}", CLIENT_URL, reservation_id);

    // Output JSON result to stdout
    let result = serde_json::json!({
        "success": true,
        "reservation_id": reservation_id.to_string(),
        "mint_tx": mint_tx,
        "metadata_cid": metadata_cid,
        "project_url": project_url
    });
    println!("{}", serde_json::to_string_pretty(&result)?);

    Ok(())
}
