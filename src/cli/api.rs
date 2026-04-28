// API client for Paygress HTTP endpoints

use anyhow::{anyhow, Result};
use reqwest::Client;
use serde::{Deserialize, Serialize};

/// API client for interacting with Paygress server
pub struct PaygressClient {
    client: Client,
    base_url: String,
}

/// Response from spawn endpoint
#[derive(Debug, Deserialize)]
pub struct SpawnResponse {
    pub success: bool,
    pub pod_id: Option<String>,
    pub ssh_host: Option<String>,
    pub ssh_port: Option<u16>,
    pub ssh_username: Option<String>,
    pub expires_at: Option<String>,
    pub duration_seconds: Option<u64>,
    pub message: Option<String>,
    pub error: Option<String>,
}

/// Response from topup endpoint
#[derive(Debug, Deserialize)]
pub struct TopupResponse {
    pub success: bool,
    pub pod_id: Option<String>,
    pub new_expires_at: Option<String>,
    pub added_seconds: Option<u64>,
    pub message: Option<String>,
    pub error: Option<String>,
}

/// Response from status endpoint
#[derive(Debug, Deserialize)]
pub struct StatusResponse {
    pub success: bool,
    pub pod_id: Option<String>,
    pub status: Option<String>,
    pub ssh_host: Option<String>,
    pub ssh_port: Option<u16>,
    pub ssh_username: Option<String>,
    pub expires_at: Option<String>,
    pub time_remaining_seconds: Option<i64>,
    pub message: Option<String>,
    pub error: Option<String>,
}

/// Pod offer/tier information
#[derive(Debug, Deserialize, Serialize)]
pub struct PodOffer {
    pub id: String,
    pub name: String,
    pub description: String,
    pub cpu_millicores: u32,
    pub memory_mb: u32,
    pub rate_msats_per_sec: u64,
}

/// Response from offers endpoint
#[derive(Debug, Deserialize)]
pub struct OffersResponse {
    pub success: bool,
    pub offers: Option<Vec<PodOffer>>,
    pub mint_urls: Option<Vec<String>>,
    pub error: Option<String>,
}

/// Health check response
#[derive(Debug, Deserialize)]
pub struct HealthResponse {
    pub status: String,
    pub service: Option<String>,
}

/// Spawn request payload
#[derive(Debug, Serialize)]
pub struct SpawnRequest {
    pub pod_spec_id: String,
    pub pod_image: String,
    pub ssh_username: String,
    pub ssh_password: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cashu_token: Option<String>,
}

/// Topup request payload
#[derive(Debug, Serialize)]
pub struct TopupRequest {
    pub pod_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cashu_token: Option<String>,
}

/// Status request payload
#[derive(Debug, Serialize)]
pub struct PodStatusRequest {
    pub pod_id: String,
}

impl PaygressClient {
    /// Create a new API client
    pub fn new(base_url: &str) -> Self {
        Self {
            client: Client::new(),
            base_url: base_url.trim_end_matches('/').to_string(),
        }
    }

    /// Check server health
    pub async fn health(&self) -> Result<HealthResponse> {
        let url = format!("{}/health", self.base_url);
        let response = self
            .client
            .get(&url)
            .send()
            .await
            .map_err(|e| anyhow!("Failed to connect to server: {}", e))?;

        if !response.status().is_success() {
            return Err(anyhow!("Server returned error: {}", response.status()));
        }

        response
            .json()
            .await
            .map_err(|e| anyhow!("Failed to parse response: {}", e))
    }

    /// Get available offers
    pub async fn get_offers(&self) -> Result<OffersResponse> {
        let url = format!("{}/offers", self.base_url);
        let response = self
            .client
            .get(&url)
            .send()
            .await
            .map_err(|e| anyhow!("Failed to connect to server: {}", e))?;

        if !response.status().is_success() {
            return Err(anyhow!("Server returned error: {}", response.status()));
        }

        response
            .json()
            .await
            .map_err(|e| anyhow!("Failed to parse response: {}", e))
    }

    /// Spawn a new pod
    pub async fn spawn_pod(&self, request: SpawnRequest) -> Result<SpawnResponse> {
        let url = format!("{}/pods/spawn", self.base_url);

        let mut req_builder = self.client.post(&url);

        // Add Cashu token as Authorization header if provided
        if let Some(ref token) = request.cashu_token {
            req_builder = req_builder.header("Authorization", format!("Cashu {}", token));
        }

        let response = req_builder
            .json(&request)
            .send()
            .await
            .map_err(|e| anyhow!("Failed to connect to server: {}", e))?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            return Err(anyhow!("Server returned error {}: {}", status, body));
        }

        response
            .json()
            .await
            .map_err(|e| anyhow!("Failed to parse response: {}", e))
    }

    /// Top up an existing pod
    pub async fn topup_pod(&self, request: TopupRequest) -> Result<TopupResponse> {
        let url = format!("{}/pods/topup", self.base_url);

        let mut req_builder = self.client.post(&url);

        // Add Cashu token as Authorization header if provided
        if let Some(ref token) = request.cashu_token {
            req_builder = req_builder.header("Authorization", format!("Cashu {}", token));
        }

        let response = req_builder
            .json(&request)
            .send()
            .await
            .map_err(|e| anyhow!("Failed to connect to server: {}", e))?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            return Err(anyhow!("Server returned error {}: {}", status, body));
        }

        response
            .json()
            .await
            .map_err(|e| anyhow!("Failed to parse response: {}", e))
    }

    /// Get pod status
    pub async fn get_pod_status(&self, pod_id: &str) -> Result<StatusResponse> {
        let url = format!("{}/pods/status", self.base_url);

        let request = PodStatusRequest {
            pod_id: pod_id.to_string(),
        };

        let response = self
            .client
            .post(&url)
            .json(&request)
            .send()
            .await
            .map_err(|e| anyhow!("Failed to connect to server: {}", e))?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            return Err(anyhow!("Server returned error {}: {}", status, body));
        }

        response
            .json()
            .await
            .map_err(|e| anyhow!("Failed to parse response: {}", e))
    }
}
