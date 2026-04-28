// Provider Service
//
// Runs on machine operator's server to:
// - Publish provider offer to Nostr
// - Send periodic heartbeats
// - Listen for and handle spawn requests

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::Mutex;
use tracing::{debug, error, info, warn};

use crate::cashu::{
    derive_seed_from_nostr_key, validate_and_redeem, CdkRedeemer, MintRedeemer, RedeemError,
};
use crate::compute::{ComputeBackend, ContainerConfig};
use crate::lxd::LxdBackend;
use crate::nostr::{
    parse_private_message_content, AccessDetailsContent, CapacityInfo, EncryptedSpawnPodRequest,
    ErrorResponseContent, HeartbeatContent, NostrRelaySubscriber, PodSpec, PrivateRequest,
    ProviderOfferContent, RelayConfig, StatusRequestContent, StatusResponseContent,
};
use crate::proxmox::{ProxmoxBackend, ProxmoxClient};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum BackendType {
    Proxmox,
    LXD,
}

impl Default for BackendType {
    fn default() -> Self {
        Self::Proxmox
    }
}

/// Provider configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderConfig {
    #[serde(default)]
    pub backend_type: BackendType,

    // Proxmox / Backend settings
    pub proxmox_url: String,
    pub proxmox_token_id: String,
    pub proxmox_token_secret: String,
    pub proxmox_node: String,
    pub proxmox_storage: String,
    pub proxmox_template: String,
    pub proxmox_bridge: String,
    pub vmid_range_start: u32,
    pub vmid_range_end: u32,

    // Nostr settings
    pub nostr_private_key: String,
    pub nostr_relays: Vec<String>,

    // Provider metadata
    pub provider_name: String,
    pub provider_location: Option<String>,
    pub public_ip: String,
    pub capabilities: Vec<String>,

    // Pricing & specs
    pub specs: Vec<PodSpec>,
    pub whitelisted_mints: Vec<String>,

    // Operational settings
    pub heartbeat_interval_secs: u64,
    pub minimum_duration_seconds: u64,

    // Tunnel settings (for providers behind NAT)
    #[serde(default)]
    pub tunnel_enabled: bool,
    #[serde(default)]
    pub tunnel_interface: Option<String>,
    #[serde(default)]
    pub ssh_port_start: Option<u16>,
    #[serde(default)]
    pub ssh_port_end: Option<u16>,

    // Cashu wallet settings (Unit 1: real mint redemption on the
    // Nostr-DM path). The wallet stores swapped proofs, keysets, and
    // quotes; one redb file holds state for every mint the provider
    // accepts. Defaults to a path next to the binary so existing
    // operators don't need to update their config.
    #[serde(default = "default_cashu_wallet_db_path")]
    pub cashu_wallet_db_path: String,
}

fn default_cashu_wallet_db_path() -> String {
    "./paygress-cashu-wallet.redb".to_string()
}

impl Default for ProviderConfig {
    fn default() -> Self {
        Self {
            backend_type: BackendType::Proxmox,
            proxmox_url: "https://localhost:8006/api2/json".to_string(),
            proxmox_token_id: "root@pam!paygress".to_string(),
            proxmox_token_secret: String::new(),
            proxmox_node: "pve".to_string(),
            proxmox_storage: "local-lvm".to_string(),
            proxmox_template: "local:vztmpl/ubuntu-22.04-standard.tar.zst".to_string(),
            proxmox_bridge: "vmbr0".to_string(),
            vmid_range_start: 1000,
            vmid_range_end: 1999,
            nostr_private_key: String::new(),
            nostr_relays: vec![
                "wss://relay.damus.io".to_string(),
                "wss://nos.lol".to_string(),
            ],
            provider_name: "Paygress Provider".to_string(),
            provider_location: None,
            public_ip: "127.0.0.1".to_string(),
            capabilities: vec!["lxc".to_string()],
            specs: vec![PodSpec {
                id: "basic".to_string(),
                name: "Basic".to_string(),
                description: "1 vCPU, 1GB RAM".to_string(),
                cpu_millicores: 1000,
                memory_mb: 1024,
                rate_msats_per_sec: 50,
            }],
            whitelisted_mints: vec!["https://mint.minibits.cash".to_string()],
            heartbeat_interval_secs: 60,
            minimum_duration_seconds: 60,
            tunnel_enabled: false,
            tunnel_interface: None,
            ssh_port_start: None,
            ssh_port_end: None,
            cashu_wallet_db_path: default_cashu_wallet_db_path(),
        }
    }
}

