// Durability and failover: driving the workload state machine from heartbeat
// observations, and the two paths that promote a warm standby — a published
// `LeaseRevocation` (graceful) and the watchdog (primary crashed).

use anyhow::Result;
use tracing::{debug, error, info, warn};

use crate::compute::ContainerStatus;
use crate::durable_workload::{HeartbeatObservation, StateMachineEvent};
use crate::nostr::LeaseRevocationContent;

use super::handlers::HandlerDeps;
use super::persistence::persist_workloads;
use super::standby::{
    primary_is_silent, schedule_standby_promotion, STANDBY_HEARTBEAT_SILENCE_SECS,
    STANDBY_WATCHDOG_INTERVAL_SECS,
};
use super::{ProviderService, StandbySlot};

/// Well under `t1=120s` / `t2=300s`, so transitions are detected promptly
/// without churning idle providers.
const ORCHESTRATOR_INTERVAL_SECS: u64 = 15;

impl ProviderService {
    /// Drain the observation buffer, advance the state machine, act on the
    /// emitted events.
    pub(super) async fn orchestrator_loop(&self) -> Result<()> {
        let interval = tokio::time::Duration::from_secs(ORCHESTRATOR_INTERVAL_SECS);
        info!(
            "Orchestrator loop starting (cadence: {}s)",
            ORCHESTRATOR_INTERVAL_SECS
        );

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
                self.publish_lease_revocation(workload_id, standby_providers, now)
                    .await;
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
                self.reclaim_failed_workload(workload_id).await;
            }
        }
    }

    async fn publish_lease_revocation(
        &self,
        workload_id: u32,
        standby_providers: Vec<String>,
        now: u64,
    ) {
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

    /// Once the workload leaves active_workloads, cleanup_loop can never see it
    /// again — anything left behind strands the container and burns its vmid for
    /// good. So only reclaim a container that already terminated itself; a
    /// still-running box belongs to a consumer who paid through `expires_at`.
    async fn reclaim_failed_workload(&self, workload_id: u32) {
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

    /// Promote ourselves when a primary stops heartbeating.
    ///
    /// The `LeaseRevocation` listener only covers *graceful* failover, where the
    /// primary still has network access and chooses to give up the lease. A hard
    /// crash publishes no revocation, so without this loop warm standby would
    /// only protect against the workload dying, not the provider hosting it.
    ///
    /// At most one promotion happens per workload: within this process because
    /// both callers funnel through `schedule_standby_promotion`, which removes
    /// the slot atomically; across processes because the winner publishes a
    /// promotion announcement that later peers check for.
    pub(super) async fn standby_watchdog_loop(&self) -> Result<()> {
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
                // timestamp so a fresh slot gets a full silence window of grace.
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
pub(super) async fn handle_lease_revocation(
    deps: &HandlerDeps,
    revocation: LeaseRevocationContent,
) {
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
