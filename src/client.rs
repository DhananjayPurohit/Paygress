// Paygress consumer-side SDK (Unit 17 of the 12-month plan).
//
// `PaygressClient` is the canonical Rust SDK for talking to a
// provider over Nostr DMs. It wraps `DiscoveryClient` (read-only
// queries) plus the spawn/topup/status round-trip flows, and
// exposes them as typed methods returning structured `*Outcome`
// enums so embedders don't have to hand-roll JSON parsing.
//
// Today the CLI hand-rolls these flows in `src/cli/commands/{spawn,
// topup,status}.rs`. A follow-up will refactor the CLI to consume
// this SDK; this PR adds the surface so external Rust callers
// (and the in-progress Python wrapper) can use it now.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::discovery::DiscoveryClient;
use crate::nostr::{
    AccessDetailsContent, EncryptedSpawnPodRequest, EncryptedTopUpPodRequest, ErrorResponseContent,
    ProviderInfo, StatusRequestContent, StatusResponseContent, TopUpResponseContent,
};

const DEFAULT_RESPONSE_TIMEOUT_SECS: u64 = 60;
const DEFAULT_MESSAGE_TYPE: &str = "nip04";

/// Builder for a Paygress consumer SDK client. Wraps the existing
/// `DiscoveryClient` with typed write-side operations.
pub struct PaygressClient {
    discovery: DiscoveryClient,
    response_timeout_secs: u64,
    message_type: String,
}

impl PaygressClient {
    /// Construct against the given relays and a Nostr private key
    /// (`nsec1...` or hex). The key is required for any operation
    /// that sends a DM (spawn / topup / status); read-only queries
    /// would also work without one but this constructor unifies the
    /// path so callers don't need two clients.
    pub async fn new(relays: Vec<String>, private_key: String) -> Result<Self> {
        let discovery = DiscoveryClient::new_with_key(relays, private_key).await?;
        Ok(Self {
            discovery,
            response_timeout_secs: DEFAULT_RESPONSE_TIMEOUT_SECS,
            message_type: DEFAULT_MESSAGE_TYPE.to_string(),
        })
    }

    /// Override how long each round-trip waits for a provider
    /// response. Defaults to 60s.
    pub fn with_response_timeout_secs(mut self, secs: u64) -> Self {
        self.response_timeout_secs = secs;
        self
    }

    /// Override the encryption mode used for outbound DMs
    /// (`"nip04"` or `"nip17"`). Defaults to `nip04`. NIP-17
    /// gift-wrap is sender-anonymous but supported by fewer relays.
    pub fn with_message_type(mut self, message_type: impl Into<String>) -> Self {
        self.message_type = message_type.into();
        self
    }

    /// Consumer's npub (handy for receipts).
    pub fn npub(&self) -> String {
        self.discovery.get_npub()
    }

    /// Underlying discovery client for read-only queries.
    pub fn discovery(&self) -> &DiscoveryClient {
        &self.discovery
    }

    /// Discover providers matching an optional filter.
    pub async fn list_offers(
        &self,
        filter: Option<crate::nostr::ProviderFilter>,
    ) -> Result<Vec<ProviderInfo>> {
        self.discovery.list_providers(filter).await
    }

    /// Send a spawn request and wait for the provider's response.
    pub async fn spawn(&self, provider_npub: &str, request: SpawnRequest) -> Result<SpawnOutcome> {
        let payload = EncryptedSpawnPodRequest {
            cashu_token: request.cashu_token,
            pod_spec_id: request.pod_spec_id,
            pod_image: request.pod_image,
            ssh_username: request.ssh_username,
            ssh_password: request.ssh_password,
            template_slug: None,
        };
        let json = serde_json::to_string(&payload)?;
        self.send_and_parse(provider_npub, json, parse_spawn_response)
            .await
    }

    /// Send a top-up request and wait for the provider's response.
    pub async fn topup(&self, provider_npub: &str, request: TopupRequest) -> Result<TopupOutcome> {
        let payload = EncryptedTopUpPodRequest {
            pod_npub: request.pod_id,
            cashu_token: request.cashu_token,
        };
        let json = serde_json::to_string(&payload)?;
        self.send_and_parse(provider_npub, json, parse_topup_response)
            .await
    }

