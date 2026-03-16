use crate::application::ports::ToolExecutionPort;
use crate::domain::capability::{CapabilityId, EffectClass, RegisteredTool, ToolClass};
use crate::domain::tool_result::ToolResultEnvelope;
use alloy::primitives::{Address, Bytes, U256};
use alloy::sol;
use alloy::sol_types::SolCall;
use std::sync::{Arc, Mutex};
use anyhow::{bail, Context, Result};
use reqwest::multipart;
use serde_json::json;
use std::path::{Path, PathBuf};
use std::str::FromStr;

sol! {
    function mintReservation(address to, uint256 reservationId, string tokenURI, string symbol, bytes authorization) external payable returns (uint256);
    function safeTransferFrom(address from, address to, uint256 tokenId);
}

const CHAIN_ID: u64 = 11155111;
const POI_API_URL: &str = "https://testnet.molecule.xyz/api/v1/inventions";
const BEACH_API_URL: &str = "https://beach.science/api/v1/posts";
const DEFAULT_ACCESS_LEVEL: &str = "PUBLIC";
const IPNFT_CONTRACT: &str = "0x152B444e60C526fe4434C721561a077269FcF61a";
const MINT_FEE_WEI: &str = "1000000000000000";
const PRIVY_API_URL: &str = "https://api.privy.io";
const DEFAULT_SEPOLIA_RPC: &str = "https://ethereum-sepolia-rpc.publicnode.com";

static WALLET_ADDRESS_CACHE: Mutex<Option<String>> = Mutex::new(None);
static SERVICE_TOKEN_CACHE: Mutex<Option<String>> = Mutex::new(None);

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
        .with_activity_description("Registering hypothesis document")
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
            "Mint an IP-NFT: submit the POI transaction on-chain, run the Molecule GraphQL metadata flow, and execute the on-chain mint. Returns structured mint outputs.",
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
        .with_activity_description("Minting IP-NFT")
        .with_required_secrets(&["PRIVY_APP_ID", "PRIVY_APP_SECRET", "PRIVY_WALLET_ID", "MOLECULE_API_KEY"])
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
        .with_activity_description("Creating data room")
        .with_required_secrets(&["MOLECULE_API_KEY", "MOLECULE_CLIENT_URL", "PRIVY_APP_ID", "PRIVY_APP_SECRET", "PRIVY_WALLET_ID"])
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
        .with_activity_description("Uploading research file")
        .with_required_secrets(&["MOLECULE_API_KEY", "PRIVY_APP_ID", "PRIVY_APP_SECRET", "PRIVY_WALLET_ID"])
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
        .with_activity_description("Creating announcement")
        .with_required_secrets(&["MOLECULE_API_KEY", "PRIVY_APP_ID", "PRIVY_APP_SECRET", "PRIVY_WALLET_ID"])
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
        .with_activity_description("Publishing Beach.science post")
        .with_required_secrets(&["BEACH_SCIENCE_API_KEY"])
        .with_host_allowlist(&["beach.science"])
        .with_output_schema(json!({"type": "object"})),
        RegisteredTool::new(
            "check_wallet_balance",
            "Check the ETH balance of a wallet address on Sepolia testnet. If no address is provided, checks the Privy agentic wallet address.",
            json!({
                "type": "object",
                "properties": {
                    "address": {
                        "type": "string",
                        "description": "Wallet address to check (0x...). Omit to use the Privy agentic wallet."
                    }
                },
                "required": []
            }),
            CapabilityId::new("desci.wallet.balance").expect("static capability is valid"),
            EffectClass::Read,
        )
        .with_tool_class(ToolClass::ExecuteTool)
        .with_activity_description("Checking wallet balance"),
        RegisteredTool::new(
            "transfer_ipnft",
            "Transfer an IP-NFT from the Privy agentic wallet to the owner wallet (EVM_WALLET_ADDRESS) and add the owner as a Molecule project co-owner. Call this AFTER the full Molecule pipeline (project creation, file upload, announcement) is complete.",
            json!({
                "type": "object",
                "properties": {
                    "token_id": {
                        "type": "string",
                        "description": "The IP-NFT token ID to transfer"
                    },
                    "ipnft_uid": {
                        "type": "string",
                        "description": "The Molecule project ipnft_uid (format: contractAddress_tokenId)"
                    },
                    "audit_path": {
                        "type": "string",
                        "description": "Optional workspace-relative path for the audit result"
                    }
                },
                "required": ["token_id", "ipnft_uid"]
            }),
            CapabilityId::new("desci.ipnft.transfer").expect("static capability is valid"),
            EffectClass::ExternalApi,
        )
        .with_tool_class(ToolClass::ExecuteTool)
        .with_activity_description("Transferring IP-NFT to owner wallet")
        .with_required_secrets(&["PRIVY_APP_ID", "PRIVY_APP_SECRET", "PRIVY_WALLET_ID"])
        .with_output_schema(json!({"type": "object"})),
    ]
}

