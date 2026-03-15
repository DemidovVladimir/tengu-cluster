use crate::application::ports::ToolExecutionPort;
use crate::domain::capability::{CapabilityId, EffectClass, RegisteredTool, ToolClass};
use crate::domain::tool_result::ToolResultEnvelope;
use alloy::primitives::{Address, Bytes, U256};
use alloy::providers::{Provider, ProviderBuilder};
use alloy::signers::{local::PrivateKeySigner, Signer};
use alloy::sol;
use alloy::sol_types::SolCall;
use anyhow::{bail, Context, Result};
use reqwest::multipart;
use serde_json::json;
use std::path::{Path, PathBuf};
use std::str::FromStr;

sol! {
    function mintReservation(address to, uint256 reservationId, string tokenURI, string symbol, bytes authorization) external payable returns (uint256);
}

const CHAIN_ID: u64 = 11155111;
const POI_API_URL: &str = "https://testnet.molecule.xyz/api/v1/inventions";
const BEACH_API_URL: &str = "https://beach.science/api/v1/posts";
const DEFAULT_ACCESS_LEVEL: &str = "PUBLIC";
const IPNFT_CONTRACT: &str = "0x152B444e60C526fe4434C721561a077269FcF61a";
const MINT_FEE_WEI: &str = "1000000000000000";

// 1x1 transparent PNG — used when no cover image is provided for minting.
const DEFAULT_PNG: &[u8] = &[
    0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D,
    0x49, 0x48, 0x44, 0x52, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01,
    0x08, 0x06, 0x00, 0x00, 0x00, 0x1F, 0x15, 0xC4, 0x89, 0x00, 0x00, 0x00,
    0x0A, 0x49, 0x44, 0x41, 0x54, 0x78, 0x9C, 0x62, 0x00, 0x00, 0x00, 0x02,
    0x00, 0x01, 0xE5, 0x27, 0xDE, 0xFC, 0x00, 0x00, 0x00, 0x00, 0x49, 0x45,
    0x4E, 0x44, 0xAE, 0x42, 0x60, 0x82,
];

