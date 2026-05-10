// LXD Backend
//
// Implements ComputeBackend using the 'lxc' command line tool.
// This is suitable for single-node setups like a VPS.

use crate::compute::{ComputeBackend, ContainerConfig, NodeStatus};
use anyhow::{Context, Result};
use async_trait::async_trait;
use std::process::Command;
use tracing::info;

pub struct LxdBackend {
    storage_pool: String,
    network_device: String,
}

impl LxdBackend {
    pub fn new(storage_pool: &str, network_device: &str) -> Self {
        Self {
            storage_pool: storage_pool.to_string(),
            network_device: network_device.to_string(),
        }
    }

    fn run_lxc(&self, args: &[&str]) -> Result<String> {
        let output = Command::new("lxc")
            .args(args)
            .output()
            .context("Failed to execute lxc command")?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(anyhow::anyhow!("lxc command failed: {}", stderr));
        }

        Ok(String::from_utf8_lossy(&output.stdout).to_string())
    }

    /// Parse lxc list JSON output, treating empty stdout as an empty array.
    /// `lxc list --format json` returns empty stdout (not `[]`) when no containers exist.
    fn parse_lxc_json(raw: &str) -> Result<serde_json::Value> {
        let s = if raw.trim().is_empty() { "[]" } else { raw };
        serde_json::from_str(s).context("Failed to parse lxc list output")
    }

    /// Return the storage pool to use: the configured one if it exists,
    /// otherwise the first pool returned by `lxc storage list`.
    fn resolve_storage_pool(&self) -> Result<String> {
        let raw = self.run_lxc(&["storage", "list", "--format", "json"])?;
        let pools: serde_json::Value =
            serde_json::from_str(if raw.trim().is_empty() { "[]" } else { &raw })
                .context("Failed to parse lxc storage list output")?;

        let names: Vec<String> = pools
            .as_array()
            .unwrap_or(&vec![])
            .iter()
            .filter_map(|p| p.get("name").and_then(|n| n.as_str()).map(str::to_string))
            .collect();

        // Use configured pool if it actually exists
        if names.contains(&self.storage_pool) {
            return Ok(self.storage_pool.clone());
        }

        // Fall back to the first available pool
        names.into_iter().next().ok_or_else(|| {
            anyhow::anyhow!(
                "No LXD storage pools found. Run `lxc storage create default dir` on the provider."
            )
        })
    }
}

#[async_trait]
impl ComputeBackend for LxdBackend {
    async fn find_available_id(&self, range_start: u32, range_end: u32) -> Result<u32> {
        let raw = self.run_lxc(&["list", "--format", "json"])?;
        let containers = Self::parse_lxc_json(&raw)?;

        let existing_ids: Vec<u32> = containers
            .as_array()
            .unwrap_or(&vec![])
            .iter()
            .filter_map(|c| c.get("name").and_then(|n| n.as_str()))
            .filter_map(|name| {
                if name.starts_with("paygress-") {
                    name.replace("paygress-", "").parse::<u32>().ok()
                } else {
                    None
                }
            })
            .collect();

        for id in range_start..=range_end {
            if !existing_ids.contains(&id) {
                return Ok(id);
            }
        }

        Err(anyhow::anyhow!(
            "No available IDs in range {}-{}",
            range_start,
            range_end
        ))
    }

    async fn create_container(&self, config: &ContainerConfig) -> Result<String> {
        let name = format!("paygress-{}", config.id);

        // 1. Launch container
        // Resolve generic names to specific images
        let image = match config.image.as_str() {
            "alpine" => "images:alpine/3.19",
            "ubuntu" => "ubuntu:22.04", // Default LTS
            other => other,
        };

        info!("Creating LXD container {} with image {}", name, image);

        // Limits
        let cpu_limit = format!("limits.cpu={}", config.cpu_cores);
        let mem_limit = format!("limits.memory={}MB", config.memory_mb);

        let pool = self.resolve_storage_pool()?;
        info!("Using storage pool: {}", pool);

        self.run_lxc(&[
            "launch",
            image,
            &name,
            "-s",
            &pool,
            "-c",
            &cpu_limit,
            "-c",
            &mem_limit,
            "-c",
            "security.nesting=true",
        ])?;

        // 2. Set root password
        // We always set root password so user can access regardless of default user
        let chpasswd_cmd = format!("echo 'root:{}' | chpasswd", config.password);

        // Retry a few times as container starts up
        for _ in 0..10 {
            match self.run_lxc(&["exec", &name, "--", "sh", "-c", &chpasswd_cmd]) {
                Ok(_) => break,
                Err(_) => tokio::time::sleep(std::time::Duration::from_secs(1)).await,
            }
        }

        // 3. Generic SSH Setup & Hardening
        // Attempt to install/enable SSH on various distros (Alpine, Debian, etc)
        let setup_script = r#"
            # Detect package manager and install SSH if missing
            if command -v apk >/dev/null; then
                # Alpine
                apk add --no-cache openssh
                rc-update add sshd default
                service sshd start
            elif command -v apt-get >/dev/null; then
                # Debian/Ubuntu
                # Usually installed, but ensure it runs
                systemctl enable ssh
                systemctl start ssh
            fi

            # Configure SSH for root access with password
            # Check if config exists
            if [ -f /etc/ssh/sshd_config ]; then
                # Remove cloud-init config that disables password auth
                rm -f /etc/ssh/sshd_config.d/*-cloudimg-settings.conf

                sed -i 's/#PermitRootLogin.*/PermitRootLogin yes/' /etc/ssh/sshd_config
                sed -i 's/PermitRootLogin.*/PermitRootLogin yes/' /etc/ssh/sshd_config
                sed -i 's/PasswordAuthentication no/PasswordAuthentication yes/' /etc/ssh/sshd_config

                # Restart service
                service sshd restart || systemctl restart ssh || systemctl restart sshd
            fi
        "#;

        let _ = self.run_lxc(&["exec", &name, "--", "sh", "-c", setup_script]);

        // 4. Setup Port Forwarding
        if let Some(port) = config.host_port {
            info!("Setting up port forwarding: Host {} -> Container 22", port);
            // lxc config device add <container> ssh proxy listen=tcp:0.0.0.0:<port> connect=tcp:127.0.0.1:22
            self.run_lxc(&[
                "config",
                "device",
                "add",
                &name,
                "ssh-proxy",
                "proxy",
                &format!("listen=tcp:0.0.0.0:{}", port),
                "connect=tcp:127.0.0.1:22",
            ])?;
        }

        Ok(name)
    }

