use crate::application::ports::ToolExecutionPort;
use crate::domain::capability::{CapabilityId, EffectClass, RegisteredTool};
use anyhow::{bail, Result};
use reqwest::multipart;
use std::path::PathBuf;

pub(crate) fn desci_tool_defs() -> Vec<RegisteredTool> {
    vec![
        RegisteredTool::new(
            "poi_register_document",
            "Register a Proof of Invention document with the Molecule POI endpoint using a workspace file path.",
            serde_json::json!({
                "type": "object",
                "properties": {
                    "document_path": {
                        "type": "string",
                        "description": "Path to the PDF or source document relative to the workspace root"
                    }
                },
                "required": ["document_path"]
            }),
            CapabilityId::new("desci.poi.register").expect("static capability is valid"),
            EffectClass::ExternalApi,
        ),
        RegisteredTool::new(
            "upload_binary_url",
            "Upload a workspace file to a presigned HTTPS URL with a fixed content type.",
            serde_json::json!({
                "type": "object",
                "properties": {
                    "url": {
                        "type": "string",
                        "description": "Presigned HTTPS URL to upload to"
                    },
                    "file_path": {
                        "type": "string",
                        "description": "Workspace-relative file path to upload"
                    },
                    "content_type": {
                        "type": "string",
                        "description": "HTTP Content-Type header to send with the upload"
                    }
                },
                "required": ["url", "file_path", "content_type"]
            }),
            CapabilityId::new("desci.upload.binary").expect("static capability is valid"),
            EffectClass::ExternalApi,
        ),
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
            "poi_register_document" => {
                let document_path = call
                    .arguments
                    .get("document_path")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| {
                        anyhow::anyhow!("poi_register_document: missing 'document_path'")
                    })?;
                let document = crate::adapters::workspace_tools::validate_path(
                    &self.workspace,
                    document_path,
                )?;
                let file_name = document
                    .file_name()
                    .and_then(|name| name.to_str())
                    .unwrap_or("document.bin")
                    .to_string();
                let bytes = std::fs::read(&document)?;
                let token = std::env::var("POI_API_KEY")
                    .map_err(|_| anyhow::anyhow!("Missing environment variable POI_API_KEY"))?;
                let client = self.client.clone();
                self.run_async(async move {
                    let part = multipart::Part::bytes(bytes).file_name(file_name);
                    let form = multipart::Form::new().part("files", part);
                    let response = client
                        .post("https://testnet.molecule.xyz/api/v1/inventions")
                        .bearer_auth(token)
                        .multipart(form)
                        .send()
                        .await?;
                    let status = response.status();
                    let text = response.text().await.unwrap_or_default();
                    if !status.is_success() {
                        bail!("HTTP {} from POI endpoint: {}", status, text);
                    }
                    Ok(format!("HTTP {} poi_register_document\n{}", status, text))
                })
            }
            "upload_binary_url" => {
                let url = call
                    .arguments
                    .get("url")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| anyhow::anyhow!("upload_binary_url: missing 'url'"))?;
                if !url.starts_with("https://") {
                    bail!("upload_binary_url only accepts https:// URLs");
                }
                let file_path = call
                    .arguments
                    .get("file_path")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| anyhow::anyhow!("upload_binary_url: missing 'file_path'"))?;
                let content_type = call
                    .arguments
                    .get("content_type")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| anyhow::anyhow!("upload_binary_url: missing 'content_type'"))?;
                let file =
                    crate::adapters::workspace_tools::validate_path(&self.workspace, file_path)?;
                let bytes = std::fs::read(&file)?;
                let client = self.client.clone();
                let url = url.to_string();
                let content_type = content_type.to_string();
                self.run_async(async move {
                    let response = client
                        .put(&url)
                        .header("Content-Type", content_type)
                        .body(bytes)
                        .send()
                        .await?;
                    let status = response.status();
                    let text = response.text().await.unwrap_or_default();
                    if !status.is_success() {
                        bail!("HTTP {} from upload URL: {}", status, text);
                    }
                    Ok(format!("HTTP {} upload_binary_url\n{}", status, text))
                })
            }
            other => bail!("Unknown DeSci tool: {}", other),
        }
    }
}