pub(crate) fn desci_tool_defs() -> Vec<RegisteredTool> {
    vec![
        RegisteredTool::new(
            "poi_register_document",
            "Register a Proof of Invention document with the Molecule POI endpoint using a workspace file path.",
            json!({
                "type": "object",
                "properties": {
                    "document_path": {
                        "type": "string",
                        "description": "Path to the PDF or source document relative to the workspace root"
                    },
                    "audit_path": {
                        "type": "string",
                        "description": "Optional workspace-relative path where the structured result should be written"
                    }
                },
                "required": ["document_path"]
            }),
            CapabilityId::new("desci.poi.register").expect("static capability is valid"),
            EffectClass::ExternalApi,
        )
        .with_tool_class(ToolClass::ExecuteTool)
        .with_required_secrets(&["POI_API_KEY"])
        .with_host_allowlist(&["testnet.molecule.xyz"])
        .with_output_schema(json!({
            "type": "object",
            "properties": {
                "tool_name": {"type": "string"},
                "status": {"type": "string"},
                "artifacts": {"type": "object"},
                "ids": {"type": "object"},
                "urls": {"type": "object"},
                "hashes": {"type": "object"}
            }
        })),
        RegisteredTool::new(
            "mint_ipnft",
            "Submit the POI transaction on-chain, reserve a token ID, and mint an IP-NFT via the Molecule GraphQL API. Returns structured mint outputs.",
            json!({
                "type": "object",
                "properties": {
                    "poi_transaction_to": {"type": "string"},
                    "poi_transaction_data": {"type": "string"},
                    "merkle_root": {"type": "string"},
                    "name": {"type": "string"},
                    "description": {"type": "string"},
                    "symbol": {"type": "string"},
                    "organization": {"type": "string"},
                    "lead_name": {"type": "string"},
                    "lead_email": {"type": "string"},
                    "topic": {"type": "string"},
                    "image_path": {"type": "string"},
                    "audit_path": {"type": "string"}
                },
                "required": [
                    "poi_transaction_to",
                    "poi_transaction_data",
                    "merkle_root",
                    "name",
                    "description",
                    "symbol",
                    "organization",
                    "lead_name",
                    "lead_email",
                    "topic"
                ]
            }),
            CapabilityId::new("desci.mint.ipnft").expect("static capability is valid"),
            EffectClass::ChainTx,
        )
        .with_tool_class(ToolClass::ExecuteTool)
        .with_required_secrets(&["EVM_PRIVATE_KEY", "EVM_RPC_URL", "MOLECULE_API_KEY"])
        .with_host_allowlist(&["staging.graphql.api.molecule.xyz"])
        .with_output_schema(json!({"type": "object"})),
        RegisteredTool::new(
            "create_molecule_project",
            "Create a Molecule project (data room) for an already minted IP-NFT.",
            json!({
                "type": "object",
                "properties": {
                    "ipnft_symbol": {"type": "string"},
                    "ipnft_token_id": {"type": "string"},
                    "audit_path": {"type": "string"}
                },
                "required": ["ipnft_symbol", "ipnft_token_id"]
            }),
            CapabilityId::new("desci.project.create").expect("static capability is valid"),
            EffectClass::ExternalApi,
        )
        .with_tool_class(ToolClass::ExecuteTool)
        .with_required_secrets(&["MOLECULE_API_KEY", "MOLECULE_SERVICE_TOKEN", "MOLECULE_CLIENT_URL"])
        .with_host_allowlist(&["staging.graphql.api.molecule.xyz", "testnet.molecule.xyz"])
        .with_output_schema(json!({"type": "object"})),
        RegisteredTool::new(
            "upload_molecule_file",
            "Upload a workspace file into a Molecule project via the full initiate-upload-finalize flow.",
            json!({
                "type": "object",
                "properties": {
                    "ipnft_uid": {"type": "string"},
                    "file_path": {"type": "string"},
                    "content_type": {"type": "string"},
                    "path": {"type": "string"},
                    "description": {"type": "string"},
                    "change_by": {"type": "string"},
                    "access_level": {"type": "string"},
                    "tags": {
                        "type": "array",
                        "items": {"type": "string"}
                    },
                    "categories": {
                        "type": "array",
                        "items": {"type": "string"}
                    },
                    "audit_path": {"type": "string"}
                },
                "required": ["ipnft_uid", "file_path", "content_type"]
            }),
            CapabilityId::new("desci.project.upload").expect("static capability is valid"),
            EffectClass::ExternalApi,
        )
        .with_tool_class(ToolClass::ExecuteTool)
        .with_required_secrets(&["MOLECULE_API_KEY", "MOLECULE_SERVICE_TOKEN"])
        .with_host_allowlist(&["staging.graphql.api.molecule.xyz"])
        .with_output_schema(json!({"type": "object"})),
        RegisteredTool::new(
            "create_molecule_announcement",
            "Create a Molecule project announcement with optional dataset attachments.",
            json!({
                "type": "object",
                "properties": {
                    "ipnft_uid": {"type": "string"},
                    "headline": {"type": "string"},
                    "body": {"type": "string"},
                    "attachments": {
                        "type": "array",
                        "items": {"type": "string"}
                    },
                    "audit_path": {"type": "string"}
                },
                "required": ["ipnft_uid", "headline", "body"]
            }),
            CapabilityId::new("desci.project.announce").expect("static capability is valid"),
            EffectClass::ExternalApi,
        )
        .with_tool_class(ToolClass::ExecuteTool)
        .with_required_secrets(&["MOLECULE_API_KEY", "MOLECULE_SERVICE_TOKEN"])
        .with_host_allowlist(&["staging.graphql.api.molecule.xyz"])
        .with_output_schema(json!({"type": "object"})),
        RegisteredTool::new(
            "publish_beach_post",
            "Publish a scientific post to Beach.science. The body should be a reader-friendly scientific summary: hypothesis, methodology, key findings, and significance. Do NOT include internal pipeline data such as merkle roots, transaction hashes, reservation IDs, metadata CIDs, or wallet addresses — those are infrastructure details, not scientific content.",
            json!({
                "type": "object",
                "properties": {
                    "title": {"type": "string", "description": "Post title — a clear scientific headline"},
                    "body": {"type": "string", "description": "Markdown body — scientific content only, no internal IDs/hashes/tx data"},
                    "post_type": {"type": "string"},
                    "audit_path": {"type": "string"}
                },
                "required": ["title", "body"]
            }),
            CapabilityId::new("desci.beach.post").expect("static capability is valid"),
            EffectClass::ExternalApi,
        )
        .with_tool_class(ToolClass::ExecuteTool)
        .with_required_secrets(&["BEACH_SCIENCE_API_KEY"])
        .with_host_allowlist(&["beach.science"])
        .with_output_schema(json!({"type": "object"})),
    ]
}

pub(crate) struct DesciToolExecutionAdapter {
    client: reqwest::Client,
    workspace: PathBuf,
    fallback_runtime: Option<tokio::runtime::Runtime>,
}

impl DesciToolExecutionAdapter {
    pub(crate) fn new(workspace: PathBuf) -> Result<Self> {
        let fallback_runtime = if tokio::runtime::Handle::try_current().is_ok() {
            None
        } else {
            Some(
                tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()?,
            )
        };
        Ok(Self {
            client: reqwest::Client::builder().build()?,
            workspace,
            fallback_runtime,
        })
    }

    fn run_async<F, T>(&self, future: F) -> T
    where
        F: std::future::Future<Output = T>,
    {
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            tokio::task::block_in_place(|| handle.block_on(future))
        } else {
            self.fallback_runtime
                .as_ref()
                .expect("no tokio runtime available")
                .block_on(future)
        }
    }
}

impl ToolExecutionPort for DesciToolExecutionAdapter {
    fn execute_tool(&self, call: &tengu_core::types::ToolCall) -> Result<String> {
        match call.name.as_str() {
            "poi_register_document" => self.execute_poi_register_document(call),
            "mint_ipnft" => self.execute_mint_ipnft(call),
            "create_molecule_project" => self.execute_create_molecule_project(call),
            "upload_molecule_file" => self.execute_upload_molecule_file(call),
            "create_molecule_announcement" => self.execute_create_molecule_announcement(call),
            "publish_beach_post" => self.execute_publish_beach_post(call),
            other => bail!("Unknown DeSci tool: {}", other),
        }
    }
}

