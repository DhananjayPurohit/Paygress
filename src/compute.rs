// Compute backend trait shared by the Docker, LXD, KVM and Proxmox
// backends.

use std::collections::HashMap;
use std::process::Stdio;

use anyhow::{Context, Result};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};

/// Host-visible name for a workload. Every name-addressed backend
/// uses this form and `find_available_id` parses the id back out of
/// it, so the two must stay in sync.
pub fn container_name(id: u32) -> String {
    format!("paygress-{}", id)
}

/// Id encoded in a `paygress-<id>` name, or `None` for anything else.
pub fn id_from_container_name(name: &str) -> Option<u32> {
    name.strip_prefix("paygress-")?.parse().ok()
}

/// Run `program` and return its stdout, failing with the child's
/// stderr when it exits non-zero.
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

/// One published port mapping. Docker translates this to
/// `-p host_port:container_port`; LXD/Proxmox ignore it and expose
/// only SSH via `ContainerConfig::host_port`.
#[derive(Debug, Clone)]
pub struct PortMapping {
    pub host_port: u16,
    pub container_port: u16,
    /// "tcp" | "udp"
    pub protocol: &'static str,
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
    /// SSH host-port forward. Distinct from `template_ports`.
    pub host_port: Option<u16>,
    /// Workload ports the consumer reaches. Docker-only.
    pub template_ports: Vec<PortMapping>,
    /// Workload environment (template defaults + consumer overrides).
    pub template_env: HashMap<String, String>,
    /// Extra `docker run` flags from the template definition.
    pub extra_runtime_args: Vec<String>,
    /// In-container path for persistent state. `None` = stateless,
    /// no volume created.
    pub data_path: Option<String>,
    /// 32-byte LUKS key for the persistent data volume. When set the
    /// Docker backend builds a LUKS-on-loop file instead of a plain
    /// named volume; the key is fed to cryptsetup over stdin and
    /// never written to disk. No-op when `data_path` is `None`.
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

    /// Whether the workload is still running.
    ///
    /// Defaults to `Running` so a backend that cannot answer never
    /// causes a destructive action to be taken on its behalf.
    async fn get_container_status(&self, _id: u32) -> Result<ContainerStatus> {
        Ok(ContainerStatus::Running)
    }
}

/// Coarse run state of a workload. Deliberately three-valued: an
/// unreachable backend must not be mistaken for a stopped workload.
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

    #[test]
    fn container_name_round_trips() {
        assert_eq!(container_name(1234), "paygress-1234");
        assert_eq!(id_from_container_name("paygress-1234"), Some(1234));
        assert_eq!(id_from_container_name("something-else"), None);
        assert_eq!(id_from_container_name("paygress-notanumber"), None);
    }
}
