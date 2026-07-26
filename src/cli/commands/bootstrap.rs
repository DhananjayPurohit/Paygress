// `paygress-cli bootstrap` — one-click setup of a fresh VPS as a
// Paygress provider: compute backend, CLI binary, provider config,
// systemd unit, and (optionally) the ngx_l402 Lightning sweep.

use anyhow::{Context, Result};
use clap::Args;
use colored::Colorize;
use nostr_sdk::ToBech32;
use std::io::Write;
use std::process::{Command, Stdio};
use std::time::Duration;

use crate::util::split_csv;

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

    /// Lightning address (user@domain.com) that earned ecash is swept
    /// to. Every redeemed Cashu token is melted to this address.
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

    /// Path to a locally built paygress-cli binary to copy instead of
    /// pulling from crates.io. Build it with `cargo build --release`.
    #[arg(long)]
    pub local_binary: Option<String>,

    /// How often ngx_l402 sweeps accumulated ecash to Lightning (seconds)
    #[arg(long, default_value = "3600")]
    pub sweep_interval_secs: u64,

    /// Minimum wallet balance in sats before ngx_l402 attempts a sweep
    #[arg(long, default_value = "100")]
    pub sweep_min_balance_sats: u64,

    /// ngx_l402 ROOT_KEY for L402 macaroon signing (32-byte hex).
    /// Auto-generated if not provided.
    #[arg(long)]
    pub root_key: Option<String>,
}

/// Resolve a Lightning Address's LNURL-pay well-known URL
/// (`https://domain/.well-known/lnurlp/user`) and confirm it answers
/// with `"tag": "payRequest"`, so a typo fails here rather than as a
/// silently broken sweep loop.
async fn validate_lightning_address(address: &str) -> Result<()> {
    let (user, domain) = address.split_once('@').with_context(|| {
        format!(
            "'{}' is not a valid Lightning Address — expected user@domain.com",
            address
        )
    })?;

    if user.is_empty() || domain.is_empty() {
        anyhow::bail!(
            "'{}' is not a valid Lightning Address — user and domain must not be empty",
            address
        );
    }

    let url = format!("https://{}/.well-known/lnurlp/{}", domain, user);

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .context("failed to build HTTP client for Lightning Address validation")?;

    let resp = client.get(&url).send().await.with_context(|| {
        format!(
            "could not reach {} — check domain and internet connectivity",
            url
        )
    })?;

    let status = resp.status();
    if !status.is_success() {
        anyhow::bail!(
            "Lightning Address endpoint returned HTTP {} for {}\n    \
             Check that '{}' is registered with that provider.",
            status,
            url,
            address
        );
    }

    let json: serde_json::Value = resp
        .json()
        .await
        .with_context(|| format!("{} did not return valid JSON", url))?;

    match json.get("tag").and_then(|t| t.as_str()) {
        Some("payRequest") => {}
        Some(other) => anyhow::bail!(
            "unexpected LNURL tag '{}' at {} — expected 'payRequest'",
            other,
            url
        ),
        None => anyhow::bail!(
            "LNURL response from {} is missing the 'tag' field:\n    {}",
            url,
            json
        ),
    }

    match (
        json.get("minSendable").and_then(|v| v.as_u64()),
        json.get("maxSendable").and_then(|v| v.as_u64()),
    ) {
        (Some(min), Some(max)) => {
            println!(" {} (sendable: {} – {} msats)", "✓".green(), min, max)
        }
        _ => println!(" {}", "✓".green()),
    }

    Ok(())
}

