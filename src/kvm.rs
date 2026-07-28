// KVM/qemu compute backend: one qemu VM per workload, giving each tenant its
// own kernel. `IsolationLevel::DedicatedHost` — closes container-escape and
// co-tenant paths, but not a host operator with hypervisor root.
//
// Storage layout under `KvmConfig::vm_root`:
//   base/<image>.img   read-only cloud image shared by every VM
//   <id>/disk.qcow2    per-VM overlay
//   <id>/seed.iso      cloud-init seed
//   <id>/qemu.pid      qemu daemon pidfile
//   <id>/serial.log    guest serial console

use std::path::PathBuf;

use anyhow::{Context, Result};
use async_trait::async_trait;
use tokio::process::Command;
use tracing::{debug, info, warn};

use crate::compute::{run_checked, ComputeBackend, ContainerConfig, ContainerStatus, NodeStatus};

const VM_ROOT: &str = "/var/lib/paygress/vm";

const DEFAULT_BASE_IMAGE_URL: &str =
    "https://cloud-images.ubuntu.com/jammy/current/jammy-server-cloudimg-amd64.img";
const DEFAULT_BASE_IMAGE_FILE: &str = "jammy-server-cloudimg-amd64.img";

#[derive(Debug, Clone)]
pub struct KvmConfig {
    /// Read-only base cloud image; per-VM qcow2 overlays sit on top. Downloaded
    /// on first spawn if absent.
    pub base_image_path: PathBuf,
    /// `None` means the operator supplies the file themselves and a missing
    /// one is an error rather than something to fetch.
    pub base_image_url: Option<String>,
    pub vm_root: PathBuf,
}

impl Default for KvmConfig {
    fn default() -> Self {
        Self {
            base_image_path: PathBuf::from(VM_ROOT)
                .join("base")
                .join(DEFAULT_BASE_IMAGE_FILE),
            base_image_url: Some(DEFAULT_BASE_IMAGE_URL.to_string()),
            vm_root: PathBuf::from(VM_ROOT),
        }
    }
}

