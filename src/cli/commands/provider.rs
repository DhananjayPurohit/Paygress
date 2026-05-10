// Provider CLI Commands
//
// Commands for machine operators to setup and run a Paygress provider.

use anyhow::Result;
use clap::{Args, Subcommand};
use colored::Colorize;
use nostr_sdk::ToBech32;
use std::process::Command;

use paygress::nostr::PodSpec;
use paygress::provider::{load_config, save_config, ProviderConfig, ProviderService};

const CONFIG_PATH: &str = "/etc/paygress/provider-config.json";

#[derive(Args)]
pub struct ProviderArgs {
    #[command(subcommand)]
    pub action: ProviderAction,
}

#[derive(Subcommand)]
pub enum ProviderAction {
    /// Initial setup - configure Proxmox connection and provider settings
    Setup(SetupArgs),

    /// Scaffold N independent provider configs on the SAME host —
    /// fresh nsec, distinct vmid range, distinct cashu wallet redb,
    /// distinct config file path per provider. Used for end-to-end
    /// warm-standby failover testing on a single VPS, where you want
    /// 3 separate paygress-provider processes (primary + 2 standbys)
    /// publishing distinct offers to Nostr.
    SetupMulti(SetupMultiArgs),

    /// Start the provider service (heartbeats + request handler)
    Start(StartArgs),

    /// Stop the provider service
    Stop,

    /// Show provider status and configuration
    Status,

    /// Edit configuration
    Config(ConfigArgs),

    /// Setup WireGuard VPN tunnel for providers behind NAT
    Tunnel(TunnelArgs),
}

#[derive(Args)]
pub struct SetupArgs {
    /// Compute backend the provider will use to run consumer
    /// workloads. Determines the offer's `isolation_level`:
    ///   - `proxmox` / `lxd` / `docker` → `shared-kernel`
    ///   - `kvm`                          → `dedicated-host` (per-VM)
    /// Default is `proxmox` for backwards compatibility with
    /// existing setups; new operators on bare-metal Linux usually
    /// want `--backend kvm` (no Proxmox install needed) or
    /// `--backend docker` (for the killer-templates path).
    #[arg(long, default_value = "proxmox", value_parser = parse_backend)]
    pub backend: paygress::provider::BackendType,

    /// Proxmox API URL (e.g., https://192.168.1.100:8006/api2/json).
    /// Required only when `--backend proxmox`.
    #[arg(long, required_if_eq("backend", "proxmox"))]
    pub proxmox_url: Option<String>,

    /// Proxmox API token ID (e.g., root@pam!paygress).
    /// Required only when `--backend proxmox`.
    #[arg(long, required_if_eq("backend", "proxmox"))]
    pub token_id: Option<String>,

    /// Proxmox API token secret.
    /// Required only when `--backend proxmox`.
    #[arg(long, required_if_eq("backend", "proxmox"))]
    pub token_secret: Option<String>,

    /// Proxmox node name
    #[arg(long, default_value = "pve")]
    pub node: String,

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
    #[arg(long, default_value = "https://mint.minibits.cash")]
    pub mints: String,
}

/// Scaffold N independent providers on the same host.
///
/// What `setup-multi` produces
/// ---------------------------
/// Per provider i in 0..count:
///   - /etc/paygress/provider-<name>-<i>.json — full config
///   - Fresh `nsec` (independent Nostr identity per provider)
///   - vmid range: [1000 + i*1000, 1999 + i*1000) — non-overlapping
///   - cashu_wallet_db_path: ./paygress-<name>-<i>.redb
///   - provider_name: "<name>-<i>"
///
/// Plus a systemd template unit (printed, not installed) the
/// operator can drop in and enable per-instance.
///
/// Why this exists
/// ---------------
/// End-to-end warm-standby failover testing needs 3 distinct
/// providers. Running 3 separate `paygress-cli provider setup`
/// invocations each requires hand-editing the resulting JSONs to
/// give them non-overlapping vmid ranges and redb paths — annoying
/// and error-prone. This subcommand does it in one shot. Designed
/// for the test loop, but operators running multiple provider
/// instances on one beefy host (e.g. burst capacity, geo-fencing
/// per provider) can reuse the same scaffolding for production.
#[derive(Args)]
pub struct SetupMultiArgs {
    /// Number of providers to scaffold. Defaults to 3 (primary + 2
    /// standbys is the minimum interesting warm-standby topology).
    #[arg(long, default_value_t = 3)]
    pub count: usize,

    /// Compute backend the providers will use. All N providers share
    /// the same backend on the same host — they're scheduling against
    /// the same Docker daemon / KVM /dev/kvm / LXD socket. The vmid
    /// ranges are partitioned so they don't collide on container ids.
    #[arg(long, default_value = "docker", value_parser = parse_backend)]
    pub backend: paygress::provider::BackendType,