impl DesciToolExecutionAdapter {
    fn execute_poi_register_document(&self, call: &tengu_core::types::ToolCall) -> Result<String> {
        let document_path = required_str(call, "document_path")?;
        let audit_path = optional_str(call, "audit_path");
        let document =
            crate::adapters::workspace_tools::validate_path(&self.workspace, document_path)?;
        let file_name = document
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("document.bin")
            .to_string();
        let bytes = std::fs::read(&document)?;
        let token = std::env::var("POI_API_KEY")
            .map_err(|_| anyhow::anyhow!("Missing environment variable POI_API_KEY"))?;
        let client = self.client.clone();
        let response_value = self.run_async(async move {
            let part = multipart::Part::bytes(bytes).file_name(file_name);
            let form = multipart::Form::new().part("files", part);
            let response = client
                .post(POI_API_URL)
                .bearer_auth(token)
                .multipart(form)
                .send()
                .await?;
            let status = response.status();
            let text = response.text().await.unwrap_or_default();
            if !status.is_success() {
                bail!("HTTP {} from POI endpoint: {}", status, text);
            }
            let value: serde_json::Value =
                serde_json::from_str(&text).context("POI endpoint returned invalid JSON")?;
            Ok::<serde_json::Value, anyhow::Error>(value)
        })?;

        let transaction_to = response_value["data"]["transaction"]["to"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("POI response missing data.transaction.to"))?
            .to_string();
        let transaction_data = response_value["data"]["transaction"]["data"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("POI response missing data.transaction.data"))?
            .to_string();
        let merkle_root = response_value["data"]["proof"]["tree"]
            .as_array()
            .and_then(|arr| arr.first())
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("POI response missing data.proof.tree[0]"))?
            .to_string();

        let envelope = ToolResultEnvelope::ok(
            "poi_register_document",
            "Registered proof of invention and captured POI transaction payload.",
        )
        .with_artifact("poi.document_path", document_path)?
        .with_artifact("poi.transaction_to", &transaction_to)?
        .with_artifact("poi.transaction_data", &transaction_data)?
        .with_hash("poi.merkle_root", merkle_root.clone())
        .with_provenance(
            "molecule.poi",
            vec![document_path.to_string()],
            vec!["testnet.molecule.xyz".to_string()],
        )
        .with_raw_response(response_value);

