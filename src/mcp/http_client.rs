// HTTP Client for MCP Server with L402 Support
//
// This module provides HTTP client functionality for the MCP server
// to call paywalled HTTP endpoints using L402 (Lightning HTTP 402) protocol.

use anyhow::{anyhow, Result};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tracing::{error, info, warn};

/// HTTP client for calling paywalled endpoints
pub struct PaywalledHttpClient {
    client: Client,
    base_url: String,
    /// Optional pre-shared L402 token for authentication
    l402_token: Option<String>,
}

impl PaywalledHttpClient {
    /// Create a new HTTP client
    pub fn new(base_url: String, l402_token: Option<String>) -> Self {
        Self {
            client: Client::new(),
            base_url,
            l402_token,
        }
    }

    /// Get offers from the HTTP API
    pub async fn get_offers(&self) -> Result<Value> {
        let url = format!("{}/offers", self.base_url);
        self.make_request("GET", &url, None).await
    }

    /// Get pod status
    pub async fn get_pod_status(&self, pod_npub: String) -> Result<Value> {
        let url = format!("{}/pods/status", self.base_url);
        let body = serde_json::json!({
            "pod_npub": pod_npub
        });
        self.make_request("POST", &url, Some(body)).await
    }

    /// Spawn a new pod
    pub async fn spawn_pod(&self, request: SpawnPodRequest) -> Result<Value> {
        let url = format!("{}/pods/spawn", self.base_url);
        let body = serde_json::to_value(&request)?;
        self.make_request("POST", &url, Some(body)).await
    }

    /// Top up an existing pod
    pub async fn topup_pod(&self, pod_npub: String, cashu_token: String) -> Result<Value> {
        let url = format!("{}/pods/topup", self.base_url);
        let body = serde_json::json!({
            "pod_npub": pod_npub,
            "cashu_token": cashu_token
        });
        self.make_request("POST", &url, Some(body)).await
    }

    /// Make an HTTP request with L402 support
    async fn make_request(&self, method: &str, url: &str, body: Option<Value>) -> Result<Value> {
        info!("🌐 Making {} request to: {}", method, url);

        let mut request_builder = match method {
            "GET" => self.client.get(url),
            "POST" => self.client.post(url),
            _ => return Err(anyhow!("Unsupported HTTP method: {}", method)),
        };

        // Add L402 token if available (using Cashu format for ngx_l402 compatibility)
        if let Some(token) = &self.l402_token {
            request_builder = request_builder.header("Authorization", format!("Cashu {}", token));
        }

        // Add body for POST requests
        if let Some(body) = body {
            request_builder = request_builder.json(&body);
        }

        // Send request
        let response = request_builder.send().await?;

        // Handle L402 Payment Required (402 status)
        if response.status() == 402 {
            let www_authenticate = response
                .headers()
                .get("www-authenticate")
                .and_then(|v| v.to_str().ok())
                .unwrap_or("");

            warn!("⚡ L402 Payment Required:");
            warn!("   WWW-Authenticate: {}", www_authenticate);
            warn!("   ");
            warn!("   To make this request, you need to:");
            warn!("   1. Pay the Lightning invoice from WWW-Authenticate header");
            warn!("   2. Get the L402 token (preimage)");
            warn!("   3. Set HTTP_L402_TOKEN environment variable");
            warn!("   ");

            return Err(anyhow!(
                "L402 Payment Required. WWW-Authenticate: {}",
                www_authenticate
            ));
        }

        // Check for other errors
        if !response.status().is_success() {
            let status = response.status();
            let error_text = response
                .text()
                .await
                .unwrap_or_else(|_| "Unknown error".to_string());
            error!(
                "❌ HTTP request failed with status {}: {}",
                status, error_text
            );
            return Err(anyhow!("HTTP request failed: {} - {}", status, error_text));
        }

        // Parse response
        let response_json: Value = response.json().await?;
        info!("✅ HTTP request successful");

        Ok(response_json)
    }
}

/// Request structure for spawning a pod
#[derive(Debug, Serialize, Deserialize)]
pub struct SpawnPodRequest {
    pub cashu_token: String,
    pub pod_spec_id: Option<String>,
    pub pod_image: String,
    pub ssh_username: String,
    pub ssh_password: String,
    pub user_pubkey: Option<String>,
}
