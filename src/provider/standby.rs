use std::collections::HashMap;
use std::sync::Arc;

use tokio::sync::Mutex;
use tracing::{debug, error, info, warn};

use crate::compute::{ComputeBackend, ContainerConfig};
use crate::durable_workload::WorkloadStateMachine;
use crate::nostr::{
    warm_standby_role, EncryptedSpawnPodRequest, NostrRelaySubscriber,
    StandbyPromotionAnnouncementContent, WarmStandbyRole,
};
use crate::provider::persistence::WorkloadInfo;

/// Cadence at which the standby watchdog re-queries heartbeats.
pub(crate) const STANDBY_WATCHDOG_INTERVAL_SECS: u64 = 30;

/// How long without a primary heartbeat before we treat the primary as crashed.
/// 3x the default 60s cadence, so two missed beats are tolerated before
/// promotion fires.
pub(crate) const STANDBY_HEARTBEAT_SILENCE_SECS: u64 = 180;

/// Standby `i` waits `i * DELAY` after observing a revocation before spawning.
/// Single-writer is best-effort: a brief two-Live window is an accepted v1
/// trade-off.
const STANDBY_PROMOTION_DELAY_SECS: u64 = 30;

/// A paid-for, acknowledged warm-standby reservation. No container exists yet;
/// the standby is armed and waiting for a `LeaseRevocation` from the primary.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct StandbySlot {
    pub workload_id: String,
    pub primary_npub: String,
    pub standby_index: usize,
    pub standby_count: usize,
    pub container_config: ContainerConfig,
    pub spec_id: String,
    pub expires_at: u64,
    pub owner_npub: String,
    /// The watchdog's silence baseline before any primary heartbeat is
    /// observed; without it a fresh slot would read `last_seen == 0` as silence
    /// and promote over a healthy primary.
    pub created_at: u64,
    /// The other standbys, queried at promotion time to detect that a peer
    /// already promoted; without it every standby would promote independently.
    pub peer_standby_npubs: Vec<String>,
}

/// `baseline` is the most recent primary heartbeat, or the slot's reservation
/// timestamp when none has been observed. `baseline == 0` means the caller
/// mis-wired the lookup and returns `false`: a missed promotion beats a
/// spurious one against a healthy primary.
pub(crate) fn primary_is_silent(now: u64, baseline: u64, threshold: u64) -> bool {
    if baseline == 0 {
        return false;
    }
    now.saturating_sub(baseline) >= threshold
}

/// Non-`WarmStandby` requests return `Primary`: in the single-provider path the
/// "primary" is just the one provider running the workload.
pub(crate) fn compute_warm_standby_role(
    self_npub: &str,
    request: &EncryptedSpawnPodRequest,
) -> WarmStandbyRole {
    use crate::durable_workload::ReplicationMode;
    match request.replication.as_ref() {
        Some(ReplicationMode::WarmStandby { standby_providers }) => {
            let primary = request.primary_npub.as_deref().unwrap_or("");
            warm_standby_role(self_npub, primary, standby_providers)
        }
        _ => WarmStandbyRole::Primary,
    }
}

/// Runs on its own task so the caller returns immediately. After the per-index
/// backoff it checks for a peer's promotion announcement, spawns the container,
/// then publishes its own announcement so higher-indexed peers back off.
pub(crate) fn schedule_standby_promotion(
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

        // Removing the slot is what makes promotion at-most-once within this
        // process (watchdog vs. revocation listener, or duplicate revocations
        // from several relays). The announcement query below covers peers.
        let slot = match standby_slots.lock().await.remove(&workload_id) {
            Some(s) => s,
            None => {
                debug!(
                    "Standby slot for workload {} already drained; skipping promotion",
                    workload_id
                );
                return;
            }
        };

        // Heartbeats cannot serve as the peer-promotion signal: every standby
        // heartbeats regardless of promotion state, so a fresh one means "peer
        // online", not "peer promoted".
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
                Ok(None) => {}
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
            // Re-insert so a later revocation retry can pick the slot up.
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
            // None, so the orchestrator doesn't re-emit a revocation on a
            // later quorum loss: that would need a fresh standby topology from
            // the consumer, which post-promotion we don't have.
            replication: crate::durable_workload::ReplicationMode::None,
            restart_policy: crate::durable_workload::RestartPolicy::default(),
            state_uri: None,
        };
        workloads
            .lock()
            .await
            .insert(slot.container_config.id, workload.clone());

        state_machine
            .lock()
            .await
            .track(crate::durable_workload::DurableWorkload {
                workload_id: slot.container_config.id,
                provider_npub: String::new(), // filled by the orchestrator's first tick
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

    // The pure gate the watchdog uses to decide whether to promote. The edge
    // cases are pinned so a refactor can't silently flip the semantics the
    // crash-detection promise rests on.

    #[test]
    fn fresh_primary_heartbeat_is_not_silent() {
        assert!(!primary_is_silent(1_000_000, 999_940, 180));
    }

    #[test]
    fn primary_just_past_threshold_is_silent() {
        assert!(primary_is_silent(1_000_000, 999_820, 180));
        // 179s old — still alive.
        assert!(!primary_is_silent(1_000_000, 999_821, 180));
    }

    #[test]
    fn unset_baseline_is_not_silent() {
        assert!(!primary_is_silent(1_000_000, 0, 180));
        assert!(!primary_is_silent(50, 0, 180));
    }

    #[test]
    fn fresh_slot_within_grace_window_is_not_silent() {
        let created_at = 1_000_000;
        let now = created_at + 30;
        assert!(!primary_is_silent(now, created_at, 180));
    }

    #[test]
    fn fresh_slot_past_grace_window_is_silent() {
        let created_at = 1_000_000;
        let now = created_at + 180;
        assert!(primary_is_silent(now, created_at, 180));
    }

    #[test]
    fn clock_skew_underflow_does_not_panic_or_misfire() {
        // baseline > now (clock went backwards, or a future-stamped event).
        assert!(!primary_is_silent(100, 200, 180));
    }

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
        // expires_at is the FIRST instant the lease no longer applies, so a
        // slot ending exactly now is reaped on this tick.
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
