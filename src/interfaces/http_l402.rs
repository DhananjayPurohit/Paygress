// HTTP Interface with L402 Payment Support via ngx_l402
//
// This module provides L402-aware HTTP endpoints that work with ngx_l402 module.
//
// Flow:
// 1. Client makes request → nginx (with ngx_l402)
// 2. ngx_l402 validates Cashu payment and returns 402 if invalid/missing
// 3. ngx_l402 forwards validated request with header:
//    - Authorization: Cashu <token>
// 4. This backend extracts token from header
// 5. Decodes token to get payment amount in msats
// 6. Calculates pod duration: amount ÷ tier_rate
// 7. Creates pod for calculated duration
//
// Supported header format from ngx_l402:
// - Authorization: Cashu cashuAeyJ0b2tlbiI6...

use anyhow::Result;
use axum::{
    extract::State,
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Json, Response},
    routing::{get, post},
    Router,
};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tracing::{error, info, warn};

use crate::pod_provisioning::PodProvisioningService;

/// L402 payment information extracted from headers
#[derive(Debug, Clone)]
pub struct L402Payment {
    pub token: String,
    pub amount_msats: u64,
}

/// Extract Cashu token and decode amount from request headers
///
/// Supports format from ngx_l402:
/// - Authorization: Cashu <token>
async fn extract_l402_payment(headers: &HeaderMap) -> Result<Option<L402Payment>, String> {
    use crate::sidecar_service::extract_token_value;

    let mut cashu_token: Option<String> = None;

    // Try Authorization header: "Cashu <token>" (ngx_l402 format)
    if let Some(auth_header) = headers.get("authorization") {
        if let Ok(auth_str) = auth_header.to_str() {
            if auth_str.starts_with("Cashu ") || auth_str.starts_with("cashu ") {
                cashu_token = Some(auth_str[6..].trim().to_string());
                info!("✅ Found Cashu token in Authorization header");
            }
        }
    }

    // Try X-Cashu header
    if cashu_token.is_none() {
        if let Some(x_cashu_header) = headers.get("x-cashu") {
            if let Ok(x_cashu_str) = x_cashu_header.to_str() {
                cashu_token = Some(x_cashu_str.trim().to_string());
                info!("✅ Found Cashu token in X-Cashu header");
            }
        }
    }

    // If we found a token, decode it to get the amount
    if let Some(token) = cashu_token {
        match extract_token_value(&token).await {
            Ok(amount_msats) => {
                info!("✅ Decoded Cashu token: {} msats", amount_msats);
                return Ok(Some(L402Payment {
                    token,
                    amount_msats,
                }));
            }
            Err(e) => {
                error!("❌ Failed to decode Cashu token: {}", e);
                return Err(format!("Invalid Cashu token: {}", e));
            }
        }
    }

    Ok(None)
}

/// Run the HTTP interface with L402 support
pub async fn run_http_l402_interface(service: Arc<PodProvisioningService>) -> Result<()> {
    info!("🌐 Starting HTTP interface with L402 support...");

    let bind_addr = std::env::var("HTTP_BIND_ADDR").unwrap_or_else(|_| "0.0.0.0:8080".to_string());

    info!(
        "✅ HTTP+L402 interface ready - listening on http://{}",
        bind_addr
    );

    // Create the router
    let app = Router::new()
        .route("/health", get(health_check))
        .route("/offers", get(get_offers))
        .route("/pods/status", post(get_pod_status))
        .route("/pods/spawn", post(spawn_pod_l402))
        .route("/pods/topup", post(topup_pod_l402))
        .with_state(service);

    // Start the HTTP server
    let listener = tokio::net::TcpListener::bind(&bind_addr)
        .await
        .map_err(|e| anyhow::anyhow!("Failed to bind to {}: {}", bind_addr, e))?;

    axum::serve(listener, app)
        .await
        .map_err(|e| anyhow::anyhow!("HTTP server error: {}", e))?;

    Ok(())
}

