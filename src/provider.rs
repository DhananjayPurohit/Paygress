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
use crate::compute::{ComputeBackend, ContainerConfig, PortMapping};
use crate::docker::DockerBackend;
use crate::durable_workload::{
    DurableWorkload, HeartbeatObservation, QuorumConfig, StateMachineEvent, WorkloadState,
    WorkloadStateMachine,
};
use crate::lxd::LxdBackend;
use crate::nostr::{
    parse_private_message_content, AccessDetailsContent, CapacityInfo, EncryptedSpawnPodRequest,
    EncryptedTopUpPodRequest, ErrorResponseContent, HeartbeatContent, LeaseRevocationContent,
    NostrRelaySubscriber, PodSpec, PrivateRequest, ProviderOfferContent, RelayConfig,
    StatusRequestContent, StatusResponseContent, TopUpResponseContent,
};
use crate::proxmox::{ProxmoxBackend, ProxmoxClient};
use crate::templates::{TemplateDefinition, TemplateName};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum BackendType {
    Proxmox,
    LXD,
    /// Docker backend. Required for the killer-templates path
    /// (#31): templates use real public Docker images that LXD
    /// can't run natively. Provider must have the `docker` CLI
    /// installed and accessible to the running user.
    Docker,
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

    /// Replication mode chosen at spawn time. `None` for the default
    /// single-container path (no failover); `WarmStandby` registers a
    /// list of standby providers so the orchestrator emits a
    /// `LeaseRevocation` on local eviction. `Checkpointed` is reserved
    /// for Unit 6 (consumer-side respawn from Blossom checkpoint).
    #[serde(default)]
    pub replication: crate::durable_workload::ReplicationMode,

    /// Restart policy applied when the workload is locally evicted
    /// without a warm-standby. Default: `OnFailure { max_attempts: 3 }`.
    #[serde(default)]
    pub restart_policy: crate::durable_workload::RestartPolicy,

    /// Optional Blossom URI of the latest published checkpoint for
    /// this workload. Populated by Unit 6 (checkpoint pipeline);
    /// included in revocation events so a standby can restore.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state_uri: Option<String>,
}

/// Provider service that manages the node
pub struct ProviderService {
    config: ProviderConfig,
    backend: Arc<dyn ComputeBackend>,
    nostr: NostrRelaySubscriber,
    redeemer: Arc<dyn MintRedeemer>,
    active_workloads: Arc<Mutex<HashMap<u32, WorkloadInfo>>>,
    stats: Arc<Mutex<ProviderStats>>,

    /// Workload state machine (Unit 5 wiring). Keyed by vmid; each
    /// entry tracks the lifecycle of one local workload through
    /// `Provisioning → Live → Suspect → Evicted/Respawning/Failed`.
    /// The orchestrator loop ticks this against the buffered
    /// `HeartbeatObservation`s and acts on emitted events.
    state_machine: Arc<Mutex<WorkloadStateMachine>>,

    /// Buffered heartbeat observations awaiting the next orchestrator
    /// tick. Filled by the heartbeat loop after each publish (one
    /// observation per relay that ACK'd), drained on each
    /// orchestrator iteration.
    observation_buffer: Arc<Mutex<Vec<HeartbeatObservation>>>,
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
            BackendType::Docker => Arc::new(DockerBackend::new()),
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
            state_machine: Arc::new(Mutex::new(WorkloadStateMachine::new(QuorumConfig::default()))),
            observation_buffer: Arc::new(Mutex::new(Vec::new())),
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