    /// Common prefix for the N providers' display names + filenames.
    /// Each provider gets `"<name>-<i>"` (zero-indexed). Pick
    /// something short — it lands in `provider_name`, the systemd
    /// instance name, the redb filename, and the config filename.
    #[arg(long, default_value = "paygress")]
    pub name: String,

    /// Whitelisted Cashu mints (comma-separated). Same list applied
    /// to every provider (they're all on the same host so they have
    /// the same network reachability).
    #[arg(long, default_value = "http://localhost:3338")]
    pub mints: String,

    /// Public IP address (auto-detected if not provided). Same value
    /// applied to every provider since they share the host.
    #[arg(long)]
    pub public_ip: Option<String>,

    /// Skip the systemd template-unit instructions section. Useful
    /// when scripting `setup-multi` in CI or when the operator
    /// already has their own service-management story.
    #[arg(long)]
    pub no_systemd: bool,
}

#[derive(Args)]
pub struct StartArgs {
    /// Path to configuration file
    #[arg(long, default_value = "/etc/paygress/provider-config.json")]
    pub config: String,

    /// Run in foreground (don't daemonize)
    #[arg(long, default_value = "true")]
    pub foreground: bool,
}

#[derive(Args)]
pub struct ConfigArgs {
    /// Show current configuration
    #[arg(long)]
    pub show: bool,

    /// Edit a specific setting
    #[arg(long)]
    pub set: Option<String>,

    /// Value for the setting
    #[arg(long)]
    pub value: Option<String>,
}

#[derive(Args)]
pub struct TunnelArgs {
    /// VPN service URL (e.g., https://vpn.cashu.icu)
    #[arg(long)]
    pub vpn_url: String,

    /// Cashu token to pay for VPN access
    #[arg(long)]
    pub token: String,

    /// WireGuard interface name
    #[arg(long, default_value = "wg0")]
    pub interface: String,
}

pub async fn execute(args: ProviderArgs, verbose: bool) -> Result<()> {
    match args.action {
        ProviderAction::Setup(setup_args) => execute_setup(setup_args, verbose).await,
        ProviderAction::SetupMulti(multi_args) => execute_setup_multi(multi_args, verbose).await,
        ProviderAction::Start(start_args) => execute_start(start_args, verbose).await,
        ProviderAction::Stop => execute_stop(verbose).await,
        ProviderAction::Status => execute_status(verbose).await,
        ProviderAction::Config(config_args) => execute_config(config_args, verbose).await,
        ProviderAction::Tunnel(tunnel_args) => execute_tunnel(tunnel_args, verbose).await,
    }
}