pub async fn execute(args: BootstrapArgs, verbose: bool) -> Result<()> {
    print_header(&args);

    let is_root = args.user == "root";
    let sudo = if is_root { "" } else { "sudo " };

    step_ssh_connection(&args)?;
    step_passwordless_sudo(&args, is_root)?;
    let use_lxd = step_install_backend(&args, sudo)?;
    step_api_token(&args, sudo, use_lxd, verbose)?;
    step_install_cli(&args, sudo)?;

    let (nostr_key, provider_name) = step_nostr_identity(&args)?;
    step_write_config(&args, &nostr_key, &provider_name, is_root, sudo, use_lxd)?;
    step_systemd_service(&args, is_root, sudo)?;
    step_ngx_l402(&args, &nostr_key, is_root, sudo).await?;
    step_start_service(&args, sudo, use_lxd)?;

    print_summary(&args, &provider_name, use_lxd);

    if !is_root && !args.dry_run {
        let _ = run_ssh_command(&args, "sudo rm -f /etc/sudoers.d/paygress-bootstrap");
        println!("  {} Temporary sudo rule removed", "✓".green());
    }
    if !args.dry_run {
        close_ssh_master(&args);
    }

    Ok(())
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

fn step_ssh_connection(args: &BootstrapArgs) -> Result<()> {
    println!("{}", "Step 1: Testing SSH Connection".blue().bold());
    println!("{}", "─".repeat(50));

    if args.dry_run {
        println!("  Would connect to {}", args.host.cyan());
    } else {
        print!("  Connecting to {}... ", args.host);
        std::io::stdout().flush()?;

        // The ControlMaster authenticates once; every later SSH call
        // reuses this socket.
        open_ssh_master(args)?;

        if !run_ssh_command(args, "echo 'Connected'")? {
            println!("{}", "FAILED".red());
            close_ssh_master(args);
            return Err(anyhow::anyhow!("SSH connection failed"));
        }
        println!("{}", "OK".green());
    }
    println!();
    Ok(())
}

/// Grant passwordless sudo for the rest of the session, so the user is
/// prompted once here instead of on every subsequent SSH call. Removed
/// again at the end of `execute`.
fn step_passwordless_sudo(args: &BootstrapArgs, is_root: bool) -> Result<()> {
    if is_root || args.dry_run {
        return Ok(());
    }

    println!(
        "{}",
        "Configuring passwordless sudo for bootstrap session...".yellow()
    );
    let grant_cmd = format!(
        "echo '{} ALL=(ALL) NOPASSWD: ALL' | sudo tee /etc/sudoers.d/paygress-bootstrap > /dev/null && echo 'GRANTED'",
        args.user
    );
    if !run_ssh_command(args, &grant_cmd)? {
        return Err(anyhow::anyhow!(
            "Failed to configure passwordless sudo. Check that your user has sudo privileges."
        ));
    }
    println!(
        "  {} sudo escalation configured (will be removed at end)",
        "✓".green()
    );
    println!();
    Ok(())
}

/// Detect the remote OS and install the matching compute backend.
/// Returns true when the LXD path was taken.
fn step_install_backend(args: &BootstrapArgs, sudo: &str) -> Result<bool> {
    println!(
        "{}",
        "Step 2: Checking OS & Installing Backend".blue().bold()
    );
    println!("{}", "─".repeat(50));

    let os_id = if args.dry_run {
        println!("  Would detect OS (assuming debian for dry-run)");
        "debian".to_string()
    } else {
        run_ssh_command_output(
            args,
            "cat /etc/os-release | grep ^ID= | cut -d= -f2 | tr -d '\"'",
        )?
        .trim()
        .to_string()
    };

    println!("  Detected OS: {}", os_id.cyan());

    let use_lxd = os_id == "ubuntu";
    if use_lxd {
        install_lxd(args, sudo)?;
    } else if !args.skip_proxmox {
        install_proxmox(args, sudo, &os_id)?;
    } else {
        println!("  Skipping Proxmox installation (--skip-proxmox)");
    }
    println!();
    Ok(use_lxd)
}

fn install_lxd(args: &BootstrapArgs, sudo: &str) -> Result<()> {
    println!(
        "{}",
        "  -> Installing LXD backend (Ubuntu detected)".green()
    );

    if args.dry_run {
        println!("  Would run: snap install lxd && lxd init --auto");
        return Ok(());
    }

    let check = run_ssh_command_output(
        args,
        "which lxd >/dev/null 2>&1 && echo 'installed' || echo 'not_installed'",
    )?;
    if check.trim() == "installed" {
        println!("  LXD is already installed.");
    } else {
        println!("  Installing LXD...");
        run_ssh_command(
            args,
            &format!("{}snap install lxd && {}lxd init --auto", sudo, sudo),
        )?;
        println!("  LXD installed and initialized!");
    }

    // `lxd init --auto` may not create a pool, and a pre-installed LXD
    // may have none.
    let pool_check = run_ssh_command_output(
        args,
        &format!("{}lxc storage list --format csv 2>/dev/null | wc -l", sudo),
    )?;
    if pool_check.trim() == "0" {
        println!("  Creating default storage pool...");
        run_ssh_command(args, &format!("{}lxc storage create default dir", sudo))?;
        println!("  Default storage pool created!");
    } else {
        println!("  Storage pool already exists.");
    }

    let net_check = run_ssh_command_output(
        args,
        &format!(
            "{}lxc network list --format csv 2>/dev/null | grep -c lxdbr0 || true",
            sudo
        ),
    )?;
    if net_check.trim() == "0" {
        println!("  Creating default network bridge (lxdbr0)...");
        run_ssh_command(args, &format!("{}lxc network create lxdbr0", sudo))?;
        println!("  Network bridge created!");
    } else {
        println!("  Network bridge already exists.");
    }

    // The pool/bridge can exist while the profile still has `devices: {}`.
    let profile_devices = run_ssh_command_output(
        args,
        &format!(
            "{}lxc profile show default 2>/dev/null | grep -c 'root:' || true",
            sudo
        ),
    )?;
    if profile_devices.trim() == "0" {
        println!("  Configuring default profile with storage and network...");
        run_ssh_command(
            args,
            &format!(
                "{}lxc profile device add default root disk path=/ pool=default",
                sudo
            ),
        )?;
        run_ssh_command(
            args,
            &format!("{}lxc network attach-profile lxdbr0 default eth0", sudo),
        )?;
        println!("  Default profile configured!");
    } else {
        println!("  Default profile already configured.");
    }

    Ok(())
}

fn install_proxmox(args: &BootstrapArgs, sudo: &str, os_id: &str) -> Result<()> {
    println!(
        "{}",
        "  -> Installing Proxmox backend (Debian assumed)".green()
    );

    if os_id != "debian" && !args.dry_run {
        println!(
            "{}",
            format!(
                "⚠️  Warning: OS is not Debian (detected: {}). Proxmox install may fail.",
                os_id
            )
            .yellow()
        );
    }

    let proxmox_check = "which pvesh >/dev/null 2>&1 && echo 'installed' || echo 'not_installed'";

    if args.dry_run {
        println!("  Would check: {}", proxmox_check.cyan());
        return Ok(());
    }

    print!("  Checking for existing Proxmox... ");
    std::io::stdout().flush()?;

    if run_ssh_command_output(args, proxmox_check)?.trim() == "installed" {
        println!("{}", "Already installed".green());
        return Ok(());
    }

    println!("{}", "Not found".yellow());
    println!();
    println!("  {} Installing Proxmox VE...", "⚙".yellow());
    println!("  ⏳ This may take 10-15 minutes");
    println!();

    let install_script = get_proxmox_install_script();
    let cmd = if sudo.is_empty() {
        install_script.to_string()
    } else {
        format!("sudo bash -c '{}'", install_script.replace('\'', "'\\''"))
    };
    run_ssh_command(args, &cmd)?;

    println!("  {} Proxmox VE installed!", "✓".green());
    Ok(())
}

fn step_api_token(args: &BootstrapArgs, sudo: &str, use_lxd: bool, verbose: bool) -> Result<()> {
    println!("{}", "Step 3: Creating Proxmox API Token".blue().bold());
    println!("{}", "─".repeat(50));

    const TOKEN_NAME: &str = "paygress";

    if use_lxd {
        println!("  Skipping Proxmox API token creation (LXD mode)");
    } else if args.dry_run {
        println!(
            "  Would run: {}",
            format!(
                "pveum user token add root@pam {} --privsep=0 2>/dev/null || pveum user token list root@pam 2>/dev/null | grep {}",
                TOKEN_NAME, TOKEN_NAME
            )
            .cyan()
        );
    } else {
        print!("  Creating API token... ");
        std::io::stdout().flush()?;

        let token_output = run_ssh_command_output(
            args,
            &format!(
                "{}pveum user token add root@pam {} --privsep=0 2>&1 || echo 'exists'",
                sudo, TOKEN_NAME
            ),
        )?;

        if token_output.contains("exists") || token_output.contains("already exists") {
            println!("{}", "Already exists".green());
        } else {
            println!("{}", "Created".green());
            if verbose {
                println!("    Token output: {}", token_output);
            }
        }
    }
    println!();
    Ok(())
}

fn step_install_cli(args: &BootstrapArgs, sudo: &str) -> Result<()> {
    println!("{}", "Step 4: Installing paygress-cli".blue().bold());
    println!("{}", "─".repeat(50));

    if args.dry_run {
        if args.local_binary.is_some() {
            println!("  Would scp local binary to remote and install to /usr/local/bin/");
        } else {
            println!("  Would run: cargo install paygress-cli");
        }
    } else if let Some(ref bin_path) = args.local_binary {
        install_cli_from_local_binary(args, sudo, bin_path)?;
    } else {
        install_cli_from_crates_io(args, sudo)?;
    }

    if !args.dry_run && args.tunnel {
        print!("  Installing WireGuard for tunnel support... ");
        std::io::stdout().flush()?;
        run_ssh_command(
            args,
            &format!(
                "export DEBIAN_FRONTEND=noninteractive && {}apt-get install -y wireguard wireguard-tools",
                sudo
            ),
        )?;
        println!("{}", "OK".green());
    }
    println!();
    Ok(())
}

fn install_cli_from_local_binary(args: &BootstrapArgs, sudo: &str, bin_path: &str) -> Result<()> {
    if !std::path::Path::new(bin_path).exists() {
        return Err(anyhow::anyhow!(
            "Local binary not found at '{}'. Build it first with: cargo build --release",
            bin_path
        ));
    }
    print!("  Copying local binary to {}... ", args.host);
    std::io::stdout().flush()?;

    // scp over the ControlMaster socket, so no re-authentication.
    let mut scp_args = vec![
        "-o".to_string(),
        "StrictHostKeyChecking=no".to_string(),
        "-o".to_string(),
        format!("ControlPath={}", control_path(&args.host, args.port)),
        "-P".to_string(),
        args.port.to_string(),
    ];
    if let Some(ref key) = args.key {
        scp_args.push("-i".to_string());
        scp_args.push(key.clone());
    }
    scp_args.push(bin_path.to_string());
    scp_args.push(format!("{}@{}:/tmp/paygress-cli", args.user, args.host));

    let scp_status = Command::new("scp")
        .args(&scp_args)
        .status()
        .context("Failed to run scp")?;
    if !scp_status.success() {
        return Err(anyhow::anyhow!(
            "scp failed — check SSH credentials and path"
        ));
    }

    // A running binary can't be overwritten ("Text file busy").
    let _ = run_ssh_command(
        args,
        &format!(
            "{}systemctl stop paygress-provider 2>/dev/null || true",
            sudo
        ),
    );

    if !run_ssh_command(
        args,
        &format!(
            "{}install -m 755 /tmp/paygress-cli /usr/local/bin/paygress-cli",
            sudo
        ),
    )? {
        return Err(anyhow::anyhow!("Failed to install binary on remote"));
    }
    println!("{}", "OK".green());
    Ok(())
}

fn install_cli_from_crates_io(args: &BootstrapArgs, sudo: &str) -> Result<()> {
    let install_cmd = format!(
        r#"
            set -e
            if ! command -v cargo &> /dev/null; then
                if [ -f "$HOME/.cargo/env" ]; then source "$HOME/.cargo/env"; fi
            fi
            if ! command -v cargo &> /dev/null; then
                echo "Installing Rust toolchain..."
                curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --default-toolchain stable
                source "$HOME/.cargo/env"
            fi
            if command -v apt-get &> /dev/null; then
                export DEBIAN_FRONTEND=noninteractive
                {0}apt-get update -q && {0}apt-get install -y build-essential pkg-config libssl-dev
            fi
            source "$HOME/.cargo/env" 2>/dev/null || true
            cargo install paygress-cli --force
            # Stop the running service before overwriting the binary
            {0}systemctl stop paygress-provider 2>/dev/null || true
            {0}cp "$HOME/.cargo/bin/paygress-cli" /usr/local/bin/paygress-cli
        "#,
        sudo
    );

    print!("  Installing paygress-cli from crates.io (this may take a few minutes)... ");
    std::io::stdout().flush()?;
    if !run_ssh_command(args, &install_cmd)? {
        return Err(anyhow::anyhow!("Failed to install paygress-cli"));
    }
    println!("{}", "OK".green());
    Ok(())
}

/// Resolve the provider's Nostr key and derive its display name. The
/// name comes from the pubkey, so it's the same on every re-run.
fn step_nostr_identity(args: &BootstrapArgs) -> Result<(String, String)> {
    println!("{}", "Step 5: Configuring Nostr".blue().bold());
    println!("{}", "─".repeat(50));

    let nostr_key = match args.nostr_key {
        Some(ref key) => {
            println!("  Using provided Nostr key");
            key.clone()
        }
        None => {
            print!("  Generating Nostr keypair... ");
            std::io::stdout().flush()?;

            let keys = nostr_sdk::Keys::generate();
            let nsec = keys
                .secret_key()
                .to_bech32()
                .map_err(|e| anyhow::anyhow!("Failed to encode key: {}", e))?;
            let npub = keys
                .public_key()
                .to_bech32()
                .map_err(|e| anyhow::anyhow!("Failed to encode public key: {}", e))?;

            println!("{}", "Done".green());
            println!("  NPUB: {}", npub.cyan());
            nsec
        }
    };

    let keys = nostr_sdk::Keys::parse(&nostr_key)
        .map_err(|e| anyhow::anyhow!("failed to parse nostr key for name derivation: {}", e))?;
    let provider_name = paygress::namegen::derive_provider_name(&keys.public_key().to_bytes());
    println!("  Provider name:  {}", provider_name.yellow().bold());
    println!();

    Ok((nostr_key, provider_name))
}

fn step_write_config(
    args: &BootstrapArgs,
    nostr_key: &str,
    provider_name: &str,
    is_root: bool,
    sudo: &str,
    use_lxd: bool,
) -> Result<()> {
    println!(
        "{}",
        "Step 6: Creating Provider Configuration".blue().bold()
    );
    println!("{}", "─".repeat(50));

    let config = render_provider_config(args, nostr_key, provider_name, use_lxd);

    if args.dry_run {
        println!("  Would create /etc/paygress/provider-config.json");
        println!("  Would create /var/lib/paygress/ (wallet db directory)");
    } else {
        let create_config = if is_root {
            format!(
                "mkdir -p /etc/paygress && mkdir -p /var/lib/paygress && cat > /etc/paygress/provider-config.json << 'EOF'\n{}\nEOF",
                config
            )
        } else {
            format!(
                "{}mkdir -p /etc/paygress && {}mkdir -p /var/lib/paygress && echo '{}' | {}tee /etc/paygress/provider-config.json > /dev/null",
                sudo, sudo, config.replace('\'', "'\\''"), sudo
            )
        };
        run_ssh_command(args, &create_config)?;
        println!(
            "  {} Created /etc/paygress/provider-config.json",
            "✓".green()
        );
        println!(
            "  {} Created /var/lib/paygress/ (wallet db directory)",
            "✓".green()
        );
    }
    println!();
    Ok(())
}

fn render_provider_config(
    args: &BootstrapArgs,
    nostr_key: &str,
    provider_name: &str,
    use_lxd: bool,
) -> String {
    let backend_type = if use_lxd { "LXD" } else { "Proxmox" };
    let proxmox_template = if use_lxd {
        "images:ubuntu/22.04"
    } else {
        "local:vztmpl/ubuntu-22.04-standard.tar.zst"
    };
    let storage = if use_lxd { "default" } else { "local-lvm" };
    let bridge = if use_lxd { "lxdbr0" } else { "vmbr0" };

    let mints_json = split_csv(&args.mints)
        .iter()
        .map(|m| format!("\"{}\"", m))
        .collect::<Vec<_>>()
        .join(", ");

    format!(
        r#"{{
  "backend_type": "{}",
  "proxmox_url": "https://127.0.0.1:8006/api2/json",
  "proxmox_token_id": "root@pam!paygress",
  "proxmox_token_secret": "REPLACE_WITH_TOKEN",
  "proxmox_node": "pve",
  "proxmox_storage": "{}",
  "proxmox_template": "{}",
  "proxmox_bridge": "{}",
  "vmid_range_start": 1000,
  "vmid_range_end": 1999,
  "nostr_private_key": "{}",
  "nostr_relays": ["wss://relay.damus.io", "wss://nos.lol"],
  "provider_name": "{}",
  "provider_location": {},
  "public_ip": "{}",
  "capabilities": ["lxc", "vm"],
  "specs": [
    {{"id": "basic", "name": "Basic", "description": "1 vCPU, 1GB RAM", "cpu_millicores": 1000, "memory_mb": 1024, "rate_msats_per_sec": 50}},
    {{"id": "standard", "name": "Standard", "description": "2 vCPU, 2GB RAM", "cpu_millicores": 2000, "memory_mb": 2048, "rate_msats_per_sec": 100}}
  ],
  "whitelisted_mints": [{}],
  "heartbeat_interval_secs": 60,
  "minimum_duration_seconds": 60,
  "cashu_wallet_db_path": "/var/lib/paygress/cashu-wallet.sqlite",
  "lightning_address": {}
}}"#,
        backend_type,
        storage,
        proxmox_template,
        bridge,
        nostr_key,
        provider_name,
        json_string_or_null(args.location.as_deref()),
        args.host,
        mints_json,
        json_string_or_null(args.lightning_address.as_deref()),
    )
}