/// Active workload tracking
#[derive(Debug, Clone, Serialize)]
pub struct WorkloadInfo {
    pub vmid: u32,
    pub workload_type: String, // "lxc" or "vm"
    pub spec_id: String,
    pub created_at: u64,
    pub expires_at: u64,
    pub owner_npub: String,
}

/// Provider service that manages the node
pub struct ProviderService {
    config: ProviderConfig,
    backend: Arc<dyn ComputeBackend>,
    nostr: NostrRelaySubscriber,
    redeemer: Arc<dyn MintRedeemer>,
    active_workloads: Arc<Mutex<HashMap<u32, WorkloadInfo>>>,
    stats: Arc<Mutex<ProviderStats>>,
}

#[derive(Debug, Clone, Default)]
struct ProviderStats {
    total_jobs_completed: u64,
    uptime_start: u64,
}

impl ProviderService {
    /// Create a new provider service
    pub async fn new(config: ProviderConfig) -> Result<Self> {
        let backend: Arc<dyn ComputeBackend> = match config.backend_type {
            BackendType::Proxmox => {
                let client = ProxmoxClient::new(
                    &config.proxmox_url,
                    &config.proxmox_token_id,
                    &config.proxmox_token_secret,
                    &config.proxmox_node,
                )?;
                Arc::new(ProxmoxBackend::new(
                    client,
                    &config.proxmox_storage,
                    &config.proxmox_bridge,
                    &config.proxmox_template,
                ))
            }
            BackendType::LXD => Arc::new(LxdBackend::new(
                &config.proxmox_storage, // Reuse storage field for pool name
                &config.proxmox_bridge,  // Reuse bridge for network
            )),
        };

        // Initialize Nostr client
        let relay_config = RelayConfig {
            relays: config.nostr_relays.clone(),
            private_key: Some(config.nostr_private_key.clone()),
        };
        let nostr = NostrRelaySubscriber::new(relay_config).await?;

        // Initialize the Cashu redeemer. Wallet identity is derived
        // deterministically from the provider's Nostr private key so
        // the same provider sees a consistent proof history across
        // restarts. The redb file holds proofs, keysets, and quotes
        // for every mint this provider accepts.
        let wallet_db = cdk_redb::wallet::WalletRedbDatabase::new(std::path::Path::new(
            &config.cashu_wallet_db_path,
        ))
        .map_err(|e| {
            anyhow::anyhow!(
                "failed to open cashu wallet database at {}: {}",
                config.cashu_wallet_db_path,
                e
            )
        })?;
        let seed = derive_seed_from_nostr_key(&config.nostr_private_key);
        let redeemer: Arc<dyn MintRedeemer> = Arc::new(CdkRedeemer::new(Arc::new(wallet_db), seed));

        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_secs();

        Ok(Self {
            config,
            backend,
            nostr,
            redeemer,
            active_workloads: Arc::new(Mutex::new(HashMap::new())),
            stats: Arc::new(Mutex::new(ProviderStats {
                total_jobs_completed: 0,
                uptime_start: now,
            })),
        })
    }

    /// Get the provider's public key (npub)
    pub fn get_npub(&self) -> String {
        self.nostr.get_service_public_key()
    }

    /// Start the provider service (runs forever)
    pub async fn run(&self) -> Result<()> {
        info!("🚀 Starting Paygress Provider Service");
        info!("Provider: {}", self.config.provider_name);
        info!("NPUB: {}", self.get_npub());

        // Publish initial offer
        self.publish_offer().await?;

        // Run heartbeat loop and request listener concurrently
        tokio::select! {
            result = self.heartbeat_loop() => {
                error!("Heartbeat loop exited: {:?}", result);
                result
            }
            result = self.listen_for_requests() => {
                error!("Request listener exited: {:?}", result);
                result
            }
            result = self.cleanup_loop() => {
                error!("Cleanup loop exited: {:?}", result);
                result
            }
        }
    }