async fn execute_setup(args: SetupArgs, _verbose: bool) -> Result<()> {
    println!("{}", "🔧 Paygress Provider Setup".blue().bold());
    println!("{}", "━".repeat(50).blue());
    println!();

    // Generate Nostr key if not provided
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

    // Create default specs
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

    let mints: Vec<String> = args
        .mints
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();

    // Determine public IP
    let public_ip = match args.public_ip {
        Some(ip) => ip,
        None => {
            println!("  {} Auto-detecting public IP...", "⚙".yellow());
            match reqwest::get("https://api.ipify.org").await {
                Ok(resp) => match resp.text().await {
                    Ok(ip) => {
                        println!("  {} Detected: {}", "✓".green(), ip.trim());
                        ip.trim().to_string()
                    }
                    Err(_) => {
                        println!(
                            "  {} Could not auto-detect IP, using 127.0.0.1",
                            "⚠".yellow()
                        );
                        "127.0.0.1".to_string()
                    }
                },
                Err(_) => {
                    println!(
                        "  {} Could not auto-detect IP, using 127.0.0.1",
                        "⚠".yellow()
                    );
                    "127.0.0.1".to_string()
                }
            }
        }
    };

    // Create configuration. proxmox_* fields are only meaningful
    // for `backend == Proxmox`; for other backends they're stored
    // as empty strings (the JSON keeps the same shape so rolling
    // back to Proxmox just needs `--proxmox-url ...` re-supplied).
    let config = ProviderConfig {
        backend_type: args.backend,
        public_ip,
        proxmox_url: args.proxmox_url.unwrap_or_default(),
        proxmox_token_id: args.token_id.unwrap_or_default(),
        proxmox_token_secret: args.token_secret.unwrap_or_default(),
        proxmox_node: args.node,
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
        provider_name: args.name.clone(),
        provider_location: args.location,
        capabilities: vec!["lxc".to_string(), "vm".to_string()],
        specs,
        whitelisted_mints: mints,
        heartbeat_interval_secs: 60,
        minimum_duration_seconds: 60,
        tunnel_enabled: false,
        tunnel_interface: None,
        ssh_port_start: None,
        ssh_port_end: None,
        cashu_wallet_db_path: "./paygress-cashu-wallet.redb".to_string(),
    };

    // Save configuration
    save_config(CONFIG_PATH, &config)?;
    println!("  {} Configuration saved to {}", "✓".green(), CONFIG_PATH);

    // Per-backend startup check. Skip the Proxmox round-trip for
    // backends that don't talk to Proxmox; for KVM, surface the
    // /dev/kvm + qemu requirement at setup time so the operator
    // doesn't discover it at first-spawn.
    println!();
    match args.backend {
        paygress::provider::BackendType::Kvm => {
            println!("  {} Verifying KVM availability...", "⚙".yellow());
            match paygress::kvm::KvmBackend::check_kvm_available().await {
                Ok(version) => println!(
                    "  {} KVM available — {} (offer publishes isolation_level=dedicated-host)",
                    "✓".green(),
                    version
                ),
                Err(e) => println!("  {} KVM unavailable: {}", "✗".red(), e),
            }
            return finalize_setup(&args.name);
        }
        paygress::provider::BackendType::Docker => {
            println!("  {} Backend = Docker; no Proxmox check.", "⚙".yellow());
            println!(
                "  {} Ensure `docker` is on PATH and the service user can run it.",
                "→".cyan()
            );
            return finalize_setup(&args.name);
        }
        paygress::provider::BackendType::LXD => {
            println!("  {} Backend = LXD; no Proxmox check.", "⚙".yellow());
            println!(
                "  {} Ensure `lxc` is on PATH and the service user is in the `lxd` group.",
                "→".cyan()
            );
            return finalize_setup(&args.name);
        }
        paygress::provider::BackendType::Proxmox => { /* fall through */ }
    }

    // Proxmox-only path: validate the API connection.
    println!("  {} Testing Proxmox connection...", "⚙".yellow());

    match paygress::proxmox::ProxmoxClient::new(
        &config.proxmox_url,
        &config.proxmox_token_id,
        &config.proxmox_token_secret,
        &config.proxmox_node,
    ) {
        Ok(client) => match client.get_node_status().await {
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
        },
        Err(e) => {
            println!("  {} Failed to create Proxmox client: {}", "✗".red(), e);
        }
    }

    finalize_setup(&args.name)
}

/// Print the post-setup "Setup Complete!" banner. Extracted from
/// `execute_setup` so per-backend early-returns share one ending.
fn finalize_setup(provider_name: &str) -> Result<()> {
    println!();
    println!("{}", "━".repeat(50).blue());
    println!("{}", "🎉 Setup Complete!".green().bold());
    println!();
    println!("To start your provider, run:");
    println!("  {} provider start", "paygress-cli".cyan());
    println!();
    println!("Your provider name: {}", provider_name.yellow());
    Ok(())
}

/// clap value-parser for `--backend`. Maps the kebab-case slug
/// (`proxmox` / `lxd` / `docker` / `kvm`) onto the
/// `BackendType` enum, with a friendly error listing valid values.
fn parse_backend(s: &str) -> std::result::Result<paygress::provider::BackendType, String> {
    match s {
        "proxmox" => Ok(paygress::provider::BackendType::Proxmox),
        "lxd" => Ok(paygress::provider::BackendType::LXD),
        "docker" => Ok(paygress::provider::BackendType::Docker),
        "kvm" => Ok(paygress::provider::BackendType::Kvm),
        other => Err(format!(
            "unknown backend `{}` (expected one of: proxmox, lxd, docker, kvm)",
            other
        )),
    }
}

/// Per-instance vmid range size. Each scaffolded provider gets
/// 1000 ids of headroom (1000-1999, 2000-2999, etc.). Plenty for
/// a test loop; a production multi-tenant host would likely pick a
/// larger window. Pulled out as a constant so the test below pins
/// the partition geometry.
const SETUP_MULTI_VMID_RANGE_SIZE: u32 = 1000;