impl KvmConfig {
    /// Overrides from the provider's config, each falling back to the stock
    /// Ubuntu default. A CI provider sets both to an image carrying docker and
    /// act; setting only the path serves a file the operator placed there and
    /// never downloads.
    pub fn for_provider(base_image_path: Option<&str>, base_image_url: Option<&str>) -> Self {
        let defaults = Self::default();
        if base_image_path.is_none() && base_image_url.is_none() {
            return defaults;
        }
        Self {
            base_image_path: base_image_path
                .map(PathBuf::from)
                .unwrap_or(defaults.base_image_path),
            // Deliberately not falling back to the stock URL: downloading
            // Ubuntu into a path the operator named `ci-sandbox.qcow2` would
            // serve the wrong image under the right name.
            base_image_url: base_image_url.map(str::to_string),
            vm_root: defaults.vm_root,
        }
    }
}

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

    /// Called at provider startup so "this host doesn't support KVM" surfaces
    /// before a consumer has paid for a spawn.
    pub async fn check_kvm_available() -> Result<String> {
        if !PathBuf::from("/dev/kvm").exists() {
            anyhow::bail!(
                "/dev/kvm not present; this host does not support KVM. \
                 Use the Docker or LXD backend, or move to a host with \
                 nested virtualization enabled."
            );
        }
        let version = run_checked("qemu-system-x86_64", &["--version"])
            .await
            .context("qemu-system-x86_64 not found on PATH; install qemu-system-x86")?;
        Ok(version.lines().next().unwrap_or("").to_string())
    }

    async fn ensure_base_image(&self) -> Result<()> {
        if self.config.base_image_path.exists() {
            return Ok(());
        }
        let Some(url) = self.config.base_image_url.as_deref() else {
            anyhow::bail!(
                "base image {} is missing and no download URL is configured; \
                 build one with images/ci-sandbox/build.sh or set kvm_base_image_url",
                self.config.base_image_path.display()
            );
        };
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
            url,
            self.config.base_image_path.display()
        );
        run_checked(
            "curl",
            &[
                "-fsSL",
                "-o",
                self.config.base_image_path.to_string_lossy().as_ref(),
                url,
            ],
        )
        .await
        .context("fetch base image")?;
        Ok(())
    }

    /// Password auth rather than key auth: the consumer already has the
    /// password from the spawn DM, and the VM is single-tenant.
    fn user_data(password: &str) -> String {
        format!(
            "#cloud-config\n\
             ssh_pwauth: true\n\
             disable_root: false\n\
             chpasswd:\n  \
               list: |\n    \
                 root:{}\n  \
               expire: false\n\
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

    /// `genisoimage` rather than `cloud-localds`: the latter isn't packaged
    /// everywhere.
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
        run_checked(
            "genisoimage",
            &[
                "-output",
                self.seed_path(id).to_string_lossy().as_ref(),
                "-volid",
                "cidata",
                "-joliet",
                "-rock",
                user_path.to_string_lossy().as_ref(),
                meta_path.to_string_lossy().as_ref(),
            ],
        )
        .await?;
        Ok(())
    }

    /// Pure: the caller must have created the seed ISO and qcow2 already.
    pub fn qemu_argv(&self, config: &ContainerConfig) -> Vec<String> {
        let id = config.id;
        let cores = config.cpu_cores.max(1);
        let mem_mb = config.memory_mb.max(512);
        let host_port = config.host_port.unwrap_or(0);

        let mut hostfwds = vec![format!("hostfwd=tcp::{}-:22", host_port)];
        for p in &config.template_ports {
            hostfwds.push(format!(
                "hostfwd={}::{}-:{}",
                p.protocol, p.host_port, p.container_port
            ));
        }
        let netdev = format!("user,id=net0,{}", hostfwds.join(","));

        vec![
            "-enable-kvm".to_string(),
            // Pass through the host CPU's features (AES-NI, AVX).
            "-cpu".to_string(),
            "host".to_string(),
            "-machine".to_string(),
            "type=q35,accel=kvm".to_string(),
            "-smp".to_string(),
            cores.to_string(),
            "-m".to_string(),
            mem_mb.to_string(),
            "-drive".to_string(),
            format!(
                "file={},if=virtio,format=qcow2",
                self.disk_path(id).display()
            ),
            "-drive".to_string(),
            format!(
                "file={},if=virtio,format=raw,readonly=on",
                self.seed_path(id).display()
            ),
            "-netdev".to_string(),
            netdev,
            "-device".to_string(),
            "virtio-net-pci,netdev=net0".to_string(),
            // Pidfile so the lifecycle is manageable by pid after
            // create_container returns.
            "-daemonize".to_string(),
            "-pidfile".to_string(),
            self.pidfile_path(id).to_string_lossy().to_string(),
            "-nographic".to_string(),
            "-serial".to_string(),
            format!("file:{}", self.serial_log_path(id).display()),
        ]
    }

    async fn create_overlay_disk(&self, id: u32, size_gb: u32) -> Result<()> {
        // -b/-F make the new qcow2 a copy-on-write overlay of the base image.
        run_checked(
            "qemu-img",
            &[
                "create",
                "-f",
                "qcow2",
                "-b",
                self.config.base_image_path.to_string_lossy().as_ref(),
                "-F",
                "qcow2",
                self.disk_path(id).to_string_lossy().as_ref(),
                &format!("{}G", size_gb.max(5)),
            ],
        )
        .await?;
        Ok(())
    }

    async fn read_pid(&self, id: u32) -> Option<i32> {
        let raw = tokio::fs::read_to_string(self.pidfile_path(id))
            .await
            .ok()?;
        raw.trim().parse().ok()
    }
}

