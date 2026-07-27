// Provider service: publishes the offer to Nostr, heartbeats, and serves spawn
// / topup / status requests over NIP-17 DMs (and, when `http_bind_addr` is set,
// over the HTTP+ngx_l402 interface in `provider_http`).
//
// The five loops `run` selects over live here (setup, offer, heartbeat, request
// listener), in `orchestrator` (state machine, standby watchdog) and in
// `cleanup` (expiry sweep).

mod cleanup;
mod config;
mod handlers;
mod orchestrator;
mod persistence;
mod standby;

pub use config::{load_config, save_config, BackendType, ProviderConfig};
pub use handlers::parse_pod_npub;
pub use persistence::WorkloadInfo;
pub use standby::StandbySlot;

pub(crate) use handlers::generate_password;

use std::collections::HashMap;
use std::sync::Arc;

use anyhow::Result;
use tokio::sync::Mutex;
use tracing::{debug, error, info, warn};

use crate::cashu::{resolve_wallet_seed, CdkRedeemer, MintRedeemer};
use crate::compute::{ComputeBackend, ContainerStatus};
use crate::docker::DockerBackend;
use crate::durable_workload::{
    DurableWorkload, HeartbeatObservation, QuorumConfig, WorkloadState, WorkloadStateMachine,
};
use crate::lxd::LxdBackend;
use crate::nostr::{
    parse_private_message_content, CapacityInfo, ErrorResponseContent, HeartbeatContent,
    NostrRelaySubscriber, PrivateRequest, ProviderOfferContent, RelayConfig,
};
use crate::proxmox::{ProxmoxBackend, ProxmoxClient};

use handlers::{handle_spawn_request, handle_status_request, handle_topup_request, HandlerDeps};
use orchestrator::handle_lease_revocation;
use persistence::{load_standby_slots, load_workloads, persist_workloads};

pub struct ProviderService {
    config: ProviderConfig,
    backend: Arc<dyn ComputeBackend>,
    nostr: NostrRelaySubscriber,
    redeemer: Arc<dyn MintRedeemer>,
    active_workloads: Arc<Mutex<HashMap<u32, WorkloadInfo>>>,
    stats: Arc<Mutex<ProviderStats>>,

    /// Keyed by vmid; tracks each local workload through
    /// `Provisioning → Live → Suspect → Evicted/Respawning/Failed`.
    state_machine: Arc<Mutex<WorkloadStateMachine>>,

    /// Heartbeat observations awaiting the next orchestrator tick: one per relay
    /// that ACK'd, drained per tick.
    observation_buffer: Arc<Mutex<Vec<HeartbeatObservation>>>,

    /// Reserved warm-standby slots keyed by consumer-assigned `workload_id`.
    standby_slots: Arc<Mutex<HashMap<String, StandbySlot>>>,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct ProviderStats {
    pub(crate) total_jobs_completed: u64,
}

impl ProviderService {
    pub async fn new(config: ProviderConfig) -> Result<Self> {
        let backend: Arc<dyn ComputeBackend> = match config.backend_type {
            BackendType::Proxmox => {
                let client = ProxmoxClient::new(
                    &config.proxmox_url,
                    &config.proxmox_token_id,
                    &config.proxmox_token_secret,
                    &config.proxmox_node,
                    config.proxmox_accept_invalid_certs,
                )?;
                Arc::new(ProxmoxBackend::new(
                    client,
                    &config.proxmox_storage,
                    &config.proxmox_bridge,
                    &config.proxmox_template,
                ))
            }
            BackendType::LXD => Arc::new(LxdBackend::new(
                &config.proxmox_storage, // storage field doubles as the pool name
                &config.proxmox_bridge,  // bridge field doubles as the network
            )),
            BackendType::Docker => Arc::new(DockerBackend::new()),
            BackendType::Kvm => {
                // Fail at startup rather than at the first spawn, when a
                // consumer has already committed a Cashu token.
                if let Err(e) = crate::kvm::KvmBackend::check_kvm_available().await {
                    tracing::error!("KVM backend selected but unavailable: {}", e);
                    anyhow::bail!("KVM backend unavailable: {}", e);
                }
                Arc::new(crate::kvm::KvmBackend::new(
                    crate::kvm::KvmConfig::for_provider(
                        config.kvm_base_image_path.as_deref(),
                        config.kvm_base_image_url.as_deref(),
                    ),
                ))
            }
        };

        let relay_config = RelayConfig {
            relays: config.nostr_relays.clone(),
            private_key: Some(config.nostr_private_key.clone()),
        };
        let nostr = NostrRelaySubscriber::new(relay_config).await?;

        // A provider upgrading across the redb → SQLite switch still has the
        // old path in its config; refuse it with an explanation rather than
        // letting the driver report "file is not a database".
        crate::cashu::ensure_not_legacy_redb_wallet(std::path::Path::new(
            &config.cashu_wallet_db_path,
        ))?;

        let wallet_db = cdk_sqlite::WalletSqliteDatabase::new(std::path::PathBuf::from(
            &config.cashu_wallet_db_path,
        ))
        .await
        .map_err(|e| {
            anyhow::anyhow!(
                "failed to open cashu wallet database at {}: {}",
                config.cashu_wallet_db_path,
                e
            )
        })?;
        let seed = resolve_wallet_seed(&config.nostr_private_key)
            .map_err(|e| anyhow::anyhow!("failed to derive wallet seed: {}", e))?;
        let redeemer: Arc<dyn MintRedeemer> = Arc::new(CdkRedeemer::new(Arc::new(wallet_db), seed));

        Ok(Self {
            config,
            backend,
            nostr,
            redeemer,
            active_workloads: Arc::new(Mutex::new(HashMap::new())),
            stats: Arc::new(Mutex::new(ProviderStats::default())),
            state_machine: Arc::new(Mutex::new(WorkloadStateMachine::new(
                QuorumConfig::default(),
            ))),
            observation_buffer: Arc::new(Mutex::new(Vec::new())),
            standby_slots: Arc::new(Mutex::new(HashMap::new())),
        })
    }