/// Scaffold a fresh `ProviderConfig` for instance `i` of a
/// `setup-multi` invocation. Pure function — no IO, no clock — so
/// the partition logic (vmid range, paths, names) is unit-testable.
fn build_multi_config(
    args: &SetupMultiArgs,
    i: usize,
    public_ip: &str,
    nostr_nsec: String,
    specs: Vec<paygress::nostr::PodSpec>,
    mints: Vec<String>,
) -> paygress::provider::ProviderConfig {
    use paygress::provider::ProviderConfig;
    let provider_name = format!("{}-{}", args.name, i);
    let i32 = i as u32;
    let vmid_start = 1000 + i32 * SETUP_MULTI_VMID_RANGE_SIZE;
    let vmid_end = vmid_start + SETUP_MULTI_VMID_RANGE_SIZE - 1;
    ProviderConfig {
        backend_type: args.backend,
        public_ip: public_ip.to_string(),
        // Proxmox-only fields are left empty; setup-multi is for
        // KVM/Docker/LXD where Proxmox-via-API doesn't apply.
        proxmox_url: String::new(),
        proxmox_token_id: String::new(),
        proxmox_token_secret: String::new(),
        proxmox_node: "pve".to_string(),
        proxmox_storage: "local-lvm".to_string(),
        proxmox_template: "local:vztmpl/ubuntu-22.04-standard.tar.zst".to_string(),
        proxmox_bridge: "vmbr0".to_string(),
        vmid_range_start: vmid_start,
        vmid_range_end: vmid_end,
        nostr_private_key: nostr_nsec,
        nostr_relays: vec![
            "wss://relay.damus.io".to_string(),
            "wss://nos.lol".to_string(),
            "wss://relay.nostr.band".to_string(),
        ],
        provider_name: provider_name.clone(),
        provider_location: None,
        capabilities: vec!["lxc".to_string(), "vm".to_string()],
        specs,
        whitelisted_mints: mints,
        heartbeat_interval_secs: 60,
        minimum_duration_seconds: 60,
        tunnel_enabled: false,
        tunnel_interface: None,
        ssh_port_start: None,
        ssh_port_end: None,
        // Each provider gets its own redb so they don't fight over
        // one wallet's localstore (cdk's per-process write lock
        // would serialize all redemptions otherwise).
        cashu_wallet_db_path: format!("./paygress-{}.redb", provider_name),
    }
}

fn config_path_for(name: &str, i: usize) -> String {
    format!("/etc/paygress/provider-{}-{}.json", name, i)
}

async fn execute_setup_multi(args: SetupMultiArgs, _verbose: bool) -> Result<()> {
    use nostr_sdk::ToBech32;
    use paygress::nostr::PodSpec;

    println!("{}", "🔧 Paygress Multi-Provider Setup".blue().bold());
    println!("{}", "━".repeat(50).blue());
    println!("  Count:    {}", args.count.to_string().yellow());
    println!("  Backend:  {:?}", args.backend);
    println!("  Prefix:   {}", args.name.yellow());
    println!();

    if args.count < 2 {
        anyhow::bail!("--count must be >= 2 (use plain `provider setup` for a single instance)");
    }
    if args.count > 32 {
        anyhow::bail!(
            "--count {} is unreasonably large; the vmid partition runs out at 32 \
             (32 * 1000 = 32000, just below the kernel's typical max-pids cap)",
            args.count
        );
    }

    // Resolve public IP once and apply to all instances. They share
    // the host so the IP is the same.
    let public_ip = match args.public_ip.clone() {
        Some(ip) => ip,
        None => {
            println!("  {} Auto-detecting public IP...", "⚙".yellow());
            match reqwest::get("https://api.ipify.org").await {
                Ok(resp) => resp
                    .text()
                    .await
                    .map(|s| s.trim().to_string())
                    .unwrap_or_else(|_| "127.0.0.1".to_string()),
                Err(_) => "127.0.0.1".to_string(),
            }
        }
    };
    println!("  {} Public IP: {}", "✓".green(), public_ip);

    let specs = vec![
        PodSpec {
            id: "basic".to_string(),
            name: "Basic".to_string(),
            description: "1 vCPU, 1GB RAM".to_string(),
            cpu_millicores: 1000,
            memory_mb: 1024,
            rate_msats_per_sec: 50,
        },
        PodSpec {
            id: "standard".to_string(),
            name: "Standard".to_string(),
            description: "2 vCPU, 2GB RAM".to_string(),
            cpu_millicores: 2000,
            memory_mb: 2048,
            rate_msats_per_sec: 100,
        },
    ];
    let mints: Vec<String> = args
        .mints
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();

    println!();
    let mut written: Vec<(String, String)> = Vec::new(); // (path, npub)
    for i in 0..args.count {
        let keys = nostr_sdk::Keys::generate();
        let nsec = keys
            .secret_key()
            .to_bech32()
            .map_err(|e| anyhow::anyhow!("encode nsec: {}", e))?;
        let npub = keys
            .public_key()
            .to_bech32()
            .map_err(|e| anyhow::anyhow!("encode npub: {}", e))?;

        let cfg = build_multi_config(&args, i, &public_ip, nsec, specs.clone(), mints.clone());
        let path = config_path_for(&args.name, i);
        save_config(&path, &cfg)?;
        println!(
            "  {} {} → {} (vmid {}-{})",
            "✓".green(),
            cfg.provider_name.yellow(),
            path,
            cfg.vmid_range_start,
            cfg.vmid_range_end,
        );
        println!("      npub: {}", npub.cyan());
        written.push((path, npub));
    }

    if !args.no_systemd {
        println!();
        println!("{}", "━".repeat(50).blue());
        println!(
            "{}",
            "systemd template unit (drop in if not present):".bold()
        );
        println!();
        println!("  /etc/systemd/system/paygress-provider@.service");
        println!();
        println!("    [Unit]");
        println!("    Description=Paygress Provider (instance %i)");
        println!("    After=network.target");
        println!();
        println!("    [Service]");
        println!("    Type=simple");
        println!("    ExecStart=/usr/local/bin/paygress-cli provider start \\");
        println!(
            "        --config /etc/paygress/provider-{}-%i.json",
            args.name
        );
        println!("    Restart=always");
        println!("    RestartSec=10");
        println!();
        println!("    [Install]");
        println!("    WantedBy=multi-user.target");
        println!();
        println!(
            "  Then enable each instance: systemctl enable --now paygress-provider@{{0..{}}}",
            args.count - 1
        );
    }

    println!();
    println!("{}", "━".repeat(50).blue());
    println!("{}", "🎉 Multi-Provider Setup Complete".green().bold());
    println!();
    println!("Verify with: {} list", "paygress-cli".cyan());
    println!(
        "(after starting the services, all {} should appear with distinct npubs)",
        args.count
    );

    Ok(())
}