    /// Send a status query and wait for the provider's response.
    pub async fn status(&self, provider_npub: &str, pod_id: String) -> Result<StatusOutcome> {
        let payload = StatusRequestContent { pod_id };
        let json = serde_json::to_string(&payload)?;
        self.send_and_parse(provider_npub, json, parse_status_response)
            .await
    }

    async fn send_and_parse<T, F>(
        &self,
        provider_npub: &str,
        request_json: String,
        parser: F,
    ) -> Result<T>
    where
        F: FnOnce(&str) -> Result<T>,
    {
        self.discovery
            .nostr()
            .send_encrypted_private_message(provider_npub, request_json, &self.message_type)
            .await
            .context("send DM to provider")?;

        let response = self
            .discovery
            .nostr()
            .wait_for_decrypted_message(provider_npub, self.response_timeout_secs)
            .await
            .context("wait for provider response")?;

        parser(&response.content)
    }
}

// ---------- request payloads ----------

/// Inputs for a spawn request. Maps onto `EncryptedSpawnPodRequest`
/// but the SDK type is the public-facing surface.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SpawnRequest {
    /// Cashu token paying for the workload.
    pub cashu_token: String,
    /// Optional spec id (`basic`, `standard`, ...). Provider's
    /// first spec is used if `None`.
    pub pod_spec_id: Option<String>,
    /// Container image to run.
    pub pod_image: String,
    pub ssh_username: String,
    pub ssh_password: String,
}

/// Inputs for a top-up request.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TopupRequest {
    /// Pod identifier as returned by [`AccessDetailsContent::pod_npub`]
    /// (e.g. `container-1234`).
    pub pod_id: String,
    pub cashu_token: String,
}

// ---------- typed outcomes ----------

/// Result of a spawn round-trip. Anything the provider sent that's
/// neither an `AccessDetailsContent` nor an `ErrorResponseContent`
/// surfaces as `Other(raw)` so callers can keep moving even when a
/// provider speaks an evolved schema.
#[derive(Debug, Clone)]
pub enum SpawnOutcome {
    Success(AccessDetailsContent),
    Error(ErrorResponseContent),
    Other(String),
}

#[derive(Debug, Clone)]
pub enum TopupOutcome {
    Success(TopUpResponseContent),
    Error(ErrorResponseContent),
    Other(String),
}

#[derive(Debug, Clone)]
pub enum StatusOutcome {
    Success(StatusResponseContent),
    Error(ErrorResponseContent),
    Other(String),
}

// ---------- response parsers ----------

/// Try to parse a provider response as an `ErrorResponseContent`.
/// Returns `Some(err)` only when the JSON has the discriminating
/// `error_type` + `message` fields and parses cleanly.
fn try_parse_error(content: &str) -> Option<ErrorResponseContent> {
    let v: serde_json::Value = serde_json::from_str(content).ok()?;
    if v.get("error_type").is_none() || v.get("message").is_none() {
        return None;
    }
    serde_json::from_value(v).ok()
}

pub fn parse_spawn_response(content: &str) -> Result<SpawnOutcome> {
    if let Some(err) = try_parse_error(content) {
        return Ok(SpawnOutcome::Error(err));
    }
    if let Ok(details) = serde_json::from_str::<AccessDetailsContent>(content) {
        return Ok(SpawnOutcome::Success(details));
    }
    Ok(SpawnOutcome::Other(content.to_string()))
}

pub fn parse_topup_response(content: &str) -> Result<TopupOutcome> {
    if let Some(err) = try_parse_error(content) {
        return Ok(TopupOutcome::Error(err));
    }
    if let Ok(resp) = serde_json::from_str::<TopUpResponseContent>(content) {
        return Ok(TopupOutcome::Success(resp));
    }
    Ok(TopupOutcome::Other(content.to_string()))
}