    /// Publish provider offer to Nostr
    async fn publish_offer(&self) -> Result<()> {
        let stats = self.stats.lock().await;

        let offer = ProviderOfferContent {
            provider_npub: self.get_npub(),
            hostname: self.config.provider_name.clone(),
            location: self.config.provider_location.clone(),
            capabilities: self.config.capabilities.clone(),
            specs: self.config.specs.clone(),
            whitelisted_mints: self.config.whitelisted_mints.clone(),
            uptime_percent: 100.0, // Will be calculated from heartbeat history
            total_jobs_completed: stats.total_jobs_completed,
            api_endpoint: None, // TODO: Add if supporting direct API
            version: crate::nostr::SCHEMA_VERSION,
            isolation_level: crate::nostr::IsolationLevel::SharedKernel,
        };

        self.nostr.publish_provider_offer(offer).await?;
        Ok(())
    }

    /// Send heartbeat every N seconds
    async fn heartbeat_loop(&self) -> Result<()> {
        let interval = tokio::time::Duration::from_secs(self.config.heartbeat_interval_secs);

        loop {
            if let Err(e) = self.send_heartbeat().await {
                warn!("Failed to send heartbeat: {}", e);
            }
            tokio::time::sleep(interval).await;
        }
    }

    /// Send a single heartbeat
    async fn send_heartbeat(&self) -> Result<()> {
        let workloads = self.active_workloads.lock().await;

        // Get node status for capacity info
        let capacity = match self.backend.get_node_status().await {
            Ok(status) => CapacityInfo {
                cpu_available: ((1.0 - status.cpu_usage) * 100000.0) as u64, // Convert to millicores
                memory_mb_available: status.memory_total.saturating_sub(status.memory_used)
                    / (1024 * 1024),
                storage_gb_available: status.disk_total.saturating_sub(status.disk_used)
                    / (1024 * 1024 * 1024),
            },
            Err(e) => {
                warn!("Failed to get node status: {}", e);
                CapacityInfo {
                    cpu_available: 0,
                    memory_mb_available: 0,
                    storage_gb_available: 0,
                }
            }
        };

        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_secs();

        let heartbeat = HeartbeatContent {
            provider_npub: self.get_npub(),
            timestamp: now,
            active_workloads: workloads.len() as u32,
            available_capacity: capacity,
            version: crate::nostr::SCHEMA_VERSION,
        };

        self.nostr.publish_heartbeat(heartbeat).await?;
        Ok(())
    }

    /// Listen for spawn requests via NIP-17
    async fn listen_for_requests(&self) -> Result<()> {
        info!("Listening for Paygress requests...");

        // Clone what we need for the handler
        let backend = self.backend.clone();
        let config = self.config.clone();
        let nostr = self.nostr.clone();
        let redeemer = self.redeemer.clone();
        let workloads = self.active_workloads.clone();
        let stats = self.stats.clone();

        self.nostr
            .subscribe_to_pod_events(move |event| {
                let backend = backend.clone();
                let config = config.clone();
                let nostr = nostr.clone();
                let redeemer = redeemer.clone();
                let workloads = workloads.clone();
                let stats = stats.clone();

                Box::pin(async move {
                    let my_pubkey = nostr.public_key().to_hex();
                    if event.pubkey == my_pubkey {
                        return Ok(());
                    }

                    debug!(
                        "Handler received event kind: {}, from: {}, message_type: {}",
                        event.kind, event.pubkey, event.message_type
                    );

                    // Parse the request
                    let request_type = match parse_private_message_content(&event.content) {
                        Ok(req) => req,
                        Err(e) => {
                            warn!("Failed to parse request from {}: {}", event.pubkey, e);
                            let error = ErrorResponseContent {
                                error_type: "invalid_request".to_string(),
                                message: "Failed to parse request".to_string(),
                                details: Some(e.to_string()),
                            };
                            let _ = nostr
                                .send_error_response_private_message(
                                    &event.pubkey,
                                    error,
                                    &event.message_type,
                                )
                                .await;
                            return Ok(());
                        }
                    };

                    debug!("Successfully parsed request metadata");

                    // Dispatch to specific handler
                    match request_type {
                        PrivateRequest::Spawn(spawn_req) => {
                            if let Err(e) = handle_spawn_request(
                                backend.as_ref(),
                                &config,
                                &nostr,
                                redeemer.as_ref(),
                                &workloads,
                                &stats,
                                &event.pubkey,
                                &event.message_type,
                                spawn_req,
                            )
                            .await
                            {
                                error!("Failed to handle spawn request: {}", e);
                            }
                        }
                        PrivateRequest::Status(status_req) => {
                            if let Err(e) = handle_status_request(
                                backend.as_ref(),
                                &config,
                                &nostr,
                                &workloads,
                                &event.pubkey,
                                &event.message_type,
                                status_req,
                            )
                            .await
                            {
                                error!("Failed to handle status request: {}", e);
                            }
                        }
                        PrivateRequest::TopUp(_) => {
                            warn!("TopUp request received but not yet fully implemented");
                            let _ = nostr
                                .send_error_response(
                                    &event.pubkey,
                                    "not_implemented",
                                    "TopUp is not yet implemented on this provider",
                                    None,
                                    &event.message_type,
                                )
                                .await;
                        }
                    }

                    Ok(())
                })
            })
            .await?;

        Ok(())
    }