        self.finalize_tool_result(&envelope, audit_path)
    }

    fn execute_mint_ipnft(&self, call: &tengu_core::types::ToolCall) -> Result<String> {
        let poi_transaction_to = required_str(call, "poi_transaction_to")?;
        let poi_transaction_data = required_str(call, "poi_transaction_data")?;
        let merkle_root = required_str(call, "merkle_root")?;
        let name = required_str(call, "name")?;
        let description = required_str(call, "description")?;
        let symbol = required_str(call, "symbol")?;
        let organization = required_str(call, "organization")?;
        let lead_name = required_str(call, "lead_name")?;
        let lead_email = required_str(call, "lead_email")?;
        let topic = required_str(call, "topic")?;
        let audit_path = optional_str(call, "audit_path");
        let image_path = optional_str(call, "image_path")
            .map(|path| resolve_optional_workspace_path(&self.workspace, path))
            .transpose()?;

        let private_key = std::env::var("EVM_PRIVATE_KEY")
            .map_err(|_| anyhow::anyhow!("Missing environment variable EVM_PRIVATE_KEY"))?;
        let rpc_url = std::env::var("EVM_RPC_URL")
            .map_err(|_| anyhow::anyhow!("Missing environment variable EVM_RPC_URL"))?;
        let api_key = std::env::var("MOLECULE_API_KEY")
            .map_err(|_| anyhow::anyhow!("Missing environment variable MOLECULE_API_KEY"))?;
        let gql_url = std::env::var("MOLECULE_LABS_URL")
            .map_err(|_| anyhow::anyhow!("Missing environment variable MOLECULE_LABS_URL"))?;
        let client_url = std::env::var("MOLECULE_CLIENT_URL")
            .unwrap_or_else(|_| "https://testnet.molecule.xyz".to_string());

        let key_hex = private_key.strip_prefix("0x").unwrap_or(&private_key);
        let signer = PrivateKeySigner::from_str(key_hex).context("invalid EVM_PRIVATE_KEY")?;
        let wallet = format!("{}", signer.address());

        // Step 1: Submit the POI transaction on-chain.
        let poi_tx_hash = self.run_async(async {
            let (tx_hash, success, _logs) = send_tx(
                &signer,
                &rpc_url,
                poi_transaction_to,
                Some(poi_transaction_data),
                Some("0"),
            )
            .await?;
            if !success {
                bail!("POI on-chain transaction reverted: {}", tx_hash);
            }
            Ok::<String, anyhow::Error>(tx_hash)
        })?;

        // Derive reservation ID: POI transaction data cast to uint256 (the merkle root).
        // This produces a 256-bit ID (> u128), which is the correct format for POI-based minting.
        let data_hex = poi_transaction_data.strip_prefix("0x").unwrap_or(poi_transaction_data);
        let data_bytes = alloy::primitives::hex::decode(data_hex)
            .context("invalid POI transaction data hex")?;
        let reservation_id = U256::from_be_slice(&data_bytes);
        if reservation_id.is_zero() {
            bail!("reservation ID is zero — POI transaction data is invalid");
        }

        // Read image data (sync — local file).
        let image_data = match &image_path {
            Some(path) => std::fs::read(path).with_context(|| format!("read image: {}", path))?,
            None => DEFAULT_PNG.to_vec(),
        };

        // Steps 2-8: Molecule GraphQL flow → on-chain mint.
        let client = self.client.clone();
        let (reservation_id, mint_tx, metadata_cid) = self.run_async(async {
            // Step 2: Generate assignment agreement.
            let mut project_data = json!({
                "project": {
                    "name": name, "description": description,
                    "initialSymbol": symbol,
                    "funding_amount": {"value":0,"currency":"USD","currency_type":"ISO4217","decimals":2},
                    "organization": organization,
                    "research_lead": {"name": lead_name, "email": lead_email},
                    "topic": topic
                },
                "connectedWalletAddress": &wallet,
                "agreementType": "POI_ASSIGNMENT",
                "chainId": CHAIN_ID,
                "ipnftId": reservation_id.to_string()
            });
            project_data["poiLocation"] = json!({
                "chainId": CHAIN_ID,
                "transactionHash": &poi_tx_hash
            });
            project_data["merkleRootHash"] = json!(merkle_root);

            let resp = ipnft_graphql(&client, &gql_url, &api_key,
                "mutation GenerateAssignmentAgreement($projectData: AWSJSON!) { generateAssignmentAgreement(projectData: $projectData) { agreementCid agreementContentHash isSuccess error { message code retryable } } }",
                json!({"projectData": project_data.to_string()}),
            ).await?;
            let node = &resp["data"]["generateAssignmentAgreement"];
            ensure_success(node, "generateAssignmentAgreement")?;
            let agreement_cid = node["agreementCid"].as_str()
                .ok_or_else(|| anyhow::anyhow!("missing agreementCid"))?.to_string();
            let agreement_hash = node["agreementContentHash"].as_str()
                .ok_or_else(|| anyhow::anyhow!("missing agreementContentHash"))?.to_string();

            // Step 3: Get image upload URL.
            let resp = ipnft_graphql(&client, &gql_url, &api_key,
                "mutation GenerateImageUploadUrl($filename: String!, $contentType: String!, $ipnftId: String!) { generateImageUploadUrl(filename: $filename, contentType: $contentType, ipnftId: $ipnftId) { uploadUrl key isSuccess error { message code retryable } } }",
                json!({"filename": "cover.png", "contentType": "image/png", "ipnftId": reservation_id.to_string()}),
            ).await?;
            let node = &resp["data"]["generateImageUploadUrl"];
            ensure_success(node, "generateImageUploadUrl")?;
            let upload_url = node["uploadUrl"].as_str()
                .ok_or_else(|| anyhow::anyhow!("missing uploadUrl"))?.to_string();
            let image_key = node["key"].as_str()
                .ok_or_else(|| anyhow::anyhow!("missing image key"))?.to_string();

            // Step 4: Upload image.
            let resp = client.put(&upload_url)
                .header("Content-Type", "image/png")
                .body(image_data)
                .send().await?;
            if !resp.status().is_success() {
                bail!("image upload failed: {}", resp.status());
            }

            // Step 5: Upload metadata.
            let metadata = json!({
                "name": name, "description": description,
                "external_url": &client_url, "terms_signature": "placeholder",
                "properties": {
                    "agreements": [{"content_hash": agreement_hash, "mime_type": "application/json", "type": "POI_ASSIGNMENT", "url": format!("ipfs://{}", agreement_cid)}],
                    "initial_symbol": symbol,
                    "project_details": {
                        "funding_amount": {"value":0,"currency":"USD","currency_type":"ISO4217","decimals":2},
                        "organization": organization,
                        "research_lead": {"name": lead_name, "email": lead_email},
                        "topic": topic
                    }
                }
            });
            let resp = ipnft_graphql(&client, &gql_url, &api_key,
                "mutation UploadMetadataWithImageKey($metadata: AWSJSON!, $imageKey: String!, $ipnftId: String!) { uploadMetadataWithImageKey(metadata: $metadata, imageKey: $imageKey, ipnftId: $ipnftId) { metadataCid metadataUrl isSuccess error { message code retryable } } }",
                json!({"metadata": metadata.to_string(), "imageKey": image_key, "ipnftId": reservation_id.to_string()}),
            ).await?;
            let node = &resp["data"]["uploadMetadataWithImageKey"];
            ensure_success(node, "uploadMetadataWithImageKey")?;
            let metadata_cid = node["metadataCid"].as_str()
                .ok_or_else(|| anyhow::anyhow!("missing metadataCid"))?.to_string();

            // Step 6: Get terms message.
            let resp = ipnft_graphql(&client, &gql_url, &api_key,
                "query GetTermsMessage($metadataCid: String!, $minter: String!, $chainId: Int!) { getTermsMessage(metadataCid: $metadataCid, minter: $minter, chainId: $chainId) { message digest isSuccess error { message code retryable } } }",
                json!({"metadataCid": &metadata_cid, "minter": &wallet, "chainId": CHAIN_ID as i64}),
            ).await?;
            let node = &resp["data"]["getTermsMessage"];
            ensure_success(node, "getTermsMessage")?;
            let terms_message = node["message"].as_str()
                .ok_or_else(|| anyhow::anyhow!("missing terms message"))?.to_string();

            // Step 7: Sign terms (EIP-191 personal_sign, v = 27/28 to match viem).
            let sig = signer.sign_message(terms_message.as_bytes()).await.context("signing failed")?;
            let v = if sig.v() { 28u8 } else { 27u8 };
            let mut sig_bytes = [0u8; 65];
            sig_bytes[..32].copy_from_slice(&sig.r().to_be_bytes::<32>());
            sig_bytes[32..64].copy_from_slice(&sig.s().to_be_bytes::<32>());
            sig_bytes[64] = v;
            let signature = format!("0x{}", alloy::primitives::hex::encode(sig_bytes));

            // Step 8: Sign off metadata.
            let resp = ipnft_graphql(&client, &gql_url, &api_key,
                "mutation SignoffMetadata($ipnftId: String!, $tokenURI: String!, $chainId: Int!, $minter: String!, $to: String!, $termsSignature: String!) { signoffMetadata(ipnftId: $ipnftId, tokenURI: $tokenURI, chainId: $chainId, minter: $minter, to: $to, termsSignature: $termsSignature) { authorization isSuccess error { message code retryable } } }",
                json!({
                    "ipnftId": reservation_id.to_string(),
                    "tokenURI": format!("ipfs://{}", metadata_cid),
                    "chainId": CHAIN_ID as i64,
                    "minter": &wallet, "to": &wallet,
                    "termsSignature": signature
                }),
            ).await?;
            let node = &resp["data"]["signoffMetadata"];
            ensure_success(node, "signoffMetadata")?;
            let authorization = node["authorization"].as_str()
                .ok_or_else(|| anyhow::anyhow!("missing authorization"))?.to_string();

            // Step 9: Mint (ABI-encoded mintReservation call).
            let auth_hex = authorization.strip_prefix("0x").unwrap_or(&authorization);
            let auth_bytes = alloy::primitives::hex::decode(auth_hex).context("invalid authorization hex")?;
            let mint_call = mintReservationCall {
                to: Address::from_str(&wallet).context("invalid wallet address")?,
                reservationId: reservation_id,
                tokenURI: format!("ipfs://{}", metadata_cid),
                symbol: symbol.to_string(),
                authorization: Bytes::from(auth_bytes),
            };
            let calldata = format!("0x{}", alloy::primitives::hex::encode(mint_call.abi_encode()));
            let (mint_tx, mint_ok, _) = send_tx(
                &signer, &rpc_url, IPNFT_CONTRACT, Some(&calldata), Some(MINT_FEE_WEI),
            ).await?;
            if !mint_ok {
                bail!("mint reverted: {}", mint_tx);
            }

            Ok::<(U256, String, String), anyhow::Error>((reservation_id, mint_tx, metadata_cid))
        })?;

        let project_url = format!(
            "{}/ipnfts/{}",
            client_url.trim_end_matches('/'),
            reservation_id
        );

        let envelope = ToolResultEnvelope::ok(
            "mint_ipnft",
            format!("Minted IP-NFT {} on Sepolia.", symbol),
        )
        .with_id("mint.reservation_id", reservation_id.to_string())
        .with_id("mint.token_id", reservation_id.to_string())
        .with_id("mint.ipnft_symbol", symbol.to_string())
        .with_hash("mint.poi_tx_hash", poi_tx_hash.clone())
        .with_hash("mint.mint_tx", mint_tx.clone())
        .with_hash("mint.metadata_cid", metadata_cid.clone())
        .with_url("mint.project_url", project_url.clone())
        .with_artifact("mint.wallet_address", wallet)?
        .with_provenance(
            "molecule.ipnft-mint",
            image_path
                .as_ref()
                .map(|p| vec![p.clone()])
                .unwrap_or_default(),
            vec![
                "testnet.molecule.xyz".to_string(),
                "staging.graphql.api.molecule.xyz".to_string(),
            ],
        )
        .with_raw_response(json!({
            "success": true,
            "reservation_id": reservation_id.to_string(),
            "mint_tx": mint_tx,
            "metadata_cid": metadata_cid,
            "project_url": project_url
        }));

        self.finalize_tool_result(&envelope, audit_path)
    }

    fn execute_create_molecule_project(
        &self,
        call: &tengu_core::types::ToolCall,
    ) -> Result<String> {
        let ipnft_symbol = required_str(call, "ipnft_symbol")?;
        let ipnft_token_id = required_str(call, "ipnft_token_id")?;
        let audit_path = optional_str(call, "audit_path");
        let response = self.run_async(async {
            molecule_graphql(
                &self.client,
                "mutation CreateProject($input: CreateProjectInput!) { createProject(input: $input) { isSuccess message error { message code retryable } project { ipnftUid ipnftSymbol ipnftAddress ipnftTokenId } } }",
                json!({"input": {"ipnftSymbol": ipnft_symbol, "ipnftTokenId": ipnft_token_id}}),
            )
            .await
        })?;
        let node = &response["data"]["createProject"];
        ensure_success(node, "createProject")?;
        let project = &node["project"];
        let ipnft_uid = project["ipnftUid"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("createProject response missing project.ipnftUid"))?;
        let token_id = project["ipnftTokenId"].as_str().ok_or_else(|| {
            anyhow::anyhow!("createProject response missing project.ipnftTokenId")
        })?;
        let client_url = std::env::var("MOLECULE_CLIENT_URL")
            .map_err(|_| anyhow::anyhow!("Missing environment variable MOLECULE_CLIENT_URL"))?;
        let project_url = format!("{}/ipnfts/{}", client_url.trim_end_matches('/'), token_id);

        let envelope = ToolResultEnvelope::ok(
            "create_molecule_project",
            "Created Molecule project data room.",
        )
        .with_id("project.ipnft_uid", ipnft_uid.to_string())
        .with_id(
            "project.ipnft_symbol",
            project["ipnftSymbol"]
                .as_str()
                .unwrap_or_default()
                .to_string(),
        )
        .with_id("project.ipnft_token_id", token_id.to_string())
        .with_artifact(
            "project.ipnft_address",
            project["ipnftAddress"].as_str().unwrap_or_default(),
        )?
        .with_url("project.project_url", project_url)
        .with_provenance(
            "molecule.graphql",
            vec![],
            vec![molecule_labs_host()?, molecule_client_host()?],
        )
        .with_raw_response(response);

        self.finalize_tool_result(&envelope, audit_path)
    }

    fn execute_upload_molecule_file(&self, call: &tengu_core::types::ToolCall) -> Result<String> {
        let ipnft_uid = required_str(call, "ipnft_uid")?;
        let file_path = required_str(call, "file_path")?;
        let content_type = required_str(call, "content_type")?;
        let audit_path = optional_str(call, "audit_path");
        let logical_path = optional_str(call, "path").unwrap_or_else(|| {
            Path::new(file_path)
                .file_name()
                .and_then(|v| v.to_str())
                .unwrap_or("upload.bin")
                .to_string()
        });
        let description = optional_str(call, "description").unwrap_or_else(|| logical_path.clone());
        let change_by = optional_str(call, "change_by")
            .or_else(|| std::env::var("PRIVY_WALLET_ADDRESS").ok())
            .ok_or_else(|| {
                anyhow::anyhow!("upload_molecule_file requires change_by or PRIVY_WALLET_ADDRESS")
            })?;
        let access_level =
            optional_str(call, "access_level").unwrap_or_else(|| DEFAULT_ACCESS_LEVEL.to_string());
        let tags = optional_string_array(call, "tags");
        let categories = optional_string_array(call, "categories");
        let file = crate::adapters::workspace_tools::validate_path(&self.workspace, file_path)?;
        let bytes = std::fs::read(&file)?;
        let content_length = bytes.len();

        let initiate = self.run_async(async {
            molecule_graphql(
                &self.client,
                "mutation InitiateCreateOrUpdateFileV2($ipnftUid: String!, $contentType: String!, $contentLength: Int!) { initiateCreateOrUpdateFileV2(ipnftUid: $ipnftUid, contentType: $contentType, contentLength: $contentLength) { uploadToken uploadUrl uploadUrlExpiry method headers { key value } useMultipart isSuccess error { message code retryable } } }",
                json!({"ipnftUid": ipnft_uid, "contentType": content_type, "contentLength": content_length}),
            )
            .await
        })?;
        let initiate_node = &initiate["data"]["initiateCreateOrUpdateFileV2"];
        ensure_success(initiate_node, "initiateCreateOrUpdateFileV2")?;
        let upload_token = initiate_node["uploadToken"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("initiateCreateOrUpdateFileV2 missing uploadToken"))?;
        let upload_url = initiate_node["uploadUrl"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("initiateCreateOrUpdateFileV2 missing uploadUrl"))?;
        let upload_method = initiate_node["method"]
            .as_str()
            .unwrap_or("PUT")
            .to_uppercase();
        // Collect required headers from the initiate response (S3 presigned URL signing headers).
        let required_headers: Vec<(String, String)> = initiate_node["headers"]
            .as_array()
            .map(|arr| {
                arr.iter()
                    .filter_map(|h| {
                        let key = h["key"].as_str()?;
                        let value = h["value"].as_str()?;
                        Some((key.to_string(), value.to_string()))
                    })
                    .collect()
            })
            .unwrap_or_default();

        let client = self.client.clone();
        self.run_async(async move {
            let mut request = match upload_method.as_str() {
                "POST" => client.post(upload_url),
                _ => client.put(upload_url),
            };
            // Apply all headers from the initiate response (includes Content-Type and S3 signing headers).
            for (key, value) in &required_headers {
                request = request.header(key.as_str(), value.as_str());
            }
            // Set Content-Type if not already provided by the initiate headers.
            if !required_headers.iter().any(|(k, _)| k.eq_ignore_ascii_case("content-type")) {
                request = request.header("Content-Type", content_type);
            }
            let response = request
                .body(bytes)
                .send()
                .await?;
            let status = response.status();
            let text = response.text().await.unwrap_or_default();
            if !status.is_success() {
                bail!("HTTP {} from Molecule upload URL: {}", status, text);
            }
            Ok::<(), anyhow::Error>(())
        })?;

        let finish = self.run_async(async {
            molecule_graphql(
                &self.client,
                "mutation FinishCreateOrUpdateFileV2($ipnftUid: String!, $uploadToken: String!, $path: String, $ref: String, $accessLevel: String!, $changeBy: String!, $description: String, $tags: [String!], $categories: [String!]) { finishCreateOrUpdateFileV2(ipnftUid: $ipnftUid, uploadToken: $uploadToken, path: $path, ref: $ref, accessLevel: $accessLevel, changeBy: $changeBy, description: $description, tags: $tags, categories: $categories) { datasetId contentHash version newHead isSuccess message error { message code retryable } } }",
                json!({
                    "ipnftUid": ipnft_uid,
                    "uploadToken": upload_token,
                    "path": logical_path,
                    "accessLevel": access_level,
                    "changeBy": change_by,
                    "description": description,
                    "tags": tags,
                    "categories": categories
                }),
            )
            .await
        })?;
        let finish_node = &finish["data"]["finishCreateOrUpdateFileV2"];
        ensure_success(finish_node, "finishCreateOrUpdateFileV2")?;
        let dataset_id = finish_node["datasetId"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("finishCreateOrUpdateFileV2 missing datasetId"))?;
        let content_hash = finish_node["contentHash"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("finishCreateOrUpdateFileV2 missing contentHash"))?;

        let envelope = ToolResultEnvelope::ok(
            "upload_molecule_file",
            "Uploaded file into Molecule data room.",
        )
        .with_id("project.dataset_id", dataset_id.to_string())
        .with_hash("project.content_hash", content_hash.to_string())
        .with_artifact("project.upload_token", upload_token)?
        .with_artifact("project.file_path", file_path)?
        .with_provenance(
            "molecule.graphql",
            vec![file_path.to_string()],
            vec![molecule_labs_host()?],
        )
        .with_raw_response(json!({"initiate": initiate, "finish": finish}));

        self.finalize_tool_result(&envelope, audit_path)
    }

    fn execute_create_molecule_announcement(
        &self,
        call: &tengu_core::types::ToolCall,
    ) -> Result<String> {
        let ipnft_uid = required_str(call, "ipnft_uid")?;
        let headline = required_str(call, "headline")?;
        let body = required_str(call, "body")?;
        let attachments = optional_string_array(call, "attachments");
        let audit_path = optional_str(call, "audit_path");

        let response = self.run_async(async {
            molecule_graphql(
                &self.client,
                "mutation CreateAnnouncementV2($ipnftUid: String!, $headline: String!, $body: String!, $attachments: [String!]) { createAnnouncementV2(ipnftUid: $ipnftUid, headline: $headline, body: $body, attachments: $attachments) { isSuccess message error { message code retryable } } }",
                json!({"ipnftUid": ipnft_uid, "headline": headline, "body": body, "attachments": attachments}),
            )
            .await
        })?;
        let node = &response["data"]["createAnnouncementV2"];
        ensure_success(node, "createAnnouncementV2")?;

        let mut envelope = ToolResultEnvelope::ok(
            "create_molecule_announcement",
            format!("Created Molecule announcement: {}", headline),
        )
        .with_provenance("molecule.graphql", vec![], vec![molecule_labs_host()?])
        .with_raw_response(response.clone());
        envelope = envelope.with_artifact("announcement.headline", headline)?;
        if let Some(message) = node["message"].as_str().filter(|m| !m.is_empty()) {
            envelope = envelope.with_artifact("announcement.message", message)?;
        }
        self.finalize_tool_result(&envelope, audit_path)
    }

    fn execute_publish_beach_post(&self, call: &tengu_core::types::ToolCall) -> Result<String> {
        let title = required_str(call, "title")?;
        let body = required_str(call, "body")?;
        let post_type = optional_str(call, "post_type").unwrap_or_else(|| "hypothesis".to_string());
        let audit_path = optional_str(call, "audit_path");
        let api_key = std::env::var("BEACH_SCIENCE_API_KEY")
            .map_err(|_| anyhow::anyhow!("Missing environment variable BEACH_SCIENCE_API_KEY"))?;
        let client = self.client.clone();
        let response = self.run_async(async move {
            let response = client
                .post(BEACH_API_URL)
                .bearer_auth(api_key)
                .json(&json!({
                    "title": title,
                    "body": body,
                    "type": post_type
                }))
                .send()
                .await?;
            let status = response.status();
            let text = response.text().await.unwrap_or_default();
            if !status.is_success() {
                bail!("HTTP {} from Beach.science: {}", status, text);
            }
            let value: serde_json::Value =
                serde_json::from_str(&text).context("Beach.science returned invalid JSON")?;
            Ok::<serde_json::Value, anyhow::Error>(value)
        })?;

        let mut envelope =
            ToolResultEnvelope::ok("publish_beach_post", "Published Beach.science post.")
                .with_provenance("beach.science", vec![], vec!["beach.science".to_string()])
                .with_raw_response(response.clone());
        if let Some(post_id) = response["id"].as_str() {
            envelope = envelope.with_id("beach.post_id", post_id.to_string());
            envelope = envelope.with_url(
                "beach.post_url",
                format!("https://beach.science/posts/{}", post_id),
            );
        }
        if let Some(image_url) = response["image_url"].as_str() {
            envelope = envelope.with_url("beach.image_url", image_url.to_string());
        }
        if let Some(status) = response["image_status"].as_str() {
            envelope = envelope.with_artifact("beach.image_status", status)?;
        }
        self.finalize_tool_result(&envelope, audit_path)
    }

    fn finalize_tool_result(
        &self,
        envelope: &ToolResultEnvelope,
        audit_path: Option<String>,
    ) -> Result<String> {
        let rendered = envelope.to_json_string()?;
        if let Some(audit_path) = audit_path {
            let path =
                crate::adapters::workspace_tools::validate_path(&self.workspace, &audit_path)?;
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(path, &rendered)?;
        }
        Ok(rendered)
    }

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

    let pending = provider
        .send_transaction(tx)
        .await
        .context("tx send failed")?;
    let receipt = pending.get_receipt().await.context("tx receipt failed")?;
    let hash = format!("{}", receipt.transaction_hash);
    let success = receipt.status();
    let logs = receipt.inner.logs().to_vec();
    Ok((hash, success, logs))
}

