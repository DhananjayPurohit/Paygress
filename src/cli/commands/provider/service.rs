// Running provider lifecycle: start, stop, status, and config inspection.

use anyhow::Result;
use clap::Args;
use colored::Colorize;
use std::process::Command;

use super::{rule, CONFIG_PATH};
use paygress::provider::{load_config, BackendType, ProviderService};

#[derive(Args)]
pub struct StartArgs {
    /// Path to configuration file
    #[arg(long, default_value = "/etc/paygress/provider-config.json")]
    pub config: String,
}

#[derive(Args)]
pub struct ConfigArgs {
    /// Show current configuration
    #[arg(long)]
    pub show: bool,
}

pub(super) async fn execute_start(args: StartArgs, _verbose: bool) -> Result<()> {
    println!("{}", "🚀 Starting Paygress Provider".blue().bold());
    println!();

    let config = load_config(&args.config)?;

    println!("  Provider: {}", config.provider_name.yellow());

    match config.backend_type {
        BackendType::Proxmox => {
            println!("  Backend:  Proxmox");
            println!("  URL:      {}", config.proxmox_url);
            println!("  Node:     {}", config.proxmox_node);
        }
        BackendType::LXD => {
            println!("  Backend:  LXD");
            println!("  Storage:  {}", config.proxmox_storage); // reused as the pool name
        }
        BackendType::Docker => {
            println!("  Backend:  Docker");
            println!("  Note:     templates require Docker; ensure `docker` is on PATH");
        }
        BackendType::Kvm => {
            println!("  Backend:  KVM/qemu (per-VM isolation, dedicated-host tier)");
            println!(
                "  Note:     requires /dev/kvm + qemu-system-x86_64; killer templates not served"
            );
        }
    }
    println!();

    let lightning_address = config.lightning_address.clone();
    let service = ProviderService::new(config).await?;

    println!("  NPUB: {}", service.get_npub().cyan());
    if let Some(ref ln) = lightning_address {
        println!("  ⚡ Lightning sweep: {}", ln.cyan());
    }
    println!();
    println!("{}", "Provider is now live! Press Ctrl+C to stop.".green());
    rule();
    println!();

    service.run().await?;

    Ok(())
}

pub(super) async fn execute_stop(_verbose: bool) -> Result<()> {
    println!("{}", "Stopping provider service...".yellow());

    // Bootstrapped providers run under systemd.
    let stopped = Command::new("systemctl")
        .args(["stop", "paygress-provider"])
        .output();
    if matches!(&stopped, Ok(o) if o.status.success()) {
        println!("{}", "Provider stopped via systemctl.".green());
        return Ok(());
    }

    // Otherwise find and kill the process directly.
    let pgrep = Command::new("pgrep")
        .args(["-f", "paygress-cli provider start"])
        .output();
    if let Ok(o) = pgrep {
        if o.status.success() {
            let pids = String::from_utf8_lossy(&o.stdout);
            for pid in pids.trim().lines() {
                let _ = Command::new("kill").arg(pid.trim()).output();
            }
            println!("{}", "Provider stopped.".green());
            return Ok(());
        }
    }

    println!("{}", "No running provider found.".yellow());
    Ok(())
}

pub(super) async fn execute_status(_verbose: bool) -> Result<()> {
    println!("{}", "📊 Provider Status".blue().bold());
    rule();

    let Ok(config) = load_config(CONFIG_PATH) else {
        println!();
        println!("  {} No configuration found.", "⚠".yellow());
        println!("  Run 'paygress-cli provider setup' first.");
        println!();
        return Ok(());
    };

    println!();
    println!("  Provider Name:  {}", config.provider_name.yellow());
    println!(
        "  Location:       {}",
        config.provider_location.as_deref().unwrap_or("Not set")
    );
    println!("  Proxmox URL:    {}", config.proxmox_url);
    println!("  Node:           {}", config.proxmox_node);
    println!();
    println!("  📦 Tiers configured:");
    for spec in &config.specs {
        println!("    • {} - {} msat/sec", spec.name, spec.rate_msats_per_sec);
    }
    println!();
    println!("  💰 Accepted mints:");
    for mint in &config.whitelisted_mints {
        println!("    • {}", mint);
    }
    println!();
    println!(
        "  ⚡ Lightning sweep: {}",
        config
            .lightning_address
            .as_deref()
            .unwrap_or("(not configured)")
            .cyan()
    );

    if config.tunnel_enabled {
        let iface = config.tunnel_interface.as_deref().unwrap_or("wg0");
        println!();
        println!("  🔒 Tunnel:");
        println!("    Interface: {}", iface);
        println!("    Public IP: {}", config.public_ip);
        if let (Some(ps), Some(pe)) = (config.ssh_port_start, config.ssh_port_end) {
            println!("    Port range: {}-{}", ps, pe);
        }
        let wg_up = Command::new("wg").args(["show", iface]).output();
        if matches!(&wg_up, Ok(o) if o.status.success()) {
            println!("    Status: {}", "UP".green());
        } else {
            println!("    Status: {}", "DOWN".red());
        }
    }

    println!();
    Ok(())
}

pub(super) async fn execute_config(args: ConfigArgs, _verbose: bool) -> Result<()> {
    if args.show {
        let config = load_config(CONFIG_PATH)?;
        println!("{}", serde_json::to_string_pretty(&config)?);
    }

    Ok(())
}