    /// Cleanup expired workloads
    async fn cleanup_loop(&self) -> Result<()> {
        let interval = tokio::time::Duration::from_secs(30);

        loop {
            tokio::time::sleep(interval).await;

            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)?
                .as_secs();

            let mut workloads = self.active_workloads.lock().await;
            let expired: Vec<u32> = workloads
                .iter()
                .filter(|(_, w)| w.expires_at <= now)
                .map(|(vmid, _)| *vmid)
                .collect();

            for vmid in expired {
                info!("Cleaning up expired workload: {}", vmid);

                if let Some(_workload) = workloads.remove(&vmid) {
                    let stop_result = self.backend.stop_container(vmid).await;
                    let result = match stop_result {
                        Ok(_) => self.backend.delete_container(vmid).await,
                        Err(e) => Err(e),
                    };

                    match result {
                        Ok(_) => {
                            info!("Cleaned up workload {}", vmid);
                            let mut stats = self.stats.lock().await;
                            stats.total_jobs_completed += 1;
                        }
                        Err(e) => error!("Failed to cleanup workload {}: {}", vmid, e),
                    }
                }
            }
        }
    }
}

// Clone impl removed as ComputeBackend is Arc'd

/// Handle a spawn request.
///
/// Redeems the provided Cashu token at the mint via the supplied
/// `MintRedeemer` (Unit 1 — see docs/plans/...). On any redemption
/// failure (invalid token, non-whitelisted mint, already-spent,
/// pending, network) we reply with a structured error and DO NOT call
/// the backend — no container is created without a successful swap.
async fn handle_spawn_request(
    backend: &dyn ComputeBackend,
    config: &ProviderConfig,
    nostr: &NostrRelaySubscriber,
    redeemer: &dyn MintRedeemer,
    workloads: &Arc<Mutex<HashMap<u32, WorkloadInfo>>>,
    stats: &Arc<Mutex<ProviderStats>>,
    requester_pubkey: &str,
    message_type: &str,
    request: EncryptedSpawnPodRequest,
) -> Result<()> {
    info!(
        "Processing spawn request from {} (tier: {:?})",
        requester_pubkey, request.pod_spec_id
    );

    // 1. Redeem Cashu token at the mint (Unit 1).
    let payment_msats = match validate_and_redeem(
        redeemer,
        &config.whitelisted_mints,
        &request.cashu_token,
    )
    .await
    {
        Ok(v) => v,
        Err(e) => {
            let (error_type, err_msg) = redeem_error_to_response(&e);
            error!("Cashu redemption failed: {}", err_msg);
            nostr
                .send_error_response(requester_pubkey, error_type, &err_msg, None, message_type)
                .await?;
            return Ok(());
        }
    };

    // 2. Find matching spec/tier
    let spec = match config
        .specs
        .iter()
        .find(|s| Some(s.id.clone()) == request.pod_spec_id)
    {
        Some(s) => s,
        None => {
            // Default to first spec if none specified or not found
            if let Some(s) = config.specs.first() {
                s
            } else {
                let err_msg = "No pod specifications available on this provider";
                error!("{}", err_msg);
                nostr
                    .send_error_response(requester_pubkey, "no_specs", err_msg, None, message_type)
                    .await?;
                return Ok(());
            }
        }
    };

    // 3. Calculate Duration
    let duration_secs = payment_msats / spec.rate_msats_per_sec;
    if duration_secs < config.minimum_duration_seconds {
        let err_msg = format!(
            "Insufficient payment for minimum duration. Required: {} msats for {}s",
            config.minimum_duration_seconds * spec.rate_msats_per_sec,
            config.minimum_duration_seconds
        );
        warn!("{}", err_msg);
        nostr
            .send_error_response(
                requester_pubkey,
                "insufficient_payment",
                &err_msg,
                None,
                message_type,
            )
            .await?;
        return Ok(());
    }

    info!(
        "Validated payment: {} msats for {}s on tier {}",
        payment_msats, duration_secs, spec.name
    );

    // 4. Find available ID
    let id = match backend
        .find_available_id(config.vmid_range_start, config.vmid_range_end)
        .await
    {
        Ok(id) => id,
        Err(e) => {
            let err_msg = format!("Failed to find available ID: {}", e);
            error!("{}", err_msg);
            nostr
                .send_error_response(
                    requester_pubkey,
                    "provisioning_error",
                    &err_msg,
                    None,
                    message_type,
                )
                .await?;
            return Ok(());
        }
    };

    // 5. Generate credentials
    let password = crate::sidecar_service::SidecarState::generate_password();

    // Calculate host port for forwarding
    let host_port = match config.ssh_port_start {
        Some(start) => start + (id - config.vmid_range_start) as u16,
        None => 30000 + (id % 10000) as u16,
    };

    // 6. Create Container
    let container_config = ContainerConfig {
        id,
        name: format!("paygress-{}", id),
        image: request.pod_image.clone(),
        cpu_cores: (spec.cpu_millicores / 1000).max(1) as u32,
        memory_mb: spec.memory_mb as u32,
        storage_gb: 10, // Default 10GB
        password: password.clone(),
        ssh_key: None,
        host_port: Some(host_port),
    };

    debug!("Calling backend.create_container for workload {}", id);
    if let Err(e) = backend.create_container(&container_config).await {
        let err_msg = format!("Backend failed to create workload: {}", e);
        error!("{}", err_msg);
        nostr
            .send_error_response(
                requester_pubkey,
                "backend_error",
                &err_msg,
                None,
                message_type,
            )
            .await?;
        return Ok(());
    }
    debug!("Successfully created container {}", id);

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_secs();

    // 7. Track Workload
    let workload = WorkloadInfo {
        vmid: id,
        workload_type: "lxc".to_string(), // Default for Proxmox/LXD
        spec_id: spec.id.clone(),
        created_at: now,
        expires_at: now + duration_secs,
        owner_npub: requester_pubkey.to_string(),
    };

    workloads.lock().await.insert(id, workload.clone());

    // Update stats
    {
        let mut s = stats.lock().await;
        s.total_jobs_completed += 1;
    }

    // 8. Get Access Details
    // Use configured public IP/host
    let host = &config.public_ip;

    // Send access details
    let expires_dt =
        chrono::DateTime::from_timestamp(workload.expires_at as i64, 0).unwrap_or_default();
    let details = AccessDetailsContent {
        pod_npub: format!("container-{}", id),
        node_port: host_port,
        expires_at: expires_dt.to_rfc3339(),
        cpu_millicores: spec.cpu_millicores,
        memory_mb: spec.memory_mb,
        pod_spec_name: spec.name.clone(),
        pod_spec_description: spec.description.clone(),
        instructions: vec![
            format!("🚀 Workload provisioned successfully!"),
            format!("👤 Username: root"),
            format!("🔑 Password: {}", password),
            format!("⌛ Expires: {}", expires_dt.format("%Y-%m-%d %H:%M:%S UTC")),
            format!("Access: You can connect to the container using SSH."),
            format!("  ssh -p {} root@{}", host_port, host),
        ],
    };

    debug!("Sending access details to {}", requester_pubkey);
    nostr
        .send_access_details_private_message(requester_pubkey, details, message_type)
        .await?;

    debug!("Access details sent successfully");

    info!("Workload {} provisioned for {} seconds", id, duration_secs);
    Ok(())
}