async fn molecule_graphql(
    client: &reqwest::Client,
    query: &str,
    variables: serde_json::Value,
) -> Result<serde_json::Value> {
    let labs_url = std::env::var("MOLECULE_LABS_URL")
        .map_err(|_| anyhow::anyhow!("Missing environment variable MOLECULE_LABS_URL"))?;
    let api_key = std::env::var("MOLECULE_API_KEY")
        .map_err(|_| anyhow::anyhow!("Missing environment variable MOLECULE_API_KEY"))?;
    let service_token = std::env::var("MOLECULE_SERVICE_TOKEN")
        .map_err(|_| anyhow::anyhow!("Missing environment variable MOLECULE_SERVICE_TOKEN"))?;

    let response = client
        .post(&labs_url)
        .header("Content-Type", "application/json")
        .header("x-api-key", api_key)
        .header("x-service-token", service_token)
        .json(&json!({ "query": query, "variables": variables }))
        .send()
        .await?;
    let status = response.status();
    let text = response.text().await.unwrap_or_default();
    if !status.is_success() {
        bail!("HTTP {} from Molecule GraphQL: {}", status, text);
    }
    Ok(serde_json::from_str(&text).context("Molecule GraphQL returned invalid JSON")?)
}

fn ensure_success(node: &serde_json::Value, op: &str) -> Result<()> {
    if !node["isSuccess"].as_bool().unwrap_or(false) {
        let message = node["error"]["message"]
            .as_str()
            .or_else(|| node["message"].as_str())
            .unwrap_or("unknown");
        bail!("{} failed: {}", op, message);
    }
    Ok(())
}