fn json_string_or_null(value: Option<&str>) -> String {
    match value {
        Some(v) => format!("\"{}\"", v),
        None => "null".to_string(),
    }
}

fn step_systemd_service(args: &BootstrapArgs, is_root: bool, sudo: &str) -> Result<()> {
    println!("{}", "Step 7: Setting Up Systemd Service".blue().bold());
    println!("{}", "─".repeat(50));

    let systemd_service = r#"[Unit]
Description=Paygress Provider Service
After=network.target pve-cluster.service

[Service]
Type=simple
ExecStart=/usr/local/bin/paygress-cli provider start --config /etc/paygress/provider-config.json
Restart=always
RestartSec=10

[Install]
WantedBy=multi-user.target
"#;

    if args.dry_run {
        println!("  Would create /etc/systemd/system/paygress-provider.service");
    } else {
        let create_service = if is_root {
            format!(
                "cat > /etc/systemd/system/paygress-provider.service << 'EOF'\n{}\nEOF\nsystemctl daemon-reload",
                systemd_service
            )
        } else {
            format!(
                "echo '{}' | {}tee /etc/systemd/system/paygress-provider.service > /dev/null && {}systemctl daemon-reload",
                systemd_service.replace('\'', "'\\''"), sudo, sudo
            )
        };
        run_ssh_command(args, &create_service)?;
        println!("  {} Created systemd service", "✓".green());
    }
    println!();
    Ok(())
}