/// Translate a `RedeemError` into the `(error_type, message)` shape the
/// Nostr error-response uses. The error-type strings are stable so
/// consumers can reason about them programmatically (retry on
/// `network`, give up on `already_spent`, etc.).
fn redeem_error_to_response(err: &RedeemError) -> (&'static str, String) {
    match err {
        RedeemError::InvalidToken(msg) => {
            ("invalid_token", format!("Invalid Cashu token: {}", msg))
        }
        RedeemError::NonWhitelistedMint { mint_url } => (
            "non_whitelisted_mint",
            format!("Mint {} is not accepted by this provider", mint_url),
        ),
        RedeemError::AlreadySpent => (
            "token_already_spent",
            "This Cashu token has already been spent at the mint".to_string(),
        ),
        RedeemError::Pending => (
            "token_pending",
            "Token is pending at the mint; retry shortly".to_string(),
        ),
        RedeemError::Network(msg) => (
            "mint_network_error",
            format!("Could not reach mint: {}", msg),
        ),
        RedeemError::UnsupportedUnit(unit) => (
            "unsupported_unit",
            format!("Token unit {} is not supported", unit),
        ),
        RedeemError::MintError(msg) => ("mint_error", format!("Mint rejected redemption: {}", msg)),
    }
}

/// Handle a status request
async fn handle_status_request(
    backend: &dyn ComputeBackend,
    config: &ProviderConfig,
    nostr: &NostrRelaySubscriber,
    workloads: &Arc<Mutex<HashMap<u32, WorkloadInfo>>>,
    requester_pubkey: &str,
    message_type: &str,
    request: StatusRequestContent,
) -> Result<()> {
    info!(
        "Processing status request for pod {} from {}",
        request.pod_id, requester_pubkey
    );

    // 1. Try to find the workload by ID (which could be vmid)
    let vmid = request.pod_id.parse::<u32>().ok();

    let workload = {
        let lock = workloads.lock().await;
        if let Some(vmid) = vmid {
            lock.get(&vmid).cloned()
        } else {
            // If not a number, maybe it's a pod_npub? (not yet implemented in tracking, but we search by owner for now)
            lock.values()
                .find(|w| w.owner_npub == request.pod_id || w.owner_npub == requester_pubkey)
                .cloned()
        }
    };

    let workload = match workload {
        Some(w) => w,
        None => {
            let err_msg = format!(
                "Workload {} not found or you don't have access",
                request.pod_id
            );
            warn!("{}", err_msg);
            nostr
                .send_error_response(requester_pubkey, "not_found", &err_msg, None, message_type)
                .await?;
            return Ok(());
        }
    };

    // 2. Check backend status
    let status_info = match backend.get_node_status().await {
        Ok(s) => s,
        Err(_) => crate::compute::NodeStatus {
            cpu_usage: 0.0,
            memory_used: 0,
            memory_total: 0,
            disk_used: 0,
            disk_total: 0,
        },
    };

    // 3. Prepare response
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_secs();

    let time_remaining = workload.expires_at.saturating_sub(now);
    let status = if time_remaining == 0 {
        "Expired"
    } else {
        "Running"
    };

    let expires_dt =
        chrono::DateTime::from_timestamp(workload.expires_at as i64, 0).unwrap_or_default();

    // Look up spec for actual resource values
    let spec = config.specs.iter().find(|s| s.id == workload.spec_id);
    let cpu = spec.map(|s| s.cpu_millicores).unwrap_or(1000);
    let mem = spec.map(|s| s.memory_mb).unwrap_or(1024);
    let host_port = match config.ssh_port_start {
        Some(start) => start + (workload.vmid - config.vmid_range_start) as u16,
        None => (30000 + (workload.vmid % 10000)) as u16,
    };

    let response = StatusResponseContent {
        pod_id: workload.vmid.to_string(),
        status: status.to_string(),
        expires_at: expires_dt.to_rfc3339(),
        time_remaining_seconds: time_remaining,
        cpu_millicores: cpu,
        memory_mb: mem,
        ssh_host: config.public_ip.clone(),
        ssh_port: host_port,
        ssh_username: "root".to_string(),
    };

    nostr
        .send_status_response(requester_pubkey, response, message_type)
        .await?;

    info!("Status response sent for workload {}", workload.vmid);
    Ok(())
}

/// Load provider config from file
pub fn load_config(path: &str) -> Result<ProviderConfig> {
    let content =
        std::fs::read_to_string(path).context(format!("Failed to read config file: {}", path))?;

    serde_json::from_str(&content).context("Failed to parse provider config")
}

/// Save provider config to file
pub fn save_config(path: &str, config: &ProviderConfig) -> Result<()> {
    let content = serde_json::to_string_pretty(config)?;
    std::fs::write(path, content).context(format!("Failed to write config file: {}", path))?;
    Ok(())
}