async fn execute_start(args: StartArgs, _verbose: bool) -> Result<()> {
    println!("{}", "🚀 Starting Paygress Provider".blue().bold());
    println!();

    // Load configuration
    let config = load_config(&args.config)?;

    println!("  Provider: {}", config.provider_name.yellow());

    match config.backend_type {
        paygress::provider::BackendType::Proxmox => {
            println!("  Backend:  Proxmox");
            println!("  URL:      {}", config.proxmox_url);
            println!("  Node:     {}", config.proxmox_node);
        }
        paygress::provider::BackendType::LXD => {
            println!("  Backend:  LXD");
            println!("  Storage:  {}", config.proxmox_storage); // Used as pool name
        }
        paygress::provider::BackendType::Docker => {
            println!("  Backend:  Docker");
            println!("  Note:     templates require Docker; ensure `docker` is on PATH");
        }
        paygress::provider::BackendType::Kvm => {
            println!("  Backend:  KVM/qemu (per-VM isolation, dedicated-host tier)");
            println!(
                "  Note:     requires /dev/kvm + qemu-system-x86_64; killer templates not served"
            );
        }
    }
    println!();

    // Create and run the provider service
    let service = ProviderService::new(config).await?;

    println!("  NPUB: {}", service.get_npub().cyan());
    println!();
    println!("{}", "Provider is now live! Press Ctrl+C to stop.".green());
    println!("{}", "━".repeat(50).blue());
    println!();

    // Run the service
    service.run().await?;

    Ok(())
}

async fn execute_stop(_verbose: bool) -> Result<()> {
    println!("{}", "Stopping provider service...".yellow());

    // Try systemctl first (for bootstrapped providers)
    let output = std::process::Command::new("systemctl")
        .args(["stop", "paygress-provider"])
        .output();

    match output {
        Ok(o) if o.status.success() => {
            println!("{}", "Provider stopped via systemctl.".green());
            return Ok(());
        }
        _ => {}
    }

    // Fallback: find and kill the process
    let output = std::process::Command::new("pgrep")
        .args(["-f", "paygress-cli provider start"])
        .output();

    if let Ok(o) = output {
        if o.status.success() {
            let pids = String::from_utf8_lossy(&o.stdout);
            for pid in pids.trim().lines() {
                let _ = std::process::Command::new("kill").arg(pid.trim()).output();
            }
            println!("{}", "Provider stopped.".green());
            return Ok(());
        }
    }

    println!("{}", "No running provider found.".yellow());
    Ok(())
}

