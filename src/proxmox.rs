// Proxmox VE API Client
//
// Provides interface to Proxmox REST API for managing LXC containers and VMs.

use anyhow::{Context, Result};
use reqwest::{header, Client};
use serde::{Deserialize, Serialize};
use tracing::{error, info, warn};

/// Proxmox API client for container and VM management
pub struct ProxmoxClient {
    client: Client,
    base_url: String,
    auth_header: String,
    node: String,
}

/// Configuration for creating an LXC container
#[derive(Debug, Clone, Serialize)]
pub struct LxcConfig {
    pub vmid: u32,
    pub hostname: String,
    pub ostemplate: String,
    pub storage: String,
    pub rootfs: String,
    pub memory: u32,
    pub cores: u32,
    pub net0: String,
    pub password: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ssh_public_keys: Option<String>,
    #[serde(default = "default_true")]
    pub start: bool,
    #[serde(default = "default_true")]
    pub unprivileged: bool,
}

fn default_true() -> bool {
    true
}

/// Configuration for creating a VM
#[derive(Debug, Clone, Serialize)]
pub struct VmConfig {
    pub vmid: u32,
    pub name: String,
    pub memory: u32,
    pub cores: u32,
    pub sockets: u32,
    pub ide2: String,  // ISO image
    pub scsi0: String, // Disk
    pub net0: String,
    pub ostype: String,
    #[serde(default = "default_true")]
    pub start: bool,
}

/// Node status information
#[derive(Debug, Clone, Deserialize)]
pub struct NodeStatus {
    pub cpu: f64,
    pub memory: MemoryInfo,
    pub uptime: u64,
    #[serde(default)]
    pub loadavg: Vec<f64>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct MemoryInfo {
    pub total: u64,
    pub used: u64,
    pub free: u64,
}

/// Container/VM status
#[derive(Debug, Clone, Deserialize)]
pub struct WorkloadStatus {
    pub vmid: u32,
    pub status: String,
    pub name: String,
    #[serde(default)]
    pub uptime: u64,
    #[serde(default)]
    pub cpu: f64,
    #[serde(default)]
    pub mem: u64,
    #[serde(default)]
    pub maxmem: u64,
}

/// Proxmox API response wrapper
#[derive(Debug, Deserialize)]
struct ProxmoxResponse<T> {
    data: Option<T>,
    #[serde(default)]
    errors: Option<serde_json::Value>,
}

/// Task response from Proxmox (for async operations)
#[derive(Debug, Deserialize)]
struct TaskResponse {
    data: String, // UPID (task ID)
}

impl ProxmoxClient {
    /// Create a new Proxmox client
    ///
    /// # Arguments
    /// * `api_url` - Base URL of the Proxmox API (e.g., "https://192.168.1.100:8006/api2/json")
    /// * `token_id` - API token ID (e.g., "root@pam!paygress")
    /// * `token_secret` - API token secret
    /// * `node` - Proxmox node name (e.g., "pve")
    pub fn new(api_url: &str, token_id: &str, token_secret: &str, node: &str) -> Result<Self> {
        // Build client with self-signed cert support
        let client = Client::builder()
            .danger_accept_invalid_certs(true) // Proxmox often uses self-signed certs
            .build()
            .context("Failed to create HTTP client")?;

        let auth_header = format!("PVEAPIToken={}={}", token_id, token_secret);

        Ok(Self {
            client,
            base_url: api_url.trim_end_matches('/').to_string(),
            auth_header,
            node: node.to_string(),
        })
    }

    /// Get the node URL prefix
    fn node_url(&self) -> String {
        format!("{}/nodes/{}", self.base_url, self.node)
    }

    // ==================== LXC Container Operations ====================

    /// Create an LXC container
    pub async fn create_lxc(&self, config: &LxcConfig) -> Result<String> {
        let url = format!("{}/lxc", self.node_url());

        info!(
            "Creating LXC container {} on node {}",
            config.vmid, self.node
        );

        let response = self
            .client
            .post(&url)
            .header(header::AUTHORIZATION, &self.auth_header)
            .form(config)
            .send()
            .await
            .context("Failed to send create LXC request")?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            error!("Failed to create LXC: {} - {}", status, body);
            anyhow::bail!("Proxmox API error: {} - {}", status, body);
        }

        let task: TaskResponse = response
            .json()
            .await
            .context("Failed to parse create LXC response")?;

        info!("LXC creation task started: {}", task.data);
        Ok(task.data)
    }