    /// Reload leases from disk and reconcile them against the backend.
    ///
    /// The backend is the authority on what exists: a container deleted while
    /// the provider was down would otherwise be tracked forever and
    /// re-announced as capacity that isn't there.
    async fn restore_workloads(&self) {
        let persisted = load_workloads(&self.config.workload_state_path);
        if persisted.is_empty() {
            return;
        }

        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);

        let mut restored = HashMap::new();
        let mut dropped = 0usize;
        for (vmid, workload) in persisted {
            match self.backend.get_container_status(vmid).await {
                Ok(ContainerStatus::Absent) => {
                    info!(
                        "workload {} no longer exists on the backend; dropping",
                        vmid
                    );
                    dropped += 1;
                    continue;
                }
                Err(e) => {
                    // An unreachable backend must not be read as "the container
                    // is gone". The cleanup sweep deletes it at expiry anyway.
                    warn!(
                        "could not verify workload {} ({}); keeping it tracked",
                        vmid, e
                    );
                }
                Ok(_) => {}
            }

            // Restored workloads re-enter as `Provisioning`, the same path a
            // fresh spawn takes.
            self.state_machine.lock().await.track(DurableWorkload {
                workload_id: vmid,
                provider_npub: self.nostr.get_service_public_key(),
                state: WorkloadState::Provisioning { since: now },
                replication: workload.replication.clone(),
                restart_policy: workload.restart_policy,
                state_uri: workload.state_uri.clone(),
                created_at: workload.created_at,
                expires_at: workload.expires_at,
            });
            restored.insert(vmid, workload);
        }

        let expired = restored.values().filter(|w| w.expires_at <= now).count();
        info!(
            "restored {} workload(s) from {} ({} dropped as missing, {} already expired and \
             due for cleanup)",
            restored.len(),
            self.config.workload_state_path,
            dropped,
            expired,
        );

        let mut lock = self.active_workloads.lock().await;
        *lock = restored;
        // Write back now so dropped entries don't linger until the next sweep.
        persist_workloads(&lock, &self.config.workload_state_path);
    }

    /// Unlike workloads there is nothing on the backend to reconcile against —
    /// a slot is a promise, not a container — so expired ones are just dropped.
    async fn restore_standby_slots(&self, now: u64) {
        let persisted = load_standby_slots(&self.config.standby_state_path);
        if persisted.is_empty() {
            return;
        }
        let total = persisted.len();
        let live: HashMap<String, StandbySlot> = persisted
            .into_iter()
            .filter(|(_, slot)| slot.expires_at > now)
            .collect();
        info!(
            "restored {} standby slot(s) ({} dropped as expired)",
            live.len(),
            total - live.len()
        );
        *self.standby_slots.lock().await = live;
    }

    pub fn get_npub(&self) -> String {
        self.nostr.get_service_public_key()
    }

    /// Arc-clones of this service's own state, so both control planes see the
    /// same live data. The Cashu redeemer is deliberately excluded — see
    /// `provider_http`.
    pub(crate) fn http_state(&self) -> crate::provider_http::ProviderHttpState {
        crate::provider_http::ProviderHttpState {
            config: self.config.clone(),
            backend: self.backend.clone(),
            active_workloads: self.active_workloads.clone(),
            stats: self.stats.clone(),
            state_machine: self.state_machine.clone(),
            provider_npub: self.get_npub(),
        }
    }