/// Deploy ngx_l402: an L402 paywall in front of the axum backend, plus
/// a periodic sweep of all accumulated ecash (Nostr-DM and HTTP paths
/// alike) to Lightning. Skipped without a Lightning address — there
/// would be no sweep target.
async fn step_ngx_l402(
    args: &BootstrapArgs,
    nostr_key: &str,
    is_root: bool,
    sudo: &str,
) -> Result<()> {
    println!(
        "{}",
        "Step 8: Deploying ngx_l402 (Lightning Sweep)".blue().bold()
    );
    println!("{}", "─".repeat(50));

    let Some(lightning_address) = args.lightning_address.as_deref() else {
        println!(
            "  {} Skipped — no --lightning-address provided.",
            "–".yellow()
        );
        println!("    Ecash from Nostr-DM redemptions will accumulate in the");
        println!("    shared wallet (/var/lib/paygress/cashu-wallet.sqlite).");
        println!("    To enable auto-sweep later, re-run bootstrap with:");
        println!(
            "      {} --lightning-address you@getalby.com",
            "paygress-cli bootstrap".cyan()
        );
        println!();
        return Ok(());
    };

    // Validate before touching the remote machine, so a typo doesn't
    // leave ngx_l402 running with a broken sweep target.
    if args.dry_run {
        println!(
            "  Would validate Lightning Address: {}",
            lightning_address.cyan()
        );
    } else {
        print!(
            "  Validating Lightning Address {}...",
            lightning_address.cyan()
        );
        std::io::stdout().flush()?;
        validate_lightning_address(lightning_address)
            .await
            .with_context(|| {
                format!(
                    "Lightning Address '{}' validation failed — \
                     fix the address or remove --lightning-address to skip ngx_l402",
                    lightning_address
                )
            })?;
    }

    // ngx_l402 gets the SAME BIP39 phrase (Cashu/NUT-13) and derives an
    // identical seed, so both processes open the wallet CdkRedeemer
    // writes to.
    let wallet_mnemonic = paygress::cashu::mnemonic_from_nostr_key(nostr_key)
        .map_err(|e| anyhow::anyhow!("failed to derive wallet mnemonic: {}", e))?;

    let root_key = match args.root_key {
        Some(ref k) => k.clone(),
        None => {
            use sha2::{Digest, Sha256};
            let mut h = Sha256::new();
            h.update(b"paygress/root_key/v1\0");
            h.update(nostr_key.as_bytes());
            hex::encode(h.finalize())
        }
    };

    let env_file = format!(
        r#"# Auto-generated by paygress bootstrap
# ngx_l402 configuration

LNURL_ADDRESS={lightning_address}
ROOT_KEY={root_key}

# Cashu wallet — shares the same db and BIP39 seed as the Paygress provider
CASHU_WALLET_MNEMONIC="{wallet_mnemonic}"
CASHU_WHITELISTED_MINTS={mints}

# Lightning sweep settings (both Nostr-DM and HTTP path ecash)
CASHU_REDEEM_ON_LIGHTNING=true
CASHU_REDEMPTION_INTERVAL_SECS={sweep_interval}
CASHU_MELT_MIN_BALANCE_SATS={sweep_min_balance}
CASHU_MELT_FEE_RESERVE_PERCENT=1
"#,
        lightning_address = lightning_address,
        root_key = root_key,
        wallet_mnemonic = wallet_mnemonic,
        mints = args.mints,
        sweep_interval = args.sweep_interval_secs,
        sweep_min_balance = args.sweep_min_balance_sats,
    );

    // The image's baked-in nginx.conf references grpc-content-server
    // (used by the ngx_l402 test suite), which doesn't exist in a
    // Paygress deployment and crash-loops nginx. Mount over it.
    let nginx_conf = r#"user  nginx;
worker_processes  auto;

error_log  /var/log/nginx/error.log warn;
pid        /var/run/nginx.pid;
load_module /etc/nginx/modules/libngx_l402_lib.so;

events {
    worker_connections  1024;
}

http {
    include       /etc/nginx/mime.types;
    default_type  application/octet-stream;
    sendfile        on;
    keepalive_timeout  65;

    server {
        listen 80;
        listen [::]:80;
        server_name _;

        location / {
            root   /usr/share/nginx/html;
            l402    on;
            l402_amount_msat_default    10000;
            l402_macaroon_timeout 0;
            try_files $uri $uri/index.html =404;
        }

        location = /metrics {
            l402_metrics;
        }

        location = /.well-known/l402-services {
            l402_manifest;
        }
    }
}
"#;

    let docker_compose = r#"services:
  nginx:
    image: ghcr.io/ngx-l402/ngx-l402:latest
    container_name: nginx-l402
    restart: always
    ports:
      - "80:80"
    env_file:
      - .env
    environment:
      - LN_CLIENT_TYPE=LNURL
      - CASHU_ECASH_SUPPORT=true
      - CASHU_DB_PATH=/var/lib/nginx/cashu-wallet.sqlite
    volumes:
      - /var/lib/paygress:/var/lib/nginx
      - /etc/paygress/nginx.conf:/etc/nginx/nginx.conf:ro
"#;

    if args.dry_run {
        println!("  Would install Docker");
        println!(
            "  Would write /etc/paygress/.env (LNURL_ADDRESS={})",
            lightning_address
        );
        println!("  Would write /etc/paygress/docker-compose.yml");
        println!("  Would run: docker compose up -d");
        println!();
        return Ok(());
    }

    print!("  Checking Docker... ");
    std::io::stdout().flush()?;
    let docker_check = run_ssh_command_output(
        args,
        "which docker >/dev/null 2>&1 && echo 'installed' || echo 'not_installed'",
    )?;
    if docker_check.trim() == "not_installed" {
        println!("{}", "not found — installing".yellow());
        run_ssh_command(
            args,
            &format!("curl -fsSL https://get.docker.com | {}sh", sudo),
        )?;
        println!("  {} Docker installed", "✓".green());
    } else {
        println!("{}", "already installed".green());
    }

    write_remote_file(
        args,
        is_root,
        sudo,
        "/etc/paygress/.env",
        &env_file,
        "ENVEOF",
    )?;
    println!("  {} Created /etc/paygress/.env", "✓".green());

    write_remote_file(
        args,
        is_root,
        sudo,
        "/etc/paygress/nginx.conf",
        nginx_conf,
        "NGINXEOF",
    )?;
    println!("  {} Created /etc/paygress/nginx.conf", "✓".green());

    write_remote_file(
        args,
        is_root,
        sudo,
        "/etc/paygress/docker-compose.yml",
        docker_compose,
        "COMPOSEEOF",
    )?;
    println!("  {} Created /etc/paygress/docker-compose.yml", "✓".green());

    run_ssh_command(
        args,
        &format!("cd /etc/paygress && {}docker compose up -d", sudo),
    )?;
    println!(
        "  {} ngx_l402 started — sweeping both Nostr-DM and HTTP ecash to Lightning ⚡",
        "✓".green()
    );
    println!();
    Ok(())
}

