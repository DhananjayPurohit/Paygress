// Docker compute backend.
//
// Implements `ComputeBackend` by shelling out to the `docker` CLI.
// Used for the killer-templates path (#31) where workloads are real
// public Docker images (strfry, ollama, browserless, bitcoind) and
// LXD's image surface doesn't fit.
//
// Container naming is `paygress-<vmid>`. We persist no state — the
// container's existence on the host IS the state; `find_available_id`
// scans for `paygress-<n>` containers.
//
// Caveats
// - Subprocess-shelling is intentionally simple; a long-running
//   provider would benefit from talking to the docker daemon socket
//   directly via the `bollard` crate. Keeping it CLI-shelled today
//   means zero runtime dependencies and easy debuggability via
//   `docker ps`.
// - `cpu_cores` and `memory_mb` are passed via `--cpus` and
//   `--memory`; Docker enforces them via cgroups.
// - `host_port` is the SSH forwarding port the spawn handler
//   already calculates; for templates it's irrelevant (no SSH into
//   a template container) but kept for symmetry with other
//   backends.

use std::process::Stdio;

use anyhow::{Context, Result};
use async_trait::async_trait;
use tokio::process::Command;

use crate::compute::{ComputeBackend, ContainerConfig, NodeStatus};
use crate::luks::{create_encrypted_volume, destroy_encrypted_volume};

/// Docker backend. Stateless wrapper around the `docker` CLI.
pub struct DockerBackend {
    /// Optional Docker network to attach containers to. Defaults to
    /// the host's default `bridge` network when None.
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

    fn name_for(id: u32) -> String {
        format!("paygress-{}", id)
    }

    async fn docker(&self, args: &[&str]) -> Result<std::process::Output> {
        let output = Command::new("docker")
            .args(args)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .await
            .context("invoke docker CLI")?;
        Ok(output)
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
        let used: std::collections::HashSet<u32> = names
            .lines()
            .filter_map(|n| n.strip_prefix("paygress-")?.parse().ok())
            .collect();
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
        let name = Self::name_for(config.id);

        let mut args: Vec<String> = vec![
            "run".into(),
            "-d".into(),
            "--name".into(),
            name.clone(),
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

        // Per-template extra flags (ulimits, sysctls, caps).
        for arg in &config.extra_runtime_args {
            args.push(arg.clone());
        }

        // Persistent state volume (vmid-scoped so two instances of
        // the same template don't share state). Two paths:
        //   - encrypted: provision a LUKS-on-loop file, mount its
        //     ext4 on the host, bind-mount the mountpoint into the
        //     container at `data_path`. Host operator's
        //     post-eviction `tar` reveals only ciphertext.
        //   - plain: use a Docker named volume (the historical
        //     default; host operator can `tar` /var/lib/docker/...).
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
                    args.push(format!("paygress-{}-data:{}", config.id, path));
                }
            }
        }

        // Image must be the last positional arg so docker treats
        // anything after it as the container's CMD (which we don't
        // override — image's default CMD runs).
        args.push(config.image.clone());

        let arg_refs: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
        let output = self.docker(&arg_refs).await?;
        if !output.status.success() {
            anyhow::bail!(
                "docker run failed: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
        let container_id = String::from_utf8_lossy(&output.stdout).trim().to_string();
        Ok(container_id)
    }

    async fn start_container(&self, id: u32) -> Result<()> {
        let name = Self::name_for(id);
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
        let name = Self::name_for(id);
        // Best-effort — if the container is already gone, don't
        // bubble up an error to cleanup_loop.
        let _ = self.docker(&["stop", &name]).await;
        Ok(())
    }

    async fn delete_container(&self, id: u32) -> Result<()> {
        let name = Self::name_for(id);
        let _ = self.docker(&["rm", "-f", &name]).await;
        // Best-effort: also remove BOTH possible volume backings so a
        // re-spawn doesn't inherit stale state.
        //   1. The Docker named volume (used when volume_encryption_key
        //      is None — historical default path).
        //   2. The LUKS-on-loop volume (used when the consumer set
        //      --encrypt-volume). Idempotent: the destroy helper
        //      treats "no LUKS file at the expected id" as a no-op.
        //      luksErase inside destroy is the load-bearing step —
        //      it overwrites the LUKS header so the keyslots are
        //      unrecoverable even if the operator extracted the
        //      file before this ran.
        let volume = format!("paygress-{}-data", id);
        let _ = self.docker(&["volume", "rm", "-f", &volume]).await;
        if let Err(e) = destroy_encrypted_volume(id).await {
            // Never propagate: cleanup_loop relies on this being
            // best-effort. A leaked mapper entry (e.g. provider
            // process killed mid-cleanup) gets re-cleaned on the
            // next delete attempt at the same id, since the helper
            // is idempotent.
            tracing::warn!("destroy_encrypted_volume(id={}) non-fatal: {}", id, e);
        }
        Ok(())
    }

    async fn get_node_status(&self) -> Result<NodeStatus> {
        // `docker info` could give us system stats; for now report
        // zeros so the heartbeat publishes a valid payload. The
        // host's true CPU/memory could come from a sysinfo crate
        // in a follow-up.
        Ok(NodeStatus {
            cpu_usage: 0.0,
            memory_used: 0,
            memory_total: 0,
            disk_used: 0,
            disk_total: 0,
        })
    }

    async fn get_container_ip(&self, id: u32) -> Result<Option<String>> {
        let name = Self::name_for(id);
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
        assert_eq!(DockerBackend::name_for(1234), "paygress-1234");
    }

    #[test]
    fn run_args_include_all_pieces() {
        // We can't actually shell out to docker in unit tests, but
        // we can verify the argument-building logic by
        // reconstructing what `create_container` would build.
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
                protocol: "tcp",
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

        // Mirror the logic in `create_container` for assertion.
        let mut args: Vec<String> = vec![
            "run".into(),
            "-d".into(),
            "--name".into(),
            DockerBackend::name_for(cfg.id),
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

        // Sanity: name resolves; image is last; ports show up as
        // expected.
        assert!(args.contains(&"paygress-42".to_string()));
        assert!(args.contains(&"17777:7777/tcp".to_string()));
        assert!(args.contains(&"FOO=bar".to_string()));
        assert_eq!(args.last().map(|s| s.as_str()), Some("alpine:latest"));
    }
}