fn required_str<'a>(call: &'a tengu_core::types::ToolCall, key: &str) -> Result<&'a str> {
    call.arguments
        .get(key)
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow::anyhow!("{}: missing '{}'", call.name, key))
}

fn optional_str(call: &tengu_core::types::ToolCall, key: &str) -> Option<String> {
    call.arguments
        .get(key)
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
}

fn optional_string_array(call: &tengu_core::types::ToolCall, key: &str) -> Vec<String> {
    call.arguments
        .get(key)
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(|s| s.to_string()))
                .collect()
        })
        .unwrap_or_default()
}

fn resolve_optional_workspace_path(workspace: &Path, raw: String) -> Result<String> {
    let path = crate::adapters::workspace_tools::validate_path(workspace, &raw)?;
    Ok(path.to_string_lossy().to_string())
}

async fn ipnft_graphql(
    client: &reqwest::Client,
    url: &str,
    api_key: &str,
    query: &str,
    variables: serde_json::Value,
) -> Result<serde_json::Value> {
    let body = json!({"query": query, "variables": variables});
    let response = client
        .post(url)
        .header("Content-Type", "application/json")
        .header("x-api-key", api_key)
        .json(&body)
        .send()
        .await?;
    let status = response.status();
    let text = response.text().await.unwrap_or_default();
    if !status.is_success() {
        bail!("HTTP {} from Molecule GraphQL: {}", status, text);
    }
    Ok(serde_json::from_str(&text).context("Molecule GraphQL returned invalid JSON")?)
}