    async fn start_container(&self, id: u32) -> Result<()> {
        let name = format!("paygress-{}", id);
        self.run_lxc(&["start", &name])?;
        Ok(())
    }

    async fn stop_container(&self, id: u32) -> Result<()> {
        let name = format!("paygress-{}", id);
        self.run_lxc(&["stop", &name])?;
        Ok(())
    }

    async fn delete_container(&self, id: u32) -> Result<()> {
        let name = format!("paygress-{}", id);
        self.run_lxc(&["delete", &name, "--force"])?;
        Ok(())
    }

    async fn get_node_status(&self) -> Result<NodeStatus> {
        // Use `free -b` for memory
        let mem_output = Command::new("free").arg("-b").output()?;
        let mem_str = String::from_utf8_lossy(&mem_output.stdout);

        // Simple parsing of `free` output
        //               total        used        free      shared  buff/cache   available
        // Mem:    16723824640  1038573568 1234567890 ...
        let mut memory_total = 0;
        let mut memory_used = 0;

        for line in mem_str.lines() {
            if line.starts_with("Mem:") {
                let parts: Vec<&str> = line.split_whitespace().collect();
                if parts.len() >= 3 {
                    memory_total = parts[1].parse().unwrap_or(0);
                    memory_used = parts[2].parse().unwrap_or(0);
                }
            }
        }

        // Use `df -B1 /` for disk
        let disk_output = Command::new("df").args(["-B1", "/"]).output()?;
        let disk_str = String::from_utf8_lossy(&disk_output.stdout);

        let mut disk_total = 0;
        let mut disk_used = 0;

        for line in disk_str.lines().skip(1) {
            // Skip header
            let parts: Vec<&str> = line.split_whitespace().collect();
            if parts.len() >= 3 {
                disk_total = parts[1].parse().unwrap_or(0);
                disk_used = parts[2].parse().unwrap_or(0);
                break;
            }
        }

        // Use /proc/loadavg for CPU
        let loadavg = std::fs::read_to_string("/proc/loadavg").unwrap_or_default();
        let load_1min: f64 = loadavg
            .split_whitespace()
            .next()
            .unwrap_or("0")
            .parse()
            .unwrap_or(0.0);
        let cpu_cores = num_cpus::get() as f64;
        let cpu_usage = (load_1min / cpu_cores).min(1.0);

        Ok(NodeStatus {
            cpu_usage,
            memory_used,
            memory_total,
            disk_used,
            disk_total,
        })
    }

    async fn get_container_ip(&self, id: u32) -> Result<Option<String>> {
        let name = format!("paygress-{}", id);
        let raw = self.run_lxc(&["list", &name, "--format", "json"])?;
        let containers = Self::parse_lxc_json(&raw)?;

        if let Some(container) = containers.as_array().and_then(|a| a.first()) {
            // Traverse json to find eth0 ipv4
            // state -> network -> eth0 -> addresses -> [family=inet] -> address
            if let Some(networks) = container.get("state").and_then(|s| s.get("network")) {
                if let Some(eth0) = networks.get("eth0") {
                    if let Some(addrs) = eth0.get("addresses").and_then(|a| a.as_array()) {
                        for addr in addrs {
                            if addr.get("family").and_then(|f| f.as_str()) == Some("inet") {
                                if let Some(ip) = addr.get("address").and_then(|a| a.as_str()) {
                                    return Ok(Some(ip.to_string()));
                                }
                            }
                        }
                    }
                }
            }
        }

        Ok(None)
    }
}
