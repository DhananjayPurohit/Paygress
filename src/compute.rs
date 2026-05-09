// Compute Backend Trait
//
// Abstracts the underlying container/VM platform (Proxmox vs LXD)

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
    pub host_port: Option<u16>,
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
