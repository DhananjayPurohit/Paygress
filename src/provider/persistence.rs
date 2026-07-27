use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use tracing::{error, warn};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkloadInfo {
    pub vmid: u32,
    /// `lxc` or `vm`.
    pub workload_type: String,
    pub spec_id: String,
    pub created_at: u64,
    pub expires_at: u64,
    pub owner_npub: String,

    /// `WarmStandby` makes the orchestrator emit a `LeaseRevocation` on local
    /// eviction.
    #[serde(default)]
    pub replication: crate::durable_workload::ReplicationMode,

    #[serde(default)]
    pub restart_policy: crate::durable_workload::RestartPolicy,

    /// Blossom URI of the latest checkpoint; a standby restores from it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state_uri: Option<String>,

    /// Consumer-assigned id, so a published revocation carries the same id the
    /// standbys keyed their slots by.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub consumer_workload_id: Option<String>,
}

/// Mirror the active-workload table to disk.
///
/// Written to a sibling temp file and renamed, because a truncated state file
/// is worse than a stale one: the loader would treat every workload past the
/// truncation point as never having existed. `rename` is atomic on POSIX.
///
/// Failures are logged, never propagated — refusing a paid-for spawn over an
/// unwritable bookkeeping file would be worse than a stale mirror.
pub(crate) fn persist_workloads(workloads: &HashMap<u32, WorkloadInfo>, path: &str) {
    let tmp = format!("{}.tmp", path);
    let encoded = match serde_json::to_vec_pretty(workloads) {
        Ok(v) => v,
        Err(e) => {
            error!("failed to encode workload state: {}", e);
            return;
        }
    };
    if let Err(e) = std::fs::write(&tmp, &encoded) {
        error!("failed to write workload state to {}: {}", tmp, e);
        return;
    }
    if let Err(e) = std::fs::rename(&tmp, path) {
        error!("failed to install workload state at {}: {}", path, e);
        let _ = std::fs::remove_file(&tmp);
    }
}

/// A missing file is the normal first-run case; a corrupt one degrades to
/// empty, because a provider that refuses to boot over unreadable bookkeeping
/// is worse than one that boots having forgotten some leases.
pub(crate) fn load_workloads(path: &str) -> HashMap<u32, WorkloadInfo> {
    let raw = match std::fs::read(path) {
        Ok(r) => r,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return HashMap::new(),
        Err(e) => {
            warn!("failed to read workload state from {}: {}", path, e);
            return HashMap::new();
        }
    };
    match serde_json::from_slice(&raw) {
        Ok(w) => w,
        Err(e) => {
            error!(
                "workload state at {} is unreadable ({}); starting with an empty table. \
                 Containers it referenced will need manual cleanup.",
                path, e
            );
            HashMap::new()
        }
    }
}

/// Mirror reserved standby slots to disk, so a restart does not silently drop
/// reservations the consumer has already paid for.
///
/// Slots whose `container_config` carries a volume-encryption key are
/// deliberately **not** written: that key is consumer key material, and the
/// alternatives are persisting the key or silently promoting to an unencrypted
/// volume. Both are worse than losing a reservation the consumer can re-make.
pub(crate) fn persist_standby_slots(slots: &HashMap<String, super::StandbySlot>, path: &str) {
    let persistable: HashMap<&String, &super::StandbySlot> = slots
        .iter()
        .filter(|(id, slot)| {
            let encrypted = slot.container_config.volume_encryption_key.is_some();
            if encrypted {
                warn!(
                    "standby slot {} holds a consumer volume key and is not persisted; \
                     a restart will drop this reservation",
                    id
                );
            }
            !encrypted
        })
        .collect();

    let tmp = format!("{}.tmp", path);
    let encoded = match serde_json::to_vec_pretty(&persistable) {
        Ok(v) => v,
        Err(e) => {
            error!("failed to encode standby slots: {}", e);
            return;
        }
    };
    if let Err(e) = std::fs::write(&tmp, &encoded) {
        error!("failed to write standby slots to {}: {}", tmp, e);
        return;
    }
    if let Err(e) = std::fs::rename(&tmp, path) {
        error!("failed to install standby slots at {}: {}", path, e);
        let _ = std::fs::remove_file(&tmp);
    }
}

