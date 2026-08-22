// LXD backend. Implements ComputeBackend via the `lxc` CLI.

use crate::compute::{
    container_name, ComputeBackend, ContainerConfig, ContainerStatus, NodeStatus,
};
use anyhow::{Context, Result};
use async_trait::async_trait;
use tokio::process::Command;
use tracing::info;

pub struct LxdBackend {
    storage_pool: String,
    /// Whether launched containers may run containers of their own.
    nesting: bool,
    privileged: bool,
}

/// Whether a provider advertising `capabilities` is offering nested containers.
///
/// A thin name over [`crate::capabilities::advertises`], so the grant here and
/// the consumer-side check in `spawn` can never drift apart.
pub fn nesting_from_capabilities<S: AsRef<str>>(capabilities: &[S]) -> bool {
    crate::capabilities::advertises(capabilities, "nesting")
}

/// Whether a provider is offering workloads a container runtime that actually
/// works — the `docker` capability.
///
/// Unprivileged LXD is not enough for a Docker daemon however much nesting it
/// has: containerd cannot write `net.ipv4.ip_unprivileged_port_start` in the
/// container's netns, and overlayfs refuses to mount, so the daemon starts and
/// then fails to run anything. Serving that as a CI sandbox means selling a box
/// the buyer cannot use.
///
/// So `docker` is a separate, louder grant than `nesting`: it launches the
/// container privileged, which is host root for the renter. That is defensible
/// only because the workload is a lease on a box the operator expects to be
/// destroyed — and it must stay the provider's decision, never a default.
pub fn docker_from_capabilities<S: AsRef<str>>(capabilities: &[S]) -> bool {
    crate::capabilities::advertises(capabilities, "docker")
}

/// `lxc launch` arguments for one workload.
///
/// Pure so the grants can be asserted in tests: the difference between an
/// unprivileged container and host root is one flag, and it should not be
/// possible to change it without a test noticing.
pub(crate) fn launch_args(
    image: &str,
    name: &str,
    pool: &str,
    cpu_cores: u32,
    memory_mb: u32,
    nesting: bool,
    privileged: bool,
) -> Vec<String> {
    let mut args: Vec<String> = ["launch", image, name, "-s", pool]
        .iter()
        .map(|s| s.to_string())
        .collect();

    for limit in [
        format!("limits.cpu={}", cpu_cores),
        format!("limits.memory={}MB", memory_mb),
    ] {
        args.push("-c".to_string());
        args.push(limit);
    }

    let mut conf: Vec<String> = Vec::new();
    if nesting || privileged {
        conf.push("security.nesting=true".to_string());
        // Docker's image extraction makes device nodes and sets trusted
        // xattrs. Without these two the daemon runs but every `docker pull`
        // fails partway through, which reads as a corrupt image rather than a
        // missing permission.
        conf.push("security.syscalls.intercept.mknod=true".to_string());
        conf.push("security.syscalls.intercept.setxattr=true".to_string());
    }
    if privileged {
        // Host root for the renter. Gated on the provider advertising
        // `docker`, and defensible only for a lease that is expected to be
        // destroyed.
        conf.push("security.privileged=true".to_string());
    }

    for c in conf {
        args.push("-c".to_string());
        args.push(c);
    }
    args
}

impl LxdBackend {
    /// `network_device` is unused: containers get their NIC from LXD's default
    /// profile.
    ///
    /// `nesting` decides whether workloads may run containers of their own. It
    /// comes from the provider advertising the `nesting` capability rather than
    /// from a separate setting, so what an offer claims and what a container
    /// can actually do cannot drift apart.
    pub fn new(storage_pool: &str, _network_device: &str, nesting: bool, privileged: bool) -> Self {
        Self {
            storage_pool: storage_pool.to_string(),
            nesting,
            privileged,
        }
    }

