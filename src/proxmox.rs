// Proxmox VE API client plus its `ComputeBackend` adapter.

use anyhow::{Context, Result};
use async_trait::async_trait;
use reqwest::{header, Client};
use serde::{Deserialize, Serialize};
use tracing::{error, info};

use crate::compute::{ComputeBackend, ContainerConfig, NodeStatus as ComputeNodeStatus};

pub struct ProxmoxClient {
    client: Client,
    base_url: String,
    auth_header: String,
    node: String,
}

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
    pub start: bool,
    pub unprivileged: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct VmConfig {
    pub vmid: u32,
    pub name: String,
    pub memory: u32,
    pub cores: u32,
    pub sockets: u32,
    /// ISO image
    pub ide2: String,
    /// Disk
    pub scsi0: String,
    pub net0: String,
    pub ostype: String,
    pub start: bool,
}

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

#[derive(Debug, Deserialize)]
struct ProxmoxResponse<T> {
    data: Option<T>,
}

/// Proxmox returns a UPID for every async operation.
#[derive(Debug, Deserialize)]
struct TaskResponse {
    data: String,
}

impl ProxmoxClient {
    /// `accept_invalid_certs` disables TLS verification. Proxmox ships
    /// self-signed certs, so operators often need it, but it stays opt-in:
    /// trusting any certificate exposes the API token to anyone on the path.
    pub fn new(
        api_url: &str,
        token_id: &str,
        token_secret: &str,
        node: &str,
        accept_invalid_certs: bool,
    ) -> Result<Self> {
        if accept_invalid_certs {
            tracing::warn!(
                "Proxmox TLS verification is disabled; the API token is exposed to \
                 anyone who can intercept the connection"
            );
        }
        let client = Client::builder()
            .danger_accept_invalid_certs(accept_invalid_certs)
            .build()
            .context("Failed to create HTTP client")?;

        Ok(Self {
            client,
            base_url: api_url.trim_end_matches('/').to_string(),
            auth_header: format!("PVEAPIToken={}={}", token_id, token_secret),
            node: node.to_string(),
        })
    }

    fn node_url(&self) -> String {
        format!("{}/nodes/{}", self.base_url, self.node)
    }

    /// POST/DELETE against `url`, returning the task UPID.
    async fn task_request(&self, request: reqwest::RequestBuilder, what: &str) -> Result<String> {
        let response = request
            .header(header::AUTHORIZATION, &self.auth_header)
            .send()
            .await
            .with_context(|| format!("Failed to send {} request", what))?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            anyhow::bail!("Failed to {}: {} - {}", what, status, body);
        }

