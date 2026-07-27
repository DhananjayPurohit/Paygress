// `provider setup` — write a single provider config and sanity-check
// the chosen backend.

use anyhow::Result;
use clap::Args;
use colored::Colorize;
use nostr_sdk::ToBech32;

use super::{detect_public_ip, parse_backend, rule, CONFIG_PATH};
use crate::util::split_csv;
use paygress::nostr::PodSpec;
use paygress::provider::{save_config, BackendType, ProviderConfig};

#[derive(Args)]
pub struct SetupArgs {
    /// Compute backend: proxmox, lxd, docker (shared-kernel) or kvm (dedicated-host)
    #[arg(long, default_value = "proxmox", value_parser = parse_backend)]
    pub backend: BackendType,

    /// Proxmox API URL, e.g. https://192.168.1.100:8006/api2/json
    #[arg(long, required_if_eq("backend", "proxmox"))]
    pub proxmox_url: Option<String>,

    /// Proxmox API token ID, e.g. root@pam!paygress
    #[arg(long, required_if_eq("backend", "proxmox"))]
    pub token_id: Option<String>,

    /// Proxmox API token secret
    #[arg(long, required_if_eq("backend", "proxmox"))]
    pub token_secret: Option<String>,

    /// Proxmox node name
    #[arg(long, default_value = "pve")]
    pub node: String,

    /// Skip TLS verification against the Proxmox API (for its self-signed certificate)
    #[arg(long)]
    pub accept_invalid_certs: bool,

    /// Storage pool name
    #[arg(long, default_value = "local-lvm")]
    pub storage: String,

    /// LXC template path
    #[arg(long, default_value = "local:vztmpl/ubuntu-22.04-standard.tar.zst")]
    pub template: String,

    /// Network bridge
    #[arg(long, default_value = "vmbr0")]
    pub bridge: String,

    /// Nostr private key (nsec format, auto-generated if not provided)
    #[arg(long)]
    pub nostr_key: Option<String>,

    /// Provider display name
    #[arg(long)]
    pub name: String,

    /// Location description (e.g., "US-East", "Germany")
    #[arg(long)]
    pub location: Option<String>,

    /// Public IP address (auto-detected if not provided)
    #[arg(long)]
    pub public_ip: Option<String>,

    /// Whitelisted Cashu mints (comma-separated)
    #[arg(long, default_value = "https://testnut.cashu.space")]
    pub mints: String,

    /// Lightning address (user@domain.com) to auto-sweep earned ecash to
    #[arg(long)]
    pub lightning_address: Option<String>,
}

