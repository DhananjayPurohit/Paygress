// KVM/qemu compute backend.
//
// Implements `ComputeBackend` by spawning a per-workload
// qemu/KVM virtual machine. Each spawn is its own VM with its own
// kernel, virtio devices, and disk. The host operator running on the
// hypervisor can still see the guest's memory (no SEV-SNP), but
// every container-escape path that exists in the Docker / LXD
// backends is closed by the VM boundary.
//
// Where this fits in paygress's isolation tiers
// ----------------------------------------------
// (See `nostr::IsolationLevel`.)
//   - `SharedKernel`        : Docker / LXD (today's default backends).
//                             Cheapest, fastest, leakiest.
//   - `DedicatedHost`       : THIS BACKEND. Per-workload VM. Defends
//                             against co-tenant attacks and container
//                             escape exploits. Does NOT defend against
//                             the host operator with hypervisor root.
//   - `AttestedResearchTier`: SEV-SNP / TDX. Defends against the
//                             host operator. Needs hardware (AMD
//                             EPYC 7003+ / Intel Sapphire Rapids).
//                             This module is the foundation that
//                             tier will extend — when an EPYC host
//                             is provisioned, the changes are:
//                               * `-machine confidential-guest-support=...`
//                               * an attestation handshake before
//                                 the consumer sends the spawn token
//                               * a measured boot chain (kernel +
//                                 initrd) the consumer can verify
//                             Everything else (cloud-init seed, disk
//                             overlay, network forwarding) reuses
//                             this code unchanged.
//
// Why qemu+KVM and not Firecracker / Cloud Hypervisor
// ----------------------------------------------------
// qemu's larger surface buys us cloud-init compatibility (so we get
// SSH onto a vanilla Ubuntu image without baking custom userdata
// into a Firecracker rootfs), virtio device flexibility (we may add
// a virtio-vsock channel later for the agent-sandbox HTTP exec
// equivalent), and the eventual SEV-SNP path. Cold-start is slower
// than Firecracker (~3-5s vs ~125ms) but the boot is amortized over
// a multi-hour-to-multi-day workload lease — not a hot path.
//
// Storage layout
// --------------
// /var/lib/paygress/vm/base/<image>.img      — read-only cloud image
// /var/lib/paygress/vm/<id>/disk.qcow2       — per-VM overlay
// /var/lib/paygress/vm/<id>/seed.iso         — cloud-init seed
// /var/lib/paygress/vm/<id>/qemu.pid         — qemu daemon pidfile
// /var/lib/paygress/vm/<id>/serial.log       — guest serial console
//
// What this v1 deliberately does NOT do
// -------------------------------------
//   - Run the killer-template Docker images. The KVM backend serves
//     a vanilla Ubuntu VM; consumers shell in via SSH and run their
//     own software. Running templates inside a VM would require
//     nested-Docker bootstrap inside cloud-init — out of scope here,
//     tracked as a follow-up.
//   - Honor `ContainerConfig.template_ports` beyond SSH. Each VM
//     gets ONE host-port forward (the `host_port` field) for SSH;
//     forwarding additional template ports lands when the v2
//     template-aware path does.
//   - Persistent data volumes (`data_path`). The VM's qcow2 disk IS
//     the persistent state; LUKS-on-loop encryption from the Docker
//     path doesn't apply (the disk is one file). Consumer-encrypted
//     disks via LUKS-inside-the-guest is a follow-up.
//   - Cleanup of VMs orphaned by a provider crash. A startup-time
//     `pidfile-and-process-still-alive?` sweep lands separately.

use std::path::PathBuf;
use std::process::Stdio;

use anyhow::{Context, Result};
use async_trait::async_trait;
use tokio::process::Command;
use tracing::{debug, info, warn};

use crate::compute::{ComputeBackend, ContainerConfig, NodeStatus};

/// Root directory for paygress-managed VMs.
const VM_ROOT: &str = "/var/lib/paygress/vm";

/// Default base image. Ubuntu 22.04 cloud image — small (~600 MB),
/// has cloud-init preinstalled, well-supported across hosting
/// providers. Operators can override via `KvmConfig`.
const DEFAULT_BASE_IMAGE_URL: &str =
    "https://cloud-images.ubuntu.com/jammy/current/jammy-server-cloudimg-amd64.img";
const DEFAULT_BASE_IMAGE_FILE: &str = "jammy-server-cloudimg-amd64.img";