        // Run heartbeat loop, request listener, cleanup loop, and
        // orchestrator loop (Unit 5 wiring) concurrently.
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
            result = self.orchestrator_loop() => {
                error!("Orchestrator loop exited: {:?}", result);
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
            stake_proof: None,
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

        let (_event_id, accepting_relays) = self.nostr.publish_heartbeat(heartbeat).await?;

        // Push one HeartbeatObservation per accepting relay into the
        // shared buffer. The orchestrator loop drains these on its
        // next tick to drive the workload state machine. Observations
        // use `now` for both `seen_at` and `event_timestamp` because
        // we just published; if a relay didn't ACK we don't fabricate
        // an observation for it.
        if !accepting_relays.is_empty() {
            let provider_npub = self.get_npub();
            let mut buf = self.observation_buffer.lock().await;
            for relay_url in accepting_relays {
                buf.push(HeartbeatObservation {
                    provider_npub: provider_npub.clone(),
                    relay_url,
                    seen_at: now,
                    event_timestamp: now,
                });
            }
        }

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
        let state_machine = self.state_machine.clone();

        self.nostr
            .subscribe_to_pod_events(move |event| {
                let backend = backend.clone();
                let config = config.clone();
                let nostr = nostr.clone();
                let redeemer = redeemer.clone();
                let workloads = workloads.clone();
                let stats = stats.clone();
                let state_machine = state_machine.clone();

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
                                &state_machine,
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
                        PrivateRequest::TopUp(topup_req) => {
                            if let Err(e) = handle_topup_request(
                                &config,
                                &nostr,
                                redeemer.as_ref(),
                                &workloads,
                                &event.pubkey,
                                &event.message_type,
                                topup_req,
                            )
                            .await
                            {
                                error!("Failed to handle topup request: {}", e);
                            }
                        }
                    }

                    Ok(())
                })
            })
            .await?;

        Ok(())
    }

    /// Orchestrator loop (Unit 5 wiring).
    ///
    /// Every 15s, drain the observation buffer, advance the workload
    /// state machine, and act on each emitted `StateMachineEvent`.
    /// 15s is chosen to be much shorter than `t1=120s` and `t2=300s`
    /// so transitions are detected promptly, but not so short that
    /// idle providers churn — the underlying state machine is a pure
    /// function and the work is bounded by the number of tracked
    /// workloads.
    async fn orchestrator_loop(&self) -> Result<()> {
        let interval = tokio::time::Duration::from_secs(15);
        info!("Orchestrator loop starting (cadence: 15s)");

        loop {
            tokio::time::sleep(interval).await;

            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)?
                .as_secs();

            // Drain buffered observations. We pass them to the state
            // machine and clear the buffer so the next tick only sees
            // newer observations — a stale ACK older than `stale_secs`
            // would already be ignored by the state machine, but
            // draining keeps the buffer bounded regardless.
            let observations: Vec<HeartbeatObservation> = {
                let mut buf = self.observation_buffer.lock().await;
                std::mem::take(&mut *buf)
            };

            // Tick the state machine. Holds the state machine lock
            // across the tick (a short, pure operation).
            let events = {
                let mut sm = self.state_machine.lock().await;
                sm.tick(now, &observations)
            };

            if events.is_empty() {
                continue;
            }

            for event in events {
                self.handle_state_machine_event(event, now).await;
            }
        }
    }

    /// Translate one `StateMachineEvent` into provider actions.
    /// Logged regardless; events that require I/O (revocation publish,
    /// respawn) are dispatched to the appropriate subsystem.
    async fn handle_state_machine_event(&self, event: StateMachineEvent, now: u64) {
        match event {
            StateMachineEvent::EnteredLive { workload_id } => {
                info!("Workload {} entered Live", workload_id);
            }
            StateMachineEvent::EnteredSuspect { workload_id } => {
                warn!(
                    "Workload {} entered Suspect (heartbeat quorum lost)",
                    workload_id
                );
            }
            StateMachineEvent::Evicted {
                workload_id,
                reason,
            } => {
                error!("Workload {} evicted: {}", workload_id, reason);
            }
            StateMachineEvent::PublishLeaseRevocation {
                workload_id,
                standby_providers,
            } => {
                let state_uri = self
                    .active_workloads
                    .lock()
                    .await
                    .get(&workload_id)
                    .and_then(|w| w.state_uri.clone());
                let revocation = LeaseRevocationContent {
                    workload_id,
                    primary_provider_npub: self.get_npub(),
                    standby_providers: standby_providers.clone(),
                    reason: "heartbeat-quorum-lost-past-t2".to_string(),
                    revoked_at: now,
                    state_uri,
                    version: crate::nostr::SCHEMA_VERSION,
                };
                match self.nostr.publish_lease_revocation(revocation).await {
                    Ok(event_id) => info!(
                        "Published lease revocation for workload {} to {} standby(s): {}",
                        workload_id,
                        standby_providers.len(),
                        event_id
                    ),
                    Err(e) => error!(
                        "Failed to publish lease revocation for workload {}: {}",
                        workload_id, e
                    ),
                }
            }
            StateMachineEvent::AttemptRespawn {
                workload_id,
                attempt,
            } => {
                info!(
                    "Attempting respawn of workload {} (attempt {})",
                    workload_id, attempt
                );
                // The full respawn path requires reconstructing the
                // ContainerConfig from the original spawn, which lives
                // in active_workloads only as `WorkloadInfo` (no image
                // / port mapping retained). Capturing the original
                // ContainerConfig is a follow-up. For now we record
                // the failure so the state machine can retry / fail
                // out deterministically rather than hanging in
                // Respawning forever.
                let mut sm = self.state_machine.lock().await;
                sm.notify_respawn_failed(
                    workload_id,
                    "respawn handler not yet implemented (follow-up)",
                );
            }
            StateMachineEvent::Failed {
                workload_id,
                reason,
            } => {
                error!("Workload {} marked Failed: {}", workload_id, reason);
                // Drop from active_workloads so cleanup_loop doesn't
                // also try to delete a container that was never
                // successfully respawned.
                let mut wl = self.active_workloads.lock().await;
                wl.remove(&workload_id);
            }
        }
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

                    // Untrack from the state machine regardless of
                    // backend stop/delete success — the lease is over,
                    // and we don't want the orchestrator continuing
                    // to drive transitions on a workload nobody is
                    // serving anymore.
                    self.state_machine.lock().await.untrack(vmid);

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
    state_machine: &Arc<Mutex<WorkloadStateMachine>>,
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
    let password = generate_password();

    // Calculate host port for SSH forwarding (LXD/Proxmox path).
    let host_port = match config.ssh_port_start {
        Some(start) => start + (id - config.vmid_range_start) as u16,
        None => 30000 + (id % 10000) as u16,
    };

    // 6. Resolve template (if requested) — image + ports + env come
    //    from the provider's OWN local registry, not consumer bytes.
    //    Unknown slugs are rejected so a consumer can't probe for
    //    accepted templates by sending arbitrary strings.
    let template = if let Some(slug) = request.template_slug.as_deref() {
        match TemplateName::from_slug(slug) {
            Some(name) => Some(TemplateDefinition::lookup(name)),
            None => {
                let err_msg = format!(
                    "Unknown template `{}` — provider does not advertise it",
                    slug
                );
                warn!("{}", err_msg);
                nostr
                    .send_error_response(
                        requester_pubkey,
                        "unknown_template",
                        &err_msg,
                        None,
                        message_type,
                    )
                    .await?;
                return Ok(());
            }
        }
    } else {
        None
    };

    // Image: template wins over consumer-supplied (sandbox).
    let image = template
        .as_ref()
        .map(|t| t.image.to_string())
        .unwrap_or_else(|| request.pod_image.clone());

    // Port mappings: each template port published on a host port
    // derived from `host_port` so multiple workloads on the same
    // provider don't collide. We allocate `host_port + i + 1` for
    // template port i (host_port itself stays for SSH where backends
    // care about it).
    let template_ports: Vec<PortMapping> = template
        .as_ref()
        .map(|t| {
            t.ports
                .iter()
                .enumerate()
                .map(|(i, p)| PortMapping {
                    host_port: host_port.saturating_add(1 + i as u16),
                    container_port: p.container_port,
                    protocol: "tcp",
                })
                .collect()
        })
        .unwrap_or_default();

    let template_env: HashMap<String, String> = template
        .as_ref()
        .map(|t| {
            t.env
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect()
        })
        .unwrap_or_default();

    let extra_runtime_args: Vec<String> = template
        .as_ref()
        .map(|t| t.extra_docker_args.iter().map(|s| s.to_string()).collect())
        .unwrap_or_default();

    let data_path: Option<String> = template
        .as_ref()
        .and_then(|t| t.data_path.map(|p| p.to_string()));

    // 7. Create Container
    let container_config = ContainerConfig {
        id,
        name: format!("paygress-{}", id),
        image,
        cpu_cores: (spec.cpu_millicores / 1000).max(1) as u32,
        memory_mb: spec.memory_mb as u32,
        storage_gb: 10, // Default 10GB
        password: password.clone(),
        ssh_key: None,
        host_port: Some(host_port),
        template_ports,
        template_env,
        extra_runtime_args,
        data_path,
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
        // Replication / restart-policy / state-uri default to the
        // safe single-container values. The Nostr request schema
        // doesn't yet carry replication preferences (would be a
        // breaking change for consumers); a follow-up will add an
        // optional field once Unit 5 wiring proves out end-to-end.
        replication: crate::durable_workload::ReplicationMode::default(),
        restart_policy: crate::durable_workload::RestartPolicy::default(),
        state_uri: None,
    };

    workloads.lock().await.insert(id, workload.clone());

    // Register the workload with the state machine (Unit 5 wiring).
    // Starts in `Provisioning`; the orchestrator promotes it to
    // `Live` on the first observation tick that sees quorum.
    state_machine.lock().await.track(DurableWorkload {
        workload_id: id,
        provider_npub: nostr.get_service_public_key(),
        state: WorkloadState::Provisioning { since: now },
        replication: workload.replication.clone(),
        restart_policy: workload.restart_policy,
        state_uri: workload.state_uri.clone(),
        created_at: now,
        expires_at: workload.expires_at,
    });

    // Update stats
    {
        let mut s = stats.lock().await;
        s.total_jobs_completed += 1;
    }

    // 8. Get Access Details
    // Use configured public IP/host
    let host = &config.public_ip;

    // Build per-template ports with their template labels so the
    // consumer doesn't have to remember the host_port + 1 + i rule.
    // We zip back to the source TemplateDefinition by matching on
    // container_port (each template's ports are unique by
    // container_port today).
    let template_access_ports: Vec<crate::nostr::TemplateAccessPort> = container_config
        .template_ports
        .iter()
        .map(|p| {
            let label = template
                .as_ref()
                .and_then(|t| {
                    t.ports
                        .iter()
                        .find(|tp| tp.container_port == p.container_port)
                })
                .map(|tp| tp.label.to_string())
                .unwrap_or_else(|| format!("port-{}", p.container_port));
            crate::nostr::TemplateAccessPort {
                host_port: p.host_port,
                container_port: p.container_port,
                protocol: p.protocol.to_string(),
                label,
            }
        })
        .collect();

    // Send access details
    let expires_dt =
        chrono::DateTime::from_timestamp(workload.expires_at as i64, 0).unwrap_or_default();

    // Instructions: keep the SSH lines for legacy/manual access,
    // append per-template-port lines so humans see them too.
    let mut instructions = vec![
        format!("🚀 Workload provisioned successfully!"),
        format!("👤 Username: root"),
        format!("🔑 Password: {}", password),
        format!("⌛ Expires: {}", expires_dt.format("%Y-%m-%d %H:%M:%S UTC")),
        format!("Access: You can connect to the container using SSH."),
        format!("  ssh -p {} root@{}", host_port, host),
    ];
    if !template_access_ports.is_empty() {
        instructions.push(format!("Workload ports:"));
        for p in &template_access_ports {
            instructions.push(format!(
                "  {} ({}): {}://{}:{}",
                p.label, p.protocol, p.protocol, host, p.host_port
            ));
        }
    }

    let details = AccessDetailsContent {
        pod_npub: format!("container-{}", id),
        node_port: host_port,
        expires_at: expires_dt.to_rfc3339(),
        cpu_millicores: spec.cpu_millicores,
        memory_mb: spec.memory_mb,
        pod_spec_name: spec.name.clone(),
        pod_spec_description: spec.description.clone(),
        instructions,
        host_address: host.clone(),
        template_ports: template_access_ports,
    };

    debug!("Sending access details to {}", requester_pubkey);
    nostr
        .send_access_details_private_message(requester_pubkey, details, message_type)
        .await?;

    debug!("Access details sent successfully");

    info!("Workload {} provisioned for {} seconds", id, duration_secs);
    Ok(())
}