pub(super) async fn execute_setup(args: SetupArgs, _verbose: bool) -> Result<()> {
    println!("{}", "🔧 Paygress Provider Setup".blue().bold());
    rule();
    println!();

    let nostr_key = match args.nostr_key {
        Some(key) => {
            println!("  {} Using provided Nostr key", "✓".green());
            key
        }
        None => {
            println!("  {} Generating new Nostr keypair...", "⚙".yellow());
            let keys = nostr_sdk::Keys::generate();
            let nsec = keys
                .secret_key()
                .to_bech32()
                .map_err(|e| anyhow::anyhow!("Failed to encode key: {}", e))?;
            println!("  {} Generated new keypair", "✓".green());
            nsec
        }
    };

    let specs = vec![
        PodSpec {
            id: "basic".to_string(),
            name: "Basic".to_string(),
            description: "1 vCPU, 1GB RAM - Great for testing".to_string(),
            cpu_millicores: 1000,
            memory_mb: 1024,
            rate_msats_per_sec: 50,
        },
        PodSpec {
            id: "standard".to_string(),
            name: "Standard".to_string(),
            description: "2 vCPU, 2GB RAM - General purpose".to_string(),
            cpu_millicores: 2000,
            memory_mb: 2048,
            rate_msats_per_sec: 100,
        },
        PodSpec {
            id: "premium".to_string(),
            name: "Premium".to_string(),
            description: "4 vCPU, 4GB RAM - High performance".to_string(),
            cpu_millicores: 4000,
            memory_mb: 4096,
            rate_msats_per_sec: 200,
        },
    ];

    let public_ip = match args.public_ip {
        Some(ip) => ip,
        None => {
            println!("  {} Auto-detecting public IP...", "⚙".yellow());
            match detect_public_ip().await {
                Some(ip) => {
                    println!("  {} Detected: {}", "✓".green(), ip);
                    ip
                }
                None => {
                    println!(
                        "  {} Could not auto-detect IP, using 127.0.0.1",
                        "⚠".yellow()
                    );
                    "127.0.0.1".to_string()
                }
            }
        }
    };

    // proxmox_* fields are only meaningful for `backend == Proxmox`;
    // other backends store empty strings so the JSON keeps its shape.
    let config = ProviderConfig {
        backend_type: args.backend,
        public_ip,
        proxmox_url: args.proxmox_url.unwrap_or_default(),
        proxmox_token_id: args.token_id.unwrap_or_default(),
        proxmox_token_secret: args.token_secret.unwrap_or_default(),
        proxmox_node: args.node,
        proxmox_accept_invalid_certs: args.accept_invalid_certs,
        proxmox_storage: args.storage,
        proxmox_template: args.template,
        proxmox_bridge: args.bridge,
        vmid_range_start: 1000,
        vmid_range_end: 1999,
        nostr_private_key: nostr_key,
        nostr_relays: vec![
            "wss://relay.damus.io".to_string(),
            "wss://nos.lol".to_string(),
            "wss://relay.nostr.band".to_string(),
        ],
        provider_name: args.name,
        provider_location: args.location,
        capabilities: vec!["lxc".to_string(), "vm".to_string()],
        specs,
        whitelisted_mints: split_csv(&args.mints),
        heartbeat_interval_secs: 60,
        minimum_duration_seconds: 60,
        tunnel_enabled: false,
        tunnel_interface: None,
        ssh_port_start: None,
        ssh_port_end: None,
        cashu_wallet_db_path: "./paygress-cashu-wallet.sqlite".to_string(),
        workload_state_path: "./paygress-workloads.json".to_string(),
        standby_state_path: "./paygress-standby-slots.json".to_string(),
        lightning_address: args.lightning_address,
        http_bind_addr: None,
    };

    save_config(CONFIG_PATH, &config)?;
    println!("  {} Configuration saved to {}", "✓".green(), CONFIG_PATH);

    // Surface each backend's requirements at setup time rather than
    // at first-spawn.
    println!();
    match args.backend {
        BackendType::Kvm => {
            println!("  {} Verifying KVM availability...", "⚙".yellow());
            match paygress::kvm::KvmBackend::check_kvm_available().await {
                Ok(version) => println!(
                    "  {} KVM available — {} (offer publishes isolation_level=dedicated-host)",
                    "✓".green(),
                    version
                ),
                Err(e) => println!("  {} KVM unavailable: {}", "✗".red(), e),
            }
            return finalize_setup(&config.provider_name);
        }
        BackendType::Docker => {
            println!("  {} Backend = Docker; no Proxmox check.", "⚙".yellow());
            println!(
                "  {} Ensure `docker` is on PATH and the service user can run it.",
                "→".cyan()
            );
            return finalize_setup(&config.provider_name);
        }
        BackendType::LXD => {
            println!("  {} Backend = LXD; no Proxmox check.", "⚙".yellow());
            println!(
                "  {} Ensure `lxc` is on PATH and the service user is in the `lxd` group.",
                "→".cyan()
            );
            return finalize_setup(&config.provider_name);
        }
        BackendType::Proxmox => {}
    }

    println!("  {} Testing Proxmox connection...", "⚙".yellow());
    check_proxmox_connection(&config).await;

    finalize_setup(&config.provider_name)
}

/// Reports the outcome; a failure here is informational, since the
/// config is already written.
async fn check_proxmox_connection(config: &ProviderConfig) {
    let client = match paygress::proxmox::ProxmoxClient::new(
        &config.proxmox_url,
        &config.proxmox_token_id,
        &config.proxmox_token_secret,
        &config.proxmox_node,
        config.proxmox_accept_invalid_certs,
    ) {
        Ok(client) => client,
        Err(e) => {
            println!("  {} Failed to create Proxmox client: {}", "✗".red(), e);
            return;
        }
    };

    match client.get_node_status().await {
        Ok(status) => {
            println!("  {} Proxmox connected!", "✓".green());
            println!("      Node CPU: {:.1}%", status.cpu * 100.0);
            println!(
                "      Memory: {} MB used",
                status.memory.used / (1024 * 1024)
            );
        }
        Err(e) => {
            println!("  {} Proxmox connection failed: {}", "✗".red(), e);
            println!("      Check your API token and URL");
        }
    }
}

/// Post-setup banner, shared by the per-backend early returns.
fn finalize_setup(provider_name: &str) -> Result<()> {
    println!();
    rule();
    println!("{}", "🎉 Setup Complete!".green().bold());
    println!();
    println!("To start your provider, run:");
    println!("  {} provider start", "paygress-cli".cyan());
    println!();
    println!("Your provider name: {}", provider_name.yellow());
    println!();
    println!(
        "Tip: add {} to auto-sweep earnings to Lightning.",
        "--lightning-address user@domain.com".cyan()
    );
    Ok(())
}
