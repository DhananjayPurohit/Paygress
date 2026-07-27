// Provider HTTP interface, served behind the `ngx_l402` nginx module and
// enabled by `http_bind_addr` in the provider config.
//
// ngx_l402 returns 402 without a valid Cashu token, redeems the token at the
// mint, and forwards the request with the token still in the Authorization
// header. These handlers MUST NOT contact the mint again — the token is
// already spent — so they only decode its face value via `extract_token_value`.

use anyhow::Result;
use axum::{
    extract::State,
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Json, Response},
    routing::{get, post},
    Router,
};
use serde::Deserialize;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::Mutex;
use tracing::{error, info, warn};

use crate::cashu::extract_token_value;
use crate::compute::{ComputeBackend, ContainerConfig};
use crate::durable_workload::{
    DurableWorkload, ReplicationMode, RestartPolicy, WorkloadState, WorkloadStateMachine,
};
use crate::provider::{
    generate_password, parse_pod_npub, ProviderConfig, ProviderStats, WorkloadInfo,
};

/// State shared with the Nostr-DM handler in `provider`.
///
/// There is deliberately no `redeemer` field: ngx_l402 has already redeemed the
/// token, so mint interaction here would double-spend.
#[derive(Clone)]
pub(crate) struct ProviderHttpState {
    pub(crate) config: ProviderConfig,
    pub(crate) backend: Arc<dyn ComputeBackend>,
    pub(crate) active_workloads: Arc<Mutex<HashMap<u32, WorkloadInfo>>>,
    pub(crate) stats: Arc<Mutex<ProviderStats>>,
    pub(crate) state_machine: Arc<Mutex<WorkloadStateMachine>>,
    pub(crate) provider_npub: String,
}

pub(crate) async fn run_provider_http_interface(
    state: ProviderHttpState,
    bind_addr: &str,
) -> Result<()> {
    info!(
        "🌐 Starting provider HTTP+ngx_l402 interface on {}",
        bind_addr
    );

    let app = Router::new()
        .route("/health", get(health_check))
        .route("/offers", get(get_offers))
        .route("/pods/spawn", post(spawn_pod))
        .route("/pods/topup", post(topup_pod))
        .with_state(state);

    let listener = tokio::net::TcpListener::bind(bind_addr)
        .await
        .map_err(|e| {
            anyhow::anyhow!(
                "failed to bind provider HTTP+ngx_l402 interface to {}: {}",
                bind_addr,
                e
            )
        })?;

    info!(
        "✅ Provider HTTP+ngx_l402 interface ready — http://{}",
        bind_addr
    );

    axum::serve(listener, app)
        .await
        .map_err(|e| anyhow::anyhow!("provider HTTP server error: {}", e))?;

    Ok(())
}

/// ngx_l402 injects the validated token as `Authorization: Cashu <token>`.
/// `X-Cashu: <token>` is also accepted for calls that bypass nginx.
fn extract_cashu_token(headers: &HeaderMap) -> Option<String> {
    if let Some(auth) = headers.get("authorization") {
        if let Ok(s) = auth.to_str() {
            // `get(..6)` rather than `s[..6]`: the header is caller-supplied
            // and a multi-byte codepoint straddling byte 6 would panic on a
            // direct slice.
            if let Some(prefix) = s.get(..6) {
                if prefix.eq_ignore_ascii_case("cashu ") {
                    return Some(s[6..].trim().to_string());
                }
            }
        }
    }
    if let Some(xc) = headers.get("x-cashu") {
        if let Ok(s) = xc.to_str() {
            return Some(s.trim().to_string());
        }
    }
    None
}

/// Header token, falling back to the body for calls that bypass nginx.
fn payment_token(headers: &HeaderMap, body_token: Option<String>) -> Option<String> {
    extract_cashu_token(headers).or_else(|| body_token.filter(|t| !t.is_empty()))
}

fn payment_required_response() -> Response {
    (
        StatusCode::PAYMENT_REQUIRED,
        Json(serde_json::json!({
            "error": "payment_required",
            "message": "Provide payment via Authorization: Cashu <token> header"
        })),
    )
        .into_response()
}

/// Face value of an already-redeemed token, in msats. `Err` holds the response
/// to return verbatim. Never contacts the mint.
async fn decode_payment_msats(token: &str, endpoint: &str) -> Result<u64, Response> {
    extract_token_value(token).await.map_err(|e| {
        error!(
            "[HTTP] {}: failed to decode Cashu token value: {}",
            endpoint, e
        );
        (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({
                "error": "invalid_token",
                "message": format!("Could not decode token value: {}", e)
            })),
        )
            .into_response()
    })
}

async fn health_check() -> Json<serde_json::Value> {
    Json(serde_json::json!({
        "status": "healthy",
        "service": "paygress-provider",
        "timestamp": chrono::Utc::now().to_rfc3339(),
    }))
}