/// Health check endpoint
async fn health_check() -> Json<serde_json::Value> {
    Json(serde_json::json!({
        "status": "healthy",
        "service": "paygress-l402",
        "timestamp": chrono::Utc::now().to_rfc3339()
    }))
}

/// Get available offers (no payment required)
async fn get_offers(
    State(service): State<Arc<PodProvisioningService>>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let request = crate::pod_provisioning::GetOffersTool {};

    match service.get_offers(request).await {
        Ok(response) => {
            let offers_json = serde_json::json!({
                "minimum_duration_seconds": response.minimum_duration_seconds,
                "whitelisted_mints": response.whitelisted_mints,
                "pod_specs": response.pod_specs,
                "payment_info": {
                    "accepted_tokens": ["cashu"],
                    "header_format": "Authorization: Cashu <token> OR X-Cashu: <token>"
                }
            });
            Ok(Json(offers_json))
        }
        Err(e) => {
            error!("Failed to get offers: {}", e);
            Err(StatusCode::INTERNAL_SERVER_ERROR)
        }
    }
}

/// Get pod status (no payment required)
async fn get_pod_status(
    State(service): State<Arc<PodProvisioningService>>,
    Json(request): Json<GetPodStatusHttpRequest>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    info!("📨 Received get pod status request via HTTP");

    let status_tool = crate::pod_provisioning::GetPodStatusTool {
        pod_npub: request.pod_npub,
    };

    match service.get_pod_status(status_tool).await {
        Ok(response) => {
            let response_json = serde_json::json!({
                "success": response.success,
                "message": response.message,
                "pod_npub": response.pod_npub,
                "found": response.found,
                "created_at": response.created_at,
                "expires_at": response.expires_at,
                "time_remaining_seconds": response.time_remaining_seconds,
                "pod_spec_name": response.pod_spec_name,
                "cpu_millicores": response.cpu_millicores,
                "memory_mb": response.memory_mb,
                "status": response.status
            });
            Ok(Json(response_json))
        }
        Err(e) => {
            error!("Failed to get pod status: {}", e);
            Err(StatusCode::INTERNAL_SERVER_ERROR)
        }
    }
}

/// Spawn a new pod with L402 payment support
///
/// Note: ngx_l402 validates payment before this endpoint is reached
/// If this function is called, payment has already been verified by nginx
async fn spawn_pod_l402(
    State(service): State<Arc<PodProvisioningService>>,
    headers: HeaderMap,
    Json(mut request): Json<SpawnPodHttpRequest>,
) -> Response {
    info!("📨 Received spawn pod request via HTTP+L402");

    // Extract payment from Authorization: Cashu <token> header (from ngx_l402 or MCP client)
    let extracted_payment = extract_l402_payment(&headers).await;

    match extracted_payment {
        Ok(Some(l402_payment)) => {
            info!(
                "✅ L402 payment from Authorization header: {} msats",
                l402_payment.amount_msats
            );
            request.cashu_token = l402_payment.token.clone();
        }
        Ok(None) => {
            // Check if body has token
            if !request.cashu_token.is_empty() {
                info!("✅ Using Cashu token from request body (direct call, bypassing nginx)");
            } else {
                error!("❌ No payment token found in headers or body");
                return (
                    StatusCode::BAD_REQUEST,
                    Json(serde_json::json!({
                        "error": "Payment token missing",
                        "message": "Provide payment via Authorization header or cashu_token in body"
                    })),
                )
                    .into_response();
            }
        }
        Err(e) => {
            error!("❌ Invalid payment token in header: {}", e);
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({
                    "error": "Invalid Payment Token",
                    "message": e
                })),
            )
                .into_response();
        }
    }

    // Process the spawn request (payment already verified by ngx_l402)
    let spawn_tool = crate::pod_provisioning::SpawnPodTool {
        cashu_token: request.cashu_token,
        pod_spec_id: request.pod_spec_id,
        pod_image: request.pod_image,
        ssh_username: request.ssh_username,
        ssh_password: request.ssh_password,
        user_pubkey: request.user_pubkey,
    };

    match service.spawn_pod(spawn_tool).await {
        Ok(response) => {
            info!(
                "✅ Pod spawned successfully: {}",
                response.pod_npub.as_deref().unwrap_or("unknown")
            );
            let response_json = serde_json::json!({
                "success": response.success,
                "message": response.message,
                "pod_npub": response.pod_npub,
                "ssh_host": response.ssh_host,
                "ssh_port": response.ssh_port,
                "ssh_username": response.ssh_username,
                "ssh_password": response.ssh_password,
                "expires_at": response.expires_at,
                "pod_spec_name": response.pod_spec_name,
                "cpu_millicores": response.cpu_millicores,
                "memory_mb": response.memory_mb,
                "instructions": response.instructions
            });
            (StatusCode::OK, Json(response_json)).into_response()
        }
        Err(e) => {
            error!("❌ Failed to spawn pod: {}", e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({
                    "error": "Failed to spawn pod",
                    "message": e.to_string()
                })),
            )
                .into_response()
        }
    }
}

