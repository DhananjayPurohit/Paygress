use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::nostr::PodSpec;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum BackendType {
    #[default]
    Proxmox,
    LXD,
    /// Requires the `docker` CLI. Templates use public Docker images that LXD
    /// cannot run natively.
    Docker,
    /// One VM per spawn, each with its own kernel. Requires `/dev/kvm` and
    /// `qemu-system-x86_64`; does not serve Docker templates.
    Kvm,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderConfig {
    #[serde(default)]
    pub backend_type: BackendType,

    pub proxmox_url: String,
    pub proxmox_token_id: String,
    pub proxmox_token_secret: String,
    pub proxmox_node: String,

    /// Disable TLS verification against the Proxmox API. Needed for the
    /// self-signed cert Proxmox ships with; leaving it off means the API token
    /// is only sent to a verified host.
    #[serde(default)]
    pub proxmox_accept_invalid_certs: bool,
    pub proxmox_storage: String,
    pub proxmox_template: String,
    pub proxmox_bridge: String,
    pub vmid_range_start: u32,
    pub vmid_range_end: u32,

    pub nostr_private_key: String,
    pub nostr_relays: Vec<String>,

    pub provider_name: String,
    pub provider_location: Option<String>,
    pub public_ip: String,
    pub capabilities: Vec<String>,

    /// Sandbox images this provider serves, advertised in its offer so a
    /// consumer can tell before paying whether the image its job needs is
    /// here. A CI provider lists the alias `images/ci-sandbox/build-lxd.sh`
    /// publishes, normally `paygress-ci`.
    ///
    /// Empty advertises nothing and refuses nothing: discovery reads silence
    /// as "unknown", so an operator who has not declared images is still
    /// reachable by consumers that name one explicitly.
    #[serde(default)]
    pub available_images: Vec<String>,

    pub specs: Vec<PodSpec>,
    pub whitelisted_mints: Vec<String>,

    pub heartbeat_interval_secs: u64,
    pub minimum_duration_seconds: u64,

    // Tunnel settings, for providers behind NAT.
    #[serde(default)]
    pub tunnel_enabled: bool,
    #[serde(default)]
    pub tunnel_interface: Option<String>,
    #[serde(default)]
    pub ssh_port_start: Option<u16>,
    #[serde(default)]
    pub ssh_port_end: Option<u16>,

    /// Shared CDK SQLite wallet. ngx_l402 opens the same file (`CASHU_DB_PATH`)
    /// to melt the proceeds to Lightning.
    #[serde(default = "default_cashu_wallet_db_path")]
    pub cashu_wallet_db_path: String,

    /// Where the active-workload table is mirrored to disk. It is the only
    /// record that a lease exists — the backend knows a container is running but
    /// not who paid for it or when it expires. Held purely in memory, a restart
    /// leaks every container and its vmid.
    #[serde(default = "default_workload_state_path")]
    pub workload_state_path: String,

    #[serde(default = "default_standby_state_path")]
    pub standby_state_path: String,

    /// Bind address for the optional HTTP+ngx_l402 interface, the port
    /// `nginx/conf.d/paygress-l402.conf` proxies to. Omit to run Nostr-DM only.
    #[serde(default)]
    pub http_bind_addr: Option<String>,

    /// Lightning address (`user@domain`) where ngx_l402 sweeps accumulated
    /// ecash. Written as `LNURL_ADDRESS` in `/etc/paygress/.env`.
    #[serde(default)]
    pub lightning_address: Option<String>,

    /// Base image every KVM overlay is cut from. Unset means stock Ubuntu; a
    /// provider serving CI points these at an image carrying docker and act
    /// (`images/ci-sandbox/build.sh`), since the KVM backend has no per-workload
    /// image to install them into. Ignored by every other backend.
    #[serde(default)]
    pub kvm_base_image_path: Option<String>,

    /// Fetched to `kvm_base_image_path` on first spawn when that file is
    /// missing.
    #[serde(default)]
    pub kvm_base_image_url: Option<String>,

    /// Refuse a spawn when the host has less than this much disk free, before
    /// the token is redeemed. A provider that accepts work it cannot store takes
    /// payment and then fails during provisioning -- or worse, succeeds and dies
    /// later with leases it can no longer sweep. `0` disables the check, and so
    /// does a backend that does not measure disk.
    #[serde(default = "default_min_free_disk_gb")]
    pub min_free_disk_gb: u64,
}

impl ProviderConfig {
    /// Host port forwarded to the workload's SSH. Derived rather than stored,
    /// so the spawn reply, the status reply and the HTTP interface all name the
    /// same port for a given vmid.
    pub(crate) fn ssh_host_port(&self, vmid: u32) -> u16 {
        match self.ssh_port_start {
            Some(start) => start + (vmid - self.vmid_range_start) as u16,
            None => 30000 + (vmid % 10000) as u16,
        }
    }
}

fn default_cashu_wallet_db_path() -> String {
    "./paygress-cashu-wallet.sqlite".to_string()
}

fn default_standby_state_path() -> String {
    "./paygress-standby-slots.json".to_string()
}

fn default_workload_state_path() -> String {
    "./paygress-workloads.json".to_string()
}

/// Room for the act runner image (~2.3 GB) plus a job's build output, with
/// enough left that the cleanup sweep can still run.
fn default_min_free_disk_gb() -> u64 {
    10
}

impl Default for ProviderConfig {
    fn default() -> Self {
        Self {
            backend_type: BackendType::Proxmox,
            proxmox_url: "https://localhost:8006/api2/json".to_string(),
            proxmox_token_id: "root@pam!paygress".to_string(),
            proxmox_token_secret: String::new(),
            proxmox_node: "pve".to_string(),
            proxmox_accept_invalid_certs: false,
            proxmox_storage: "local-lvm".to_string(),
            proxmox_template: "local:vztmpl/ubuntu-22.04-standard.tar.zst".to_string(),
            proxmox_bridge: "vmbr0".to_string(),
            vmid_range_start: 1000,
            vmid_range_end: 1999,
            nostr_private_key: String::new(),
            nostr_relays: vec![
                "wss://relay.damus.io".to_string(),
                "wss://nos.lol".to_string(),
            ],
            provider_name: "Paygress Provider".to_string(),
            provider_location: None,
            public_ip: "127.0.0.1".to_string(),
            capabilities: vec!["lxc".to_string()],
            available_images: Vec::new(),
            specs: vec![PodSpec {
                id: "basic".to_string(),
                name: "Basic".to_string(),
                description: "1 vCPU, 1GB RAM".to_string(),
                cpu_millicores: 1000,
                memory_mb: 1024,
                rate_msats_per_sec: 50,
            }],
            whitelisted_mints: vec!["https://testnut.cashu.space".to_string()],
            heartbeat_interval_secs: 60,
            minimum_duration_seconds: 60,
            tunnel_enabled: false,
            tunnel_interface: None,
            ssh_port_start: None,
            ssh_port_end: None,
            cashu_wallet_db_path: default_cashu_wallet_db_path(),
            workload_state_path: default_workload_state_path(),
            standby_state_path: default_standby_state_path(),
            http_bind_addr: None,
            lightning_address: None,
            kvm_base_image_path: None,
            kvm_base_image_url: None,
            min_free_disk_gb: default_min_free_disk_gb(),
        }
    }
}

pub fn load_config(path: &str) -> Result<ProviderConfig> {
    let content = std::fs::read_to_string(path)
        .with_context(|| format!("Failed to read config file: {}", path))?;

    serde_json::from_str(&content).context("Failed to parse provider config")
}

pub fn save_config(path: &str, config: &ProviderConfig) -> Result<()> {
    let content = serde_json::to_string_pretty(config)?;
    std::fs::write(path, content)
        .with_context(|| format!("Failed to write config file: {}", path))?;
    Ok(())
}

#[cfg(test)]
mod headroom_config_tests {
    use super::*;

    /// A provider config written before this field existed must keep loading --
    /// the live CI provider's is one of them, and a parse failure there takes
    /// the provider down rather than merely skipping the check.
    #[test]
    fn config_without_the_field_defaults_to_ten_gb() {
        let json = r#"{
            "backend_type": "LXD",
            "proxmox_url": "https://127.0.0.1:8006/api2/json",
            "proxmox_token_id": "unused",
            "proxmox_token_secret": "unused",
            "proxmox_node": "pve",
            "proxmox_accept_invalid_certs": false,
            "proxmox_storage": "default",
            "proxmox_template": "ubuntu:24.04",
            "proxmox_bridge": "lxdbr0",
            "vmid_range_start": 2000,
            "vmid_range_end": 2999,
            "nostr_private_key": "nsec1example",
            "nostr_relays": ["wss://nos.lol"],
            "provider_name": "CIRunner",
            "provider_location": "VPS",
            "public_ip": "203.0.113.10",
            "capabilities": ["lxc", "docker"],
            "specs": [],
            "whitelisted_mints": ["https://testnut.cashu.space"],
            "heartbeat_interval_secs": 60,
            "minimum_duration_seconds": 60,
            "tunnel_enabled": false
        }"#;

        let config: ProviderConfig =
            serde_json::from_str(json).expect("a config predating the field must still load");
        assert_eq!(config.min_free_disk_gb, 10);
    }

    #[test]
    fn an_explicit_zero_survives_the_round_trip() {
        let json = r#"{
            "backend_type": "LXD",
            "proxmox_url": "https://127.0.0.1:8006/api2/json",
            "proxmox_token_id": "unused",
            "proxmox_token_secret": "unused",
            "proxmox_node": "pve",
            "proxmox_accept_invalid_certs": false,
            "proxmox_storage": "default",
            "proxmox_template": "ubuntu:24.04",
            "proxmox_bridge": "lxdbr0",
            "vmid_range_start": 2000,
            "vmid_range_end": 2999,
            "nostr_private_key": "nsec1example",
            "nostr_relays": ["wss://nos.lol"],
            "provider_name": "CIRunner",
            "provider_location": "VPS",
            "public_ip": "203.0.113.10",
            "capabilities": ["lxc"],
            "specs": [],
            "whitelisted_mints": ["https://testnut.cashu.space"],
            "heartbeat_interval_secs": 60,
            "minimum_duration_seconds": 60,
            "tunnel_enabled": false,
            "min_free_disk_gb": 0
        }"#;

        let config: ProviderConfig = serde_json::from_str(json).expect("explicit zero must load");
        assert_eq!(config.min_free_disk_gb, 0);
    }
}