    /// Start an LXC container
    pub async fn start_lxc(&self, vmid: u32) -> Result<String> {
        let url = format!("{}/lxc/{}/status/start", self.node_url(), vmid);

        info!("Starting LXC container {}", vmid);

        let response = self
            .client
            .post(&url)
            .header(header::AUTHORIZATION, &self.auth_header)
            .send()
            .await
            .context("Failed to send start LXC request")?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            anyhow::bail!("Failed to start LXC {}: {} - {}", vmid, status, body);
        }

        let task: TaskResponse = response
            .json()
            .await
            .context("Failed to parse start LXC response")?;

        Ok(task.data)
    }

    /// Stop an LXC container
    pub async fn stop_lxc(&self, vmid: u32) -> Result<String> {
        let url = format!("{}/lxc/{}/status/stop", self.node_url(), vmid);

        info!("Stopping LXC container {}", vmid);

        let response = self
            .client
            .post(&url)
            .header(header::AUTHORIZATION, &self.auth_header)
            .send()
            .await
            .context("Failed to send stop LXC request")?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            anyhow::bail!("Failed to stop LXC {}: {} - {}", vmid, status, body);
        }

        let task: TaskResponse = response
            .json()
            .await
            .context("Failed to parse stop LXC response")?;

        Ok(task.data)
    }

    /// Delete an LXC container
    pub async fn delete_lxc(&self, vmid: u32) -> Result<String> {
        let url = format!("{}/lxc/{}", self.node_url(), vmid);

        info!("Deleting LXC container {}", vmid);

        let response = self
            .client
            .delete(&url)
            .header(header::AUTHORIZATION, &self.auth_header)
            .send()
            .await
            .context("Failed to send delete LXC request")?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            anyhow::bail!("Failed to delete LXC {}: {} - {}", vmid, status, body);
        }

        let task: TaskResponse = response
            .json()
            .await
            .context("Failed to parse delete LXC response")?;

        Ok(task.data)
    }

    /// Get LXC container status
    pub async fn get_lxc_status(&self, vmid: u32) -> Result<WorkloadStatus> {
        let url = format!("{}/lxc/{}/status/current", self.node_url(), vmid);

        let response = self
            .client
            .get(&url)
            .header(header::AUTHORIZATION, &self.auth_header)
            .send()
            .await
            .context("Failed to get LXC status")?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            anyhow::bail!("Failed to get LXC {} status: {} - {}", vmid, status, body);
        }

        let resp: ProxmoxResponse<WorkloadStatus> = response
            .json()
            .await
            .context("Failed to parse LXC status response")?;

        resp.data.context("No status data returned")
    }

    /// List all LXC containers on the node
    pub async fn list_lxc(&self) -> Result<Vec<WorkloadStatus>> {
        let url = format!("{}/lxc", self.node_url());

        let response = self
            .client
            .get(&url)
            .header(header::AUTHORIZATION, &self.auth_header)
            .send()
            .await
            .context("Failed to list LXC containers")?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            anyhow::bail!("Failed to list LXC containers: {} - {}", status, body);
        }

        let resp: ProxmoxResponse<Vec<WorkloadStatus>> = response
            .json()
            .await
            .context("Failed to parse LXC list response")?;

        Ok(resp.data.unwrap_or_default())
    }

    // ==================== VM Operations ====================

    /// Create a VM
    pub async fn create_vm(&self, config: &VmConfig) -> Result<String> {
        let url = format!("{}/qemu", self.node_url());

        info!("Creating VM {} on node {}", config.vmid, self.node);

        let response = self
            .client
            .post(&url)
            .header(header::AUTHORIZATION, &self.auth_header)
            .form(config)
            .send()
            .await
            .context("Failed to send create VM request")?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            error!("Failed to create VM: {} - {}", status, body);
            anyhow::bail!("Proxmox API error: {} - {}", status, body);
        }

        let task: TaskResponse = response
            .json()
            .await
            .context("Failed to parse create VM response")?;

        info!("VM creation task started: {}", task.data);
        Ok(task.data)
    }

    /// Start a VM
    pub async fn start_vm(&self, vmid: u32) -> Result<String> {
        let url = format!("{}/qemu/{}/status/start", self.node_url(), vmid);

        info!("Starting VM {}", vmid);

        let response = self
            .client
            .post(&url)
            .header(header::AUTHORIZATION, &self.auth_header)
            .send()
            .await
            .context("Failed to send start VM request")?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            anyhow::bail!("Failed to start VM {}: {} - {}", vmid, status, body);
        }

        let task: TaskResponse = response
            .json()
            .await
            .context("Failed to parse start VM response")?;

        Ok(task.data)
    }

    /// Stop a VM
    pub async fn stop_vm(&self, vmid: u32) -> Result<String> {
        let url = format!("{}/qemu/{}/status/stop", self.node_url(), vmid);

        info!("Stopping VM {}", vmid);

        let response = self
            .client
            .post(&url)
            .header(header::AUTHORIZATION, &self.auth_header)
            .send()
            .await
            .context("Failed to send stop VM request")?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            anyhow::bail!("Failed to stop VM {}: {} - {}", vmid, status, body);
        }

        let task: TaskResponse = response
            .json()
            .await
            .context("Failed to parse stop VM response")?;

        Ok(task.data)
    }

    /// Delete a VM
    pub async fn delete_vm(&self, vmid: u32) -> Result<String> {
        let url = format!("{}/qemu/{}", self.node_url(), vmid);

        info!("Deleting VM {}", vmid);

        let response = self
            .client
            .delete(&url)
            .header(header::AUTHORIZATION, &self.auth_header)
            .send()
            .await
            .context("Failed to send delete VM request")?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            anyhow::bail!("Failed to delete VM {}: {} - {}", vmid, status, body);
        }

        let task: TaskResponse = response
            .json()
            .await
            .context("Failed to parse delete VM response")?;

        Ok(task.data)
    }

    /// Get VM status
    pub async fn get_vm_status(&self, vmid: u32) -> Result<WorkloadStatus> {
        let url = format!("{}/qemu/{}/status/current", self.node_url(), vmid);

        let response = self
            .client
            .get(&url)
            .header(header::AUTHORIZATION, &self.auth_header)
            .send()
            .await
            .context("Failed to get VM status")?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            anyhow::bail!("Failed to get VM {} status: {} - {}", vmid, status, body);
        }

        let resp: ProxmoxResponse<WorkloadStatus> = response
            .json()
            .await
            .context("Failed to parse VM status response")?;

        resp.data.context("No status data returned")
    }

    /// List all VMs on the node
    pub async fn list_vm(&self) -> Result<Vec<WorkloadStatus>> {
        let url = format!("{}/qemu", self.node_url());

        let response = self
            .client
            .get(&url)
            .header(header::AUTHORIZATION, &self.auth_header)
            .send()
            .await
            .context("Failed to list VMs")?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            anyhow::bail!("Failed to list VMs: {} - {}", status, body);
        }

        let resp: ProxmoxResponse<Vec<WorkloadStatus>> = response
            .json()
            .await
            .context("Failed to parse VM list response")?;

        Ok(resp.data.unwrap_or_default())
    }

    // ==================== Node Operations ====================

    /// Get node status (CPU, memory, uptime)
    pub async fn get_node_status(&self) -> Result<NodeStatus> {
        let url = format!("{}/status", self.node_url());

        let response = self
            .client
            .get(&url)
            .header(header::AUTHORIZATION, &self.auth_header)
            .send()
            .await
            .context("Failed to get node status")?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            anyhow::bail!("Failed to get node status: {} - {}", status, body);
        }

        let resp: ProxmoxResponse<NodeStatus> = response
            .json()
            .await
            .context("Failed to parse node status response")?;

        resp.data.context("No node status data returned")
    }

    /// Find the next available VMID in a given range
    pub async fn find_available_vmid(&self, range_start: u32, range_end: u32) -> Result<u32> {
        let lxc_list = self.list_lxc().await?;
        let vm_list = self.list_vm().await?;

        let used_ids: std::collections::HashSet<u32> = lxc_list
            .iter()
            .chain(vm_list.iter())
            .map(|w| w.vmid)
            .collect();

        for vmid in range_start..=range_end {
            if !used_ids.contains(&vmid) {
                return Ok(vmid);
            }
        }

        anyhow::bail!("No available VMID in range {}-{}", range_start, range_end)
    }

    /// Wait for a task to complete
    pub async fn wait_for_task(&self, upid: &str, timeout_secs: u64) -> Result<()> {
        use tokio::time::{sleep, Duration};

        let start = std::time::Instant::now();
        let timeout = Duration::from_secs(timeout_secs);

        loop {
            if start.elapsed() > timeout {
                anyhow::bail!("Task {} timed out after {} seconds", upid, timeout_secs);
            }

            let url = format!("{}/tasks/{}/status", self.node_url(), upid);

            let response = self
                .client
                .get(&url)
                .header(header::AUTHORIZATION, &self.auth_header)
                .send()
                .await?;

            if response.status().is_success() {
                #[derive(Deserialize)]
                struct TaskStatus {
                    status: String,
                    #[serde(default)]
                    exitstatus: Option<String>,
                }

                let resp: ProxmoxResponse<TaskStatus> = response.json().await?;

                if let Some(task) = resp.data {
                    if task.status == "stopped" {
                        if let Some(exit) = task.exitstatus {
                            if exit == "OK" {
                                return Ok(());
                            } else {
                                anyhow::bail!("Task failed with: {}", exit);
                            }
                        }
                        return Ok(());
                    }
                }
            }

            sleep(Duration::from_secs(2)).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_lxc_config_serialization() {
        let config = LxcConfig {
            vmid: 100,
            hostname: "test-container".to_string(),
            ostemplate: "local:vztmpl/ubuntu-22.04-standard.tar.zst".to_string(),
            storage: "local-lvm".to_string(),
            rootfs: "local-lvm:8".to_string(),
            memory: 1024,
            cores: 1,
            net0: "name=eth0,bridge=vmbr0,ip=dhcp".to_string(),
            password: "testpass".to_string(),
            ssh_public_keys: None,
            start: true,
            unprivileged: true,
        };

        // Should serialize without errors
        let _serialized = serde_urlencoded::to_string(&config).unwrap();
    }
}

// ==================== ComputeBackend Implementation ====================

use crate::compute::{ComputeBackend, ContainerConfig, NodeStatus as ComputeNodeStatus};
use async_trait::async_trait;

/// Wrapper around ProxmoxClient to implement ComputeBackend trait
pub struct ProxmoxBackend {
    client: ProxmoxClient,
    storage: String,
    bridge: String,
    template: String,
}

impl ProxmoxBackend {
    pub fn new(client: ProxmoxClient, storage: &str, bridge: &str, template: &str) -> Self {
        Self {
            client,
            storage: storage.to_string(),
            bridge: bridge.to_string(),
            template: template.to_string(),
        }
    }
}

#[async_trait]
impl ComputeBackend for ProxmoxBackend {
    async fn find_available_id(&self, range_start: u32, range_end: u32) -> Result<u32> {
        self.client
            .find_available_vmid(range_start, range_end)
            .await
    }

    async fn create_container(&self, config: &ContainerConfig) -> Result<String> {
        // Use default template if config doesn't specify (config.image is usually "ubuntu:22.04" etc)
        // But Proxmox needs full template path "local:vztmpl/..."
        // For now, we ignore config.image and use self.template

        let lxc = LxcConfig {
            vmid: config.id,
            hostname: config.name.clone(),
            ostemplate: self.template.clone(),
            storage: self.storage.clone(),
            rootfs: format!("{}:8", self.storage),
            memory: config.memory_mb,
            cores: config.cpu_cores,
            net0: format!("name=eth0,bridge={},ip=dhcp", self.bridge),
            password: config.password.clone(),
            ssh_public_keys: config.ssh_key.clone(),
            start: true,
            unprivileged: true,
        };

        let task = self.client.create_lxc(&lxc).await?;
        self.client.wait_for_task(&task, 120).await?;
        Ok(config.id.to_string())
    }

    async fn start_container(&self, id: u32) -> Result<()> {
        let task = self.client.start_lxc(id).await?;
        self.client.wait_for_task(&task, 60).await?;
        Ok(())
    }

    async fn stop_container(&self, id: u32) -> Result<()> {
        let task = self.client.stop_lxc(id).await?;
        self.client.wait_for_task(&task, 60).await?;
        Ok(())
    }

    async fn delete_container(&self, id: u32) -> Result<()> {
        let task = self.client.delete_lxc(id).await?;
        self.client.wait_for_task(&task, 60).await?;
        Ok(())
    }

    async fn get_node_status(&self) -> Result<ComputeNodeStatus> {
        let status = self.client.get_node_status().await?;
        Ok(ComputeNodeStatus {
            cpu_usage: status.cpu,
            memory_used: status.memory.used,
            memory_total: status.memory.total,
            disk_used: 0,
            disk_total: 0,
        })
    }

    async fn get_container_ip(&self, _id: u32) -> Result<Option<String>> {
        // Proxmox API connection logic to get IP is complex without guest agent
        Ok(None)
    }
}