/// Top up an existing pod with L402 payment support
///
/// Note: ngx_l402 validates payment before this endpoint is reached
async fn topup_pod_l402(
    State(service): State<Arc<PodProvisioningService>>,
    headers: HeaderMap,
    Json(mut request): Json<TopUpPodHttpRequest>,
) -> Response {
    info!("📨 Received topup pod request via HTTP+L402");

    // Extract payment from Authorization: Cashu <token> header (from ngx_l402 or MCP client)
    let extracted_payment = extract_l402_payment(&headers).await;

    match extracted_payment {
        Ok(Some(l402_payment)) => {
            info!(
                "✅ L402 payment from Authorization header for top-up: {} msats",
                l402_payment.amount_msats
            );
            request.cashu_token = l402_payment.token;
        }
        Ok(None) => {
            if !request.cashu_token.is_empty() {
                info!("✅ Using Cashu token from request body for top-up (direct call, bypassing nginx)");
            } else {
                error!("❌ No payment token found in headers or body");
                return (
                    StatusCode::BAD_REQUEST,
                    Json(serde_json::json!({
                        "error": "Payment token missing",
                        "message": "Provide payment via Authorization header or cashu_token in body"
                    })),
                )
                    .into_response();
            }
        }
        Err(e) => {
            error!("❌ Invalid payment token in header: {}", e);
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({
                    "error": "Invalid Payment Token",
                    "message": e
                })),
            )
                .into_response();
        }
    }

    // Process the topup request (payment already verified by ngx_l402)
    let topup_tool = crate::pod_provisioning::TopUpPodTool {
        pod_npub: request.pod_npub,
        cashu_token: request.cashu_token,
    };

    match service.topup_pod(topup_tool).await {
        Ok(response) => {
            info!("✅ Pod topped up successfully: {}", response.pod_npub);
            let response_json = serde_json::json!({
                "success": response.success,
                "message": response.message,
                "pod_npub": response.pod_npub,
                "extended_duration_seconds": response.extended_duration_seconds,
                "new_expires_at": response.new_expires_at
            });
            (StatusCode::OK, Json(response_json)).into_response()
        }
        Err(e) => {
            error!("❌ Failed to topup pod: {}", e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({
                    "error": "Failed to topup pod",
                    "message": e.to_string()
                })),
            )
                .into_response()
        }
    }
}

/// HTTP request structures
#[derive(Debug, Deserialize)]
struct SpawnPodHttpRequest {
    #[serde(default)]
    pub cashu_token: String,
    pub pod_spec_id: Option<String>,
    pub pod_image: String,
    pub ssh_username: String,
    pub ssh_password: String,
    pub user_pubkey: Option<String>,
}

#[derive(Debug, Deserialize)]
struct TopUpPodHttpRequest {
    pub pod_npub: String,
    #[serde(default)]
    pub cashu_token: String,
}

#[derive(Debug, Deserialize)]
struct GetPodStatusHttpRequest {
    pub pod_npub: String,
}