async fn get_offers(State(state): State<ProviderHttpState>) -> Json<serde_json::Value> {
    Json(serde_json::json!({
        "provider_name": state.config.provider_name,
        "location": state.config.provider_location,
        "specs": state.config.specs,
        "whitelisted_mints": state.config.whitelisted_mints,
        "minimum_duration_seconds": state.config.minimum_duration_seconds,
        "payment_info": {
            "accepted_tokens": ["cashu"],
            "header_format": "Authorization: Cashu <token>  OR  X-Cashu: <token>"
        }
    }))
}

/// Spawn a container, paid for by the token ngx_l402 already redeemed.
async fn spawn_pod(
    State(state): State<ProviderHttpState>,
    headers: HeaderMap,
    Json(request): Json<SpawnPodRequest>,
) -> Response {
    info!("📨 [HTTP] spawn request received");

    let Some(cashu_token) = payment_token(&headers, request.cashu_token) else {
        warn!("[HTTP] spawn: no Cashu token provided");
        return payment_required_response();
    };

    let payment_msats = match decode_payment_msats(&cashu_token, "spawn").await {
        Ok(v) => v,
        Err(resp) => return resp,
    };

    let spec = match state
        .config
        .specs
        .iter()
        .find(|s| request.pod_spec_id.as_deref() == Some(s.id.as_str()))
        .or_else(|| state.config.specs.first())
    {
        Some(s) => s.clone(),
        None => {
            error!("[HTTP] spawn: no pod specs configured on this provider");
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(serde_json::json!({
                    "error": "no_specs",
                    "message": "No pod specs configured on this provider"
                })),
            )
                .into_response();
        }
    };

    let duration_secs = payment_msats / spec.rate_msats_per_sec;
    if duration_secs < state.config.minimum_duration_seconds {
        let required = state.config.minimum_duration_seconds * spec.rate_msats_per_sec;
        warn!(
            "[HTTP] spawn: insufficient payment ({} msats, need {})",
            payment_msats, required
        );
        return (
            StatusCode::PAYMENT_REQUIRED,
            Json(serde_json::json!({
                "error": "insufficient_payment",
                "message": format!(
                    "Need {} msats for minimum {}s at spec '{}'; received {} msats",
                    required,
                    state.config.minimum_duration_seconds,
                    spec.name,
                    payment_msats
                )
            })),
        )
            .into_response();
    }

    let id = match state
        .backend
        .find_available_id(state.config.vmid_range_start, state.config.vmid_range_end)
        .await
    {
        Ok(id) => id,
        Err(e) => {
            error!("[HTTP] spawn: no available VMID: {}", e);
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(serde_json::json!({
                    "error": "no_capacity",
                    "message": "No available capacity; try again later"
                })),
            )
                .into_response();
        }
    };

    let password = generate_password();
    let host_port = state.config.ssh_host_port(id);

    let container_config = ContainerConfig {
        id,
        name: format!("paygress-{}", id),
        image: request
            .pod_image
            .as_deref()
            .unwrap_or("ubuntu:22.04")
            .to_string(),
        cpu_cores: (spec.cpu_millicores / 1000).max(1) as u32,
        memory_mb: spec.memory_mb as u32,
        storage_gb: 10,
        password: password.clone(),
        ssh_key: None,
        host_port: Some(host_port),
        template_ports: Vec::new(),
        template_env: HashMap::new(),
        extra_runtime_args: Vec::new(),
        data_path: None,
        volume_encryption_key: None,
    };

    if let Err(e) = state.backend.create_container(&container_config).await {
        error!("[HTTP] spawn: backend create_container failed: {}", e);
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({
                "error": "backend_error",
                "message": e.to_string()
            })),
        )
            .into_response();
    }

    // Register in the shared tables so cleanup_loop, orchestrator_loop and the
    // Nostr DM handler all see the same state.
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();

    let workload = WorkloadInfo {
        vmid: id,
        workload_type: "lxc".to_string(),
        spec_id: spec.id.clone(),
        created_at: now,
        expires_at: now + duration_secs,
        owner_npub: request.requester_npub.unwrap_or_default(),
        replication: ReplicationMode::default(),
        restart_policy: RestartPolicy::default(),
        state_uri: None,
        consumer_workload_id: None,
    };

    state
        .active_workloads
        .lock()
        .await
        .insert(id, workload.clone());

    state.state_machine.lock().await.track(DurableWorkload {
        workload_id: id,
        provider_npub: state.provider_npub.clone(),
        state: WorkloadState::Provisioning { since: now },
        replication: workload.replication.clone(),
        restart_policy: workload.restart_policy,
        state_uri: None,
        created_at: now,
        expires_at: workload.expires_at,
    });

    {
        let mut stats = state.stats.lock().await;
        stats.total_jobs_completed += 1;
    }

    let expires_dt = chrono::DateTime::from_timestamp(workload.expires_at as i64, 0)
        .unwrap_or_default()
        .to_rfc3339();

    info!(
        "✅ [HTTP] container {} spawned via ngx_l402 (spec={}, port={}, expires={})",
        id, spec.name, host_port, expires_dt
    );

    (
        StatusCode::OK,
        Json(serde_json::json!({
            "success": true,
            "pod_id": format!("container-{}", id),
            "ssh_host": state.config.public_ip,
            "ssh_port": host_port,
            "username": "root",
            "password": password,
            "expires_at": expires_dt,
            "duration_seconds": duration_secs,
            "spec_name": spec.name,
            "cpu_millicores": spec.cpu_millicores,
            "memory_mb": spec.memory_mb,
            "instructions": [
                format!("ssh -p {} root@{}", host_port, state.config.public_ip),
                format!("Password: {}", password),
                format!("Expires at: {}", expires_dt),
            ]
        })),
    )
        .into_response()
}

