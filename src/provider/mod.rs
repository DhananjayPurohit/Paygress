// Provider service: publishes the offer to Nostr, heartbeats, and serves spawn
// / topup / status requests over NIP-17 DMs (and, when `http_bind_addr` is set,
// over the HTTP+ngx_l402 interface in `provider_http`).

mod config;
mod handlers;
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
    DurableWorkload, HeartbeatObservation, QuorumConfig, StateMachineEvent, WorkloadState,
    WorkloadStateMachine,
};
use crate::lxd::LxdBackend;
use crate::nostr::{
    parse_private_message_content, CapacityInfo, ErrorResponseContent, HeartbeatContent,
    LeaseRevocationContent, NostrRelaySubscriber, PrivateRequest, ProviderOfferContent,
    RelayConfig,
};
use crate::proxmox::{ProxmoxBackend, ProxmoxClient};

use handlers::{handle_spawn_request, handle_status_request, handle_topup_request, HandlerDeps};
use persistence::{load_standby_slots, load_workloads, persist_workloads};
use standby::{
    primary_is_silent, schedule_standby_promotion, STANDBY_HEARTBEAT_SILENCE_SECS,
    STANDBY_WATCHDOG_INTERVAL_SECS,
};

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

    /// Heartbeat observations awaiting the next orchestrator tick. Filled by
    /// the heartbeat loop (one per relay that ACK'd), drained per tick.
    observation_buffer: Arc<Mutex<Vec<HeartbeatObservation>>>,

    /// Reserved warm-standby slots keyed by consumer-assigned `workload_id`.
    /// Drained when a matching `LeaseRevocation` arrives or the slot expires.
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
                Arc::new(crate::kvm::KvmBackend::new(crate::kvm::KvmConfig::default()))
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
    ///
    /// Restored workloads re-enter the state machine as `Provisioning`, the
    /// same path a fresh spawn takes.
    /// Reload standby reservations. Unlike workloads there is nothing on the
    /// backend to reconcile against — a slot is a promise, not a container —
    /// so expired ones are simply dropped.
    async fn restore_standby_slots(&self, now: u64) {
        let persisted = load_standby_slots(&self.config.standby_state_path);
        if persisted.is_empty() {
            return;
        }
        let total = persisted.len();
        let live: std::collections::HashMap<String, StandbySlot> = persisted
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

    pub fn get_npub(&self) -> String {
        self.nostr.get_service_public_key()
    }

    /// Shared state for the HTTP+ngx_l402 interface: Arc-clones of this
    /// service's own state, so both control planes see the same live data.
    /// The Cashu redeemer is deliberately excluded — see `provider_http`.
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

        // `pending()` keeps the select! branch dormant when the HTTP interface
        // is disabled.
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

        // One observation per relay that ACK'd. `now` serves as both timestamps
        // because we just published; relays that didn't ACK get no observation.
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

    /// Every 15s, drain the observation buffer, advance the state machine, and
    /// act on the emitted events. 15s is well under `t1=120s` / `t2=300s` so
    /// transitions are detected promptly without churning idle providers.
    async fn orchestrator_loop(&self) -> Result<()> {
        let interval = tokio::time::Duration::from_secs(15);
        info!("Orchestrator loop starting (cadence: 15s)");

        loop {
            tokio::time::sleep(interval).await;

            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)?
                .as_secs();

            let observations: Vec<HeartbeatObservation> = {
                let mut buf = self.observation_buffer.lock().await;
                std::mem::take(&mut *buf)
            };

            let events = {
                let mut sm = self.state_machine.lock().await;
                sm.tick(now, &observations)
            };

            for event in events {
                self.handle_state_machine_event(event, now).await;
            }
        }
    }

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
                // The vmid-derived fallback is unreachable for a real
                // warm-standby workload (the spawn handler requires the UUID);
                // it just keeps this call total.
                let (consumer_workload_id, state_uri) = {
                    let lock = self.active_workloads.lock().await;
                    let entry = lock.get(&workload_id);
                    let cid = entry
                        .and_then(|w| w.consumer_workload_id.clone())
                        .unwrap_or_else(|| format!("vmid-{}", workload_id));
                    (cid, entry.and_then(|w| w.state_uri.clone()))
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
                // Respawning needs the original ContainerConfig, which
                // `WorkloadInfo` does not retain. Record the failure so the
                // state machine retries or fails out deterministically instead
                // of hanging in Respawning.
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

                // Once the workload leaves active_workloads, cleanup_loop can
                // never see it again — anything left behind strands the
                // container and burns its vmid for good. So only reclaim a
                // container that already terminated itself; a still-running box
                // belongs to a consumer who paid through `expires_at`.
                match self.backend.get_container_status(workload_id).await {
                    Ok(ContainerStatus::Stopped) => {
                        if let Err(e) = self.backend.delete_container(workload_id).await {
                            warn!(
                                "failed to delete self-terminated workload {}: {}",
                                workload_id, e
                            );
                        }
                    }
                    Ok(ContainerStatus::Absent) => {}
                    Ok(ContainerStatus::Running) => {
                        warn!(
                            "workload {} failed but its container is still running; \
                             leaving it to the expiry sweep",
                            workload_id
                        );
                        return;
                    }
                    Err(e) => {
                        warn!(
                            "could not determine container status for failed workload {}: {} \
                             — leaving it to the expiry sweep",
                            workload_id, e
                        );
                        return;
                    }
                }

                self.state_machine.lock().await.untrack(workload_id);
                let mut wl = self.active_workloads.lock().await;
                wl.remove(&workload_id);
                persist_workloads(&wl, &self.config.workload_state_path);
            }
        }
    }

    async fn cleanup_loop(&self) -> Result<()> {
        let interval = tokio::time::Duration::from_secs(30);

        loop {
            tokio::time::sleep(interval).await;

            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)?
                .as_secs();

            self.reap_expired_workloads(now).await;
            self.reap_expired_standby_slots(now).await;
        }
    }

    async fn reap_expired_workloads(&self, now: u64) {
        let mut workloads = self.active_workloads.lock().await;
        let expired: Vec<u32> = workloads
            .iter()
            .filter(|(_, w)| w.expires_at <= now)
            .map(|(vmid, _)| *vmid)
            .collect();

        for vmid in expired {
            info!("Cleaning up expired workload: {}", vmid);

            if workloads.remove(&vmid).is_none() {
                continue;
            }

            // Delete unconditionally: the workload is already out of the map, so
            // a failed stop that skipped the delete would leak the container and
            // its vmid forever, with no retry. `delete --force` handles running
            // and stopped alike.
            if let Err(e) = self.backend.stop_container(vmid).await {
                warn!("stop failed for {} ({}), deleting anyway", vmid, e);
            }
            let result = self.backend.delete_container(vmid).await;

            // Untrack regardless of backend success: the lease is over, and the
            // orchestrator should not keep driving transitions on it.
            self.state_machine.lock().await.untrack(vmid);

            match result {
                Ok(_) => {
                    info!("Cleaned up workload {}", vmid);
                    self.stats.lock().await.total_jobs_completed += 1;
                }
                Err(e) => error!("Failed to cleanup workload {}: {}", vmid, e),
            }

            // Persist per workload, not once per sweep: a crash midway through
            // would otherwise resurrect entries whose containers are gone.
            persist_workloads(&workloads, &self.config.workload_state_path);
        }
    }

    /// Drop slots whose lease window passed without a failover. The watchdog
    /// already skips past-expiry slots, but without this the map grows
    /// unbounded on a long-running provider.
    async fn reap_expired_standby_slots(&self, now: u64) {
        let mut slots = self.standby_slots.lock().await;
        let expired: Vec<String> = slots
            .iter()
            .filter(|(_, slot)| slot.expires_at <= now)
            .map(|(workload_id, _)| workload_id.clone())
            .collect();
        let any_expired = !expired.is_empty();
        for workload_id in expired {
            if let Some(slot) = slots.remove(&workload_id) {
                info!(
                    "Expiring standby slot for workload {} (index {}/{}, primary {}, expired at {})",
                    workload_id,
                    slot.standby_index,
                    slot.standby_count,
                    slot.primary_npub,
                    slot.expires_at
                );
            }
        }
        if !any_expired {
            return;
        }
        persistence::persist_standby_slots(&slots, &self.config.standby_state_path);
    }

    /// Promote ourselves when a primary stops heartbeating.
    ///
    /// The `LeaseRevocation` listener only covers *graceful* failover — the
    /// primary still has network access and chooses to give up the lease. It
    /// does not cover a hard crash (process death, host offline, kernel panic),
    /// where no revocation is ever published. Without this loop, warm standby
    /// would only protect against the workload dying, not the provider hosting
    /// it, which is the more common failure.
    ///
    /// At most one promotion happens per workload: within this process because
    /// both callers funnel through `schedule_standby_promotion`, which removes
    /// the slot atomically; across processes because the winner publishes a
    /// promotion announcement that later peers check for.
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

            // Many slots may share one primary; query each npub once.
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
                // With no heartbeat observed yet, fall back to the reservation
                // timestamp so a fresh slot gets a full silence window of grace
                // instead of promoting over a healthy primary on the first tick.
                let silence_baseline = match heartbeats.get(&slot.primary_npub) {
                    Some(hb) if hb.timestamp != 0 => hb.timestamp,
                    _ => slot.created_at,
                };
                if !primary_is_silent(now, silence_baseline, STANDBY_HEARTBEAT_SILENCE_SECS) {
                    continue;
                }
                warn!(
                    "Primary {} silent for {}s on slot workload_id={} (threshold {}s); \
                     triggering standby promotion",
                    slot.primary_npub,
                    now.saturating_sub(silence_baseline),
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

/// Schedule promotion if the revocation matches one of our reserved slots.
async fn handle_lease_revocation(deps: &HandlerDeps, revocation: LeaseRevocationContent) {
    info!(
        "Lease revocation observed: workload_id={}, primary={}, reason={}, state_uri={:?}, standbys={:?}",
        revocation.workload_id,
        revocation.primary_provider_npub,
        revocation.reason,
        revocation.state_uri,
        revocation.standby_providers,
    );

    let slot = deps
        .standby_slots
        .lock()
        .await
        .get(&revocation.workload_id)
        .cloned();
    let Some(slot) = slot else {
        debug!(
            "Revocation workload_id={} did not match any local standby slot; ignoring",
            revocation.workload_id
        );
        return;
    };

    if slot.primary_npub != revocation.primary_provider_npub {
        warn!(
            "Revocation primary_npub ({}) does not match slot's primary ({}); ignoring",
            revocation.primary_provider_npub, slot.primary_npub
        );
        return;
    }

    schedule_standby_promotion(
        deps.backend.clone(),
        deps.workloads.clone(),
        deps.state_machine.clone(),
        deps.standby_slots.clone(),
        deps.nostr.clone(),
        slot,
    );
}