pub(crate) struct DesciToolExecutionAdapter {
    client: reqwest::Client,
    workspace: PathBuf,
    fallback_runtime: Option<tokio::runtime::Runtime>,
    cancel: Option<Arc<std::sync::atomic::AtomicBool>>,
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
            cancel: None,
        })
    }

    pub(crate) fn with_cancel(mut self, flag: Arc<std::sync::atomic::AtomicBool>) -> Self {
        self.cancel = Some(flag);
        self
    }

    fn is_cancelled(&self) -> bool {
        self.cancel
            .as_ref()
            .map_or(false, |f| f.load(std::sync::atomic::Ordering::Relaxed))
    }

    fn check_cancel(&self) -> Result<()> {
        if self.is_cancelled() {
            bail!("Operation cancelled by /stop");
        }
        Ok(())
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
            "check_wallet_balance" => self.execute_check_wallet_balance(call),
            "transfer_ipnft" => self.execute_transfer_ipnft(call),
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

        let api_key = std::env::var("MOLECULE_API_KEY")
            .map_err(|_| anyhow::anyhow!("Missing environment variable MOLECULE_API_KEY"))?;
        let gql_url = std::env::var("MOLECULE_LABS_URL")
            .map_err(|_| anyhow::anyhow!("Missing environment variable MOLECULE_LABS_URL"))?;
        let client_url = std::env::var("MOLECULE_CLIENT_URL")
            .unwrap_or_else(|_| "https://testnet.molecule.xyz".to_string());

        // Resolve wallet address from Privy agentic wallet.
        let client = self.client.clone();
        let wallet = self.run_async(async { privy_wallet_address(&client).await })?;
        self.check_cancel()?;

        // Pre-check: ensure symbol is available on Molecule (up to 5 alternatives).
        let symbol = self.run_async(async {
            find_available_symbol(&client, symbol, 5).await
        })?;
        self.check_cancel()?;

        // Step 1: Submit the POI transaction on-chain via Privy.
        let poi_tx_hash = self.run_async(async {
            let tx_hash = privy_send_transaction(
                &client, poi_transaction_to, Some(poi_transaction_data), Some("0"),
            ).await?;
            let success = wait_for_receipt(&client, &tx_hash).await?;
            if !success {
                bail!("POI on-chain transaction reverted: {}", tx_hash);
            }
            Ok::<String, anyhow::Error>(tx_hash)
        })?;

        self.check_cancel()?;

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

        self.check_cancel()?;

        // Steps 2-9: Molecule GraphQL flow → on-chain mint via Privy.
        let cancel_flag = self.cancel.clone();
        let check = || -> Result<()> {
            if cancel_flag.as_ref().map_or(false, |f| f.load(std::sync::atomic::Ordering::Relaxed)) {
                bail!("Operation cancelled by /stop");
            }
            Ok(())
        };
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

            check()?;
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

            check()?;
            // Step 6: Get terms message.
            let resp = ipnft_graphql(&client, &gql_url, &api_key,
                "query GetTermsMessage($metadataCid: String!, $minter: String!, $chainId: Int!) { getTermsMessage(metadataCid: $metadataCid, minter: $minter, chainId: $chainId) { message digest isSuccess error { message code retryable } } }",
                json!({"metadataCid": &metadata_cid, "minter": &wallet, "chainId": CHAIN_ID as i64}),
            ).await?;
            let node = &resp["data"]["getTermsMessage"];
            ensure_success(node, "getTermsMessage")?;
            let terms_message = node["message"].as_str()
                .ok_or_else(|| anyhow::anyhow!("missing terms message"))?.to_string();

            // Step 7: Sign terms via Privy personal_sign.
            let signature = privy_personal_sign(&client, &terms_message).await?;

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

            check()?;
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
            let mint_tx = privy_send_transaction(
                &client, IPNFT_CONTRACT, Some(&calldata), Some(MINT_FEE_WEI),
            ).await?;
            let mint_ok = wait_for_receipt(&client, &mint_tx).await?;
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
            .ok_or_else(|| anyhow::anyhow!("createProject response missing project.ipnftUid"))?
            .to_string();
        let token_id = project["ipnftTokenId"]
            .as_str()
            .ok_or_else(|| {
                anyhow::anyhow!("createProject response missing project.ipnftTokenId")
            })?
            .to_string();
        let client_url = std::env::var("MOLECULE_CLIENT_URL")
            .map_err(|_| anyhow::anyhow!("Missing environment variable MOLECULE_CLIENT_URL"))?;
        let project_url = format!("{}/ipnfts/{}", client_url.trim_end_matches('/'), token_id);

        let envelope = ToolResultEnvelope::ok(
            "create_molecule_project",
            "Created Molecule project data room.",
        )
        .with_id("project.ipnft_uid", ipnft_uid.clone())
        .with_id(
            "project.ipnft_symbol",
            project["ipnftSymbol"]
                .as_str()
                .unwrap_or_default()
                .to_string(),
        )
        .with_id("project.ipnft_token_id", token_id.clone())
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

        let upload_bytes_len = bytes.len();
        let client = self.client.clone();
        let s3_upload_result = self.run_async(async move {
            tracing::info!(
                url_len = upload_url.len(),
                method = %upload_method,
                content_length = upload_bytes_len,
                "Uploading {} bytes to presigned S3 URL",
                upload_bytes_len
            );
            let mut request = match upload_method.as_str() {
                "POST" => client.post(upload_url),
                _ => client.put(upload_url),
            };
            // Apply all headers from the initiate response (includes Content-Type and S3 signing headers).
            for (key, value) in &required_headers {
                request = request.header(key.as_str(), value.as_str());
            }
            // Set Content-Type and Content-Length if not already provided by the initiate headers.
            if !required_headers.iter().any(|(k, _)| k.eq_ignore_ascii_case("content-type")) {
                request = request.header("Content-Type", content_type);
            }
            if !required_headers.iter().any(|(k, _)| k.eq_ignore_ascii_case("content-length")) {
                request = request.header("Content-Length", upload_bytes_len.to_string());
            }
            let response = request
                .body(bytes)
                .send()
                .await?;
            let status = response.status();
            let text = response.text().await.unwrap_or_default();
            if !status.is_success() {
                bail!("S3 upload failed: HTTP {} — {}", status, text);
            }
            tracing::info!(status = %status, bytes = upload_bytes_len, "S3 upload completed");
            Ok::<(u16, usize), anyhow::Error>((status.as_u16(), upload_bytes_len))
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
        .with_raw_response(json!({
            "initiate": initiate,
            "s3_upload": {
                "status": s3_upload_result.0,
                "bytes_uploaded": s3_upload_result.1
            },
            "finish": finish
        }));

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

    fn execute_check_wallet_balance(&self, call: &tengu_core::types::ToolCall) -> Result<String> {
        let explicit_address = optional_str(call, "address");
        let client = self.client.clone();
        let (address, balance_wei, balance_eth) = self.run_async(async {
            let address = match explicit_address {
                Some(addr) => addr,
                None => privy_wallet_address(&client).await?,
            };
            let rpc_url = std::env::var("EVM_RPC_URL")
                .unwrap_or_else(|_| DEFAULT_SEPOLIA_RPC.to_string());
            let resp = client
                .post(&rpc_url)
                .json(&json!({
                    "jsonrpc": "2.0",
                    "method": "eth_getBalance",
                    "params": [&address, "latest"],
                    "id": 1
                }))
                .send()
                .await?;
            let body: serde_json::Value = resp.json().await?;
            let hex_balance = body["result"]
                .as_str()
                .ok_or_else(|| anyhow::anyhow!("RPC eth_getBalance returned no result: {:?}", body))?;
            let wei = U256::from_str_radix(
                hex_balance.strip_prefix("0x").unwrap_or(hex_balance), 16,
            ).context("invalid balance hex")?;
            let eth = format_wei_as_eth(wei);
            Ok::<(String, String, String), anyhow::Error>((address, wei.to_string(), eth))
        })?;

        let envelope = ToolResultEnvelope::ok(
            "check_wallet_balance",
            format!("{} ETH (Sepolia)", balance_eth),
        )
        .with_artifact("wallet.address", &address)?
        .with_artifact("wallet.balance_wei", &balance_wei)?
        .with_artifact("wallet.balance_eth", &balance_eth)?
        .with_artifact("wallet.network", "sepolia")?;

        Ok(envelope.to_json_string()?)
    }

    fn execute_transfer_ipnft(&self, call: &tengu_core::types::ToolCall) -> Result<String> {
        let token_id_str = required_str(call, "token_id")?;
        let ipnft_uid = required_str(call, "ipnft_uid")?;
        let audit_path = optional_str(call, "audit_path");

        let owner = std::env::var("EVM_WALLET_ADDRESS")
            .map_err(|_| anyhow::anyhow!("EVM_WALLET_ADDRESS not set — cannot transfer"))?;
        if owner.is_empty() {
            bail!("EVM_WALLET_ADDRESS is empty — cannot transfer");
        }

        let client = self.client.clone();
        let (transfer_tx, add_owner_ok) = self.run_async(async {
            let privy_addr = privy_wallet_address(&client).await?;
            if privy_addr.to_lowercase() == owner.to_lowercase() {
                bail!("Privy wallet and owner wallet are the same — no transfer needed");
            }

            let token_id = U256::from_str_radix(token_id_str, 10)
                .context("invalid token_id — expected decimal string")?;

            // 1. Transfer NFT on-chain.
            tracing::info!(from = %privy_addr, to = %owner, token_id = %token_id, "Transferring IP-NFT to owner wallet");
            let transfer_call = safeTransferFromCall {
                from: Address::from_str(&privy_addr).context("invalid privy wallet address")?,
                to: Address::from_str(&owner).context("invalid owner wallet address")?,
                tokenId: token_id,
            };
            let calldata = format!("0x{}", alloy::primitives::hex::encode(transfer_call.abi_encode()));
            let tx = privy_send_transaction(&client, IPNFT_CONTRACT, Some(&calldata), None).await?;
            let ok = wait_for_receipt(&client, &tx).await?;
            if !ok {
                bail!("NFT transfer reverted: {}", tx);
            }

            // 2. Add owner wallet to Molecule project.
            tracing::info!(ipnft_uid = %ipnft_uid, owner = %owner, "Adding owner to Molecule project");
            let add_resp = molecule_graphql(
                &client,
                "mutation AddProjectOwner($ipnftUid: String!, $ownerAddress: String!) { addProjectOwner(ipnftUid: $ipnftUid, ownerAddress: $ownerAddress) { isSuccess message error { message code retryable } } }",
                json!({"ipnftUid": ipnft_uid, "ownerAddress": owner}),
            ).await;
            let add_ok = match add_resp {
                Ok(resp) => resp["data"]["addProjectOwner"]["isSuccess"].as_bool().unwrap_or(false),
                Err(e) => {
                    tracing::warn!("addProjectOwner failed (non-fatal): {}", e);
                    false
                }
            };

            Ok::<(String, bool), anyhow::Error>((tx, add_ok))
        })?;

        let envelope = ToolResultEnvelope::ok(
            "transfer_ipnft",
            format!("Transferred IP-NFT to {}", owner),
        )
        .with_hash("transfer.tx", transfer_tx.clone())
        .with_artifact("transfer.from", "privy_wallet")?
        .with_artifact("transfer.to", &owner)?
        .with_artifact("transfer.token_id", token_id_str)?
        .with_artifact("transfer.project_owner_added", if add_owner_ok { "true" } else { "false" })?
        .with_raw_response(json!({
            "transfer_tx": transfer_tx,
            "owner_address": owner,
            "project_owner_added": add_owner_ok
        }));

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

// ---------------------------------------------------------------------------
// Privy agentic wallet helpers
// ---------------------------------------------------------------------------

/// Resolve the wallet address from Privy. Cached after first call.
async fn privy_wallet_address(client: &reqwest::Client) -> Result<String> {
    if let Some(cached) = WALLET_ADDRESS_CACHE.lock().unwrap().as_ref() {
        return Ok(cached.clone());
    }

    let app_id = std::env::var("PRIVY_APP_ID")
        .map_err(|_| anyhow::anyhow!("Missing environment variable PRIVY_APP_ID"))?;
    let app_secret = std::env::var("PRIVY_APP_SECRET")
        .map_err(|_| anyhow::anyhow!("Missing environment variable PRIVY_APP_SECRET"))?;
    let wallet_id = std::env::var("PRIVY_WALLET_ID")
        .map_err(|_| anyhow::anyhow!("Missing environment variable PRIVY_WALLET_ID"))?;

    let resp = client
        .get(format!("{}/v1/wallets/{}", PRIVY_API_URL, wallet_id))
        .basic_auth(&app_id, Some(&app_secret))
        .header("privy-app-id", &app_id)
        .send()
        .await?;
    let status = resp.status();
    let body: serde_json::Value = resp.json().await?;
    if !status.is_success() {
        bail!("Privy wallet lookup failed: {} {:?}", status, body);
    }
    let address = body["address"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("Privy response missing address"))?
        .to_string();

    *WALLET_ADDRESS_CACHE.lock().unwrap() = Some(address.clone());
    Ok(address)
}

/// Send an on-chain transaction via Privy agentic wallet. Returns tx hash.
async fn privy_send_transaction(
    client: &reqwest::Client,
    to: &str,
    data: Option<&str>,
    value: Option<&str>,
) -> Result<String> {
    let app_id = std::env::var("PRIVY_APP_ID")
        .map_err(|_| anyhow::anyhow!("Missing environment variable PRIVY_APP_ID"))?;
    let app_secret = std::env::var("PRIVY_APP_SECRET")
        .map_err(|_| anyhow::anyhow!("Missing environment variable PRIVY_APP_SECRET"))?;
    let wallet_id = std::env::var("PRIVY_WALLET_ID")
        .map_err(|_| anyhow::anyhow!("Missing environment variable PRIVY_WALLET_ID"))?;

    // Privy requires 0x-prefixed hex values.
    let hex_value = match value {
        Some(v) => format!("0x{:x}", U256::from_str_radix(v, 10).context("invalid tx value")?),
        None => "0x0".to_string(),
    };
    let mut transaction = json!({ "to": to, "value": hex_value });
    if let Some(d) = data {
        transaction["data"] = json!(d);
    }

    let resp = client
        .post(format!("{}/v1/wallets/{}/rpc", PRIVY_API_URL, wallet_id))
        .basic_auth(&app_id, Some(&app_secret))
        .header("privy-app-id", &app_id)
        .json(&json!({
            "method": "eth_sendTransaction",
            "caip2": format!("eip155:{}", CHAIN_ID),
            "params": { "transaction": transaction }
        }))
        .send()
        .await?;
    let status = resp.status();
    let body: serde_json::Value = resp.json().await?;
    if !status.is_success() {
        bail!("Privy eth_sendTransaction failed: {} {:?}", status, body);
    }
    body["data"]["hash"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("Privy response missing data.hash"))
        .map(String::from)
}

/// Sign a message via Privy personal_sign. Returns the signature hex string.
async fn privy_personal_sign(
    client: &reqwest::Client,
    message: &str,
) -> Result<String> {
    let app_id = std::env::var("PRIVY_APP_ID")
        .map_err(|_| anyhow::anyhow!("Missing environment variable PRIVY_APP_ID"))?;
    let app_secret = std::env::var("PRIVY_APP_SECRET")
        .map_err(|_| anyhow::anyhow!("Missing environment variable PRIVY_APP_SECRET"))?;
    let wallet_id = std::env::var("PRIVY_WALLET_ID")
        .map_err(|_| anyhow::anyhow!("Missing environment variable PRIVY_WALLET_ID"))?;

    let resp = client
        .post(format!("{}/v1/wallets/{}/rpc", PRIVY_API_URL, wallet_id))
        .basic_auth(&app_id, Some(&app_secret))
        .header("privy-app-id", &app_id)
        .json(&json!({
            "method": "personal_sign",
            "params": { "message": message, "encoding": "utf-8" }
        }))
        .send()
        .await?;
    let status = resp.status();
    let body: serde_json::Value = resp.json().await?;
    if !status.is_success() {
        bail!("Privy personal_sign failed: {} {:?}", status, body);
    }
    let raw_sig = body["data"]["signature"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("Privy response missing data.signature"))?;

    // Normalize v: Privy may return v=0/1, Molecule expects v=27/28.
    let hex = raw_sig.strip_prefix("0x").unwrap_or(raw_sig);
    let mut sig_bytes = alloy::primitives::hex::decode(hex).context("invalid signature hex")?;
    if sig_bytes.len() == 65 && sig_bytes[64] < 27 {
        sig_bytes[64] += 27;
    }
    Ok(format!("0x{}", alloy::primitives::hex::encode(sig_bytes)))
}

/// Wait for a transaction receipt. Uses EVM_RPC_URL or a public Sepolia endpoint.
async fn wait_for_receipt(client: &reqwest::Client, tx_hash: &str) -> Result<bool> {
    let rpc_url = std::env::var("EVM_RPC_URL")
        .unwrap_or_else(|_| DEFAULT_SEPOLIA_RPC.to_string());

    for _ in 0..90 {
        let resp = client
            .post(&rpc_url)
            .json(&json!({
                "jsonrpc": "2.0",
                "method": "eth_getTransactionReceipt",
                "params": [tx_hash],
                "id": 1
            }))
            .send()
            .await?;
        let body: serde_json::Value = resp.json().await?;
        if let Some(result) = body.get("result") {
            if !result.is_null() {
                let status = result["status"].as_str().unwrap_or("0x0");
                return Ok(status == "0x1");
            }
        }
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
    }
    bail!("Transaction receipt not found after 180s: {}", tx_hash)
}

/// Acquire a Molecule service token via Privy wallet signing.
/// Cached after first successful acquisition.
async fn acquire_service_token(client: &reqwest::Client) -> Result<String> {
    if let Some(cached) = SERVICE_TOKEN_CACHE.lock().unwrap().as_ref() {
        return Ok(cached.clone());
    }

    let labs_url = std::env::var("MOLECULE_LABS_URL")
        .map_err(|_| anyhow::anyhow!("Missing environment variable MOLECULE_LABS_URL"))?;
    let api_key = std::env::var("MOLECULE_API_KEY")
        .map_err(|_| anyhow::anyhow!("Missing environment variable MOLECULE_API_KEY"))?;
    let wallet_address = privy_wallet_address(client).await?;

    // Step A: Get the sign-in message.
    let resp = client
        .post(&labs_url)
        .header("Content-Type", "application/json")
        .header("x-api-key", &api_key)
        .json(&json!({
            "query": "query GetServiceSignInMessage($walletAddress: String!, $serviceName: String!) { getServiceSignInMessage(walletAddress: $walletAddress, serviceName: $serviceName) { message } }",
            "variables": { "walletAddress": &wallet_address, "serviceName": "tengu-agent" }
        }))
        .send()
        .await?;
    let body: serde_json::Value = resp.json().await?;
    let message = body["data"]["getServiceSignInMessage"]["message"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("getServiceSignInMessage returned no message: {:?}", body))?;

    // Step B: Sign with Privy personal_sign.
    let signature = privy_personal_sign(client, message).await?;

    // Step C: Exchange signature for service token.
    let resp = client
        .post(&labs_url)
        .header("Content-Type", "application/json")
        .header("x-api-key", &api_key)
        .json(&json!({
            "query": "mutation GenerateServiceToken($serviceName: String!, $expiresIn: String!, $walletAddress: String, $messageSignature: String) { generateServiceToken(serviceName: $serviceName, expiresIn: $expiresIn, walletAddress: $walletAddress, messageSignature: $messageSignature) { token isSuccess message } }",
            "variables": {
                "serviceName": "tengu-agent",
                "expiresIn": "720h",
                "walletAddress": &wallet_address,
                "messageSignature": &signature
            }
        }))
        .send()
        .await?;
    let body: serde_json::Value = resp.json().await?;
    let token = body["data"]["generateServiceToken"]["token"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("generateServiceToken returned no token: {:?}", body))?
        .to_string();

    *SERVICE_TOKEN_CACHE.lock().unwrap() = Some(token.clone());
    Ok(token)
}

fn invalidate_service_token_cache() {
    *SERVICE_TOKEN_CACHE.lock().unwrap() = None;
}

/// Check if a symbol is already taken by an existing Molecule project.
/// Uses the public `projectsV2` query (no auth required).
async fn is_symbol_taken(client: &reqwest::Client, symbol: &str) -> Result<bool> {
    let labs_url = std::env::var("MOLECULE_LABS_URL")
        .map_err(|_| anyhow::anyhow!("Missing environment variable MOLECULE_LABS_URL"))?;
    let api_key = std::env::var("MOLECULE_API_KEY")
        .map_err(|_| anyhow::anyhow!("Missing environment variable MOLECULE_API_KEY"))?;
    let upper = symbol.to_uppercase();
    let mut page = 0;
    loop {
        let resp = client
            .post(&labs_url)
            .header("Content-Type", "application/json")
            .header("x-api-key", &api_key)
            .json(&json!({
                "query": "query GetProjectsV2($page: Int, $perPage: Int) { projectsV2(page: $page, perPage: $perPage) { nodes { ipnftSymbol } pageInfo { hasNextPage } } }",
                "variables": {"page": page, "perPage": 100}
            }))
            .send()
            .await?;
        let body: serde_json::Value = resp.json().await?;
        let nodes = body["data"]["projectsV2"]["nodes"]
            .as_array()
            .ok_or_else(|| anyhow::anyhow!("projectsV2 returned no nodes: {:?}", body))?;
        for node in nodes {
            if let Some(s) = node["ipnftSymbol"].as_str() {
                if s.to_uppercase() == upper {
                    return Ok(true);
                }
            }
        }
        let has_next = body["data"]["projectsV2"]["pageInfo"]["hasNextPage"]
            .as_bool()
            .unwrap_or(false);
        if !has_next {
            break;
        }
        page += 1;
    }
    Ok(false)
}

/// Find an available symbol, appending numeric suffixes if needed.
/// Returns the original symbol if available, otherwise tries symbol2, symbol3, ... up to max_attempts.
async fn find_available_symbol(
    client: &reqwest::Client,
    base_symbol: &str,
    max_attempts: usize,
) -> Result<String> {
    // Try original symbol first.
    if !is_symbol_taken(client, base_symbol).await? {
        tracing::info!(symbol = %base_symbol, "Symbol is available");
        return Ok(base_symbol.to_string());
    }
    tracing::warn!(symbol = %base_symbol, "Symbol already taken, trying alternatives");

    for i in 2..=(max_attempts + 1) {
        let candidate = format!("{}{}", base_symbol, i);
        if !is_symbol_taken(client, &candidate).await? {
            tracing::info!(symbol = %candidate, "Found available symbol");
            return Ok(candidate);
        }
        tracing::warn!(symbol = %candidate, "Also taken");
    }
    bail!(
        "All symbol variants taken ({} through {}{}). Choose a different base symbol.",
        base_symbol,
        base_symbol,
        max_attempts + 1
    )
}

// ---------------------------------------------------------------------------
// Molecule GraphQL helpers
// ---------------------------------------------------------------------------

async fn molecule_graphql(
    client: &reqwest::Client,
    query: &str,
    variables: serde_json::Value,
) -> Result<serde_json::Value> {
    let labs_url = std::env::var("MOLECULE_LABS_URL")
        .map_err(|_| anyhow::anyhow!("Missing environment variable MOLECULE_LABS_URL"))?;
    let api_key = std::env::var("MOLECULE_API_KEY")
        .map_err(|_| anyhow::anyhow!("Missing environment variable MOLECULE_API_KEY"))?;

    // Use explicit env var if set, otherwise auto-acquire via Privy signing.
    let service_token = match std::env::var("MOLECULE_SERVICE_TOKEN") {
        Ok(token) if !token.is_empty() => token,
        _ => acquire_service_token(client).await?,
    };

    let response = client
        .post(&labs_url)
        .header("Content-Type", "application/json")
        .header("x-api-key", &api_key)
        .header("x-service-token", &service_token)
        .json(&json!({ "query": query, "variables": variables }))
        .send()
        .await?;
    let status = response.status();
    let text = response.text().await.unwrap_or_default();

    // Token expired — invalidate cache, acquire fresh token, retry once.
    if status == reqwest::StatusCode::UNAUTHORIZED || status == reqwest::StatusCode::FORBIDDEN {
        // Only retry if we were using a cached/auto-acquired token (not an explicit env var).
        if std::env::var("MOLECULE_SERVICE_TOKEN").map_or(true, |t| t.is_empty()) {
            tracing::warn!("Service token expired, re-acquiring via Privy");
            invalidate_service_token_cache();
            let fresh_token = acquire_service_token(client).await?;
            let retry = client
                .post(&labs_url)
                .header("Content-Type", "application/json")
                .header("x-api-key", &api_key)
                .header("x-service-token", &fresh_token)
                .json(&json!({ "query": query, "variables": variables }))
                .send()
                .await?;
            let retry_status = retry.status();
            let retry_text = retry.text().await.unwrap_or_default();
            if !retry_status.is_success() {
                bail!("HTTP {} from Molecule GraphQL (after token refresh): {}", retry_status, retry_text);
            }
            return Ok(serde_json::from_str(&retry_text).context("Molecule GraphQL returned invalid JSON")?);
        }
    }

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

/// Format U256 wei value as human-readable ETH string.
fn format_wei_as_eth(wei: U256) -> String {
    let divisor = U256::from(1_000_000_000_000_000_000u64);
    let whole = wei / divisor;
    let frac = wei % divisor;
    // Show 6 decimal places.
    let frac_scaled = frac * U256::from(1_000_000u64) / divisor;
    format!("{}.{:06}", whole, frac_scaled.to::<u64>())
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
