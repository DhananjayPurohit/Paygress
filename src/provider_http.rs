// Provider HTTP Interface with ngx_l402 Support
//
// Exposes HTTP endpoints for the Proxmox / LXD / KVM provider that are
// compatible with the `ngx_l402` nginx module.
//
// Payment flow (single-redemption, no double-spend risk):
//   Client
//     → nginx  (ngx_l402 issues 402 if no valid Cashu token; verifies
//               the token format, checks the amount against
//               `l402_amount_msat_default`, redeems it at the Cashu mint,
//               records the token hash in Redis to prevent replay, then
//               forwards the request with the token still in
//               `Authorization: Cashu <token>`)
//     → POST paygress:8080/pods/spawn  OR  POST paygress:8080/pods/topup
//     → this axum backend: decodes the token's face value from the header
//               (no mint call — ngx_l402 already redeemed it), then
//               provisions / extends the container via the configured
//               backend (Proxmox / LXD / KVM).
//
// The backend MUST NOT call the Cashu mint again — the token is already
// spent. `extract_token_value()` reads the proof amounts from the decoded
// token without any network I/O.
//
// Endpoints:
//   GET  /health        — liveness probe (no payment)
//   GET  /offers        — available pod specs and pricing (no payment)
//   POST /pods/spawn    — spawn a new container (ngx_l402 paywall)
//   POST /pods/topup    — extend an existing lease (ngx_l402 paywall)
//
// The nginx config in nginx/conf.d/paygress-l402.conf already targets
// port 8080 with the correct `l402 on` directives — enabling
// `http_bind_addr` in the provider config is all that is needed to
// activate this path.

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
use crate::provider::{generate_password, parse_pod_npub, ProviderConfig, ProviderStats, WorkloadInfo};

// ─── Shared state ─────────────────────────────────────────────────────────────

/// State shared between the Nostr-DM handler and the HTTP+ngx_l402
/// interface. All fields are Arc-wrapped so the struct is cheaply
/// cloneable into axum's `State<_>` extractor.
///
/// There is intentionally **no** `redeemer` field here: ngx_l402 fully
/// redeems the Cashu token at the nginx layer before forwarding the
/// request. The backend only decodes the token's face value from the
/// forwarded header — no mint interaction required or safe.
#[derive(Clone)]
pub(crate) struct ProviderHttpState {
    pub(crate) config: ProviderConfig,
    pub(crate) backend: Arc<dyn ComputeBackend>,
    pub(crate) active_workloads: Arc<Mutex<HashMap<u32, WorkloadInfo>>>,
    pub(crate) stats: Arc<Mutex<ProviderStats>>,
    pub(crate) state_machine: Arc<Mutex<WorkloadStateMachine>>,
    /// Provider's Nostr public key, stored here so the HTTP handler can
    /// record it in the state-machine's `DurableWorkload` without needing
    /// a live Nostr connection.
    pub(crate) provider_npub: String,
}

// ─── Entry point ──────────────────────────────────────────────────────────────

