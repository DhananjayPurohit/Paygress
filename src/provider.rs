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
    parse_private_message_content, warm_standby_role, AccessDetailsContent, CapacityInfo,
    EncryptedSpawnPodRequest, EncryptedTopUpPodRequest, ErrorResponseContent, HeartbeatContent,
    LeaseRevocationContent, NostrRelaySubscriber, PodSpec, PrivateRequest, ProviderOfferContent,
    RelayConfig, StandbyPromotionAnnouncementContent, StatusRequestContent, StatusResponseContent,
    TopUpResponseContent, WarmStandbyRole,
};
use crate::proxmox::{ProxmoxBackend, ProxmoxClient};
use crate::templates::{TemplateDefinition, TemplateName};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum BackendType {
    Proxmox,
    LXD,
    /// Docker backend. Required for the killer-templates path
    /// (#31): templates use real public Docker images that LXD
    /// can't run natively. Provider must have the `docker` CLI
    /// installed and accessible to the running user.
    Docker,
    /// KVM/qemu backend. Each spawn is its own VM with its own
    /// kernel — no co-tenant attacks via container escape.
    /// Publishes `IsolationLevel::DedicatedHost` on the offer so
    /// consumers filtering by `--isolation-level dedicated-host`
    /// match this provider. Requires `/dev/kvm` and
    /// `qemu-system-x86_64` on the host. Killer templates (Docker
    /// images) are NOT served on this backend in v1; consumers get
    /// vanilla Ubuntu VMs with SSH access.
    Kvm,
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

    /// Consumer-assigned workload identifier (UUID-shaped string),
    /// set from `EncryptedSpawnPodRequest.workload_id` at spawn time
    /// for warm-standby workloads. Used by the orchestrator's
    /// `PublishLeaseRevocation` handler so the published revocation
    /// carries the same id the standbys keyed their slots by.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub consumer_workload_id: Option<String>,
}

/// A standby slot reserved for a warm-standby workload. The
/// consumer's spawn request was paid for and acknowledged, but no
/// container has been created yet — the standby is "armed" and
/// waiting for a `LeaseRevocation` event from the primary.
///
/// Stored in `ProviderService::standby_slots` keyed by `workload_id`
/// (consumer-assigned UUID, shared by primary and all standbys).
/// On revocation, the standby's promotion handler:
///   1. Sleeps `index * promotion_delay_secs` (ordered backoff for
///      single-writer; standby 0 promotes immediately, standby 1
///      waits one delay window, etc.)
///   2. Spawns the container using `container_config`
///   3. Removes the slot, adds to `active_workloads`, registers with
///      the state machine as primary
#[derive(Debug, Clone)]
pub struct StandbySlot {
    pub workload_id: String,
    pub primary_npub: String,
    pub standby_index: usize,
    pub standby_count: usize,
    pub container_config: ContainerConfig,
    pub spec_id: String,
    pub expires_at: u64,
    pub owner_npub: String,
    /// Unix-second timestamp at which the slot was reserved. Used as
    /// the silence-baseline by the watchdog when no primary heartbeat
    /// has been observed yet — without this, a fresh slot would treat
    /// `last_seen == 0` as immediate silence and promote a healthy
    /// primary on the first watchdog tick (race window: 0–60s while
    /// waiting for the primary's next heartbeat to land on the relay).
    pub created_at: u64,
    /// Npubs of the OTHER standbys for this workload (excludes self
    /// and the primary). Used at promotion-time to detect that a
    /// lower-indexed peer has already promoted: a freshly-promoted
    /// peer publishes a heartbeat from its own npub immediately, and
    /// this standby queries those npubs before claiming the slot
    /// itself. Without this list, every standby would promote
    /// independently after the silence window, producing split-brain.
    pub peer_standby_npubs: Vec<String>,
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

    /// Reserved warm-standby slots, keyed by consumer-assigned
    /// `workload_id`. Populated when a spawn request arrives with
    /// `replication = WarmStandby` AND the role-detection helper
    /// classifies this provider as a standby. Drained when a
    /// matching `LeaseRevocation` arrives (slot promotes to a real
    /// active workload) or when the slot's expiry passes (cleanup).
    standby_slots: Arc<Mutex<HashMap<String, StandbySlot>>>,
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
            BackendType::Kvm => {
                // Fail-fast: surface the "this host doesn't have
                // KVM" error at provider startup, not at first
                // spawn (when a paying consumer has already
                // committed a Cashu token).
                if let Err(e) = crate::kvm::KvmBackend::check_kvm_available().await {
                    tracing::error!("KVM backend selected but unavailable: {}", e);
                    anyhow::bail!("KVM backend unavailable: {}", e);
                }
                Arc::new(crate::kvm::KvmBackend::new(crate::kvm::KvmConfig::default()))
            }
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
            state_machine: Arc::new(Mutex::new(WorkloadStateMachine::new(
                QuorumConfig::default(),
            ))),
            observation_buffer: Arc::new(Mutex::new(Vec::new())),
            standby_slots: Arc::new(Mutex::new(HashMap::new())),
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