async fn execute_status(_verbose: bool) -> Result<()> {
    println!("{}", "📊 Provider Status".blue().bold());
    println!("{}", "━".repeat(50).blue());

    // Try to load config
    match load_config(CONFIG_PATH) {
        Ok(config) => {
            println!();
            println!("  Provider Name:  {}", config.provider_name.yellow());
            println!(
                "  Location:       {}",
                config.provider_location.as_deref().unwrap_or("Not set")
            );
            println!("  Proxmox URL:    {}", config.proxmox_url);
            println!("  Node:           {}", config.proxmox_node);
            println!();
            println!("  {} Tiers configured:", "📦".to_string());
            for spec in &config.specs {
                println!("    • {} - {} msat/sec", spec.name, spec.rate_msats_per_sec);
            }
            println!();
            println!("  {} Accepted mints:", "💰".to_string());
            for mint in &config.whitelisted_mints {
                println!("    • {}", mint);
            }
            if config.tunnel_enabled {
                println!();
                println!("  {} Tunnel:", "🔒".to_string());
                println!(
                    "    Interface: {}",
                    config.tunnel_interface.as_deref().unwrap_or("wg0")
                );
                println!("    Public IP: {}", config.public_ip);
                if let (Some(ps), Some(pe)) = (config.ssh_port_start, config.ssh_port_end) {
                    println!("    Port range: {}-{}", ps, pe);
                }
                // Check if WireGuard interface is up
                let iface = config.tunnel_interface.as_deref().unwrap_or("wg0");
                let wg_status = Command::new("wg").args(["show", iface]).output();
                match wg_status {
                    Ok(o) if o.status.success() => println!("    Status: {}", "UP".green()),
                    _ => println!("    Status: {}", "DOWN".red()),
                }
            }
        }
        Err(_) => {
            println!();
            println!("  {} No configuration found.", "⚠".yellow());
            println!("  Run 'paygress-cli provider setup' first.");
        }
    }

    println!();
    Ok(())
}

async fn execute_config(args: ConfigArgs, _verbose: bool) -> Result<()> {
    if args.show {
        let config = load_config(CONFIG_PATH)?;
        let json = serde_json::to_string_pretty(&config)?;
        println!("{}", json);
        return Ok(());
    }

    if let (Some(key), Some(value)) = (args.set, args.value) {
        println!("Setting {} = {}", key, value);
        // TODO: Implement config editing
        println!("{}", "Config editing not yet implemented".yellow());
    }

    Ok(())
}

/// Check if the current process is running as root (uid 0).
fn nix_is_root() -> bool {
    Command::new("id")
        .arg("-u")
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim() == "0")
        .unwrap_or(false)
}

