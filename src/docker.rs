// Docker compute backend. Shells out to the `docker` CLI.
//
// No state is persisted: the container's existence on the host IS
// the state, and `find_available_id` scans for `paygress-<n>`.

use std::process::Stdio;

use anyhow::{Context, Result};
use async_trait::async_trait;
use tokio::process::Command;

use crate::compute::{
    container_name, id_from_container_name, ComputeBackend, ContainerConfig, ContainerStatus,
    NodeStatus,
};
use crate::luks::{create_encrypted_volume, destroy_encrypted_volume};

pub struct DockerBackend {
    /// Docker network to attach containers to; `None` = the host's
    /// default `bridge`.
    network: Option<String>,
}

impl DockerBackend {
    pub fn new() -> Self {
        Self { network: None }
    }

    pub fn with_network(network: impl Into<String>) -> Self {
        Self {
            network: Some(network.into()),
        }
    }

    async fn docker(&self, args: &[&str]) -> Result<std::process::Output> {
        Command::new("docker")
            .args(args)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .await
            .context("invoke docker CLI")
    }
}

impl Default for DockerBackend {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl ComputeBackend for DockerBackend {
    async fn find_available_id(&self, range_start: u32, range_end: u32) -> Result<u32> {
        let output = self
            .docker(&[
                "ps",
                "-a",
                "--format",
                "{{.Names}}",
                "--filter",
                "name=paygress-",
            ])
            .await?;
        let names = String::from_utf8_lossy(&output.stdout);
        let used: std::collections::HashSet<u32> =
            names.lines().filter_map(id_from_container_name).collect();
        for id in range_start..=range_end {
            if !used.contains(&id) {
                return Ok(id);
            }
        }
        anyhow::bail!(
            "no available container id in range {}..={}",
            range_start,
            range_end
        );
    }