/// Per-instance configuration for the KVM backend. Provider
/// operators tweak these at startup.
#[derive(Debug, Clone)]
pub struct KvmConfig {
    /// Path to the read-only base cloud image. The backend creates
    /// per-VM qcow2 overlays on top, so multiple workloads share one
    /// physical base. Downloaded on first spawn if absent.
    pub base_image_path: PathBuf,
    /// URL the backend fetches the base image from when
    /// `base_image_path` is missing. Defaults to the Ubuntu cloud
    /// image.
    pub base_image_url: String,
    /// Where per-VM directories live.
    pub vm_root: PathBuf,
}

impl Default for KvmConfig {
    fn default() -> Self {
        Self {
            base_image_path: PathBuf::from(VM_ROOT)
                .join("base")
                .join(DEFAULT_BASE_IMAGE_FILE),
            base_image_url: DEFAULT_BASE_IMAGE_URL.to_string(),
            vm_root: PathBuf::from(VM_ROOT),
        }
    }
}

/// KVM/qemu backend.
pub struct KvmBackend {
    config: KvmConfig,
}

impl KvmBackend {
    pub fn new(config: KvmConfig) -> Self {
        Self { config }
    }

    fn vm_dir(&self, id: u32) -> PathBuf {
        self.config.vm_root.join(id.to_string())
    }

    fn disk_path(&self, id: u32) -> PathBuf {
        self.vm_dir(id).join("disk.qcow2")
    }

    fn seed_path(&self, id: u32) -> PathBuf {
        self.vm_dir(id).join("seed.iso")
    }

    fn pidfile_path(&self, id: u32) -> PathBuf {
        self.vm_dir(id).join("qemu.pid")
    }

    fn serial_log_path(&self, id: u32) -> PathBuf {
        self.vm_dir(id).join("serial.log")
    }

    /// Verify qemu + KVM support is present. Provider should call
    /// this at startup; surfacing "your host doesn't support KVM"
    /// at config time is much better than at first-spawn time.
    pub async fn check_kvm_available() -> Result<String> {
        if !PathBuf::from("/dev/kvm").exists() {
            anyhow::bail!(
                "/dev/kvm not present; this host does not support KVM. \
                 Use the Docker or LXD backend, or move to a host with \
                 nested virtualization enabled."
            );
        }
        let out = Command::new("qemu-system-x86_64")
            .arg("--version")
            .output()
            .await
            .context("qemu-system-x86_64 not found on PATH; install qemu-system-x86")?;
        if !out.status.success() {
            anyhow::bail!(
                "qemu-system-x86_64 --version failed: {}",
                String::from_utf8_lossy(&out.stderr)
            );
        }
        Ok(String::from_utf8_lossy(&out.stdout)
            .lines()
            .next()
            .unwrap_or("")
            .to_string())
    }

    /// Ensure the base image is present, downloading on first call.
    /// Idempotent: subsequent calls no-op.
    async fn ensure_base_image(&self) -> Result<()> {
        if self.config.base_image_path.exists() {
            return Ok(());
        }
        let parent = self
            .config
            .base_image_path
            .parent()
            .context("base_image_path has no parent")?;
        tokio::fs::create_dir_all(parent)
            .await
            .context("create base image directory")?;
        info!(
            "Downloading base image from {} to {}",
            self.config.base_image_url,
            self.config.base_image_path.display()
        );
        let out = Command::new("curl")
            .args([
                "-fsSL",
                "-o",
                self.config.base_image_path.to_string_lossy().as_ref(),
                &self.config.base_image_url,
            ])
            .output()
            .await
            .context("invoke curl to fetch base image")?;
        if !out.status.success() {
            anyhow::bail!(
                "curl failed to fetch base image: {}",
                String::from_utf8_lossy(&out.stderr)
            );
        }
        Ok(())
    }

    /// Cloud-init userdata: enable SSH password auth, set the root
    /// password the consumer supplied. Yes, password auth — not key
    /// auth — because the consumer already trusts this provider with
    /// the password (it lived in the spawn DM); requiring an extra
    /// pubkey upload is friction without a security gain when the
    /// VM is single-tenant.
    fn user_data(password: &str) -> String {
        format!(
            "#cloud-config\n\
             ssh_pwauth: true\n\
             disable_root: false\n\
             chpasswd:\n  \
               list: |\n    \
                 root:{}\n  \
               expire: false\n\
             # Keep the boot fast: skip waiting for slow services.\n\
             timezone: Etc/UTC\n",
            password
        )
    }

