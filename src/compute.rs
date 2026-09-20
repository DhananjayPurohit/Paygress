// Compute backend trait shared by the Docker, LXD, KVM and Proxmox backends.

use std::collections::HashMap;
use std::process::Stdio;

use anyhow::{Context, Result};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};

/// Host-visible name for a workload. `find_available_id` parses the id back out
/// of this form, so the two must stay in sync.
pub fn container_name(id: u32) -> String {
    format!("paygress-{}", id)
}

pub fn id_from_container_name(name: &str) -> Option<u32> {
    name.strip_prefix("paygress-")?.parse().ok()
}

/// Run `program`, failing with the child's stderr when it exits non-zero.
pub(crate) async fn run_checked(program: &str, args: &[&str]) -> Result<String> {
    let out = tokio::process::Command::new(program)
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await
        .with_context(|| format!("invoke {}", program))?;
    if !out.status.success() {
        anyhow::bail!(
            "{} failed: {}",
            program,
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).to_string())
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NodeStatus {
    pub cpu_usage: f64,
    pub memory_used: u64,
    pub memory_total: u64,
    pub disk_used: u64,
    pub disk_total: u64,
}

impl NodeStatus {
    /// `None` when the backend does not measure disk -- Docker, KVM and Proxmox
    /// report zeroes rather than a reading, and treating that as "no space" would
    /// refuse every spawn they serve.
    pub fn free_disk_bytes(&self) -> Option<u64> {
        (self.disk_total > 0).then(|| self.disk_total.saturating_sub(self.disk_used))
    }

    /// Whether a spawn has room. Unknown disk is room enough: the alternative is
    /// a check that takes three of four backends offline the day it ships.
    pub fn has_disk_headroom(&self, min_free_gb: u64) -> bool {
        if min_free_gb == 0 {
            return true;
        }
        match self.free_disk_bytes() {
            Some(free) => free >= min_free_gb.saturating_mul(1024 * 1024 * 1024),
            None => true,
        }
    }
}

/// One published port mapping. Docker-only; LXD/Proxmox expose just SSH via
/// `ContainerConfig::host_port`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PortMapping {
    pub host_port: u16,
    pub container_port: u16,
    /// "tcp" | "udp"
    pub protocol: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContainerConfig {
    pub id: u32,
    pub name: String,
    pub image: String,
    pub cpu_cores: u32,
    pub memory_mb: u32,
    pub storage_gb: u32,
    pub password: String,
    pub ssh_key: Option<String>,
    /// SSH host-port forward. Distinct from `template_ports`.
    pub host_port: Option<u16>,
    pub template_ports: Vec<PortMapping>,
    /// Template defaults plus consumer overrides.
    pub template_env: HashMap<String, String>,
    /// Extra `docker run` flags from the template definition.
    pub extra_runtime_args: Vec<String>,
    /// In-container path for persistent state. `None` = stateless.
    pub data_path: Option<String>,
    /// 32-byte LUKS key for the data volume. When set the Docker backend builds
    /// a LUKS-on-loop file instead of a plain named volume. No-op when
    /// `data_path` is `None`.
    ///
    /// `serde(skip)`: this is consumer key material. It stays in memory for the
    /// life of the process and is never written to provider-side state.
    #[serde(skip)]
    pub volume_encryption_key: Option<[u8; 32]>,
}

#[async_trait]
pub trait ComputeBackend: Send + Sync {
    async fn find_available_id(&self, range_start: u32, range_end: u32) -> Result<u32>;

    /// Returns the backend's container ID/name.
    async fn create_container(&self, config: &ContainerConfig) -> Result<String>;

    async fn start_container(&self, id: u32) -> Result<()>;

    async fn stop_container(&self, id: u32) -> Result<()>;

    async fn delete_container(&self, id: u32) -> Result<()>;

    async fn get_node_status(&self) -> Result<NodeStatus>;

    async fn get_container_ip(&self, id: u32) -> Result<Option<String>>;

    /// Defaults to `Running` so a backend that cannot answer never causes a
    /// destructive action to be taken on its behalf.
    async fn get_container_status(&self, _id: u32) -> Result<ContainerStatus> {
        Ok(ContainerStatus::Running)
    }
}

/// Three-valued: an unreachable backend must not be mistaken for a stopped
/// workload.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContainerStatus {
    Running,
    Stopped,
    /// The backend answered, but the workload is not in its list.
    Absent,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn status_with_disk(total: u64, used: u64) -> NodeStatus {
        NodeStatus {
            cpu_usage: 0.0,
            memory_used: 0,
            memory_total: 0,
            disk_used: used,
            disk_total: total,
        }
    }

    const GB: u64 = 1024 * 1024 * 1024;

    #[test]
    fn a_full_disk_has_no_headroom() {
        // The Sep 18 shape: 193G total, 437M free. The provider took payment and
        // tried to build a container anyway.
        let status = status_with_disk(193 * GB, 193 * GB - (437 * 1024 * 1024));
        assert!(!status.has_disk_headroom(10));
    }

    #[test]
    fn a_healthy_disk_has_headroom() {
        assert!(status_with_disk(193 * GB, 67 * GB).has_disk_headroom(10));
    }

    #[test]
    fn a_backend_that_does_not_measure_disk_is_not_refused() {
        // Docker, KVM and Proxmox report zeroes.
        assert!(status_with_disk(0, 0).has_disk_headroom(10));
        assert_eq!(status_with_disk(0, 0).free_disk_bytes(), None);
    }

    #[test]
    fn zero_threshold_disables_the_check() {
        assert!(status_with_disk(193 * GB, 193 * GB).has_disk_headroom(0));
    }

    #[test]
    fn used_over_total_does_not_underflow() {
        assert_eq!(
            status_with_disk(10 * GB, 12 * GB).free_disk_bytes(),
            Some(0)
        );
        assert!(!status_with_disk(10 * GB, 12 * GB).has_disk_headroom(1));
    }

    #[test]
    fn container_name_round_trips() {
        assert_eq!(container_name(1234), "paygress-1234");
        assert_eq!(id_from_container_name("paygress-1234"), Some(1234));
        assert_eq!(id_from_container_name("something-else"), None);
        assert_eq!(id_from_container_name("paygress-notanumber"), None);
    }
}