/// Extend an existing container's lease, paid for by the token ngx_l402 already
/// redeemed.
async fn topup_pod(
    State(state): State<ProviderHttpState>,
    headers: HeaderMap,
    Json(request): Json<TopUpPodRequest>,
) -> Response {
    info!("📨 [HTTP] topup request for {}", request.pod_npub);

    let Some(cashu_token) = payment_token(&headers, request.cashu_token) else {
        return payment_required_response();
    };

    let vmid = match parse_pod_npub(&request.pod_npub) {
        Some(v) => v,
        None => {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({
                    "error": "invalid_pod_id",
                    "message": format!(
                        "Cannot parse pod id from '{}'; expected 'container-<N>' or a numeric id",
                        request.pod_npub
                    )
                })),
            )
                .into_response();
        }
    };

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();

    // Snapshot under a brief lock rather than holding it across the decode.
    let (spec_id, current_expires_at) = {
        let lock = state.active_workloads.lock().await;
        match lock.get(&vmid) {
            Some(w) => (w.spec_id.clone(), w.expires_at),
            None => {
                return (
                    StatusCode::NOT_FOUND,
                    Json(serde_json::json!({
                        "error": "not_found",
                        "message": format!("Pod '{}' not found", request.pod_npub)
                    })),
                )
                    .into_response();
            }
        }
    };

    if current_expires_at <= now {
        return (
            StatusCode::GONE,
            Json(serde_json::json!({
                "error": "lease_expired",
                "message": "Pod lease has already expired; spawn a new pod instead"
            })),
        )
            .into_response();
    }

    let spec = match state.config.specs.iter().find(|s| s.id == spec_id) {
        Some(s) => s.clone(),
        None => {
            error!(
                "[HTTP] topup: pod {} references unknown spec '{}'",
                vmid, spec_id
            );
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({
                    "error": "spec_unavailable",
                    "message": "Pod references an unknown spec; provider misconfiguration"
                })),
            )
                .into_response();
        }
    };

    let payment_msats = match decode_payment_msats(&cashu_token, "topup").await {
        Ok(v) => v,
        Err(resp) => return resp,
    };

    let extension_secs = payment_msats / spec.rate_msats_per_sec;
    if extension_secs == 0 {
        return (
            StatusCode::PAYMENT_REQUIRED,
            Json(serde_json::json!({
                "error": "insufficient_payment",
                "message": format!(
                    "{} msats buys 0 seconds at {} msats/sec",
                    payment_msats, spec.rate_msats_per_sec
                )
            })),
        )
            .into_response();
    }

    // Re-check existence: cleanup_loop may have removed the workload since the
    // snapshot above.
    let new_expires_at = {
        let mut lock = state.active_workloads.lock().await;
        match lock.get_mut(&vmid) {
            Some(w) => {
                w.expires_at = w.expires_at.saturating_add(extension_secs);
                w.expires_at
            }
            None => {
                return (
                    StatusCode::GONE,
                    Json(serde_json::json!({
                        "error": "race_lost",
                        "message": "Pod was cleaned up before topup could be applied; \
                                    token has been spent — spawn a new pod"
                    })),
                )
                    .into_response();
            }
        }
    };

    let new_expires_dt = chrono::DateTime::from_timestamp(new_expires_at as i64, 0)
        .unwrap_or_default()
        .to_rfc3339();

    info!(
        "✅ [HTTP] topup applied to {}: +{}s (new expiry: {})",
        request.pod_npub, extension_secs, new_expires_dt
    );

    (
        StatusCode::OK,
        Json(serde_json::json!({
            "success": true,
            "pod_npub": request.pod_npub,
            "extended_duration_seconds": extension_secs,
            "new_expires_at": new_expires_dt,
            "message": format!(
                "Lease extended by {}s ({} msats @ {} msats/sec)",
                extension_secs, payment_msats, spec.rate_msats_per_sec
            )
        })),
    )
        .into_response()
}

#[derive(Debug, Deserialize)]
struct SpawnPodRequest {
    #[serde(default)]
    cashu_token: Option<String>,
    pod_spec_id: Option<String>,
    pod_image: Option<String>,
    requester_npub: Option<String>,
}

#[derive(Debug, Deserialize)]
struct TopUpPodRequest {
    /// Pod identifier returned by spawn, e.g. `container-1000`.
    pod_npub: String,
    #[serde(default)]
    cashu_token: Option<String>,
}