        // Run heartbeat loop, request listener, cleanup loop,
        // orchestrator loop (Unit 5 wiring), and standby watchdog
        // (closes the hard-crash failover gap) concurrently.
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
            result = self.standby_watchdog_loop() => {
                error!("Standby watchdog loop exited: {:?}", result);
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
            // Derive isolation tier from the configured backend.
            // KVM is per-VM (DedicatedHost). Docker / LXD / Proxmox
            // share the host kernel (SharedKernel) — same tier as
            // historical default. SEV-SNP / TDX gets its own
            // backend later and bumps to AttestedResearchTier.
            isolation_level: match self.config.backend_type {
                BackendType::Kvm => crate::nostr::IsolationLevel::DedicatedHost,
                BackendType::Proxmox | BackendType::LXD | BackendType::Docker => {
                    crate::nostr::IsolationLevel::SharedKernel
                }
            },
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
        let standby_slots = self.standby_slots.clone();

        self.nostr
            .subscribe_to_pod_events(move |event| {
                let backend = backend.clone();
                let config = config.clone();
                let nostr = nostr.clone();
                let redeemer = redeemer.clone();
                let workloads = workloads.clone();
                let stats = stats.clone();
                let state_machine = state_machine.clone();
                let standby_slots = standby_slots.clone();

                Box::pin(async move {
                    let my_pubkey = nostr.public_key().to_hex();
                    if event.pubkey == my_pubkey {
                        return Ok(());
                    }

                    debug!(
                        "Handler received event kind: {}, from: {}, message_type: {}",
                        event.kind, event.pubkey, event.message_type
                    );

                    // Lease revocation events take a separate path
                    // (Unit 5 standby-side promotion). Public events,
                    // no decryption, no response.
                    if let Some(revocation) = crate::nostr::parse_revocation_event(&event) {
                        info!(
                            "Lease revocation observed: workload_id={}, primary={}, reason={}, state_uri={:?}, standbys={:?}",
                            revocation.workload_id,
                            revocation.primary_provider_npub,
                            revocation.reason,
                            revocation.state_uri,
                            revocation.standby_providers,
                        );
                        // Look up the matching standby slot. If
                        // present, schedule the promotion task; the
                        // ordered backoff inside that task gives us
                        // single-writer across N standbys (best-effort
                        // — see scheduler for the v1 caveat).
                        let workload_id = revocation.workload_id.clone();
                        let primary_npub = revocation.primary_provider_npub.clone();
                        let slot_opt = standby_slots.lock().await.get(&workload_id).cloned();
                        if let Some(slot) = slot_opt {
                            if slot.primary_npub != primary_npub {
                                warn!(
                                    "Revocation primary_npub ({}) does not match slot's primary ({}); ignoring",
                                    primary_npub, slot.primary_npub
                                );
                                return Ok(());
                            }
                            schedule_standby_promotion(
                                backend.clone(),
                                workloads.clone(),
                                state_machine.clone(),
                                standby_slots.clone(),
                                nostr.clone(),
                                slot,
                            );
                        } else {
                            debug!(
                                "Revocation workload_id={} did not match any local standby slot; ignoring",
                                workload_id
                            );
                        }
                        return Ok(());
                    }

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
                                &standby_slots,
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
                // Look up the workload's consumer-assigned UUID (set
                // at spawn time from request.workload_id). If absent,
                // fall back to a derived id based on the local vmid —
                // this case is unreachable for a real warm-standby
                // workload because the spawn handler enforces the
                // UUID, but the fallback keeps the publish call
                // total in case of state-machine bugs.
                let (consumer_workload_id, state_uri) = {
                    let lock = self.active_workloads.lock().await;
                    let entry = lock.get(&workload_id);
                    let cid = entry
                        .and_then(|w| w.consumer_workload_id.clone())
                        .unwrap_or_else(|| format!("vmid-{}", workload_id));
                    let suri = entry.and_then(|w| w.state_uri.clone());
                    (cid, suri)
                };
                let revocation = LeaseRevocationContent {
                    workload_id: consumer_workload_id.clone(),
                    primary_provider_npub: self.get_npub(),
                    standby_providers: standby_providers.clone(),
                    reason: "heartbeat-quorum-lost-past-t2".to_string(),
                    revoked_at: now,
                    state_uri,
                    version: crate::nostr::SCHEMA_VERSION,
                };
                match self.nostr.publish_lease_revocation(revocation).await {
                    Ok(event_id) => info!(
                        "Published lease revocation for workload {} (vmid {}) to {} standby(s): {}",
                        consumer_workload_id,
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
            drop(workloads);

            // Expire reserved-but-never-promoted standby slots whose
            // lease window has passed. Without this, a standby slot
            // for a workload that never failed over (the common case
            // — primaries usually outlive their lease) accumulates in
            // the slots map until process restart. The watchdog skips
            // promotion-on-silence checks for past-expiry slots
            // anyway, but the memory grows unbounded across long-
            // running providers serving many warm-standby workloads.
            let mut slots = self.standby_slots.lock().await;
            let expired_slots: Vec<String> = slots
                .iter()
                .filter(|(_, slot)| slot.expires_at <= now)
                .map(|(workload_id, _)| workload_id.clone())
                .collect();
            for workload_id in expired_slots {
                if let Some(slot) = slots.remove(&workload_id) {
                    info!(
                        "Expiring standby slot for workload {} (index {}/{}, primary {}, expired at {})",
                        workload_id, slot.standby_index, slot.standby_count, slot.primary_npub, slot.expires_at
                    );
                }
            }
        }
    }

    /// Standby watchdog: detect a primary that has stopped publishing
    /// heartbeats and promote ourselves on its behalf.
    ///
    /// Why this exists
    /// ---------------
    /// PR #43 wired the standby's `LeaseRevocation` listener: when
    /// the primary's orchestrator emits a revocation event (graceful
    /// "I can no longer keep this workload alive"), the standby
    /// promotes after `index * STANDBY_PROMOTION_DELAY_SECS`. That
    /// covers the *graceful* failover case — primary still has
    /// network access to publish, just decided to give up the lease.
    ///
    /// It does NOT cover **hard crash**: primary process dies, host
    /// loses network, kernel panics, etc. No revocation event ever
    /// fires; standbys never promote. Without this loop, paygress's
    /// warm-standby promise reduces to "high-availability against
    /// the workload itself dying, not against the provider hosting
    /// it dying" — which is the more common failure mode.
    ///
    /// How it works
    /// ------------
    /// Every `STANDBY_WATCHDOG_INTERVAL_SECS`, the standby:
    ///   1. Snapshots its `standby_slots`.
    ///   2. Batches the unique primary npubs across those slots and
    ///      asks Nostr for the latest heartbeat per primary.
    ///   3. For each slot whose primary's last heartbeat is older
    ///      than `STANDBY_HEARTBEAT_SILENCE_SECS`, fires
    ///      `schedule_standby_promotion(slot)`.
    ///
    /// The race with the existing revocation listener is handled by
    /// `schedule_standby_promotion` itself: it removes the slot from
    /// the map atomically inside the spawned task, so first caller
    /// wins. Watchdog and revocation listener can both fire for the
    /// same slot; only one promotion runs.
    async fn standby_watchdog_loop(&self) -> Result<()> {
        let interval = tokio::time::Duration::from_secs(STANDBY_WATCHDOG_INTERVAL_SECS);
        info!(
            "Standby watchdog loop starting (cadence: {}s, silence threshold: {}s)",
            STANDBY_WATCHDOG_INTERVAL_SECS, STANDBY_HEARTBEAT_SILENCE_SECS
        );

        loop {
            tokio::time::sleep(interval).await;

            let slots: Vec<StandbySlot> = {
                let lock = self.standby_slots.lock().await;
                lock.values().cloned().collect()
            };
            if slots.is_empty() {
                continue;
            }

            // Dedupe primaries — many slots may share one primary
            // (single workload with N standbys), no point querying
            // its heartbeat once per slot.
            let primary_npubs: Vec<String> = slots
                .iter()
                .map(|s| s.primary_npub.clone())
                .collect::<std::collections::HashSet<_>>()
                .into_iter()
                .collect();

            let heartbeats = match self.nostr.get_latest_heartbeats_multi(primary_npubs).await {
                Ok(hb) => hb,
                Err(e) => {
                    warn!(
                        "standby watchdog: heartbeat batch query failed: {}; \
                         skipping this tick (will retry next interval)",
                        e
                    );
                    continue;
                }
            };

            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)?
                .as_secs();

            for slot in slots {
                let last_seen = heartbeats
                    .get(&slot.primary_npub)
                    .map(|hb| hb.timestamp)
                    .unwrap_or(0);
                // When no heartbeat has been observed yet (last_seen == 0),
                // fall back to the slot's reservation timestamp. This
                // gives a fresh standby a full silence-window of grace
                // before promoting — otherwise the watchdog would fire
                // on the first tick (0-30s after spawn) and promote a
                // healthy primary that simply hasn't published its next
                // heartbeat yet.
                let silence_baseline = if last_seen == 0 {
                    slot.created_at
                } else {
                    last_seen
                };
                if !primary_is_silent(now, silence_baseline, STANDBY_HEARTBEAT_SILENCE_SECS) {
                    continue;
                }
                let silence_secs = now.saturating_sub(silence_baseline);
                warn!(
                    "Primary {} silent for {}s on slot workload_id={} (threshold {}s); \
                     triggering standby promotion (assumes hard crash; revocation \
                     listener and watchdog dedupe via slot.remove)",
                    slot.primary_npub,
                    silence_secs,
                    slot.workload_id,
                    STANDBY_HEARTBEAT_SILENCE_SECS
                );
                schedule_standby_promotion(
                    self.backend.clone(),
                    self.active_workloads.clone(),
                    self.state_machine.clone(),
                    self.standby_slots.clone(),
                    self.nostr.clone(),
                    slot,
                );
            }
        }
    }
}

/// Cadence at which the standby watchdog re-queries heartbeats. 30s
/// matches `cleanup_loop` so we don't add a new periodic on the box.
const STANDBY_WATCHDOG_INTERVAL_SECS: u64 = 30;

/// Heartbeat-silence threshold: how long without a primary heartbeat
/// before we treat the primary as crashed. 180s = 3× the default
/// 60s heartbeat cadence — gives the primary two missed beats of
/// grace (transient relay flake, brief network blip) before
/// promotion fires. Configurable in `ProviderConfig` is a
/// follow-up; today it's a constant so the silence window is
/// uniform across all standbys.
const STANDBY_HEARTBEAT_SILENCE_SECS: u64 = 180;

/// Pure-function silence check, factored out for unit testing. The
/// watchdog calls this for each (now, baseline, threshold) tuple,
/// where `baseline` is either the timestamp of the most-recently
/// observed primary heartbeat or, when none has been observed yet,
/// the slot's reservation timestamp. The caller is responsible for
/// picking the right baseline — this function only does the
/// arithmetic.
///
/// Returns `true` iff `now - baseline >= threshold`, meaning the
/// primary has been silent (relative to the chosen baseline) for at
/// least the threshold window.
///
/// `baseline == 0` is treated as "unknown" and returns `false` — the
/// caller forgot to provide a baseline, and we'd rather not promote
/// than promote spuriously.
fn primary_is_silent(now: u64, baseline: u64, threshold: u64) -> bool {
    if baseline == 0 {
        return false;
    }
    now.saturating_sub(baseline) >= threshold
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
    standby_slots: &Arc<Mutex<HashMap<String, StandbySlot>>>,
    requester_pubkey: &str,
    message_type: &str,
    request: EncryptedSpawnPodRequest,
) -> Result<()> {
    info!(
        "Processing spawn request from {} (tier: {:?})",
        requester_pubkey, request.pod_spec_id
    );

    // Self-role detection for warm-standby spawns. The consumer
    // sends the SAME shape of request to N+1 providers — the
    // primary and each standby. Each provider compares its own
    // npub against the request's primary_npub / standby_providers
    // to figure out which path to take. Single-replication spawns
    // (the common case) skip this entirely.
    let role = compute_warm_standby_role(&nostr.get_service_public_key(), &request);
    if matches!(role, WarmStandbyRole::NotAddressed) {
        if request
            .replication
            .as_ref()
            .map(|r| {
                matches!(
                    r,
                    crate::durable_workload::ReplicationMode::WarmStandby { .. }
                )
            })
            .unwrap_or(false)
        {
            // The consumer set replication=WarmStandby but neither
            // designated us as primary nor included us in the
            // standby list. Refuse to spend the token — they sent
            // to the wrong provider.
            let err_msg =
                "warm-standby spawn arrived at a provider not designated as primary or standby";
            warn!("{}", err_msg);
            nostr
                .send_error_response(
                    requester_pubkey,
                    "not_addressed",
                    err_msg,
                    None,
                    message_type,
                )
                .await?;
            return Ok(());
        }
    }

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

    let mut template_env: HashMap<String, String> = template
        .as_ref()
        .map(|t| {
            t.env
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect()
        })
        .unwrap_or_default();

    // For templates that bake in the paygress-exec HTTP server
    // (currently `agent-sandbox`), inject the same credentials the
    // consumer will see in AccessDetails — `root` + the
    // provider-generated `password`. The instructions block already
    // tells the consumer to use "root + this password", so the exec
    // server reusing those creds means there's exactly one secret
    // to manage per spawn.
    //
    // The template defaults EXEC_USER/EXEC_PASS to empty strings;
    // the server returns 503 until they're non-empty, so this
    // overlay is what unlocks /exec.
    if let Some(t) = template.as_ref() {
        if t.env.contains_key("EXEC_USER") {
            template_env.insert("EXEC_USER".to_string(), "root".to_string());
        }
        if t.env.contains_key("EXEC_PASS") {
            template_env.insert("EXEC_PASS".to_string(), password.clone());
        }
    }

    let extra_runtime_args: Vec<String> = template
        .as_ref()
        .map(|t| t.extra_docker_args.iter().map(|s| s.to_string()).collect())
        .unwrap_or_default();

    let data_path: Option<String> = template
        .as_ref()
        .and_then(|t| t.data_path.map(|p| p.to_string()));

    // Volume encryption (Phase 2): decode the consumer-supplied key
    // from the spawn request. Silently None for stateless workloads
    // (data_path is None) since there's nothing to encrypt — saves
    // surfacing a confusing "key supplied but ignored" warning to a
    // consumer who set --encrypt-volume on a stateless template.
    let volume_encryption_key = match (&data_path, request.volume_encryption.as_ref()) {
        (Some(_), Some(ve)) => match ve.decoded_key() {
            Ok(key) => {
                info!(
                    "Spawn request includes volume_encryption (algorithm={}, version={}); will create LUKS-encrypted data volume",
                    ve.algorithm, ve.version
                );
                Some(key)
            }
            Err(e) => {
                error!(
                    "Rejecting spawn: malformed volume_encryption.key_b64: {}",
                    e
                );
                let err_payload = ErrorResponseContent {
                    error_type: "invalid_volume_encryption".to_string(),
                    message: format!("volume_encryption rejected: {}", e),
                    details: None,
                };
                let _ = nostr
                    .send_error_response_private_message(
                        requester_pubkey,
                        err_payload,
                        message_type,
                    )
                    .await;
                return Ok(());
            }
        },
        (None, Some(_)) => {
            warn!(
                "Spawn request set volume_encryption but template has no data_path; encryption is a no-op for stateless workloads"
            );
            None
        }
        _ => None,
    };

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
        volume_encryption_key,
    };

    // ---- Standby branch ----
    //
    // If self is a standby for this workload (warm-standby spawn,
    // self.npub in standby_providers), DON'T create the container
    // yet. Reserve the slot, return a standby-confirmation
    // AccessDetails to the consumer, and wait for a
    // `LeaseRevocation` from the primary to trigger promotion.
    //
    // The token has already been redeemed at step 1, so the
    // consumer paid for the reservation — providers earn revenue
    // for offering standby capacity even if the primary never
    // fails over.
    if let WarmStandbyRole::Standby { index, count } = role {
        let workload_id = match request.workload_id.as_deref() {
            Some(id) if !id.is_empty() => id.to_string(),
            _ => {
                let err_msg = "warm-standby spawn missing workload_id (consumer-assigned UUID required to coordinate primary + standbys)";
                warn!("{}", err_msg);
                nostr
                    .send_error_response(
                        requester_pubkey,
                        "missing_workload_id",
                        err_msg,
                        None,
                        message_type,
                    )
                    .await?;
                return Ok(());
            }
        };
        let primary_npub = request.primary_npub.clone().unwrap_or_default();
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_secs();
        // Compute peer-standby npubs: the full standby set minus
        // self. Used at promotion-time to detect that a lower-indexed
        // peer has already promoted (their fresh heartbeat under
        // their own npub is the dedup signal). For non-WarmStandby
        // replication this branch is unreachable (role would be
        // Primary), so we can safely default to empty.
        let self_npub = nostr.get_service_public_key();
        let peer_standby_npubs: Vec<String> = match request.replication.as_ref() {
            Some(crate::durable_workload::ReplicationMode::WarmStandby { standby_providers }) => {
                standby_providers
                    .iter()
                    .filter(|p| !crate::nostr::npubs_equal(p, &self_npub))
                    .cloned()
                    .collect()
            }
            _ => Vec::new(),
        };
        let slot = StandbySlot {
            workload_id: workload_id.clone(),
            primary_npub,
            standby_index: index,
            standby_count: count,
            container_config: container_config.clone(),
            spec_id: spec.id.clone(),
            expires_at: now + duration_secs,
            owner_npub: requester_pubkey.to_string(),
            created_at: now,
            peer_standby_npubs,
        };
        info!(
            "Reserved standby slot for workload_id={} (index {}/{}, expires at {})",
            workload_id, index, count, slot.expires_at
        );
        standby_slots.lock().await.insert(workload_id.clone(), slot);

        // Send a structured "standby reserved" AccessDetails so the
        // consumer-side coordinator can confirm the reservation
        // landed. Reuse AccessDetailsContent's shape with a
        // distinguishing instructions block; adding a new
        // dedicated content type would be a wire-schema bump for
        // a single edge case.
        let expires_dt =
            chrono::DateTime::from_timestamp((now + duration_secs) as i64, 0).unwrap_or_default();
        let details = AccessDetailsContent {
            pod_npub: format!("standby-slot-{}", workload_id),
            node_port: 0, // No live container yet; 0 signals "reserved, not running"
            expires_at: expires_dt.to_rfc3339(),
            cpu_millicores: spec.cpu_millicores,
            memory_mb: spec.memory_mb,
            pod_spec_name: spec.name.clone(),
            pod_spec_description: spec.description.clone(),
            instructions: vec![
                format!(
                    "🛏️  Standby slot reserved (index {}/{} for workload {}).",
                    index, count, workload_id
                ),
                format!(
                    "Will promote on LeaseRevocation event from primary {}.",
                    request.primary_npub.as_deref().unwrap_or("(unset)")
                ),
                format!(
                    "Expected promotion delay: {} seconds (index * 30s backoff).",
                    index * 30
                ),
            ],
            host_address: config.public_ip.clone(),
            template_ports: Vec::new(),
        };
        nostr
            .send_access_details_private_message(requester_pubkey, details, message_type)
            .await?;
        return Ok(());
    }

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

    // 7. Track Workload.
    //
    // Replication mode flows from the consumer's spawn request. Old
    // clients (no `replication` field) default to None — identical
    // behavior to before this PR. New clients can opt into
    // `WarmStandby { standby_providers }` by sending the same spawn
    // request to every provider in the set; the orchestrator's
    // `PublishLeaseRevocation` event from #34 then has a real
    // `standby_providers` list to address. Standby-side promotion
    // (subscribing + acting on incoming revocations) lands in a
    // follow-up.
    let replication = request
        .replication
        .clone()
        .unwrap_or_else(crate::durable_workload::ReplicationMode::default);
    let workload = WorkloadInfo {
        vmid: id,
        workload_type: "lxc".to_string(), // Default for Proxmox/LXD
        spec_id: spec.id.clone(),
        created_at: now,
        expires_at: now + duration_secs,
        owner_npub: requester_pubkey.to_string(),
        replication,
        restart_policy: crate::durable_workload::RestartPolicy::default(),
        state_uri: None,
        // Carried through from the spawn request so the orchestrator
        // can publish revocations addressed to the consumer's UUID
        // (which standbys key their slot table by).
        consumer_workload_id: request.workload_id.clone().filter(|s| !s.is_empty()),
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

    // 2. Prepare response
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

/// Per-standby ordered backoff window. Standby at index `i` waits
/// `i * STANDBY_PROMOTION_DELAY_SECS` after observing a revocation
/// before spawning the container locally. Single-writer guarantee
/// is best-effort: if standby 0 promotes within ~30s, standby 1
/// will NOT see a fresh heartbeat by the time its window opens
/// only if heartbeat cadence is > 30s. We accept a brief two-Live
/// window as a v1 trade-off for the workloads where warm-standby
/// makes sense (relays — idempotent; databases — needs deeper
/// coordination than v1 ships).
const STANDBY_PROMOTION_DELAY_SECS: u64 = 30;

/// Self-role detection for a warm-standby spawn. Reads the request's
/// `replication.standby_providers` and `primary_npub`, compares to
/// `self_npub`, returns the role this provider should take.
///
/// Returns `WarmStandbyRole::Primary` for non-WarmStandby requests
/// too — which is correct: in the single-replication path the
/// "primary" is just "the one provider running this workload."
fn compute_warm_standby_role(
    self_npub: &str,
    request: &EncryptedSpawnPodRequest,
) -> WarmStandbyRole {
    use crate::durable_workload::ReplicationMode;
    match request.replication.as_ref() {
        Some(ReplicationMode::WarmStandby { standby_providers }) => {
            let primary = request.primary_npub.as_deref().unwrap_or("");
            warm_standby_role(self_npub, primary, standby_providers)
        }
        // No-replication / Checkpointed → there's only one provider
        // running this; treat it as primary so the existing flow
        // is unchanged.
        _ => WarmStandbyRole::Primary,
    }
}

/// Spawn the standby promotion task. Runs on its own tokio task so
/// the request handler returns immediately. Sleeps for the
/// per-standby ordered backoff, then:
///   1. Pre-emption check — query for any
///      `StandbyPromotionAnnouncement` event for this workload_id
///      authored by a peer standby. If one exists, drop the slot
///      without spawning (a peer beat us to it).
///   2. Spawn the container locally.
///   3. Publish a `StandbyPromotionAnnouncement` so higher-indexed
///      peers' pre-emption check finds it and they back off.
fn schedule_standby_promotion(
    backend: Arc<dyn ComputeBackend>,
    workloads: Arc<Mutex<HashMap<u32, WorkloadInfo>>>,
    state_machine: Arc<Mutex<WorkloadStateMachine>>,
    standby_slots: Arc<Mutex<HashMap<String, StandbySlot>>>,
    nostr: NostrRelaySubscriber,
    slot: StandbySlot,
) {
    let delay_secs = (slot.standby_index as u64).saturating_mul(STANDBY_PROMOTION_DELAY_SECS);
    let workload_id = slot.workload_id.clone();
    let standby_index = slot.standby_index;
    info!(
        "Scheduling standby promotion for workload {} after {}s backoff (standby index {})",
        workload_id, delay_secs, standby_index
    );
    tokio::spawn(async move {
        if delay_secs > 0 {
            tokio::time::sleep(std::time::Duration::from_secs(delay_secs)).await;
        }

        // Re-check the slot is still ours to claim. If a lower-
        // indexed standby already promoted, it should have removed
        // its slot — but each standby manages only its OWN slots,
        // not its peers'. So this check only guards against double-
        // firing on the same provider (e.g. duplicate revocation
        // events from multiple relays).
        let still_present = {
            let mut slots = standby_slots.lock().await;
            slots.remove(&workload_id)
        };
        let slot = match still_present {
            Some(s) => s,
            None => {
                debug!(
                    "Standby slot for workload {} already drained; skipping promotion",
                    workload_id
                );
                return;
            }
        };

        // Pre-emption check: has a peer standby already promoted
        // for this exact workload_id? Query the dedicated
        // `StandbyPromotionAnnouncement` event kind (38386), which
        // is published exactly once per promotion. Heartbeats can't
        // serve this role: every standby provider runs a periodic
        // heartbeat loop regardless of promotion state, so a fresh
        // heartbeat from a peer means "peer is online", NOT "peer
        // promoted". The announcement event is unambiguous.
        if !slot.peer_standby_npubs.is_empty() {
            match nostr
                .query_standby_promotion_announcements(&slot.workload_id, &slot.peer_standby_npubs)
                .await
            {
                Ok(Some(announcement)) => {
                    info!(
                        "Peer standby {} already promoted workload {} at {}; dropping slot without spawning",
                        announcement.new_primary_npub,
                        announcement.workload_id,
                        announcement.promoted_at
                    );
                    return;
                }
                Ok(None) => {
                    // No peer has announced — proceed with promotion.
                }
                Err(e) => {
                    warn!(
                        "Failed to query peer promotion announcements for workload {}: {}; proceeding with promotion (best-effort)",
                        slot.workload_id, e
                    );
                }
            }
        }

        info!(
            "Promoting standby slot {} → primary (vmid {})",
            slot.workload_id, slot.container_config.id
        );
        if let Err(e) = backend.create_container(&slot.container_config).await {
            error!(
                "Standby promotion failed for workload {}: backend error: {}",
                slot.workload_id, e
            );
            // Re-insert the slot so a later revocation retry could
            // pick it up. Operator-level alerting is left to the
            // logs; the consumer-facing observability story for
            // failed promotion is a follow-up.
            standby_slots
                .lock()
                .await
                .insert(slot.workload_id.clone(), slot);
            return;
        }

        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let workload = WorkloadInfo {
            vmid: slot.container_config.id,
            workload_type: "lxc".to_string(),
            spec_id: slot.spec_id.clone(),
            created_at: now,
            expires_at: slot.expires_at,
            owner_npub: slot.owner_npub.clone(),
            consumer_workload_id: Some(slot.workload_id.clone()),
            // After promotion this provider IS the primary — record
            // replication as None so the orchestrator doesn't try
            // to re-emit a revocation if quorum is lost again
            // (we'd need a fresh standby topology for that, which
            // the consumer hasn't provided post-promotion).
            replication: crate::durable_workload::ReplicationMode::None,
            restart_policy: crate::durable_workload::RestartPolicy::default(),
            state_uri: None,
        };
        let active_workloads_count = {
            let mut w = workloads.lock().await;
            w.insert(slot.container_config.id, workload.clone());
            w.len() as u32
        };

        state_machine
            .lock()
            .await
            .track(crate::durable_workload::DurableWorkload {
                workload_id: slot.container_config.id,
                provider_npub: String::new(), // filled by orchestrator on first heartbeat tick
                state: crate::durable_workload::WorkloadState::Provisioning { since: now },
                replication: workload.replication.clone(),
                restart_policy: workload.restart_policy,
                state_uri: workload.state_uri.clone(),
                created_at: now,
                expires_at: workload.expires_at,
            });

        info!(
            "Standby promotion complete: workload {} now running locally (vmid {})",
            slot.workload_id, slot.container_config.id
        );
        let _ = active_workloads_count; // currently unused now that we publish announcement instead of heartbeat

        // Publish a `StandbyPromotionAnnouncement` IMMEDIATELY so
        // higher-indexed peer standbys see it on their next
        // pre-emption check (which fires after their per-index
        // backoff). Without this announcement, peers would either
        // produce a duplicate primary (no signal) or both drop
        // their slots (heartbeat-based dedup, since every peer
        // emits its own periodic heartbeat regardless of
        // promotion state).
        let announcement = StandbyPromotionAnnouncementContent {
            workload_id: slot.workload_id.clone(),
            new_primary_npub: nostr.get_service_public_key(),
            promoted_at: now,
            version: crate::nostr::SCHEMA_VERSION,
        };
        if let Err(e) = nostr
            .publish_standby_promotion_announcement(announcement)
            .await
        {
            warn!(
                "Post-promotion announcement publish failed for workload {}: {}; peer standbys will not back off and may produce a duplicate primary",
                slot.workload_id, e
            );
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::durable_workload::ReplicationMode;

    fn req_with(
        replication: Option<ReplicationMode>,
        primary_npub: Option<&str>,
    ) -> EncryptedSpawnPodRequest {
        EncryptedSpawnPodRequest {
            cashu_token: "tok".to_string(),
            pod_spec_id: Some("basic".to_string()),
            pod_image: "ubuntu:22.04".to_string(),
            ssh_username: "u".to_string(),
            ssh_password: "p".to_string(),
            template_slug: None,
            replication,
            primary_npub: primary_npub.map(|s| s.to_string()),
            workload_id: Some("wid-test".to_string()),
            volume_encryption: None,
        }
    }

    #[test]
    fn role_is_primary_for_non_warm_standby() {
        // No-replication request → role is Primary regardless of
        // self_npub (existing single-provider behavior unchanged).
        let r = compute_warm_standby_role("npub1self", &req_with(None, None));
        assert_eq!(r, WarmStandbyRole::Primary);

        let r = compute_warm_standby_role(
            "npub1self",
            &req_with(Some(ReplicationMode::Checkpointed), None),
        );
        assert_eq!(r, WarmStandbyRole::Primary);
    }

    #[test]
    fn role_is_primary_when_self_is_designated_primary() {
        let r = compute_warm_standby_role(
            "npub1primary",
            &req_with(
                Some(ReplicationMode::WarmStandby {
                    standby_providers: vec!["npub1b".to_string(), "npub1c".to_string()],
                }),
                Some("npub1primary"),
            ),
        );
        assert_eq!(r, WarmStandbyRole::Primary);
    }

    #[test]
    fn role_is_standby_with_correct_index_when_self_in_list() {
        let r = compute_warm_standby_role(
            "npub1c",
            &req_with(
                Some(ReplicationMode::WarmStandby {
                    standby_providers: vec!["npub1b".to_string(), "npub1c".to_string()],
                }),
                Some("npub1primary"),
            ),
        );
        assert_eq!(r, WarmStandbyRole::Standby { index: 1, count: 2 });
    }

    #[test]
    fn role_is_not_addressed_when_self_unknown_to_topology() {
        let r = compute_warm_standby_role(
            "npub1stranger",
            &req_with(
                Some(ReplicationMode::WarmStandby {
                    standby_providers: vec!["npub1b".to_string(), "npub1c".to_string()],
                }),
                Some("npub1primary"),
            ),
        );
        assert_eq!(r, WarmStandbyRole::NotAddressed);
    }

    // ----- standby watchdog: primary_is_silent decision -----
    //
    // The pure-function gate the watchdog uses to decide whether to
    // fire `schedule_standby_promotion` for a slot. Pinning the
    // edge cases here so a future refactor can't silently flip the
    // semantics that the warm-standby crash-detection promise rests
    // on.

    #[test]
    fn fresh_primary_heartbeat_is_not_silent() {
        // Heartbeat 60s old, threshold 180s — comfortably alive.
        assert!(!primary_is_silent(1_000_000, 999_940, 180));
    }

    #[test]
    fn primary_just_past_threshold_is_silent() {
        // 180s old vs 180s threshold — promotion fires.
        assert!(primary_is_silent(1_000_000, 999_820, 180));
        // 179s old — still alive (one second of grace).
        assert!(!primary_is_silent(1_000_000, 999_821, 180));
    }

    #[test]
    fn unset_baseline_is_not_silent() {
        // baseline == 0 is the "unknown / caller forgot" sentinel.
        // The watchdog must always pass either a real last-heartbeat
        // timestamp or the slot's reservation timestamp; a 0 here
        // means the caller mis-wired the lookup, in which case
        // returning false (alive) is the safe failure mode — better
        // a missed promotion than a spurious one against a healthy
        // primary.
        assert!(!primary_is_silent(1_000_000, 0, 180));
        assert!(!primary_is_silent(50, 0, 180));
    }

    #[test]
    fn fresh_slot_within_grace_window_is_not_silent() {
        // No heartbeat observed yet, baseline = slot.created_at.
        // 30s after reservation, still 150s of grace — primary is
        // alive (just hasn't published its first heartbeat to us
        // yet, or it landed on a relay we're not subscribed to).
        let created_at = 1_000_000;
        let now = created_at + 30;
        assert!(!primary_is_silent(now, created_at, 180));
    }

    #[test]
    fn fresh_slot_past_grace_window_is_silent() {
        // No heartbeat observed yet AND we've waited the full
        // silence window since slot reservation. Either the primary
        // crashed before publishing any heartbeat we could see, or
        // it never came up. Either way, promote.
        let created_at = 1_000_000;
        let now = created_at + 180;
        assert!(primary_is_silent(now, created_at, 180));
    }

    #[test]
    fn clock_skew_underflow_does_not_panic_or_misfire() {
        // baseline > now (clock went backwards or relay returned
        // a future-stamped event). saturating_sub yields 0; 0 < any
        // positive threshold; so primary is treated as alive. Better
        // false-negative (no promotion) than panic.
        assert!(!primary_is_silent(100, 200, 180));
    }

    // ----- standby slot expiry: cleanup_loop selection -----

    fn make_slot(workload_id: &str, expires_at: u64) -> StandbySlot {
        StandbySlot {
            workload_id: workload_id.to_string(),
            primary_npub: "npub1primary".to_string(),
            standby_index: 0,
            standby_count: 1,
            container_config: ContainerConfig {
                id: 1,
                name: "test".to_string(),
                image: "img".to_string(),
                cpu_cores: 1,
                memory_mb: 1024,
                storage_gb: 10,
                password: "p".to_string(),
                ssh_key: None,
                host_port: None,
                template_ports: vec![],
                template_env: HashMap::new(),
                extra_runtime_args: vec![],
                data_path: None,
                volume_encryption_key: None,
            },
            spec_id: "basic".to_string(),
            expires_at,
            owner_npub: "npub1owner".to_string(),
            created_at: 0,
            peer_standby_npubs: vec![],
        }
    }

    fn select_expired(slots: &HashMap<String, StandbySlot>, now: u64) -> Vec<String> {
        slots
            .iter()
            .filter(|(_, slot)| slot.expires_at <= now)
            .map(|(workload_id, _)| workload_id.clone())
            .collect()
    }

    #[test]
    fn select_expired_returns_only_past_expiry_slots() {
        let mut slots = HashMap::new();
        slots.insert("active".to_string(), make_slot("active", 2_000));
        slots.insert("expired".to_string(), make_slot("expired", 999));
        let mut expired = select_expired(&slots, 1_000);
        expired.sort();
        assert_eq!(expired, vec!["expired".to_string()]);
    }

    #[test]
    fn select_expired_treats_expires_at_equals_now_as_expired() {
        // A slot whose lease ends *exactly now* should be reaped on
        // this tick, not held over for the next 30s tick. expires_at
        // is the FIRST instant the lease no longer applies.
        let mut slots = HashMap::new();
        slots.insert("boundary".to_string(), make_slot("boundary", 1_000));
        let expired = select_expired(&slots, 1_000);
        assert_eq!(expired, vec!["boundary".to_string()]);
    }

    #[test]
    fn select_expired_returns_empty_when_no_slots_expired() {
        let mut slots = HashMap::new();
        slots.insert("a".to_string(), make_slot("a", 9_999));
        slots.insert("b".to_string(), make_slot("b", 9_999));
        assert!(select_expired(&slots, 1_000).is_empty());
    }
}
