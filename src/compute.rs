// Compute Backend Trait
//
// Abstracts the underlying container/VM platform (Proxmox vs LXD vs
// Docker). The Docker backend (src/docker.rs) is the one that uses
// ports + env in `ContainerConfig`; LXD/Proxmox backends ignore
// those fields today and only use the SSH-style fields.

use std::collections::HashMap;

use anyhow::Result;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NodeStatus {
    pub cpu_usage: f64,    // 0.0 to 1.0
    pub memory_used: u64,  // bytes
    pub memory_total: u64, // bytes
    pub disk_used: u64,    // bytes
    pub disk_total: u64,   // bytes
}

/// One published port mapping. The Docker backend translates this to
/// a `-p host_port:container_port` flag; LXD/Proxmox can ignore
/// (they expose the SSH port via the existing host_port field).
#[derive(Debug, Clone)]
pub struct PortMapping {
    pub host_port: u16,
    pub container_port: u16,
    pub protocol: &'static str, // "tcp" | "udp"
}

#[derive(Debug, Clone)]
pub struct ContainerConfig {
    pub id: u32,
    pub name: String,
    pub image: String,
    pub cpu_cores: u32,
    pub memory_mb: u32,
    pub storage_gb: u32,
    pub password: String,
    pub ssh_key: Option<String>,
    /// SSH host-port forwarding (LXD/Proxmox: SSH access). Distinct
    /// from `template_ports` which are workload-specific.
    pub host_port: Option<u16>,
    /// Workload ports the consumer reaches (e.g. nostr-relay 7777,
    /// bitcoind RPC 18443). Empty for non-template spawns. Docker
    /// backend translates each to `-p host:container`; LXD/Proxmox
    /// backends ignore for now.
    pub template_ports: Vec<PortMapping>,
    /// Workload environment variables (template defaults +
    /// consumer overrides). Docker backend passes via `-e KEY=VAL`.
    pub template_env: HashMap<String, String>,

    /// Extra `docker run` flags from the template definition (e.g.
    /// `--ulimit nofile=1048576:1048576` for strfry). LXD/Proxmox
    /// backends ignore these.
    pub extra_runtime_args: Vec<String>,

    /// In-container path for the workload's persistent state.
    /// Docker backend mounts a vmid-scoped volume there.
    /// `None` = stateless (no volume created).
    pub data_path: Option<String>,

    /// Optional 32-byte LUKS key for the persistent data volume.
    /// When set (Phase 2 of consumer-encrypted-volumes), the
    /// `DockerBackend` creates a LUKS-on-loop file instead of a
    /// plain Docker named volume; the key is fed to `cryptsetup
    /// luksFormat`/`luksOpen` over stdin and never persisted to
    /// disk. On `delete_container` the LUKS header is erased
    /// (`cryptsetup luksErase`) so the keyslots are unrecoverable
    /// even if the operator forensically extracts the underlying
    /// file.
    ///
    /// Provider populates this from
    /// `EncryptedSpawnPodRequest.volume_encryption.decoded_key()`
    /// when present; `None` means a plain volume (today's default).
    /// `data_path: None` makes this field a no-op (stateless
    /// workloads have nothing to encrypt).
    pub volume_encryption_key: Option<[u8; 32]>,
}

#[async_trait]
pub trait ComputeBackend: Send + Sync {
    /// Find an available ID in the given range
    async fn find_available_id(&self, range_start: u32, range_end: u32) -> Result<u32>;

    /// Create a new container
    async fn create_container(&self, config: &ContainerConfig) -> Result<String>; // Returns container ID/Name

    /// Start a container
    async fn start_container(&self, id: u32) -> Result<()>;

    /// Stop a container
    async fn stop_container(&self, id: u32) -> Result<()>;

    /// Delete a container
    async fn delete_container(&self, id: u32) -> Result<()>;

    /// Get node resource usage
    async fn get_node_status(&self) -> Result<NodeStatus>;

    /// Get public IP of the container/VM
    async fn get_container_ip(&self, id: u32) -> Result<Option<String>>;
}