#[async_trait]
impl ComputeBackend for KvmBackend {
    async fn find_available_id(&self, range_start: u32, range_end: u32) -> Result<u32> {
        let mut used = std::collections::HashSet::new();
        if let Ok(mut entries) = tokio::fs::read_dir(&self.config.vm_root).await {
            while let Ok(Some(entry)) = entries.next_entry().await {
                if let Some(Ok(n)) = entry.file_name().to_str().map(str::parse::<u32>) {
                    used.insert(n);
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

        tokio::fs::create_dir_all(self.vm_dir(id))
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
        run_checked("qemu-system-x86_64", &arg_refs).await?;

        let pid = self
            .read_pid(id)
            .await
            .context("qemu daemonized but pidfile missing — boot failed before pidfile write?")?;
        info!("KVM VM id={} live (pid {})", id, pid);
        Ok(format!("paygress-vm-{}", id))
    }

    /// No-op: the daemonized qemu is started by `create_container`.
    async fn start_container(&self, _id: u32) -> Result<()> {
        Ok(())
    }

    /// Liveness is read from the process, not from the directory: a dead qemu
    /// leaves its directory behind, and `find_available_id` derives ids from
    /// that listing, so the id would never be reclaimed.
    async fn get_container_status(&self, id: u32) -> Result<ContainerStatus> {
        if !self.vm_dir(id).exists() {
            return Ok(ContainerStatus::Absent);
        }
        let Some(pid) = self.read_pid(id).await else {
            return Ok(ContainerStatus::Stopped);
        };
        // Cheapest liveness check that needs no signal permissions.
        if std::path::Path::new(&format!("/proc/{}", pid)).exists() {
            Ok(ContainerStatus::Running)
        } else {
            Ok(ContainerStatus::Stopped)
        }
    }

    async fn stop_container(&self, id: u32) -> Result<()> {
        if let Some(pid) = self.read_pid(id).await {
            // SIGTERM makes qemu press the guest's ACPI power button.
            let _ = Command::new("kill")
                .args(["-TERM", &pid.to_string()])
                .status()
                .await;
        }
        Ok(())
    }

    async fn delete_container(&self, id: u32) -> Result<()> {
        let _ = self.stop_container(id).await;
        if let Some(pid) = self.read_pid(id).await {
            // TERM didn't take; escalate after a grace period.
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
        Ok(NodeStatus {
            cpu_usage: 0.0,
            memory_used: 0,
            memory_total: 0,
            disk_used: 0,
            disk_total: 0,
        })
    }

    async fn get_container_ip(&self, _id: u32) -> Result<Option<String>> {
        // User-mode networking NATs everything; the guest is reached via the
        // host IP plus the SSH hostfwd.
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
            image: String::new(),
            cpu_cores: 2,
            memory_mb: 2048,
            storage_gb: 10,
            password: "secret".to_string(),
            ssh_key: None,
            host_port: Some(31000),
            template_ports: vec![PortMapping {
                host_port: 18789,
                container_port: 18789,
                protocol: "tcp".to_string(),
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
        tiny.memory_mb = 64;
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

    #[test]
    fn unconfigured_provider_keeps_the_stock_image() {
        let config = KvmConfig::for_provider(None, None);
        assert_eq!(config.base_image_path, KvmConfig::default().base_image_path);
        assert_eq!(
            config.base_image_url.as_deref(),
            Some(DEFAULT_BASE_IMAGE_URL)
        );
    }

    #[test]
    fn a_custom_image_path_alone_never_downloads() {
        // Otherwise a provider serving `ci-sandbox.qcow2` would silently fetch
        // stock Ubuntu into that name and run CI jobs without docker or act.
        let config = KvmConfig::for_provider(Some("/srv/ci-sandbox.qcow2"), None);
        assert_eq!(
            config.base_image_path,
            PathBuf::from("/srv/ci-sandbox.qcow2")
        );
        assert!(config.base_image_url.is_none());
    }

    #[test]
    fn a_configured_url_is_used_verbatim() {
        let config = KvmConfig::for_provider(
            Some("/srv/ci-sandbox.qcow2"),
            Some("https://example.invalid/ci.qcow2"),
        );
        assert_eq!(
            config.base_image_url.as_deref(),
            Some("https://example.invalid/ci.qcow2")
        );
    }
}