    fn meta_data(id: u32) -> String {
        format!(
            "instance-id: paygress-{0}\nlocal-hostname: paygress-{0}\n",
            id
        )
    }

    /// Build the cloud-init seed ISO. Uses `genisoimage` — the
    /// `cloud-localds` wrapper would be more concise but it's
    /// less universally available across distros.
    async fn make_seed_iso(&self, id: u32, password: &str) -> Result<()> {
        let dir = self.vm_dir(id);
        let user_path = dir.join("user-data");
        let meta_path = dir.join("meta-data");
        tokio::fs::write(&user_path, Self::user_data(password))
            .await
            .context("write user-data")?;
        tokio::fs::write(&meta_path, Self::meta_data(id))
            .await
            .context("write meta-data")?;
        let out = Command::new("genisoimage")
            .args([
                "-output",
                self.seed_path(id).to_string_lossy().as_ref(),
                "-volid",
                "cidata",
                "-joliet",
                "-rock",
                user_path.to_string_lossy().as_ref(),
                meta_path.to_string_lossy().as_ref(),
            ])
            .output()
            .await
            .context("invoke genisoimage")?;
        if !out.status.success() {
            anyhow::bail!(
                "genisoimage failed: {}",
                String::from_utf8_lossy(&out.stderr)
            );
        }
        Ok(())
    }

    /// Build the qemu-system-x86_64 argv. Pure function — testable
    /// without spawning anything. Caller is responsible for the seed
    /// ISO + qcow2 already existing.
    pub fn qemu_argv(&self, config: &ContainerConfig) -> Vec<String> {
        let id = config.id;
        let cores = config.cpu_cores.max(1);
        let mem_mb = config.memory_mb.max(512);
        let host_port = config.host_port.unwrap_or(0);

        // user-mode networking + hostfwd: SSH always, plus any
        // template ports the consumer asked for.
        let mut hostfwds = vec![format!("hostfwd=tcp::{}-:22", host_port)];
        for p in &config.template_ports {
            hostfwds.push(format!(
                "hostfwd={}::{}-:{}",
                p.protocol, p.host_port, p.container_port
            ));
        }
        let netdev = format!("user,id=net0,{}", hostfwds.join(","));

        vec![
            // Acceleration + CPU model: -enable-kvm uses the host's
            // KVM module; -cpu host passes through the physical CPU
            // features so guests can use AES-NI / AVX / etc.
            "-enable-kvm".to_string(),
            "-cpu".to_string(),
            "host".to_string(),
            "-machine".to_string(),
            "type=q35,accel=kvm".to_string(),
            "-smp".to_string(),
            cores.to_string(),
            "-m".to_string(),
            mem_mb.to_string(),
            // Primary disk: per-VM qcow2 overlay on the read-only base.
            "-drive".to_string(),
            format!(
                "file={},if=virtio,format=qcow2",
                self.disk_path(id).display()
            ),
            // Cloud-init seed: rom image, qemu picks it up at boot.
            "-drive".to_string(),
            format!(
                "file={},if=virtio,format=raw,readonly=on",
                self.seed_path(id).display()
            ),
            "-netdev".to_string(),
            netdev,
            "-device".to_string(),
            "virtio-net-pci,netdev=net0".to_string(),
            // Daemonize + pidfile so we can manage the lifecycle
            // post-spawn via the pid (kill / wait).
            "-daemonize".to_string(),
            "-pidfile".to_string(),
            self.pidfile_path(id).to_string_lossy().to_string(),
            // No graphical console; serial captured to a file for
            // operator debugging when a guest fails to boot.
            "-nographic".to_string(),
            "-serial".to_string(),
            format!("file:{}", self.serial_log_path(id).display()),
        ]
    }

    async fn create_overlay_disk(&self, id: u32, size_gb: u32) -> Result<()> {
        // qemu-img create -f qcow2 -b <base> -F qcow2 <overlay> <size>G
        // The -b backing-file + -F backing-format pair makes the
        // overlay reference the base; only modified blocks live in
        // the overlay.
        let out = Command::new("qemu-img")
            .args([
                "create",
                "-f",
                "qcow2",
                "-b",
                self.config.base_image_path.to_string_lossy().as_ref(),
                "-F",
                "qcow2",
                self.disk_path(id).to_string_lossy().as_ref(),
                &format!("{}G", size_gb.max(5)),
            ])
            .output()
            .await
            .context("invoke qemu-img create")?;
        if !out.status.success() {
            anyhow::bail!(
                "qemu-img create failed: {}",
                String::from_utf8_lossy(&out.stderr)
            );
        }
        Ok(())
    }