/// Handle a TopUp request (Unit 2 of the 12-month plan).
///
/// Looks up the workload by its `pod_npub` (which the spawn handler
/// returned as `container-<vmid>`), verifies the requester owns it,
/// redeems the supplied Cashu token at the mint (Unit 1), and
/// extends `expires_at` by `redeemed_msats / spec.rate_msats_per_sec`
/// under the existing workloads mutex.
///
/// Mutex discipline: redemption (a network call to the mint) happens
/// BEFORE we re-acquire the workloads lock, so the lock is never held
/// across an external request.
async fn handle_topup_request(
    config: &ProviderConfig,
    nostr: &NostrRelaySubscriber,
    redeemer: &dyn MintRedeemer,
    workloads: &Arc<Mutex<HashMap<u32, WorkloadInfo>>>,
    requester_pubkey: &str,
    message_type: &str,
    request: EncryptedTopUpPodRequest,
) -> Result<()> {
    info!(
        "Processing topup request from {} for {}",
        requester_pubkey, request.pod_npub
    );

    let vmid = match parse_pod_npub(&request.pod_npub) {
        Some(v) => v,
        None => {
            let err_msg = format!(
                "Could not parse pod identifier `{}`; expected `container-<id>` or numeric id",
                request.pod_npub
            );
            warn!("{}", err_msg);
            nostr
                .send_error_response(
                    requester_pubkey,
                    "invalid_pod_id",
                    &err_msg,
                    None,
                    message_type,
                )
                .await?;
            return Ok(());
        }
    };

    // 1. Snapshot the workload + spec under a brief read-only lock so
    //    we know how to bill before we redeem.
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_secs();

    let (spec_id, current_expires_at) = {
        let lock = workloads.lock().await;
        match lock.get(&vmid) {
            Some(w) if w.owner_npub == requester_pubkey => (w.spec_id.clone(), w.expires_at),
            Some(_) => {
                drop(lock);
                let err_msg = "Pod not owned by requester";
                warn!("{}: vmid={}", err_msg, vmid);
                nostr
                    .send_error_response(requester_pubkey, "not_owner", err_msg, None, message_type)
                    .await?;
                return Ok(());
            }
            None => {
                drop(lock);
                let err_msg = format!("Pod {} not found", request.pod_npub);
                warn!("{}", err_msg);
                nostr
                    .send_error_response(
                        requester_pubkey,
                        "not_found",
                        &err_msg,
                        None,
                        message_type,
                    )
                    .await?;
                return Ok(());
            }
        }
    };

    if current_expires_at <= now {
        let err_msg = format!(
            "Pod {} lease has already expired; spawn a new pod instead",
            request.pod_npub
        );
        warn!("{}", err_msg);
        nostr
            .send_error_response(
                requester_pubkey,
                "lease_expired",
                &err_msg,
                None,
                message_type,
            )
            .await?;
        return Ok(());
    }

    let spec = match config.specs.iter().find(|s| s.id == spec_id) {
        Some(s) => s.clone(),
        None => {
            // Spec referenced by the workload no longer exists in
            // config — provider misconfiguration. Refuse the topup
            // rather than silently mis-billing.
            let err_msg = format!(
                "Pod {} references unknown spec `{}`; provider misconfiguration",
                request.pod_npub, spec_id
            );
            error!("{}", err_msg);
            nostr
                .send_error_response(
                    requester_pubkey,
                    "spec_unavailable",
                    &err_msg,
                    None,
                    message_type,
                )
                .await?;
            return Ok(());
        }
    };

    // 2. Redeem the topup token (no workloads lock held).
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
            error!("Topup redemption failed: {}", err_msg);
            nostr
                .send_error_response(requester_pubkey, error_type, &err_msg, None, message_type)
                .await?;
            return Ok(());
        }
    };

    let extension_secs = payment_msats / spec.rate_msats_per_sec;
    if extension_secs == 0 {
        let err_msg = format!(
            "Insufficient topup: {} msats buys 0 seconds at {} msats/sec",
            payment_msats, spec.rate_msats_per_sec
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

    // 3. Apply the extension under the workloads lock. We re-check
    //    ownership and existence after re-locking to defend against
    //    cleanup having run between our snapshot and now.
    let new_expires_at = {
        let mut lock = workloads.lock().await;
        match lock.get_mut(&vmid) {
            Some(w) if w.owner_npub == requester_pubkey => {
                w.expires_at = w.expires_at.saturating_add(extension_secs);
                w.expires_at
            }
            _ => {
                // Vanished or ownership changed between snapshots.
                // The token is already spent at the mint — this is a
                // race the consumer should retry by spawning a new
                // pod. We surface a distinct error so the CLI can
                // explain it.
                drop(lock);
                let err_msg =
                    "Pod was cleaned up before topup could be applied; token has been spent";
                error!("{}: vmid={}", err_msg, vmid);
                nostr
                    .send_error_response(requester_pubkey, "race_lost", err_msg, None, message_type)
                    .await?;
                return Ok(());
            }
        }
    };

    let new_expires_dt =
        chrono::DateTime::from_timestamp(new_expires_at as i64, 0).unwrap_or_default();
    let response = TopUpResponseContent {
        success: true,
        pod_npub: request.pod_npub.clone(),
        extended_duration_seconds: extension_secs,
        new_expires_at: new_expires_dt.to_rfc3339(),
        message: format!(
            "Lease extended by {}s ({} msats @ {} msats/sec)",
            extension_secs, payment_msats, spec.rate_msats_per_sec
        ),
    };

    nostr
        .send_topup_response_private_message(requester_pubkey, response, message_type)
        .await?;

    info!(
        "Topup applied to {}: +{}s (now expires at {})",
        request.pod_npub, extension_secs, new_expires_at
    );
    Ok(())
}

/// Generate a 16-character alphanumeric SSH password. Lives here
/// (rather than `sidecar_service`) so the Nostr-DM canonical path
/// doesn't depend on the legacy K8s pipeline that Unit 7 gates
/// behind the `kubernetes` Cargo feature.
fn generate_password() -> String {
    use rand::Rng;
    const CHARSET: &[u8] = b"abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789";
    let mut rng = rand::thread_rng();
    (0..16)
        .map(|_| {
            let idx = rng.gen_range(0..CHARSET.len());
            CHARSET[idx] as char
        })
        .collect()
}

/// Parse a pod identifier emitted by the spawn handler back into the
/// internal vmid. Accepts both `container-<vmid>` (the format
/// AccessDetailsContent returns to the consumer) and a bare numeric
/// id (for callers that already know it).
pub fn parse_pod_npub(pod_npub: &str) -> Option<u32> {
    if let Some(rest) = pod_npub.strip_prefix("container-") {
        rest.parse().ok()
    } else {
        pod_npub.parse().ok()
    }
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
