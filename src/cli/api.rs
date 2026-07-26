// HTTP client for the Paygress provider's axum endpoints (`--server` mode).

use anyhow::{anyhow, Result};
use reqwest::{Client, RequestBuilder, Response};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

pub struct PaygressClient {
    client: Client,
    base_url: String,
}

#[derive(Debug, Deserialize)]
pub struct SpawnResponse {
    pub success: bool,
    pub pod_id: Option<String>,
    pub ssh_host: Option<String>,
    pub ssh_port: Option<u16>,
    pub ssh_username: Option<String>,
    pub expires_at: Option<String>,
    pub duration_seconds: Option<u64>,
    pub error: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct TopupResponse {
    pub success: bool,
    pub pod_id: Option<String>,
    pub new_expires_at: Option<String>,
    pub added_seconds: Option<u64>,
    pub message: Option<String>,
    pub error: Option<String>,
}

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
    pub error: Option<String>,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct PodOffer {
    pub id: String,
    pub name: String,
    pub description: String,
    pub cpu_millicores: u32,
    pub memory_mb: u32,
    pub rate_msats_per_sec: u64,
}

#[derive(Debug, Deserialize)]
pub struct OffersResponse {
    pub success: bool,
    pub offers: Option<Vec<PodOffer>>,
    pub mint_urls: Option<Vec<String>>,
    pub error: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct SpawnRequest {
    pub pod_spec_id: String,
    pub pod_image: String,
    pub ssh_username: String,
    pub ssh_password: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cashu_token: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct TopupRequest {
    pub pod_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cashu_token: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct PodStatusRequest {
    pub pod_id: String,
}

/// Attach the Cashu token as an `Authorization` header when present.
fn with_cashu_auth(builder: RequestBuilder, token: Option<&String>) -> RequestBuilder {
    match token {
        Some(t) => builder.header("Authorization", format!("Cashu {}", t)),
        None => builder,
    }
}

async fn parse_json<T: DeserializeOwned>(response: Response) -> Result<T> {
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

impl PaygressClient {
    pub fn new(base_url: &str) -> Self {
        Self {
            client: Client::new(),
            base_url: base_url.trim_end_matches('/').to_string(),
        }
    }

    async fn send(&self, builder: RequestBuilder) -> Result<Response> {
        builder
            .send()
            .await
            .map_err(|e| anyhow!("Failed to connect to server: {}", e))
    }

    /// Liveness probe. Errors carry the server's status code.
    pub async fn health(&self) -> Result<()> {
        let url = format!("{}/health", self.base_url);
        let response = self.send(self.client.get(&url)).await?;
        if !response.status().is_success() {
            return Err(anyhow!("Server returned error: {}", response.status()));
        }
        Ok(())
    }

    pub async fn get_offers(&self) -> Result<OffersResponse> {
        let url = format!("{}/offers", self.base_url);
        let response = self.send(self.client.get(&url)).await?;
        if !response.status().is_success() {
            return Err(anyhow!("Server returned error: {}", response.status()));
        }
        response
            .json()
            .await
            .map_err(|e| anyhow!("Failed to parse response: {}", e))
    }

    pub async fn spawn_pod(&self, request: SpawnRequest) -> Result<SpawnResponse> {
        let url = format!("{}/pods/spawn", self.base_url);
        let builder =
            with_cashu_auth(self.client.post(&url), request.cashu_token.as_ref()).json(&request);
        parse_json(self.send(builder).await?).await
    }

    pub async fn topup_pod(&self, request: TopupRequest) -> Result<TopupResponse> {
        let url = format!("{}/pods/topup", self.base_url);
        let builder =
            with_cashu_auth(self.client.post(&url), request.cashu_token.as_ref()).json(&request);
        parse_json(self.send(builder).await?).await
    }

    pub async fn get_pod_status(&self, pod_id: &str) -> Result<StatusResponse> {
        let url = format!("{}/pods/status", self.base_url);
        let request = PodStatusRequest {
            pod_id: pod_id.to_string(),
        };
        let builder = self.client.post(&url).json(&request);
        parse_json(self.send(builder).await?).await
    }
}