    /// Read the daemonized qemu's PID from its pidfile.
    async fn read_pid(&self, id: u32) -> Option<i32> {
        let p = self.pidfile_path(id);
        let bytes = tokio::fs::read_to_string(&p).await.ok()?;
        bytes.trim().parse().ok()
    }
}

#[async_trait]
impl ComputeBackend for KvmBackend {
    /// Scan vm_root for existing per-id directories and pick the
    /// next free id in range.
    async fn find_available_id(&self, range_start: u32, range_end: u32) -> Result<u32> {
        let mut used = std::collections::HashSet::new();
        if let Ok(mut entries) = tokio::fs::read_dir(&self.config.vm_root).await {
            while let Ok(Some(entry)) = entries.next_entry().await {
                if let Some(name) = entry.file_name().to_str() {
                    if let Ok(n) = name.parse::<u32>() {
                        used.insert(n);
                    }
                }
            }
        }
        for id in range_start..=range_end {
            if !used.contains(&id) {
                return Ok(id);
            }
        }
        anyhow::bail!(
            "no available VM id in range {}..={}",
            range_start,
            range_end
        );
    }

    async fn create_container(&self, config: &ContainerConfig) -> Result<String> {
        let id = config.id;
        info!(
            "Provisioning KVM VM: id={} cores={} mem={}MB disk={}GB",
            id, config.cpu_cores, config.memory_mb, config.storage_gb
        );

        self.ensure_base_image().await?;

        let dir = self.vm_dir(id);
        tokio::fs::create_dir_all(&dir)
            .await
            .context("create vm directory")?;

        self.create_overlay_disk(id, config.storage_gb)
            .await
            .context("create overlay disk")?;
        self.make_seed_iso(id, &config.password)
            .await
            .context("build cloud-init seed iso")?;

        let argv = self.qemu_argv(config);
        debug!("qemu argv: {:?}", argv);
        let arg_refs: Vec<&str> = argv.iter().map(|s| s.as_str()).collect();
        let out = Command::new("qemu-system-x86_64")
            .args(&arg_refs)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .await
            .context("invoke qemu-system-x86_64")?;
        if !out.status.success() {
            // qemu's own error landed on stderr; surface it to the
            // operator so a "missing /dev/kvm" or "image not found"
            // is obvious.
            anyhow::bail!(
                "qemu-system-x86_64 failed: {}",
                String::from_utf8_lossy(&out.stderr)
            );
        }

        let pid = self
            .read_pid(id)
            .await
            .context("qemu daemonized but pidfile missing — boot failed before pidfile write?")?;
        info!("KVM VM id={} live (pid {})", id, pid);
        Ok(format!("paygress-vm-{}", id))
    }

    /// Daemonized qemu starts on `create_container`; this is a no-op
    /// since the lifecycle is single-shot. If we ever migrate to a
    /// non-daemonized qemu (e.g. for live migration support),
    /// `start_container` becomes the place that spawns the process.
    async fn start_container(&self, _id: u32) -> Result<()> {
        Ok(())
    }

    async fn stop_container(&self, id: u32) -> Result<()> {
        if let Some(pid) = self.read_pid(id).await {
            // Best-effort SIGTERM; qemu shuts down its guest cleanly
            // when it receives SIGTERM (sends ACPI power button).
            let _ = Command::new("kill")
                .args(["-TERM", &pid.to_string()])
                .status()
                .await;
        }
        Ok(())
    }

    async fn delete_container(&self, id: u32) -> Result<()> {
        // Stop first (idempotent if already gone), then nuke the dir.
        let _ = self.stop_container(id).await;
        if let Some(pid) = self.read_pid(id).await {
            // If TERM didn't take, escalate after a short wait.
            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
            let _ = Command::new("kill")
                .args(["-KILL", &pid.to_string()])
                .status()
                .await;
        }
        let dir = self.vm_dir(id);
        if dir.exists() {
            if let Err(e) = tokio::fs::remove_dir_all(&dir).await {
                warn!("remove {} non-fatal: {}", dir.display(), e);
            }
        }
        Ok(())
    }