    /// Run the provider until one of its loops exits.
    pub async fn run(&self) -> Result<()> {
        info!("🚀 Starting Paygress Provider Service");
        info!("Provider: {}", self.config.provider_name);
        info!("NPUB: {}", self.get_npub());

        self.restore_workloads().await;
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        self.restore_standby_slots(now).await;
        self.publish_offer().await?;

        let http_fut: std::pin::Pin<Box<dyn std::future::Future<Output = Result<()>> + Send>> =
            if let Some(ref addr) = self.config.http_bind_addr {
                info!("HTTP+ngx_l402 interface enabled on {}", addr);
                let state = self.http_state();
                let addr = addr.clone();
                Box::pin(async move {
                    crate::provider_http::run_provider_http_interface(state, &addr).await
                })
            } else {
                Box::pin(std::future::pending::<anyhow::Result<()>>())
            };

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
            result = http_fut => {
                error!("HTTP+ngx_l402 interface exited: {:?}", result);
                result
            }
        }
    }

    async fn publish_offer(&self) -> Result<()> {
        let stats = self.stats.lock().await;

        let offer = ProviderOfferContent {
            provider_npub: self.get_npub(),
            hostname: self.config.provider_name.clone(),
            location: self.config.provider_location.clone(),
            capabilities: self.config.capabilities.clone(),
            specs: self.config.specs.clone(),
            whitelisted_mints: self.config.whitelisted_mints.clone(),
            uptime_percent: 100.0,
            total_jobs_completed: stats.total_jobs_completed,
            api_endpoint: None,
            version: crate::nostr::SCHEMA_VERSION,
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

    async fn heartbeat_loop(&self) -> Result<()> {
        let interval = tokio::time::Duration::from_secs(self.config.heartbeat_interval_secs);

        loop {
            if let Err(e) = self.send_heartbeat().await {
                warn!("Failed to send heartbeat: {}", e);
            }
            tokio::time::sleep(interval).await;
        }
    }

    async fn send_heartbeat(&self) -> Result<()> {
        let workloads = self.active_workloads.lock().await;

        let capacity = match self.backend.get_node_status().await {
            Ok(status) => CapacityInfo {
                cpu_available: ((1.0 - status.cpu_usage) * 100000.0) as u64,
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

        // `now` serves as both timestamps because we just published; relays that
        // didn't ACK get no observation.
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

    async fn listen_for_requests(&self) -> Result<()> {
        info!("Listening for Paygress requests...");

        let deps = HandlerDeps {
            backend: self.backend.clone(),
            config: self.config.clone(),
            nostr: self.nostr.clone(),
            redeemer: self.redeemer.clone(),
            workloads: self.active_workloads.clone(),
            stats: self.stats.clone(),
            state_machine: self.state_machine.clone(),
            standby_slots: self.standby_slots.clone(),
        };

        self.nostr
            .subscribe_to_pod_events(move |event| {
                let deps = deps.clone();

                Box::pin(async move {
                    let my_pubkey = deps.nostr.public_key().to_hex();
                    if event.pubkey == my_pubkey {
                        return Ok(());
                    }

                    debug!(
                        "Handler received event kind: {}, from: {}, message_type: {}",
                        event.kind, event.pubkey, event.message_type
                    );

                    // Revocations are public events: no decryption, no response.
                    if let Some(revocation) = crate::nostr::parse_revocation_event(&event) {
                        handle_lease_revocation(&deps, revocation).await;
                        return Ok(());
                    }

                    let request_type = match parse_private_message_content(&event.content) {
                        Ok(req) => req,
                        Err(e) => {
                            warn!("Failed to parse request from {}: {}", event.pubkey, e);
                            let error = ErrorResponseContent {
                                error_type: "invalid_request".to_string(),
                                message: "Failed to parse request".to_string(),
                                details: Some(e.to_string()),
                            };
                            let _ = deps
                                .nostr
                                .send_error_response_private_message(
                                    &event.pubkey,
                                    error,
                                    &event.message_type,
                                )
                                .await;
                            return Ok(());
                        }
                    };

                    let outcome = match request_type {
                        PrivateRequest::Spawn(req) => {
                            handle_spawn_request(&deps, &event.pubkey, &event.message_type, *req)
                                .await
                                .map_err(|e| ("spawn", e))
                        }
                        PrivateRequest::Status(req) => {
                            handle_status_request(&deps, &event.pubkey, &event.message_type, req)
                                .await
                                .map_err(|e| ("status", e))
                        }
                        PrivateRequest::TopUp(req) => {
                            handle_topup_request(&deps, &event.pubkey, &event.message_type, req)
                                .await
                                .map_err(|e| ("topup", e))
                        }
                    };
                    if let Err((kind, e)) = outcome {
                        error!("Failed to handle {} request: {}", kind, e);
                    }

                    Ok(())
                })
            })
            .await?;

        Ok(())
    }
}
