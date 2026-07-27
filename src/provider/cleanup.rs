// Expiry sweep: the only path that reclaims a container once its lease runs out,
// and the only one that frees the vmid for re-use.

use anyhow::Result;
use tracing::{error, info, warn};

use super::persistence::{persist_standby_slots, persist_workloads};
use super::ProviderService;

const CLEANUP_INTERVAL_SECS: u64 = 30;

impl ProviderService {
    pub(super) async fn cleanup_loop(&self) -> Result<()> {
        let interval = tokio::time::Duration::from_secs(CLEANUP_INTERVAL_SECS);

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

            // Untrack regardless of backend success: the lease is over.
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
    /// already skips past-expiry slots, but without this the map grows unbounded
    /// on a long-running provider.
    async fn reap_expired_standby_slots(&self, now: u64) {
        let mut slots = self.standby_slots.lock().await;
        let expired: Vec<String> = slots
            .iter()
            .filter(|(_, slot)| slot.expires_at <= now)
            .map(|(workload_id, _)| workload_id.clone())
            .collect();
        if expired.is_empty() {
            return;
        }
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
        persist_standby_slots(&slots, &self.config.standby_state_path);
    }
}