/// Write `content` to `path` on the remote: a heredoc as root, or
/// `echo | sudo tee` otherwise.
fn write_remote_file(
    args: &BootstrapArgs,
    is_root: bool,
    sudo: &str,
    path: &str,
    content: &str,
    heredoc_tag: &str,
) -> Result<()> {
    let cmd = if is_root {
        format!(
            "cat > {} << '{}'\n{}\n{}",
            path, heredoc_tag, content, heredoc_tag
        )
    } else {
        format!(
            "echo '{}' | {}tee {} > /dev/null",
            content.replace('\'', "'\\''"),
            sudo,
            path
        )
    };
    run_ssh_command(args, &cmd)?;
    Ok(())
}

fn step_start_service(args: &BootstrapArgs, sudo: &str, use_lxd: bool) -> Result<()> {
    println!("{}", "Step 9: Starting Provider Service".blue().bold());
    println!("{}", "─".repeat(50));

    if args.dry_run {
        println!("  Would run: systemctl enable --now paygress-provider");
    } else if use_lxd {
        run_ssh_command(
            args,
            &format!(
                "{}systemctl enable paygress-provider && {}systemctl restart paygress-provider",
                sudo, sudo
            ),
        )?;
        println!("  {} Service started successfully!", "✓".green());
    } else {
        // The Proxmox config still needs its API token pasted in.
        println!(
            "  {} Service configured (not started - needs API token)",
            "✓".green()
        );
        println!();
        println!("  To complete setup, SSH into the server and:");
        println!("    1. Get your API token: pveum user token list root@pam");
        println!("    2. Update /etc/paygress/provider-config.json");
        println!("    3. Start: systemctl enable --now paygress-provider");
    }
    println!();
    Ok(())
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

fn control_path(host: &str, port: u16) -> String {
    format!("/tmp/paygress-ssh-{}-{}", host, port)
}

fn base_ssh_args(args: &BootstrapArgs) -> Vec<String> {
    let mut v = vec![
        "-o".to_string(),
        "StrictHostKeyChecking=no".to_string(),
        "-o".to_string(),
        "ControlMaster=auto".to_string(),
        "-o".to_string(),
        format!("ControlPath={}", control_path(&args.host, args.port)),
        "-o".to_string(),
        "ControlPersist=10m".to_string(),
        "-p".to_string(),
        args.port.to_string(),
    ];
    if let Some(ref key) = args.key {
        v.push("-i".to_string());
        v.push(key.clone());
    }
    v
}

/// Route the ssh argv through `sshpass` when a password was supplied,
/// so no step re-prompts. Returns (program, argv).
fn ssh_invocation(args: &BootstrapArgs, ssh_args: Vec<String>) -> (String, Vec<String>) {
    match args.password {
        Some(ref password) => {
            let mut v = vec!["-p".to_string(), password.clone(), "ssh".to_string()];
            v.extend(ssh_args);
            ("sshpass".to_string(), v)
        }
        None => ("ssh".to_string(), ssh_args),
    }
}

fn missing_program_hint(program: &str) -> &'static str {
    if program == "sshpass" {
        "Is sshpass installed? (apt-get install sshpass / brew install sshpass)"
    } else {
        ""
    }
}