pub fn parse_status_response(content: &str) -> Result<StatusOutcome> {
    if let Some(err) = try_parse_error(content) {
        return Ok(StatusOutcome::Error(err));
    }
    if let Ok(resp) = serde_json::from_str::<StatusResponseContent>(content) {
        return Ok(StatusOutcome::Success(resp));
    }
    Ok(StatusOutcome::Other(content.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn err_json() -> String {
        serde_json::to_string(&ErrorResponseContent {
            error_type: "token_already_spent".to_string(),
            message: "This Cashu token has already been spent".to_string(),
            details: None,
        })
        .unwrap()
    }

    fn access_json() -> String {
        serde_json::to_string(&AccessDetailsContent {
            pod_npub: "container-42".to_string(),
            node_port: 30042,
            expires_at: "2026-04-30T00:00:00Z".to_string(),
            cpu_millicores: 1000,
            memory_mb: 1024,
            pod_spec_name: "Basic".to_string(),
            pod_spec_description: "1 vCPU, 1GB".to_string(),
            instructions: vec!["ssh -p 30042 root@host".to_string()],
        })
        .unwrap()
    }

    fn topup_json() -> String {
        serde_json::to_string(&TopUpResponseContent {
            success: true,
            pod_npub: "container-42".to_string(),
            extended_duration_seconds: 3600,
            new_expires_at: "2026-04-30T01:00:00Z".to_string(),
            message: "extended".to_string(),
        })
        .unwrap()
    }

    fn status_json() -> String {
        serde_json::to_string(&StatusResponseContent {
            pod_id: "42".to_string(),
            status: "Running".to_string(),
            expires_at: "2026-04-30T00:00:00Z".to_string(),
            time_remaining_seconds: 3600,
            cpu_millicores: 1000,
            memory_mb: 1024,
            ssh_host: "1.2.3.4".to_string(),
            ssh_port: 30042,
            ssh_username: "root".to_string(),
        })
        .unwrap()
    }

    #[test]
    fn spawn_success_round_trip() {
        let out = parse_spawn_response(&access_json()).unwrap();
        match out {
            SpawnOutcome::Success(d) => assert_eq!(d.pod_npub, "container-42"),
            other => panic!("expected Success, got {:?}", other),
        }
    }

    #[test]
    fn spawn_error_routes_to_error_variant() {
        let out = parse_spawn_response(&err_json()).unwrap();
        match out {
            SpawnOutcome::Error(e) => {
                assert_eq!(e.error_type, "token_already_spent");
            }
            other => panic!("expected Error, got {:?}", other),
        }
    }

    #[test]
    fn spawn_unknown_payload_routes_to_other() {
        let out = parse_spawn_response(r#"{"weird":"future-thing"}"#).unwrap();
        assert!(matches!(out, SpawnOutcome::Other(_)));
    }

    #[test]
    fn topup_success_round_trip() {
        let out = parse_topup_response(&topup_json()).unwrap();
        match out {
            TopupOutcome::Success(r) => assert_eq!(r.extended_duration_seconds, 3600),
            other => panic!("expected Success, got {:?}", other),
        }
    }

    #[test]
    fn topup_error_routes_to_error_variant() {
        let out = parse_topup_response(&err_json()).unwrap();
        assert!(matches!(out, TopupOutcome::Error(_)));
    }

    #[test]
    fn status_success_round_trip() {
        let out = parse_status_response(&status_json()).unwrap();
        match out {
            StatusOutcome::Success(s) => assert_eq!(s.pod_id, "42"),
            other => panic!("expected Success, got {:?}", other),
        }
    }

    #[test]
    fn status_error_routes_to_error_variant() {
        let out = parse_status_response(&err_json()).unwrap();
        assert!(matches!(out, StatusOutcome::Error(_)));
    }

    #[test]
    fn error_with_details_parses_fully() {
        let payload = serde_json::json!({
            "error_type": "non_whitelisted_mint",
            "message": "Mint https://attacker.example is not accepted",
            "details": "operator-tunable"
        })
        .to_string();
        match parse_spawn_response(&payload).unwrap() {
            SpawnOutcome::Error(e) => {
                assert_eq!(e.error_type, "non_whitelisted_mint");
                assert_eq!(e.details.as_deref(), Some("operator-tunable"));
            }
            other => panic!("expected Error, got {:?}", other),
        }
    }

    #[test]
    fn malformed_json_does_not_panic() {
        // Provider sent something we can't even tokenize.
        let out = parse_topup_response("definitely not json").unwrap();
        assert!(matches!(out, TopupOutcome::Other(_)));
    }
}