    async fn get_node_status(&self) -> Result<NodeStatus> {
        // Same minimal report as DockerBackend; a sysinfo-backed
        // implementation lands in a follow-up.
        Ok(NodeStatus {
            cpu_usage: 0.0,
            memory_used: 0,
            memory_total: 0,
            disk_used: 0,
            disk_total: 0,
        })
    }

    async fn get_container_ip(&self, _id: u32) -> Result<Option<String>> {
        // Guest is reachable via the host's IP + the SSH host_port
        // forward. We don't expose the guest's internal 10.0.2.x IP
        // because user-mode networking NATs everything.
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::compute::PortMapping;

    fn cfg(id: u32) -> ContainerConfig {
        ContainerConfig {
            id,
            name: format!("paygress-vm-{}", id),
            image: String::new(), // ignored by KVM v1
            cpu_cores: 2,
            memory_mb: 2048,
            storage_gb: 10,
            password: "secret".to_string(),
            ssh_key: None,
            host_port: Some(31000),
            template_ports: vec![PortMapping {
                host_port: 18789,
                container_port: 18789,
                protocol: "tcp",
            }],
            template_env: Default::default(),
            extra_runtime_args: vec![],
            data_path: None,
            volume_encryption_key: None,
        }
    }

    #[test]
    fn qemu_argv_includes_kvm_acceleration_and_cpu_host() {
        let backend = KvmBackend::new(KvmConfig::default());
        let argv = backend.qemu_argv(&cfg(42));
        assert!(argv.iter().any(|a| a == "-enable-kvm"));
        let cpu_idx = argv.iter().position(|a| a == "-cpu").unwrap();
        assert_eq!(argv[cpu_idx + 1], "host");
    }

    #[test]
    fn qemu_argv_forwards_ssh_and_template_ports() {
        let backend = KvmBackend::new(KvmConfig::default());
        let argv = backend.qemu_argv(&cfg(42));
        let netdev = argv
            .iter()
            .position(|a| a == "-netdev")
            .map(|i| argv[i + 1].clone())
            .unwrap();
        assert!(
            netdev.contains("hostfwd=tcp::31000-:22"),
            "ssh hostfwd missing in: {netdev}"
        );
        assert!(
            netdev.contains("hostfwd=tcp::18789-:18789"),
            "template hostfwd missing in: {netdev}"
        );
    }

    #[test]
    fn qemu_argv_pidfile_and_disk_paths_are_id_scoped() {
        let backend = KvmBackend::new(KvmConfig::default());
        let argv = backend.qemu_argv(&cfg(7));
        let pidfile_idx = argv.iter().position(|a| a == "-pidfile").unwrap();
        assert!(argv[pidfile_idx + 1].contains("/7/qemu.pid"));
        let drives: Vec<&String> = argv
            .iter()
            .enumerate()
            .filter(|(i, a)| *a == "-drive" && *i + 1 < argv.len())
            .map(|(i, _)| &argv[i + 1])
            .collect();
        assert!(drives.iter().any(|d| d.contains("/7/disk.qcow2")));
        assert!(drives.iter().any(|d| d.contains("/7/seed.iso")));
    }

    #[test]
    fn qemu_argv_memory_floor() {
        let backend = KvmBackend::new(KvmConfig::default());
        let mut tiny = cfg(1);
        tiny.memory_mb = 64; // way below qemu's reasonable floor
        let argv = backend.qemu_argv(&tiny);
        let m_idx = argv.iter().position(|a| a == "-m").unwrap();
        assert_eq!(argv[m_idx + 1], "512", "must clamp to 512 MB minimum");
    }

    #[test]
    fn paths_are_id_scoped_and_under_vm_root() {
        let backend = KvmBackend::new(KvmConfig::default());
        for (a, b) in [(1u32, 2u32), (10, 20), (999, 1000)] {
            assert_ne!(backend.vm_dir(a), backend.vm_dir(b));
            assert_ne!(backend.disk_path(a), backend.disk_path(b));
            assert!(backend.vm_dir(a).starts_with(VM_ROOT));
        }
    }

    #[test]
    fn user_data_includes_password_and_enables_pwauth() {
        let ud = KvmBackend::user_data("hunter2");
        assert!(ud.contains("ssh_pwauth: true"));
        assert!(ud.contains("root:hunter2"));
    }
}