/// Open a persistent ControlMaster connection (authenticates once).
fn open_ssh_master(args: &BootstrapArgs) -> Result<()> {
    let cp = control_path(&args.host, args.port);
    if std::path::Path::new(&cp).exists() {
        return Ok(());
    }
    let mut ssh_args = base_ssh_args(args);
    ssh_args.extend([
        "-o".to_string(),
        "ControlMaster=yes".to_string(),
        "-N".to_string(), // no command — just keep the connection open
        "-f".to_string(), // background immediately after auth
        format!("{}@{}", args.user, args.host),
    ]);
    let (program, final_args) = ssh_invocation(args, ssh_args);

    let status = Command::new(&program)
        .args(&final_args)
        .status()
        .with_context(|| {
            format!(
                "Failed to open SSH master connection. {}",
                missing_program_hint(&program)
            )
        })?;
    if !status.success() {
        return Err(anyhow::anyhow!("SSH master connection failed"));
    }
    Ok(())
}

fn close_ssh_master(args: &BootstrapArgs) {
    let cp = control_path(&args.host, args.port);
    let _ = Command::new("ssh")
        .args([
            "-o",
            &format!("ControlPath={}", cp),
            "-O",
            "exit",
            &format!("{}@{}", args.user, args.host),
        ])
        .output();
}

