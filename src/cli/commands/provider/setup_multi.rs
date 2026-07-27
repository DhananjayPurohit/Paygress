// `provider setup-multi` — scaffold N independent providers on one host.
// Each gets its own nsec, a non-overlapping vmid range and its own wallet
// sqlite; warm-standby failover testing needs several distinct providers
// and keeping those disjoint by hand is error-prone.

use anyhow::Result;
use clap::Args;
use colored::Colorize;
use nostr_sdk::ToBech32;

use super::{detect_public_ip, parse_backend, rule};
use crate::util::split_csv;
use paygress::nostr::PodSpec;
use paygress::provider::{save_config, BackendType, ProviderConfig};

/// Per-instance vmid headroom (1000-1999, 2000-2999, ...). A constant
/// so the test below can pin the partition geometry.
const VMID_RANGE_SIZE: u32 = 1000;

#[derive(Args)]
pub struct SetupMultiArgs {
    /// Number of providers to scaffold
    #[arg(long, default_value_t = 3)]
    pub count: usize,

    /// Compute backend shared by all N providers
    #[arg(long, default_value = "docker", value_parser = parse_backend)]
    pub backend: BackendType,

    /// Common prefix for the providers' names and filenames; each gets "<name>-<i>"
    #[arg(long, default_value = "paygress")]
    pub name: String,

    /// Whitelisted Cashu mints (comma-separated), applied to every provider
    #[arg(long, default_value = "https://testnut.cashu.space")]
    pub mints: String,

    /// Public IP address (auto-detected if not provided)
    #[arg(long)]
    pub public_ip: Option<String>,

    /// Skip the systemd template-unit instructions section
    #[arg(long)]
    pub no_systemd: bool,

    /// Lightning address (user@domain.com) applied to every instance
    #[arg(long)]
    pub lightning_address: Option<String>,
}

pub(super) async fn execute_setup_multi(args: SetupMultiArgs, _verbose: bool) -> Result<()> {
    println!("{}", "🔧 Paygress Multi-Provider Setup".blue().bold());
    rule();
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

    // One IP for all instances: they share the host.
    let public_ip = match args.public_ip.as_deref() {
        Some(ip) => ip.to_string(),
        None => {
            println!("  {} Auto-detecting public IP...", "⚙".yellow());
            detect_public_ip()
                .await
                .unwrap_or_else(|| "127.0.0.1".to_string())
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
    let mints = split_csv(&args.mints);

    println!();
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
    }

    if !args.no_systemd {
        print_systemd_template(&args);
    }

    println!();
    rule();
    println!("{}", "🎉 Multi-Provider Setup Complete".green().bold());
    println!();
    println!("Verify with: {} list", "paygress-cli".cyan());
    println!(
        "(after starting the services, all {} should appear with distinct npubs)",
        args.count
    );

    Ok(())
}

/// Printed, not installed — the operator decides where it lands.
fn print_systemd_template(args: &SetupMultiArgs) {
    println!();
    rule();
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

/// Pure — no IO, no clock — so the partition logic is unit-testable.
fn build_multi_config(
    args: &SetupMultiArgs,
    i: usize,
    public_ip: &str,
    nostr_nsec: String,
    specs: Vec<PodSpec>,
    mints: Vec<String>,
) -> ProviderConfig {
    let provider_name = format!("{}-{}", args.name, i);
    let vmid_start = 1000 + i as u32 * VMID_RANGE_SIZE;
    let vmid_end = vmid_start + VMID_RANGE_SIZE - 1;
    ProviderConfig {
        backend_type: args.backend,
        public_ip: public_ip.to_string(),
        // setup-multi targets KVM/Docker/LXD, so Proxmox-via-API
        // fields stay empty.
        proxmox_url: String::new(),
        proxmox_token_id: String::new(),
        proxmox_token_secret: String::new(),
        proxmox_node: "pve".to_string(),
        proxmox_accept_invalid_certs: false,
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
        // Own SQLite db per provider: cdk's per-process write lock
        // would otherwise serialize all redemptions.
        cashu_wallet_db_path: format!("./paygress-{}.sqlite", provider_name),
        workload_state_path: format!("./paygress-{}-workloads.json", provider_name),
        standby_state_path: format!("./paygress-{}-standby-slots.json", provider_name),
        http_bind_addr: None,
        lightning_address: args.lightning_address.clone(),
        kvm_base_image_path: None,
        kvm_base_image_url: None,
    }
}

fn config_path_for(name: &str, i: usize) -> String {
    format!("/etc/paygress/provider-{}-{}.json", name, i)
}

#[cfg(test)]
mod setup_multi_tests {
    use super::*;

    fn args(count: usize) -> SetupMultiArgs {
        SetupMultiArgs {
            count,
            backend: BackendType::Docker,
            name: "test".to_string(),
            mints: "https://testnut.cashu.space".to_string(),
            public_ip: Some("203.0.113.1".to_string()),
            no_systemd: true,
            lightning_address: None,
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
    fn sqlite_paths_are_unique_per_instance() {
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
            "sqlite paths must be unique per instance: {:?}",
            paths
        );
    }

    #[test]
    fn config_path_is_filesystem_safe() {
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