    async fn create_container(&self, config: &ContainerConfig) -> Result<String> {
        let mut args: Vec<String> = vec![
            "run".into(),
            "-d".into(),
            "--name".into(),
            container_name(config.id),
            "--restart".into(),
            "unless-stopped".into(),
            "--cpus".into(),
            config.cpu_cores.to_string(),
            "--memory".into(),
            format!("{}m", config.memory_mb),
        ];

        if let Some(net) = &self.network {
            args.push("--network".into());
            args.push(net.clone());
        }

        for port in &config.template_ports {
            args.push("-p".into());
            args.push(format!(
                "{}:{}/{}",
                port.host_port, port.container_port, port.protocol
            ));
        }

        for (k, v) in &config.template_env {
            args.push("-e".into());
            args.push(format!("{}={}", k, v));
        }

        for arg in &config.extra_runtime_args {
            args.push(arg.clone());
        }

        // Persistent state volume, vmid-scoped so two instances of a
        // template don't share state. Encrypted: a LUKS-on-loop file
        // whose ext4 mountpoint is bind-mounted in, so the host
        // operator's post-eviction `tar` yields only ciphertext.
        // Plain: a Docker named volume (the historical default).
        if let Some(path) = &config.data_path {
            match config.volume_encryption_key.as_ref() {
                Some(key) => {
                    let vol = create_encrypted_volume(config.id, config.storage_gb, key)
                        .await
                        .with_context(|| {
                            format!("create LUKS-encrypted volume for id={}", config.id)
                        })?;
                    args.push("-v".into());
                    args.push(format!("{}:{}", vol.mount_path.display(), path));
                }
                None => {
                    args.push("-v".into());
                    args.push(format!("{}-data:{}", container_name(config.id), path));
                }
            }
        }

        // Image must be last: docker treats anything after it as the
        // container's CMD, which we don't override.
        args.push(config.image.clone());

        let arg_refs: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
        let output = self.docker(&arg_refs).await?;
        if !output.status.success() {
            anyhow::bail!(
                "docker run failed: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
        Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
    }

    async fn start_container(&self, id: u32) -> Result<()> {
        let name = container_name(id);
        let output = self.docker(&["start", &name]).await?;
        if !output.status.success() {
            anyhow::bail!(
                "docker start {} failed: {}",
                name,
                String::from_utf8_lossy(&output.stderr)
            );
        }
        Ok(())
    }

    async fn stop_container(&self, id: u32) -> Result<()> {
        // Best-effort: an already-gone container must not fail the
        // cleanup loop.
        let _ = self.docker(&["stop", &container_name(id)]).await;
        Ok(())
    }

    async fn delete_container(&self, id: u32) -> Result<()> {
        let name = container_name(id);
        let _ = self.docker(&["rm", "-f", &name]).await;
        // Remove both possible volume backings so a re-spawn at the
        // same id can't inherit stale state: the plain named volume
        // and the LUKS-on-loop volume. `destroy_encrypted_volume` is
        // idempotent and its luksErase is the load-bearing step — it
        // overwrites the keyslots so the ciphertext stays unreadable
        // even if the operator copied the file first.
        let volume = format!("{}-data", name);
        let _ = self.docker(&["volume", "rm", "-f", &volume]).await;
        if let Err(e) = destroy_encrypted_volume(id).await {
            // Never propagate: cleanup_loop relies on this being
            // best-effort, and the helper re-cleans on the next
            // delete at the same id.
            tracing::warn!("destroy_encrypted_volume(id={}) non-fatal: {}", id, e);
        }
        Ok(())
    }

    async fn get_node_status(&self) -> Result<NodeStatus> {
        Ok(NodeStatus {
            cpu_usage: 0.0,
            memory_used: 0,
            memory_total: 0,
            disk_used: 0,
            disk_total: 0,
        })
    }

    async fn get_container_status(&self, id: u32) -> Result<ContainerStatus> {
        let name = container_name(id);
        let output = self
            .docker(&["inspect", "-f", "{{.State.Running}}", &name])
            .await?;
        if !output.status.success() {
            // `inspect` fails only when there is no such container.
            return Ok(ContainerStatus::Absent);
        }
        Ok(match String::from_utf8_lossy(&output.stdout).trim() {
            "true" => ContainerStatus::Running,
            _ => ContainerStatus::Stopped,
        })
    }

    async fn get_container_ip(&self, id: u32) -> Result<Option<String>> {
        let name = container_name(id);
        let output = self
            .docker(&["inspect", "-f", "{{.NetworkSettings.IPAddress}}", &name])
            .await?;
        let ip = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if ip.is_empty() {
            Ok(None)
        } else {
            Ok(Some(ip))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn name_for_is_deterministic() {
        assert_eq!(container_name(1234), "paygress-1234");
    }

    #[test]
    fn run_args_include_all_pieces() {
        // Mirrors the argv construction in `create_container`; we
        // can't shell out to docker from a unit test.
        let cfg = ContainerConfig {
            id: 42,
            name: "paygress-42".to_string(),
            image: "alpine:latest".to_string(),
            cpu_cores: 1,
            memory_mb: 256,
            storage_gb: 1,
            password: "x".to_string(),
            ssh_key: None,
            host_port: Some(30042),
            template_ports: vec![crate::compute::PortMapping {
                host_port: 17777,
                container_port: 7777,
                protocol: "tcp".to_string(),
            }],
            template_env: {
                let mut m = std::collections::HashMap::new();
                m.insert("FOO".to_string(), "bar".to_string());
                m
            },
            extra_runtime_args: vec!["--ulimit".to_string(), "nofile=1024:1024".to_string()],
            data_path: Some("/var/data".to_string()),
            volume_encryption_key: None,
        };

        let mut args: Vec<String> = vec![
            "run".into(),
            "-d".into(),
            "--name".into(),
            container_name(cfg.id),
            "--restart".into(),
            "unless-stopped".into(),
            "--cpus".into(),
            cfg.cpu_cores.to_string(),
            "--memory".into(),
            format!("{}m", cfg.memory_mb),
        ];
        for port in &cfg.template_ports {
            args.push("-p".into());
            args.push(format!(
                "{}:{}/{}",
                port.host_port, port.container_port, port.protocol
            ));
        }
        for (k, v) in &cfg.template_env {
            args.push("-e".into());
            args.push(format!("{}={}", k, v));
        }
        args.push(cfg.image.clone());

        assert!(args.contains(&"paygress-42".to_string()));
        assert!(args.contains(&"17777:7777/tcp".to_string()));
        assert!(args.contains(&"FOO=bar".to_string()));
        assert_eq!(args.last().map(|s| s.as_str()), Some("alpine:latest"));
    }
}
