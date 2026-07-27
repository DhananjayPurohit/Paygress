// `paygress-cli bootstrap` — one-click setup of a fresh VPS as a
// Paygress provider: compute backend, CLI binary, provider config,
// systemd unit, and (optionally) the ngx_l402 Lightning sweep.

mod backend;
mod config;
mod install;
mod ngx_l402;
mod ssh;

use anyhow::Result;
use clap::Args;
use colored::Colorize;

#[derive(Args)]
pub struct BootstrapArgs {
    /// Target server IP or hostname
    #[arg(long)]
    pub host: String,

    /// SSH user (must have sudo privileges)
    #[arg(long, default_value = "root")]
    pub user: String,

    /// SSH password (use --key for key-based auth)
    #[arg(long)]
    pub password: Option<String>,

    /// SSH private key path
    #[arg(long)]
    pub key: Option<String>,

    /// SSH port
    #[arg(long, default_value = "22")]
    pub port: u16,

    /// Location description (e.g., "US-East", "Germany")
    #[arg(long)]
    pub location: Option<String>,

    /// Nostr private key (nsec format, auto-generated if not provided)
    #[arg(long)]
    pub nostr_key: Option<String>,

    /// Whitelisted Cashu mints (comma-separated)
    #[arg(long, default_value = "https://testnut.cashu.space")]
    pub mints: String,

    /// Lightning address (user@domain.com) that earned ecash is swept to
    #[arg(long)]
    pub lightning_address: Option<String>,

    /// Skip Proxmox installation (assumes already installed)
    #[arg(long)]
    pub skip_proxmox: bool,

    /// Dry run - show commands without executing
    #[arg(long)]
    pub dry_run: bool,

    /// Install WireGuard for tunnel support (for machines behind NAT)
    #[arg(long)]
    pub tunnel: bool,

    /// Path to a locally built paygress-cli binary to copy instead of pulling from crates.io
    #[arg(long)]
    pub local_binary: Option<String>,

    /// How often ngx_l402 sweeps accumulated ecash to Lightning (seconds)
    #[arg(long, default_value = "3600")]
    pub sweep_interval_secs: u64,

    /// Minimum wallet balance in sats before ngx_l402 attempts a sweep
    #[arg(long, default_value = "100")]
    pub sweep_min_balance_sats: u64,

    /// ngx_l402 ROOT_KEY for L402 macaroon signing (32-byte hex, auto-generated if not provided)
    #[arg(long)]
    pub root_key: Option<String>,
}

impl BootstrapArgs {
    fn is_root(&self) -> bool {
        self.user == "root"
    }

    /// Command prefix that elevates a remote command, empty when the SSH
    /// user is already root.
    fn sudo(&self) -> &'static str {
        if self.is_root() {
            ""
        } else {
            "sudo "
        }
    }
}

pub async fn execute(args: BootstrapArgs, verbose: bool) -> Result<()> {
    print_header(&args);

    ssh::step_ssh_connection(&args)?;
    ssh::step_passwordless_sudo(&args)?;
    let use_lxd = backend::step_install_backend(&args)?;
    backend::step_api_token(&args, use_lxd, verbose)?;
    install::step_install_cli(&args)?;

    let (nostr_key, provider_name) = config::step_nostr_identity(&args)?;
    config::step_write_config(&args, &nostr_key, &provider_name, use_lxd)?;
    config::step_systemd_service(&args)?;
    ngx_l402::step_ngx_l402(&args, &nostr_key).await?;
    config::step_start_service(&args, use_lxd)?;

    print_summary(&args, &provider_name, use_lxd);

    if !args.is_root() && !args.dry_run {
        let _ = ssh::run_ssh_command(&args, "sudo rm -f /etc/sudoers.d/paygress-bootstrap");
        println!("  {} Temporary sudo rule removed", "✓".green());
    }
    if !args.dry_run {
        ssh::close_ssh_master(&args);
    }

    Ok(())
}

/// Banner shared by every numbered step.
fn step_banner(title: &str) {
    println!("{}", title.blue().bold());
    println!("{}", "─".repeat(50));
}

fn print_header(args: &BootstrapArgs) {
    println!(
        "{}",
        "╔════════════════════════════════════════════════════════════╗".blue()
    );
    println!(
        "{}",
        "║              🚀 PAYGRESS BOOTSTRAP                         ║".blue()
    );
    println!(
        "{}",
        "║     One-Click Proxmox + Provider Setup                     ║".blue()
    );
    println!(
        "{}",
        "╚════════════════════════════════════════════════════════════╝".blue()
    );
    println!();

    if args.dry_run {
        println!(
            "{}",
            "🔍 DRY RUN MODE - Commands will be shown but not executed".yellow()
        );
        println!();
    }

    println!("Target: {}", format!("{}@{}", args.user, args.host).cyan());
    println!("Name:   {} (derived from Nostr key)", "auto".dimmed());
    if let Some(ref loc) = args.location {
        println!("Location: {}", loc);
    }
    println!();
}

fn print_summary(args: &BootstrapArgs, provider_name: &str, use_lxd: bool) {
    println!("{}", "═".repeat(60).green());
    println!("{}", "🎉 BOOTSTRAP COMPLETE!".green().bold());
    println!("{}", "═".repeat(60).green());
    println!();
    println!("  Provider Name: {}", provider_name.yellow());
    println!("  Server:        {}", args.host.cyan());

    if let Some(ref ln) = args.lightning_address {
        println!("  Lightning:     {} ⚡", ln.cyan());
        println!(
            "  Sweep every:   {}s (min {} sats)",
            args.sweep_interval_secs, args.sweep_min_balance_sats
        );
        println!("  ngx_l402:      running on port 80 🟢");
    }
    println!("  Wallet DB:     /var/lib/paygress/cashu-wallet.sqlite");
    println!("  Config:        /etc/paygress/provider-config.json");

    if !use_lxd {
        println!("  Proxmox UI:    https://{}:8006", args.host);
        println!();
        println!("  📋 Next Steps:");
        println!("    1. SSH into {} and get your API token", args.host);
        println!("    2. Update the config with the token secret");
        println!("    3. Start the service: systemctl start paygress-provider");
    } else {
        println!("  Backend:       LXD (Native)");
        println!("  Provider:      Running 🟢");
    }

    println!();
    println!("  Users can discover you with:");
    println!("    {} list", "paygress-cli".cyan());
    println!();
}