async fn execute_tunnel(args: TunnelArgs, _verbose: bool) -> Result<()> {
    println!("{}", "WireGuard Tunnel Setup".blue().bold());
    println!("{}", "━".repeat(50).blue());
    println!();

    // Determine if we need sudo (non-root user)
    let need_sudo = !nix_is_root();
    let sudo: &[&str] = if need_sudo { &["sudo"] } else { &[] };

    let wg_conf_path = format!("/etc/wireguard/{}.conf", args.interface);

    // Check if config already exists (use sudo to read since /etc/wireguard may be 700)
    let exists = if need_sudo {
        Command::new("sudo")
            .args(["test", "-f", &wg_conf_path])
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    } else {
        std::path::Path::new(&wg_conf_path).exists()
    };

    if exists {
        println!(
            "  {} WireGuard config already exists at {}",
            "!".yellow(),
            wg_conf_path
        );
        println!("  Delete it first if you want to re-provision.");
        println!();

        // Still try to extract info and update provider config
        let config_content = if need_sudo {
            let out = Command::new("sudo").args(["cat", &wg_conf_path]).output()?;
            String::from_utf8_lossy(&out.stdout).to_string()
        } else {
            std::fs::read_to_string(&wg_conf_path)?
        };
        if let Some((public_ip, port_start, port_end)) = parse_wg_config(&config_content) {
            update_provider_tunnel_config(&args.interface, &public_ip, port_start, port_end)?;
        }
        return Ok(());
    }

    // 1. Ensure WireGuard is installed
    print!("  Checking WireGuard installation... ");
    let wg_check = Command::new("which").arg("wg-quick").output();
    match wg_check {
        Ok(o) if o.status.success() => {
            println!("{}", "OK".green());
        }
        _ => {
            println!("{}", "not found, installing...".yellow());
            let mut cmd_args: Vec<&str> = sudo.to_vec();
            cmd_args.extend_from_slice(&[
                "apt-get",
                "install",
                "-y",
                "wireguard",
                "wireguard-tools",
            ]);
            let prog = cmd_args.remove(0);
            let install = Command::new(prog)
                .args(&cmd_args)
                .env("DEBIAN_FRONTEND", "noninteractive")
                .output();
            match install {
                Ok(o) if o.status.success() => {
                    println!("  {} WireGuard installed", "V".green());
                }
                _ => {
                    return Err(anyhow::anyhow!(
                        "Failed to install WireGuard. Install manually: sudo apt install wireguard wireguard-tools"
                    ));
                }
            }
        }
    }

    // 2. Download WireGuard config from VPN service
    print!("  Requesting VPN config from {}... ", args.vpn_url);
    let client = reqwest::Client::new();
    let version = env!("CARGO_PKG_VERSION");
    let response = client
        .get(&args.vpn_url)
        .header("Authorization", format!("Cashu {}", args.token))
        .header("User-Agent", format!("Paygress-CLI/{}", version))
        .send()
        .await?;

    if !response.status().is_success() {
        println!("{}", "FAILED".red());
        return Err(anyhow::anyhow!(
            "VPN service returned {}: {}",
            response.status(),
            response.text().await.unwrap_or_default()
        ));
    }

    let wg_config = response.text().await?;
    println!("{}", "OK".green());

    // 3. Validate config
    if !wg_config.contains("[Interface]") {
        println!(
            "  {} Received invalid config (no [Interface] section)",
            "X".red()
        );
        return Err(anyhow::anyhow!(
            "Invalid WireGuard config received from VPN service"
        ));
    }
    println!("  {} Config validated", "V".green());

    // 4. Save config (use sudo tee to write to /etc/wireguard)
    if need_sudo {
        let mut mkdir = Command::new("sudo")
            .args(["mkdir", "-p", "/etc/wireguard"])
            .spawn()?;
        mkdir.wait()?;

        // Write config via sudo tee
        let mut tee = Command::new("sudo")
            .args(["tee", &wg_conf_path])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::null())
            .spawn()?;
        if let Some(ref mut stdin) = tee.stdin {
            use std::io::Write;
            stdin.write_all(wg_config.as_bytes())?;
        }
        tee.wait()?;

        Command::new("sudo")
            .args(["chmod", "600", &wg_conf_path])
            .output()?;
    } else {
        std::fs::create_dir_all("/etc/wireguard")?;
        std::fs::write(&wg_conf_path, &wg_config)?;
        Command::new("chmod")
            .args(["600", &wg_conf_path])
            .output()?;
    }
    println!("  {} Saved to {}", "V".green(), wg_conf_path);

    // 5. Parse tunnel details
    let (public_ip, port_start, port_end) = parse_wg_config(&wg_config)
        .ok_or_else(|| anyhow::anyhow!("Could not extract tunnel IP from WireGuard config"))?;

    println!("  {} Tunnel public IP: {}", "V".green(), public_ip.cyan());
    if let (Some(ps), Some(pe)) = (port_start, port_end) {
        println!("  {} Port range: {}-{}", "V".green(), ps, pe);
    }

    // 6. Start WireGuard interface
    print!("  Starting WireGuard interface {}... ", args.interface);
    let mut wg_args: Vec<&str> = sudo.to_vec();
    wg_args.extend_from_slice(&["wg-quick", "up", &args.interface]);
    let prog = wg_args.remove(0);
    let output = Command::new(prog).args(&wg_args).output()?;

    if output.status.success() {
        println!("{}", "UP".green());
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr);
        if stderr.contains("already exists") {
            println!("{}", "ALREADY UP".yellow());
        } else {
            println!("{}", "FAILED".red());
            println!("  {}", stderr.trim());
            return Err(anyhow::anyhow!("Failed to start WireGuard interface"));
        }
    }

    // 7. Enable on boot
    if need_sudo {
        let _ = Command::new("sudo")
            .args([
                "systemctl",
                "enable",
                &format!("wg-quick@{}", args.interface),
            ])
            .output();
    } else {
        let _ = Command::new("systemctl")
            .args(["enable", &format!("wg-quick@{}", args.interface)])
            .output();
    }
    println!("  {} Enabled on boot", "V".green());

    // 8. Update provider config
    update_provider_tunnel_config(&args.interface, &public_ip, port_start, port_end)?;

    println!();
    println!("{}", "━".repeat(50).blue());
    println!("{}", "Tunnel Active!".green().bold());
    println!();
    println!("  Public IP:  {}", public_ip.cyan());
    println!("  Interface:  {}", args.interface);
    if let (Some(ps), Some(pe)) = (port_start, port_end) {
        println!("  Port range: {}-{}", ps, pe);
    }
    println!();
    println!("  Your provider will now be reachable through the VPN tunnel.");
    println!(
        "  Restart the provider service to apply: {} provider start",
        "paygress-cli".cyan()
    );

    Ok(())
}