fn molecule_labs_host() -> Result<String> {
    let url = std::env::var("MOLECULE_LABS_URL")
        .map_err(|_| anyhow::anyhow!("Missing environment variable MOLECULE_LABS_URL"))?;
    Ok(reqwest::Url::parse(&url)
        .context("invalid MOLECULE_LABS_URL")?
        .host_str()
        .unwrap_or_default()
        .to_string())
}

fn molecule_client_host() -> Result<String> {
    let url = std::env::var("MOLECULE_CLIENT_URL")
        .map_err(|_| anyhow::anyhow!("Missing environment variable MOLECULE_CLIENT_URL"))?;
    Ok(reqwest::Url::parse(&url)
        .context("invalid MOLECULE_CLIENT_URL")?
        .host_str()
        .unwrap_or_default()
        .to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    #[test]
    fn desci_tools_include_native_execute_wrappers() {
        let tools = desci_tool_defs();
        let names: BTreeMap<_, _> = tools
            .iter()
            .map(|tool| (tool.def.name.as_str(), tool.metadata.tool_class))
            .collect();
        assert!(names.contains_key("mint_ipnft"));
        assert!(names.contains_key("create_molecule_project"));
        assert!(names.contains_key("upload_molecule_file"));
        assert!(names.contains_key("create_molecule_announcement"));
        assert!(names.contains_key("publish_beach_post"));
        assert_eq!(names["mint_ipnft"], ToolClass::ExecuteTool);
    }

    #[test]
    fn optional_string_array_handles_missing_values() {
        let call = tengu_core::types::ToolCall {
            id: "1".into(),
            name: "upload_molecule_file".into(),
            arguments: json!({}),
        };
        assert!(optional_string_array(&call, "tags").is_empty());
    }

    #[test]
    fn reservation_id_from_poi_data_is_large() {
        // POI transaction data (merkle root) cast to uint256 should be > u128.
        let data_hex = "c6ca4467b7c69b44ef01d5b3cc5c9f4aa0a25a2db13c78055b8b4cb41bbda676";
        let data_bytes = alloy::primitives::hex::decode(data_hex).unwrap();
        let reservation_id = U256::from_be_slice(&data_bytes);
        assert!(!reservation_id.is_zero());
        assert!(reservation_id > U256::from(u128::MAX));
    }

}