    async fn run_lxc(&self, args: &[&str]) -> Result<String> {
        let output = Command::new("lxc")
            .args(args)
            .output()
            .await
            .context("Failed to execute lxc command")?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(anyhow::anyhow!("lxc command failed: {}", stderr));
        }

        Ok(String::from_utf8_lossy(&output.stdout).to_string())
    }

    /// Run `lxc` with `stdin` piped in.
    ///
    /// Exists so secrets never reach a command line or a shell: the value goes
    /// to the child's stdin instead of being interpolated into `sh -c`, where a
    /// quote in it would escape.
    async fn run_lxc_stdin(&self, args: &[&str], stdin_data: &str) -> Result<()> {
        use std::process::Stdio;
        use tokio::io::AsyncWriteExt;

        let mut child = Command::new("lxc")
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .context("Failed to execute lxc command")?;

        child
            .stdin
            .as_mut()
            .context("failed to open stdin on the lxc process")?
            .write_all(stdin_data.as_bytes())
            .await?;

        let output = child.wait_with_output().await?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(anyhow::anyhow!("lxc command failed: {}", stderr));
        }
        Ok(())
    }

    /// `lxc list --format json` prints empty stdout (not `[]`) when nothing
    /// exists, which `serde_json` rejects.
    fn parse_lxc_json(raw: &str) -> Result<serde_json::Value> {
        let s = if raw.trim().is_empty() { "[]" } else { raw };
        serde_json::from_str(s).context("Failed to parse lxc list output")
    }

    /// The configured pool if it exists, else the first one LXD reports.
    async fn resolve_storage_pool(&self) -> Result<String> {
        let raw = self
            .run_lxc(&["storage", "list", "--format", "json"])
            .await?;
        let pools = Self::parse_lxc_json(&raw)?;

        let names: Vec<String> = pools
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|p| p.get("name").and_then(|n| n.as_str()).map(str::to_string))
            .collect();

        if names.contains(&self.storage_pool) {
            return Ok(self.storage_pool.clone());
        }

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
        let raw = self.run_lxc(&["list", "--format", "json"]).await?;
        let containers = Self::parse_lxc_json(&raw)?;

        let existing_ids: std::collections::HashSet<u32> = containers
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|c| c.get("name").and_then(|n| n.as_str()))
            .filter_map(crate::compute::id_from_container_name)
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
        let name = container_name(config.id);

        let image = match config.image.as_str() {
            "alpine" => "images:alpine/3.19",
            "ubuntu" => "ubuntu:22.04",
            other => other,
        };

        info!("Creating LXD container {} with image {}", name, image);

        let pool = self.resolve_storage_pool().await?;
        info!("Using storage pool: {}", pool);

        let args = launch_args(
            image,
            &name,
            &pool,
            config.cpu_cores,
            config.memory_mb,
            self.nesting,
            self.privileged,
        );
        let args: Vec<&str> = args.iter().map(String::as_str).collect();

        self.run_lxc(&args).await?;

        // Retry while the container finishes booting. The credential goes over
        // stdin, never through a shell.
        let credential = format!("root:{}\n", config.password);
        for _ in 0..10 {
            match self
                .run_lxc_stdin(&["exec", &name, "-T", "--", "chpasswd"], &credential)
                .await
            {
                Ok(_) => break,
                Err(_) => tokio::time::sleep(std::time::Duration::from_secs(1)).await,
            }
        }

        // Best-effort SSH enablement across the distros we serve.
        let setup_script = r#"
            if command -v apk >/dev/null; then
                apk add --no-cache openssh
                rc-update add sshd default
                service sshd start
            elif command -v apt-get >/dev/null; then
                systemctl enable ssh
                systemctl start ssh
            fi

            if [ -f /etc/ssh/sshd_config ]; then
                # cloud-init ships a drop-in that disables password auth
                rm -f /etc/ssh/sshd_config.d/*-cloudimg-settings.conf

                sed -i 's/#PermitRootLogin.*/PermitRootLogin yes/' /etc/ssh/sshd_config
                sed -i 's/PermitRootLogin.*/PermitRootLogin yes/' /etc/ssh/sshd_config
                sed -i 's/PasswordAuthentication no/PasswordAuthentication yes/' /etc/ssh/sshd_config

                service sshd restart || systemctl restart ssh || systemctl restart sshd
            fi
        "#;

        let _ = self
            .run_lxc(&["exec", &name, "--", "sh", "-c", setup_script])
            .await;

        if let Some(port) = config.host_port {
            info!("Setting up port forwarding: Host {} -> Container 22", port);
            self.run_lxc(&[
                "config",
                "device",
                "add",
                &name,
                "ssh-proxy",
                "proxy",
                &format!("listen=tcp:0.0.0.0:{}", port),
                "connect=tcp:127.0.0.1:22",
            ])
            .await?;
        }

        Ok(name)
    }

    async fn start_container(&self, id: u32) -> Result<()> {
        self.run_lxc(&["start", &container_name(id)]).await?;
        Ok(())
    }

    async fn get_container_status(&self, id: u32) -> Result<ContainerStatus> {
        let name = container_name(id);
        let raw = self
            .run_lxc(&["list", &format!("^{}$", name), "--format", "json"])
            .await?;
        let containers = Self::parse_lxc_json(&raw)?;
        let entry = containers.as_array().and_then(|a| {
            a.iter()
                .find(|c| c.get("name").and_then(|n| n.as_str()) == Some(&name))
        });

        Ok(match entry {
            None => ContainerStatus::Absent,
            Some(c) => match c.get("status").and_then(|s| s.as_str()) {
                Some("Running") => ContainerStatus::Running,
                // Stopped, Frozen and anything else are all "not serving the
                // tenant" for our purposes.
                Some(_) => ContainerStatus::Stopped,
                None => ContainerStatus::Running,
            },
        })
    }

    /// Idempotent: `lxc stop` exits non-zero on an already-stopped instance,
    /// which is a normal state here — CI workloads power themselves off when
    /// their job ends. Treating that as an error would strand the container,
    /// since the cleanup path only deletes after a successful stop.
    async fn stop_container(&self, id: u32) -> Result<()> {
        match self.run_lxc(&["stop", &container_name(id)]).await {
            Ok(_) => Ok(()),
            Err(e) => {
                let msg = e.to_string().to_lowercase();
                if msg.contains("already stopped") || msg.contains("not running") {
                    Ok(())
                } else {
                    Err(e)
                }
            }
        }
    }

    async fn delete_container(&self, id: u32) -> Result<()> {
        self.run_lxc(&["delete", &container_name(id), "--force"])
            .await?;
        Ok(())
    }

    async fn get_node_status(&self) -> Result<NodeStatus> {
        let mem_output = Command::new("free").arg("-b").output().await?;
        let mem_str = String::from_utf8_lossy(&mem_output.stdout);

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

        let disk_output = Command::new("df").args(["-B1", "/"]).output().await?;
        let disk_str = String::from_utf8_lossy(&disk_output.stdout);

        let mut disk_total = 0;
        let mut disk_used = 0;
        for line in disk_str.lines().skip(1) {
            let parts: Vec<&str> = line.split_whitespace().collect();
            if parts.len() >= 3 {
                disk_total = parts[1].parse().unwrap_or(0);
                disk_used = parts[2].parse().unwrap_or(0);
                break;
            }
        }

        let loadavg = std::fs::read_to_string("/proc/loadavg").unwrap_or_default();
        let load_1min: f64 = loadavg
            .split_whitespace()
            .next()
            .unwrap_or("0")
            .parse()
            .unwrap_or(0.0);
        let cpu_usage = (load_1min / num_cpus::get() as f64).min(1.0);

        Ok(NodeStatus {
            cpu_usage,
            memory_used,
            memory_total,
            disk_used,
            disk_total,
        })
    }

    async fn get_container_ip(&self, id: u32) -> Result<Option<String>> {
        let raw = self
            .run_lxc(&["list", &container_name(id), "--format", "json"])
            .await?;
        let containers = Self::parse_lxc_json(&raw)?;

        let Some(container) = containers.as_array().and_then(|a| a.first()) else {
            return Ok(None);
        };
        let addresses = container
            .get("state")
            .and_then(|s| s.get("network"))
            .and_then(|n| n.get("eth0"))
            .and_then(|e| e.get("addresses"))
            .and_then(|a| a.as_array());

        for addr in addresses.into_iter().flatten() {
            if addr.get("family").and_then(|f| f.as_str()) == Some("inet") {
                if let Some(ip) = addr.get("address").and_then(|a| a.as_str()) {
                    return Ok(Some(ip.to_string()));
                }
            }
        }

        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use super::{docker_from_capabilities, launch_args, nesting_from_capabilities};

    fn args_for(nesting: bool, privileged: bool) -> Vec<String> {
        launch_args(
            "ubuntu:24.04",
            "pg-1",
            "default",
            2,
            2048,
            nesting,
            privileged,
        )
    }

    // The difference between an unprivileged container and host root is one
    // flag, so each grant is pinned rather than left to review.
    #[test]
    fn a_plain_workload_gets_no_grants() {
        let a = args_for(false, false);
        assert!(!a.iter().any(|s| s.starts_with("security.")), "{a:?}");
    }

    #[test]
    fn nesting_alone_never_grants_privileged() {
        let a = args_for(true, false);
        assert!(a.contains(&"security.nesting=true".to_string()));
        assert!(a.contains(&"security.syscalls.intercept.mknod=true".to_string()));
        assert!(a.contains(&"security.syscalls.intercept.setxattr=true".to_string()));
        assert!(
            !a.contains(&"security.privileged=true".to_string()),
            "{a:?}"
        );
    }

    #[test]
    fn docker_implies_the_nesting_a_daemon_needs() {
        let a = args_for(false, true);
        assert!(a.contains(&"security.privileged=true".to_string()));
        // Privileged without nesting is still a box that cannot run a
        // container, which is the useless-lease outcome worth designing out.
        assert!(a.contains(&"security.nesting=true".to_string()), "{a:?}");
    }

    #[test]
    fn limits_and_pool_are_always_passed() {
        let a = args_for(true, true);
        assert!(a.contains(&"limits.cpu=2".to_string()));
        assert!(a.contains(&"limits.memory=2048MB".to_string()));
        assert_eq!(a[..5], ["launch", "ubuntu:24.04", "pg-1", "-s", "default"]);
    }

    #[test]
    fn docker_is_a_separate_grant_from_nesting() {
        assert!(docker_from_capabilities(&["lxc", "docker"]));
        assert!(!docker_from_capabilities(&["lxc", "nesting"]));
        assert!(!nesting_from_capabilities(&["lxc", "docker"]));
    }

    #[test]
    fn advertising_nesting_enables_it() {
        assert!(nesting_from_capabilities(&[
            "lxc", "vm", "nesting", "docker"
        ]));
    }

    #[test]
    fn not_advertising_nesting_leaves_it_off() {
        // The default provider config advertises only "lxc", so an operator who
        // never opted in must not be handing out nesting.
        assert!(!nesting_from_capabilities(&["lxc"]));
        assert!(!nesting_from_capabilities::<&str>(&[]));
    }

    #[test]
    fn matching_ignores_case_and_padding() {
        assert!(nesting_from_capabilities(&["Nesting"]));
        assert!(nesting_from_capabilities(&[" nesting "]));
    }

    // "nested" is not "nesting": a near-miss must not grant the capability.
    #[test]
    fn near_misses_do_not_grant_it() {
        assert!(!nesting_from_capabilities(&[
            "nested",
            "nest",
            "nesting-vm"
        ]));
    }
}