        let task: TaskResponse = response
            .json()
            .await
            .with_context(|| format!("Failed to parse {} response", what))?;
        Ok(task.data)
    }

    /// GET `url` and unwrap the `data` envelope.
    async fn get_data<T: serde::de::DeserializeOwned>(&self, url: &str, what: &str) -> Result<T> {
        let response = self
            .client
            .get(url)
            .header(header::AUTHORIZATION, &self.auth_header)
            .send()
            .await
            .with_context(|| format!("Failed to {}", what))?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            anyhow::bail!("Failed to {}: {} - {}", what, status, body);
        }

        let resp: ProxmoxResponse<T> = response
            .json()
            .await
            .with_context(|| format!("Failed to parse {} response", what))?;
        resp.data
            .with_context(|| format!("No data returned for {}", what))
    }

    pub async fn create_lxc(&self, config: &LxcConfig) -> Result<String> {
        info!(
            "Creating LXC container {} on node {}",
            config.vmid, self.node
        );
        let url = format!("{}/lxc", self.node_url());
        let task = self
            .task_request(self.client.post(&url).form(config), "create LXC")
            .await
            .inspect_err(|e| error!("Failed to create LXC: {}", e))?;
        info!("LXC creation task started: {}", task);
        Ok(task)
    }

    pub async fn start_lxc(&self, vmid: u32) -> Result<String> {
        info!("Starting LXC container {}", vmid);
        let url = format!("{}/lxc/{}/status/start", self.node_url(), vmid);
        self.task_request(self.client.post(&url), "start LXC").await
    }

    pub async fn stop_lxc(&self, vmid: u32) -> Result<String> {
        info!("Stopping LXC container {}", vmid);
        let url = format!("{}/lxc/{}/status/stop", self.node_url(), vmid);
        self.task_request(self.client.post(&url), "stop LXC").await
    }

    pub async fn delete_lxc(&self, vmid: u32) -> Result<String> {
        info!("Deleting LXC container {}", vmid);
        let url = format!("{}/lxc/{}", self.node_url(), vmid);
        self.task_request(self.client.delete(&url), "delete LXC")
            .await
    }

    pub async fn get_lxc_status(&self, vmid: u32) -> Result<WorkloadStatus> {
        let url = format!("{}/lxc/{}/status/current", self.node_url(), vmid);
        self.get_data(&url, "get LXC status").await
    }

    pub async fn list_lxc(&self) -> Result<Vec<WorkloadStatus>> {
        let url = format!("{}/lxc", self.node_url());
        self.get_data(&url, "list LXC containers").await
    }

    pub async fn create_vm(&self, config: &VmConfig) -> Result<String> {
        info!("Creating VM {} on node {}", config.vmid, self.node);
        let url = format!("{}/qemu", self.node_url());
        let task = self
            .task_request(self.client.post(&url).form(config), "create VM")
            .await
            .inspect_err(|e| error!("Failed to create VM: {}", e))?;
        info!("VM creation task started: {}", task);
        Ok(task)
    }

    pub async fn start_vm(&self, vmid: u32) -> Result<String> {
        info!("Starting VM {}", vmid);
        let url = format!("{}/qemu/{}/status/start", self.node_url(), vmid);
        self.task_request(self.client.post(&url), "start VM").await
    }

    pub async fn stop_vm(&self, vmid: u32) -> Result<String> {
        info!("Stopping VM {}", vmid);
        let url = format!("{}/qemu/{}/status/stop", self.node_url(), vmid);
        self.task_request(self.client.post(&url), "stop VM").await
    }

    pub async fn delete_vm(&self, vmid: u32) -> Result<String> {
        info!("Deleting VM {}", vmid);
        let url = format!("{}/qemu/{}", self.node_url(), vmid);
        self.task_request(self.client.delete(&url), "delete VM")
            .await
    }

    pub async fn get_vm_status(&self, vmid: u32) -> Result<WorkloadStatus> {
        let url = format!("{}/qemu/{}/status/current", self.node_url(), vmid);
        self.get_data(&url, "get VM status").await
    }

    pub async fn list_vm(&self) -> Result<Vec<WorkloadStatus>> {
        let url = format!("{}/qemu", self.node_url());
        self.get_data(&url, "list VMs").await
    }

    pub async fn get_node_status(&self) -> Result<NodeStatus> {
        let url = format!("{}/status", self.node_url());
        self.get_data(&url, "get node status").await
    }

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

    pub async fn wait_for_task(&self, upid: &str, timeout_secs: u64) -> Result<()> {
        use tokio::time::{sleep, Duration};

        #[derive(Deserialize)]
        struct TaskStatus {
            status: String,
            #[serde(default)]
            exitstatus: Option<String>,
        }

        let start = std::time::Instant::now();
        let timeout = Duration::from_secs(timeout_secs);
        let url = format!("{}/tasks/{}/status", self.node_url(), upid);

        loop {
            if start.elapsed() > timeout {
                anyhow::bail!("Task {} timed out after {} seconds", upid, timeout_secs);
            }

            let response = self
                .client
                .get(&url)
                .header(header::AUTHORIZATION, &self.auth_header)
                .send()
                .await?;

            if response.status().is_success() {
                let resp: ProxmoxResponse<TaskStatus> = response.json().await?;
                if let Some(task) = resp.data {
                    if task.status == "stopped" {
                        match task.exitstatus.as_deref() {
                            Some("OK") | None => return Ok(()),
                            Some(exit) => anyhow::bail!("Task failed with: {}", exit),
                        }
                    }
                }
            }

            sleep(Duration::from_secs(2)).await;
        }
    }
}

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
        // `config.image` is a Docker-style reference; Proxmox needs a
        // `local:vztmpl/...` path, so the operator-configured template wins.
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
        // Needs the guest agent; not wired up.
        Ok(None)
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

        let _serialized = serde_urlencoded::to_string(&config).unwrap();
    }
}