pub(crate) fn load_standby_slots(path: &str) -> HashMap<String, super::StandbySlot> {
    let raw = match std::fs::read(path) {
        Ok(r) => r,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return HashMap::new(),
        Err(e) => {
            warn!("failed to read standby slots from {}: {}", path, e);
            return HashMap::new();
        }
    };
    match serde_json::from_slice(&raw) {
        Ok(s) => s,
        Err(e) => {
            error!(
                "standby slots at {} are unreadable ({}); starting with none reserved.",
                path, e
            );
            HashMap::new()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_path(name: &str) -> String {
        let mut p = std::env::temp_dir();
        p.push(format!(
            "paygress-workload-state-{}-{}.json",
            std::process::id(),
            name
        ));
        p.to_string_lossy().into_owned()
    }

    fn workload(vmid: u32, expires_at: u64) -> WorkloadInfo {
        WorkloadInfo {
            vmid,
            workload_type: "lxc".to_string(),
            spec_id: "ci".to_string(),
            created_at: 1000,
            expires_at,
            owner_npub: "npub1consumer".to_string(),
            replication: Default::default(),
            restart_policy: Default::default(),
            state_uri: None,
            consumer_workload_id: None,
        }
    }

    fn slot(
        workload_id: &str,
        expires_at: u64,
        key: Option<[u8; 32]>,
    ) -> super::super::StandbySlot {
        super::super::StandbySlot {
            workload_id: workload_id.to_string(),
            primary_npub: "npub1primary".to_string(),
            standby_index: 0,
            standby_count: 2,
            container_config: crate::compute::ContainerConfig {
                id: 2000,
                name: "paygress-2000".to_string(),
                image: "ubuntu:24.04".to_string(),
                cpu_cores: 1,
                memory_mb: 512,
                storage_gb: 5,
                password: "pw".to_string(),
                ssh_key: None,
                host_port: Some(2000),
                template_ports: vec![],
                template_env: HashMap::new(),
                extra_runtime_args: vec![],
                data_path: None,
                volume_encryption_key: key,
            },
            spec_id: "basic".to_string(),
            expires_at,
            owner_npub: "npub1consumer".to_string(),
            created_at: 100,
            peer_standby_npubs: vec![],
        }
    }

    #[test]
    fn standby_slots_round_trip() {
        let p = temp_path("slots");
        let mut map = HashMap::new();
        map.insert("w-1".to_string(), slot("w-1", 999, None));
        persist_standby_slots(&map, &p);

        let loaded = load_standby_slots(&p);
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded["w-1"].expires_at, 999);
        assert_eq!(loaded["w-1"].owner_npub, "npub1consumer");
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn slots_holding_consumer_key_material_are_never_written() {
        // Persisting the key would put consumer key material on provider
        // disk; loading it back as `None` would silently promote to an
        // unencrypted volume. Dropping the reservation is the safe option.
        let p = temp_path("slots-encrypted");
        let mut map = HashMap::new();
        map.insert("plain".to_string(), slot("plain", 999, None));
        map.insert(
            "encrypted".to_string(),
            slot("encrypted", 999, Some([7u8; 32])),
        );
        persist_standby_slots(&map, &p);

        let raw = std::fs::read_to_string(&p).unwrap();
        assert!(!raw.contains("encrypted"), "encrypted slot leaked: {}", raw);
        assert!(!raw.contains("volume_encryption_key"));

        let loaded = load_standby_slots(&p);
        assert_eq!(loaded.len(), 1);
        assert!(loaded.contains_key("plain"));
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn missing_state_file_loads_empty() {
        let p = temp_path("absent");
        let _ = std::fs::remove_file(&p);
        assert!(load_workloads(&p).is_empty());
    }

    #[test]
    fn round_trips_the_fields_cleanup_depends_on() {
        let p = temp_path("roundtrip");
        let mut map = HashMap::new();
        map.insert(2000, workload(2000, 1234567890));
        map.insert(2001, workload(2001, 1234567999));
        persist_workloads(&map, &p);

        let loaded = load_workloads(&p);
        assert_eq!(loaded.len(), 2);
        // expires_at drives the cleanup sweep and owner_npub gates topup;
        // losing either would strand or misassign a lease.
        assert_eq!(loaded[&2000].expires_at, 1234567890);
        assert_eq!(loaded[&2001].expires_at, 1234567999);
        assert_eq!(loaded[&2000].owner_npub, "npub1consumer");
        assert_eq!(loaded[&2000].spec_id, "ci");

        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn corrupt_state_degrades_to_empty_rather_than_failing() {
        let p = temp_path("corrupt");
        std::fs::write(&p, b"{ this is not json").unwrap();
        assert!(load_workloads(&p).is_empty());
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn write_leaves_no_temp_file_behind() {
        let p = temp_path("tmpfile");
        let map = HashMap::from([(2000, workload(2000, 42))]);
        persist_workloads(&map, &p);
        assert!(!std::path::Path::new(&format!("{}.tmp", p)).exists());
        assert!(std::path::Path::new(&p).exists());
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn rewrite_replaces_rather_than_merges() {
        // Cleanup removes entries by rewriting the whole table; if a rewrite
        // merged, deleted workloads would resurrect on restart.
        let p = temp_path("replace");
        persist_workloads(&HashMap::from([(2000, workload(2000, 1))]), &p);
        persist_workloads(&HashMap::from([(2001, workload(2001, 2))]), &p);

        let loaded = load_workloads(&p);
        assert_eq!(loaded.len(), 1);
        assert!(loaded.contains_key(&2001));
        let _ = std::fs::remove_file(&p);
    }
}