/// Parse WireGuard config to extract public IP and port range.
/// Returns (public_ip, optional_port_start, optional_port_end)
fn parse_wg_config(config: &str) -> Option<(String, Option<u16>, Option<u16>)> {
    // Extract public IP from Endpoint field (e.g., "Endpoint = 1.2.3.4:51820")
    let public_ip = config
        .lines()
        .find(|l| l.trim().starts_with("Endpoint"))
        .and_then(|l| l.split('=').nth(1))
        .map(|v| v.trim().split(':').next().unwrap_or("").to_string())
        .filter(|s| !s.is_empty())?;

    // Try to extract port range from comments (e.g., "# Public Ports: 1.2.3.4:11000-11999")
    let (port_start, port_end) = config
        .lines()
        .find(|l| l.contains("Public Ports:") || l.contains("Port Range:"))
        .and_then(|l| {
            // Extract "11000-11999" from the line
            let re_part = l.split(':').last()?;
            let range_str = re_part.trim().split(':').last()?.trim();
            let mut parts = range_str.split('-');
            let start: u16 = parts.next()?.trim().parse().ok()?;
            let end: u16 = parts.next()?.trim().parse().ok()?;
            Some((Some(start), Some(end)))
        })
        .unwrap_or((None, None));

    Some((public_ip, port_start, port_end))
}

/// Update provider config with tunnel settings
fn update_provider_tunnel_config(
    interface: &str,
    public_ip: &str,
    port_start: Option<u16>,
    port_end: Option<u16>,
) -> Result<()> {
    match load_config(CONFIG_PATH) {
        Ok(mut config) => {
            config.tunnel_enabled = true;
            config.tunnel_interface = Some(interface.to_string());
            config.public_ip = public_ip.to_string();
            config.ssh_port_start = port_start;
            config.ssh_port_end = port_end;
            save_config(CONFIG_PATH, &config)?;
            println!(
                "  {} Provider config updated (public_ip={}, tunnel=enabled)",
                "✓".green(),
                public_ip
            );
        }
        Err(_) => {
            println!(
                "  {} No provider config found at {}. Run 'provider setup' first.",
                "⚠".yellow(),
                CONFIG_PATH
            );
            println!("  Tunnel is active but provider config not updated.");
        }
    }
    Ok(())
}

#[cfg(test)]
mod setup_multi_tests {
    use super::*;
    use paygress::nostr::PodSpec;

    fn args(count: usize) -> SetupMultiArgs {
        SetupMultiArgs {
            count,
            backend: paygress::provider::BackendType::Docker,
            name: "test".to_string(),
            mints: "http://localhost:3338".to_string(),
            public_ip: Some("203.0.113.1".to_string()),
            no_systemd: true,
        }
    }

    fn empty_specs() -> Vec<PodSpec> {
        vec![]
    }

    #[test]
    fn vmid_ranges_do_not_overlap() {
        let a = args(5);
        let mut ranges: Vec<(u32, u32)> = Vec::new();
        for i in 0..5 {
            let cfg = build_multi_config(
                &a,
                i,
                "203.0.113.1",
                "nsec1placeholder".to_string(),
                empty_specs(),
                vec![],
            );
            ranges.push((cfg.vmid_range_start, cfg.vmid_range_end));
        }
        // No overlap between any pair: either a is fully below b
        // or b is fully below a.
        for (i, (a_lo, a_hi)) in ranges.iter().enumerate() {
            for (j, (b_lo, b_hi)) in ranges.iter().enumerate() {
                if i == j {
                    continue;
                }
                assert!(
                    a_hi < b_lo || b_hi < a_lo,
                    "vmid ranges {} and {} overlap: ({},{}) vs ({},{})",
                    i,
                    j,
                    a_lo,
                    a_hi,
                    b_lo,
                    b_hi
                );
            }
        }
    }

    #[test]
    fn redb_paths_are_unique_per_instance() {
        let a = args(3);
        let paths: Vec<String> = (0..3)
            .map(|i| {
                build_multi_config(
                    &a,
                    i,
                    "203.0.113.1",
                    "nsec1placeholder".to_string(),
                    empty_specs(),
                    vec![],
                )
                .cashu_wallet_db_path
            })
            .collect();
        let unique: std::collections::HashSet<_> = paths.iter().collect();
        assert_eq!(
            paths.len(),
            unique.len(),
            "redb paths must be unique per instance: {:?}",
            paths
        );
    }

    #[test]
    fn config_path_is_filesystem_safe() {
        // No whitespace, no dots in the prefix-name slot. The
        // name="test" + i=2 should produce a clean path.
        let path = config_path_for("test", 2);
        assert_eq!(path, "/etc/paygress/provider-test-2.json");
    }

    #[test]
    fn provider_names_carry_the_index() {
        let a = args(3);
        let names: Vec<String> = (0..3)
            .map(|i| {
                build_multi_config(
                    &a,
                    i,
                    "203.0.113.1",
                    "nsec1placeholder".to_string(),
                    empty_specs(),
                    vec![],
                )
                .provider_name
            })
            .collect();
        assert_eq!(names, vec!["test-0", "test-1", "test-2"]);
    }
}