fn run_ssh_command(args: &BootstrapArgs, cmd: &str) -> Result<bool> {
    let mut ssh_args = base_ssh_args(args);
    ssh_args.push("-t".to_string()); // allocate PTY for interactive commands
    ssh_args.push(format!("{}@{}", args.user, args.host));
    ssh_args.push(cmd.to_string());
    let (program, final_args) = ssh_invocation(args, ssh_args);

    let status = Command::new(&program)
        .args(&final_args)
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()
        .with_context(|| {
            format!(
                "Failed to execute {} command. {}",
                program,
                missing_program_hint(&program)
            )
        })?;

    Ok(status.success())
}

fn run_ssh_command_output(args: &BootstrapArgs, cmd: &str) -> Result<String> {
    let mut ssh_args = base_ssh_args(args);
    ssh_args.push(format!("{}@{}", args.user, args.host));
    ssh_args.push(cmd.to_string());
    let (program, final_args) = ssh_invocation(args, ssh_args);

    let output = Command::new(&program)
        .args(&final_args)
        .output()
        .with_context(|| {
            format!(
                "Failed to execute {} command. {}",
                program,
                missing_program_hint(&program)
            )
        })?;

    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

fn get_proxmox_install_script() -> &'static str {
    r#"
# Proxmox VE Installation Script
set -e

# Check OS information
if [ -f /etc/os-release ]; then
    . /etc/os-release
    OS=$ID
    VERSION=$VERSION_ID
else
    echo "ERROR: Cannot detect OS"
    exit 1
fi

echo "Detected OS: $OS $VERSION"

# Proxmox VE 8.x requires Debian 12 (Bookworm)
if [ "$OS" != "debian" ] || [ "$VERSION" != "12" ]; then
    echo "ERROR: Proxmox VE installation requires Debian 12 (Bookworm)."
    echo "Current OS is $PRETTY_NAME."
    echo "Please rebuild this server with Debian 12 and try again."
    exit 1
fi

# Add Proxmox repository
echo "Adding Proxmox repository..."
echo "deb [arch=amd64] http://download.proxmox.com/debian/pve bookworm pve-no-subscription" > /etc/apt/sources.list.d/pve-install-repo.list

# Add repository key
wget https://enterprise.proxmox.com/debian/proxmox-release-bookworm.gpg -O /etc/apt/trusted.gpg.d/proxmox-release-bookworm.gpg

# Add /etc/hosts entry for itself if missing (required for Proxmox request)
IP=$(hostname -I | awk '{print $1}')
HOSTNAME=$(hostname)
if ! grep -q "$IP $HOSTNAME" /etc/hosts; then
    echo "Adding host entry to /etc/hosts..."
    echo "$IP $HOSTNAME.local $HOSTNAME" >> /etc/hosts
fi

# Update and install
export DEBIAN_FRONTEND=noninteractive
apt-get update
apt-get full-upgrade -y
apt-get install -y proxmox-ve postfix open-iscsi chrony

# Remove os-prober (conflicts with Proxmox)
apt-get remove -y os-prober 2>/dev/null || true

echo "Proxmox VE installation complete!"
echo "A reboot may be required."
"#
}