/// Start the provider HTTP+ngx_l402 interface.
/// Binds on `bind_addr` and serves until an error occurs or the
/// containing `tokio::select!` in `ProviderService::run()` cancels it.
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

    let listener = tokio::net::TcpListener::bind(bind_addr).await.map_err(|e| {
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

// ─── Header helpers ───────────────────────────────────────────────────────────

/// Extract a Cashu token from request headers.
///
/// ngx_l402 injects the validated token as:
///   `Authorization: Cashu <token>`
///
/// `X-Cashu: <token>` is also accepted for direct calls that bypass
/// nginx (e.g. integration tests, local development).
fn extract_cashu_token(headers: &HeaderMap) -> Option<String> {
    // Authorization: Cashu <token>  (ngx_l402 canonical)
    if let Some(auth) = headers.get("authorization") {
        if let Ok(s) = auth.to_str() {
            // Case-insensitive prefix check so both "Cashu " and "cashu " work.
            if s.len() > 6 && s[..6].eq_ignore_ascii_case("cashu ") {
                return Some(s[6..].trim().to_string());
            }
        }
    }
    // X-Cashu: <token>  (alternative)
    if let Some(xc) = headers.get("x-cashu") {
        if let Ok(s) = xc.to_str() {
            return Some(s.trim().to_string());
        }
    }
    None
}

// ─── Handlers ─────────────────────────────────────────────────────────────────

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

/// Spawn a container with Cashu payment enforced by ngx_l402.
///
/// ngx_l402 intercepts the request, returns HTTP 402 if no valid token
/// is present, verifies the format and amount, redeems the token at the
/// Cashu mint, and forwards to this handler with the token still in
/// `Authorization: Cashu <token>`. This handler decodes the token's
/// face value to compute the container lifetime, then provisions the
/// container via the configured backend (Proxmox / LXD / KVM).
async fn spawn_pod(
    State(state): State<ProviderHttpState>,
    headers: HeaderMap,
    Json(request): Json<SpawnPodRequest>,
) -> Response {
    info!("📨 [HTTP] spawn request received");

    // 1. Extract Cashu token: ngx_l402 header takes priority over the
    //    body fallback (the body fallback enables direct testing without
    //    nginx in front).
    let cashu_token = match extract_cashu_token(&headers)
        .or_else(|| request.cashu_token.filter(|t| !t.is_empty()))
    {
        Some(t) => t,
        None => {
            warn!("[HTTP] spawn: no Cashu token provided");
            return (
                StatusCode::PAYMENT_REQUIRED,
                Json(serde_json::json!({
                    "error": "payment_required",
                    "message": "Provide payment via Authorization: Cashu <token> header"
                })),
            )
                .into_response();
        }
    };

    // 2. Decode the token's face value.
    //    ngx_l402 already verified the token format, checked the amount
    //    against `l402_amount_msat_default`, redeemed it at the Cashu
    //    mint, and stored its hash in Redis — so the token is already
    //    spent. We must NOT call the mint again; we only read the proof
    //    amounts from the decoded token (no network I/O).
    let payment_msats = match extract_token_value(&cashu_token).await {
        Ok(v) => v,
        Err(e) => {
            error!("[HTTP] spawn: failed to decode Cashu token value: {}", e);
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({
                    "error": "invalid_token",
                    "message": format!("Could not decode token value: {}", e)
                })),
            )
                .into_response();
        }
    };

    // 3. Find the requested pod spec; fall back to the first spec if
    //    `pod_spec_id` is absent or doesn't match.
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

    // 4. Calculate container lifetime from the payment amount.
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

    // 5. Allocate a VMID from the configured range.
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

    // 6. Generate credentials and compute the SSH host port.
    let password = generate_password();
    let host_port = match state.config.ssh_port_start {
        Some(start) => start + (id - state.config.vmid_range_start) as u16,
        None => 30000 + (id % 10000) as u16,
    };

    // 7. Provision the container via the configured backend.
    let image = request
        .pod_image
        .as_deref()
        .unwrap_or("ubuntu:22.04")
        .to_string();

    let container_config = ContainerConfig {
        id,
        name: format!("paygress-{}", id),
        image,
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

    // 8. Register the workload in the shared tracking tables so the
    //    cleanup_loop, orchestrator_loop, and Nostr DM handler all see
    //    the same live state.
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
        // HTTP callers may optionally supply their Nostr npub so the
        // cleanup_loop can send a revocation DM if needed; defaults to
        // empty for anonymous HTTP requests.
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

    // Wire into the state machine so the orchestrator loop drives lifecycle
    // transitions (Provisioning → Live → Suspect …) for this workload.
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

/// Extend an existing container's lease with Cashu payment via ngx_l402.
///
/// Same payment flow as spawn: ngx_l402 redeems the token at the nginx
/// layer, then this handler decodes the face value (no mint call) and
/// extends `expires_at` in the shared workloads map.
async fn topup_pod(
    State(state): State<ProviderHttpState>,
    headers: HeaderMap,
    Json(request): Json<TopUpPodRequest>,
) -> Response {
    info!("📨 [HTTP] topup request for {}", request.pod_npub);

    // 1. Extract token.
    let cashu_token = match extract_cashu_token(&headers)
        .or_else(|| request.cashu_token.filter(|t| !t.is_empty()))
    {
        Some(t) => t,
        None => {
            return (
                StatusCode::PAYMENT_REQUIRED,
                Json(serde_json::json!({
                    "error": "payment_required",
                    "message": "Provide payment via Authorization: Cashu <token> header"
                })),
            )
                .into_response();
        }
    };

    // 2. Resolve VMID from the pod identifier.
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

    // 3. Snapshot workload state under a brief read-only lock so we know
    //    the billing rate before decoding the token (avoids holding the
    //    lock across the token-decoding step).
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

    // 4. Resolve the spec (needed to compute the billing rate).
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

    // 5. Decode the token's face value (ngx_l402 already redeemed it).
    //    Same reasoning as spawn: the token is already spent at the mint.
    //    We only extract the proof-amount sum without any network call.
    let payment_msats = match extract_token_value(&cashu_token).await {
        Ok(v) => v,
        Err(e) => {
            error!("[HTTP] topup: failed to decode Cashu token value: {}", e);
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({
                    "error": "invalid_token",
                    "message": format!("Could not decode token value: {}", e)
                })),
            )
                .into_response();
        }
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

    // 6. Apply the extension — re-lock and re-verify existence to guard
    //    against a cleanup_loop race between the snapshot above and now.
    let new_expires_at = {
        let mut lock = state.active_workloads.lock().await;
        match lock.get_mut(&vmid) {
            Some(w) => {
                w.expires_at = w.expires_at.saturating_add(extension_secs);
                w.expires_at
            }
            None => {
                // Pod was cleaned up between our snapshot and this lock;
                // the token is already spent at the mint.
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

// ─── Request types ─────────────────────────────────────────────────────────────

/// Request body for `POST /pods/spawn`.
///
/// The Cashu token is normally injected by ngx_l402 via
/// `Authorization: Cashu <token>` and does not need to be in the body.
/// The body field is provided as a fallback for direct testing without
/// nginx in front.
#[derive(Debug, Deserialize)]
struct SpawnPodRequest {
    /// Cashu token (body fallback; ngx_l402 header takes priority).
    #[serde(default)]
    cashu_token: Option<String>,
    /// Pod spec id (e.g. `"basic"`, `"standard"`).
    /// Defaults to the first configured spec if absent.
    pod_spec_id: Option<String>,
    /// Container image. Defaults to `ubuntu:22.04`.
    pod_image: Option<String>,
    /// Optional Nostr npub of the requester.
    /// Used for ownership tracking so the Nostr DM path can cross-
    /// reference containers spawned via HTTP.
    requester_npub: Option<String>,
}

/// Request body for `POST /pods/topup`.
#[derive(Debug, Deserialize)]
struct TopUpPodRequest {
    /// Pod identifier returned by the spawn endpoint, e.g. `container-1000`.
    pod_npub: String,
    /// Cashu token (body fallback; ngx_l402 header takes priority).
    #[serde(default)]
    cashu_token: Option<String>,
}
