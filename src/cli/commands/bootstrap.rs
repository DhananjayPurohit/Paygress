// Bootstrap CLI Command
//
// One-click setup for a fresh VPS/machine.
// Installs Proxmox VE + Paygress and configures everything.

use anyhow::{Context, Result};
use clap::Args;
use colored::Colorize;
use nostr_sdk::ToBech32;
use std::io::Write;
use std::process::{Command, Stdio};
use std::time::Duration;

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

    /// Whitelisted Cashu mints (comma-separated). Defaults to the
    /// `testnut.cashu.space` testnet mint — see
    /// `cli::commands::provider::SetupArgs::mints` for the rationale
    /// (mainnet receive flow unverified end-to-end against this cdk
    /// version).
    #[arg(long, default_value = "https://testnut.cashu.space")]
    pub mints: String,

    /// Lightning address for automatic sweep of earned ecash to Lightning.
    /// Every redeemed Cashu token is automatically melted to this address.
    /// Format: user@domain.com  (e.g. myprovider@getalby.com)
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

    /// Path to a locally built paygress-cli binary to copy instead of pulling from crates.io.
    /// Useful for testing without a crates.io release.
    /// Build it first with: cargo build --release
    #[arg(long)]
    pub local_binary: Option<String>,

    /// How often ngx_l402 sweeps accumulated ecash to Lightning (seconds).
    /// Default: 3600 (1 hour). Use a smaller value like 60 for testing.
    #[arg(long, default_value = "3600")]
    pub sweep_interval_secs: u64,

    /// Minimum wallet balance in sats before ngx_l402 attempts a sweep.
    /// Default: 100 sats.
    #[arg(long, default_value = "100")]
    pub sweep_min_balance_sats: u64,

    /// ngx_l402 ROOT_KEY for L402 macaroon signing (32-byte hex).
    /// Auto-generated if not provided.
    #[arg(long)]
    pub root_key: Option<String>,
}

/// Validate a Lightning Address by resolving its LNURL-pay well-known URL.
///
/// A Lightning Address `user@domain.com` maps to:
///   GET https://domain.com/.well-known/lnurlp/user
///
/// A valid service returns JSON with `"tag": "payRequest"` plus
/// `minSendable` / `maxSendable` fields. We verify this before deploying
/// ngx_l402 so the provider gets an early, clear error rather than a
/// silently broken sweep loop.
async fn validate_lightning_address(address: &str) -> Result<()> {
    // Parse user@domain
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

    // Print the send range so the provider can see the address is live
    if let (Some(min), Some(max)) = (
        json.get("minSendable").and_then(|v| v.as_u64()),
        json.get("maxSendable").and_then(|v| v.as_u64()),
    ) {
        println!(" {} (sendable: {} – {} msats)", "✓".green(), min, max);
    } else {
        println!(" {}", "✓".green());
    }

    Ok(())
}

pub async fn execute(args: BootstrapArgs, verbose: bool) -> Result<()> {
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

    let target = format!("{}@{}", args.user, args.host);
    let is_root = args.user == "root";
    let sudo = if is_root { "" } else { "sudo " };

    println!("Target: {}", target.cyan());
    println!("Name:   {} (derived from Nostr key)", "auto".dimmed());
    if let Some(ref loc) = args.location {
        println!("Location: {}", loc);
    }
    println!();

    // Step 1: Test SSH connection
    println!("{}", "Step 1: Testing SSH Connection".blue().bold());
    println!("{}", "─".repeat(50));

    if args.dry_run {
        println!("  Would connect to {}", args.host.cyan());
    } else {
        print!("  Connecting to {}... ", args.host);
        std::io::stdout().flush()?;

        // Open the ControlMaster — authenticates once, all subsequent SSH calls reuse this socket
        open_ssh_master(&args)?;

        if !run_ssh_command(&args, "echo 'Connected'")? {
            println!("{}", "FAILED".red());
            close_ssh_master(&args);
            return Err(anyhow::anyhow!("SSH connection failed"));
        }
        println!("{}", "OK".green());
    }
    println!();

    // Step 1.5: Grant passwordless sudo for the rest of this bootstrap session
    // (prompts once here so every subsequent SSH call is non-interactive)
    if !is_root && !args.dry_run {
        println!(
            "{}",
            "Configuring passwordless sudo for bootstrap session...".yellow()
        );
        let grant_cmd = format!(
            "echo '{} ALL=(ALL) NOPASSWD: ALL' | sudo tee /etc/sudoers.d/paygress-bootstrap > /dev/null && echo 'GRANTED'",
            args.user
        );
        if !run_ssh_command(&args, &grant_cmd)? {
            return Err(anyhow::anyhow!(
                "Failed to configure passwordless sudo. Check that your user has sudo privileges."
            ));
        }
        println!(
            "  {} sudo escalation configured (will be removed at end)",
            "✓".green()
        );
        println!();
    }

    // Step 2: Check OS & Install Backend
    println!(
        "{}",
        "Step 2: Checking OS & Installing Backend".blue().bold()
    );
    println!("{}", "─".repeat(50));

    let os_id = if args.dry_run {
        println!("  Would detect OS (assuming debian for dry-run)");
        "debian".to_string()
    } else {
        let output = run_ssh_command_output(
            &args,
            "cat /etc/os-release | grep ^ID= | cut -d= -f2 | tr -d '\"'",
        )?;
        output.trim().to_string()
    };

    println!("  Detected OS: {}", os_id.cyan());

    let use_lxd = os_id == "ubuntu";

    if use_lxd {
        println!(
            "{}",
            "  -> Installing LXD backend (Ubuntu detected)".green()
        );

        if args.dry_run {
            println!("  Would run: snap install lxd && lxd init --auto");
        } else {
            // Check if LXD is installed
            let check = run_ssh_command_output(
                &args,
                "which lxd >/dev/null 2>&1 && echo 'installed' || echo 'not_installed'",
            )?;
            if check.trim() == "installed" {
                println!("  LXD is already installed.");
            } else {
                println!("  Installing LXD...");
                let install_cmd = format!("{}snap install lxd && {}lxd init --auto", sudo, sudo);
                run_ssh_command(&args, &install_cmd)?;
                println!("  LXD installed and initialized!");
            }

            // Ensure default storage pool exists (lxd init --auto may not create one,
            // or LXD may have been pre-installed without a pool)
            let pool_check = run_ssh_command_output(
                &args,
                &format!("{}lxc storage list --format csv 2>/dev/null | wc -l", sudo),
            )?;
            if pool_check.trim() == "0" {
                println!("  Creating default storage pool...");
                let create_pool = format!("{}lxc storage create default dir", sudo);
                run_ssh_command(&args, &create_pool)?;
                println!("  Default storage pool created!");
            } else {
                println!("  Storage pool already exists.");
            }

            // Ensure default network bridge exists
            let net_check = run_ssh_command_output(
                &args,
                &format!(
                    "{}lxc network list --format csv 2>/dev/null | grep -c lxdbr0 || true",
                    sudo
                ),
            )?;
            if net_check.trim() == "0" {
                println!("  Creating default network bridge (lxdbr0)...");
                let create_net = format!("{}lxc network create lxdbr0", sudo);
                run_ssh_command(&args, &create_net)?;
                println!("  Network bridge created!");
            } else {
                println!("  Network bridge already exists.");
            }

            // Ensure default profile has root disk and network devices
            // (pool/bridge may exist but profile may have empty devices: {})
            let profile_devices = run_ssh_command_output(
                &args,
                &format!(
                    "{}lxc profile show default 2>/dev/null | grep -c 'root:' || true",
                    sudo
                ),
            )?;
            if profile_devices.trim() == "0" {
                println!("  Configuring default profile with storage and network...");
                let add_root = format!(
                    "{}lxc profile device add default root disk path=/ pool=default",
                    sudo
                );
                run_ssh_command(&args, &add_root)?;
                let add_net = format!("{}lxc network attach-profile lxdbr0 default eth0", sudo);
                run_ssh_command(&args, &add_net)?;
                println!("  Default profile configured!");
            } else {
                println!("  Default profile already configured.");
            }
        }
    } else if !args.skip_proxmox {
        // Proxmox (Debian) path
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

        let proxmox_check =
            "which pvesh >/dev/null 2>&1 && echo 'installed' || echo 'not_installed'";

        if args.dry_run {
            println!("  Would check: {}", proxmox_check.cyan());
        } else {
            print!("  Checking for existing Proxmox... ");
            std::io::stdout().flush()?;

            let output = run_ssh_command_output(&args, proxmox_check)?;

            if output.trim() == "installed" {
                println!("{}", "Already installed".green());
            } else {
                println!("{}", "Not found".yellow());
                println!();
                println!("  {} Installing Proxmox VE...", "⚙".yellow());
                println!("  {} This may take 10-15 minutes", "⏳".to_string());
                println!();

                // Run Proxmox installation script
                let install_script = get_proxmox_install_script();
                // If not root, run with sudo bash
                let cmd = if is_root {
                    install_script.to_string()
                } else {
                    format!("sudo bash -c '{}'", install_script.replace("'", "'\\''"))
                };

                run_ssh_command(&args, &cmd)?;

                println!("  {} Proxmox VE installed!", "✓".green());
            }
        }
    } else {
        println!("  Skipping Proxmox installation (--skip-proxmox)");
    }
    println!();

    // Step 3: Create API Token
    println!("{}", "Step 3: Creating Proxmox API Token".blue().bold());
    println!("{}", "─".repeat(50));

    let token_name = "paygress";
    let create_token_cmd = format!(
        "pveum user token add root@pam {} --privsep=0 2>/dev/null || pveum user token list root@pam 2>/dev/null | grep {}",
        token_name, token_name
    );

    // Only check for token if we are using Proxmox (skipping for LXD)
    if !use_lxd {
        if args.dry_run {
            println!("  Would run: {}", create_token_cmd.cyan());
        } else {
            print!("  Creating API token... ");
            std::io::stdout().flush()?;

            let token_output = run_ssh_command_output(
                &args,
                &format!(
                    "{}pveum user token add root@pam {} --privsep=0 2>&1 || echo 'exists'",
                    sudo, token_name
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
    } else {
        println!("  Skipping Proxmox API token creation (LXD mode)");
    }
    println!();

    // Step 4: Install paygress-cli
    println!("{}", "Step 4: Installing paygress-cli".blue().bold());
    println!("{}", "─".repeat(50));

    if args.dry_run {
        if args.local_binary.is_some() {
            println!("  Would scp local binary to remote and install to /usr/local/bin/");
        } else {
            println!("  Would run: cargo install paygress-cli");
        }
    } else if let Some(ref bin_path) = args.local_binary {
        // --- Local binary mode: scp the pre-built binary, no compilation needed ---
        if !std::path::Path::new(bin_path).exists() {
            return Err(anyhow::anyhow!(
                "Local binary not found at '{}'. Build it first with: cargo build --release",
                bin_path
            ));
        }
        print!("  Copying local binary to {}... ", args.host);
        std::io::stdout().flush()?;

        // scp via the ControlMaster socket so no re-authentication
        let cp = control_path(&args.host, args.port);
        let mut scp_args = vec![
            "-o".to_string(),
            "StrictHostKeyChecking=no".to_string(),
            "-o".to_string(),
            format!("ControlPath={}", cp),
            "-P".to_string(),
            args.port.to_string(),
        ];
        if let Some(ref key) = args.key {
            scp_args.push("-i".to_string());
            scp_args.push(key.clone());
        }
        scp_args.push(bin_path.clone());
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

        // Stop the service first — can't overwrite a running binary ("Text file busy")
        let _ = run_ssh_command(
            &args,
            &format!(
                "{}systemctl stop paygress-provider 2>/dev/null || true",
                sudo
            ),
        );

        // Move into place
        let install_remote = format!(
            "{}install -m 755 /tmp/paygress-cli /usr/local/bin/paygress-cli",
            sudo
        );
        if !run_ssh_command(&args, &install_remote)? {
            return Err(anyhow::anyhow!("Failed to install binary on remote"));
        }
        println!("{}", "OK".green());
    } else {
        // --- crates.io mode: cargo install on the remote ---
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
        if !run_ssh_command(&args, &install_cmd)? {
            return Err(anyhow::anyhow!("Failed to install paygress-cli"));
        }
        println!("{}", "OK".green());
    }

    if !args.dry_run && args.tunnel {
        print!("  Installing WireGuard for tunnel support... ");
        std::io::stdout().flush()?;
        let wg_install = format!(
            "export DEBIAN_FRONTEND=noninteractive && {}apt-get install -y wireguard wireguard-tools",
            sudo
        );
        run_ssh_command(&args, &wg_install)?;
        println!("{}", "OK".green());
    }
    println!();

    // Step 5: Generate Nostr Key
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

    // Derive provider name from the Nostr public key — always automatic,
    // deterministic, and idempotent (same key → same name every run).
    let provider_name: String = {
        let keys = nostr_sdk::Keys::parse(&nostr_key)
            .map_err(|e| anyhow::anyhow!("failed to parse nostr key for name derivation: {}", e))?;
        let pubkey_bytes = keys.public_key().to_bytes();
        let name = paygress::namegen::derive_provider_name(&pubkey_bytes);
        println!("  Provider name:  {}", name.yellow().bold());
        name
    };
    println!();

    // Step 6: Create Configuration
    println!(
        "{}",
        "Step 6: Creating Provider Configuration".blue().bold()
    );
    println!("{}", "─".repeat(50));

    // Explicitly set backend type based on OS detection, otherwise it defaults to Proxmox
    let backend_type = if use_lxd { "LXD" } else { "Proxmox" };
    let proxmox_template = if use_lxd {
        "images:ubuntu/22.04"
    } else {
        "local:vztmpl/ubuntu-22.04-standard.tar.zst"
    };
    let storage = if use_lxd { "default" } else { "local-lvm" }; // LXD default pool is usually 'default'
    let bridge = if use_lxd { "lxdbr0" } else { "vmbr0" }; // LXD default bridge is lxdbr0

    // Pre-format mints as a JSON array so "a,b" → ["a", "b"]
    let mints_json = args
        .mints
        .split(',')
        .map(|m| format!("\"{}\"", m.trim()))
        .collect::<Vec<_>>()
        .join(", ");

    let config = format!(
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
        args.location
            .as_ref()
            .map(|l| format!("\"{}\"", l))
            .unwrap_or("null".to_string()),
        args.host,
        mints_json,
        args.lightning_address
            .as_ref()
            .map(|a| format!("\"{}\"", a))
            .unwrap_or("null".to_string()),
    );

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
                sudo, sudo, config.replace("'", "'\\''"), sudo
            )
        };
        run_ssh_command(&args, &create_config)?;
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

    // Step 7: Create Systemd Service
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
                systemd_service.replace("'", "'\\''"), sudo, sudo
            )
        };
        run_ssh_command(&args, &create_service)?;
        println!("  {} Created systemd service", "✓".green());
    }
    println!();

    // Step 8: Deploy ngx_l402 (optional — only when --lightning-address is given)
    //
    // ngx_l402 serves two purposes:
    //   1. HTTP+L402 paywall in front of the axum backend (HTTP path).
    //   2. Periodic Lightning sweep of ALL accumulated ecash — from both
    //      the Nostr-DM path and the HTTP path — via the shared SQLite wallet.
    //
    // Without a lightning address there is no meaningful sweep target, so
    // we skip this step. Ecash from Nostr-DM redemptions will accumulate
    // in /var/lib/paygress/cashu-wallet.sqlite and can be swept later by
    // re-running bootstrap with --lightning-address.
    println!(
        "{}",
        "Step 8: Deploying ngx_l402 (Lightning Sweep)".blue().bold()
    );
    println!("{}", "─".repeat(50));

    if let Some(ref lightning_address) = args.lightning_address {
        // Validate the Lightning Address before touching the remote machine.
        // Catches typos and unregistered addresses immediately, rather than
        // letting ngx_l402 start with a broken sweep target.
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

        // Derive the shared BIP39 wallet mnemonic from the nostr key
        // (Cashu/NUT-13 standard). ngx_l402 receives the SAME phrase via
        // CASHU_WALLET_MNEMONIC and derives an identical seed, so both open the
        // same CDK wallet that CdkRedeemer writes to.
        let wallet_mnemonic = paygress::cashu::mnemonic_from_nostr_key(&nostr_key)
            .map_err(|e| anyhow::anyhow!("failed to derive wallet mnemonic: {}", e))?;

        // Deterministic ROOT_KEY for ngx_l402 macaroon signing (32 bytes / 64 hex chars).
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

        // Minimal nginx.conf for Paygress deployment — no gRPC upstream.
        // The image's baked-in nginx.conf references grpc-content-server:50051
        // (needed for the ngx_l402 test suite) which doesn't exist in a
        // standalone Paygress deployment and causes a crash-loop. We mount
        // this file over the image's config so nginx starts cleanly.
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
        } else {
            // Install Docker if not present
            print!("  Checking Docker... ");
            std::io::stdout().flush()?;
            let docker_check = run_ssh_command_output(
                &args,
                "which docker >/dev/null 2>&1 && echo 'installed' || echo 'not_installed'",
            )?;
            if docker_check.trim() == "not_installed" {
                println!("{}", "not found — installing".yellow());
                let install_docker = format!("curl -fsSL https://get.docker.com | {}sh", sudo);
                run_ssh_command(&args, &install_docker)?;
                println!("  {} Docker installed", "✓".green());
            } else {
                println!("{}", "already installed".green());
            }

            // Write .env
            let write_env = if is_root {
                format!("cat > /etc/paygress/.env << 'ENVEOF'\n{}\nENVEOF", env_file)
            } else {
                format!(
                    "echo '{}' | {}tee /etc/paygress/.env > /dev/null",
                    env_file.replace("'", "'\\''"),
                    sudo
                )
            };
            run_ssh_command(&args, &write_env)?;
            println!("  {} Created /etc/paygress/.env", "✓".green());

            // Write nginx.conf (overrides the image's baked-in config which
            // references grpc-content-server — not present in Paygress deploys)
            let write_nginx = if is_root {
                format!(
                    "cat > /etc/paygress/nginx.conf << 'NGINXEOF'\n{}\nNGINXEOF",
                    nginx_conf
                )
            } else {
                format!(
                    "echo '{}' | {}tee /etc/paygress/nginx.conf > /dev/null",
                    nginx_conf.replace("'", "'\\''"),
                    sudo
                )
            };
            run_ssh_command(&args, &write_nginx)?;
            println!("  {} Created /etc/paygress/nginx.conf", "✓".green());

            // Write docker-compose.yml
            let write_compose = if is_root {
                format!(
                    "cat > /etc/paygress/docker-compose.yml << 'COMPOSEEOF'\n{}\nCOMPOSEEOF",
                    docker_compose
                )
            } else {
                format!(
                    "echo '{}' | {}tee /etc/paygress/docker-compose.yml > /dev/null",
                    docker_compose.replace("'", "'\\''"),
                    sudo
                )
            };
            run_ssh_command(&args, &write_compose)?;
            println!("  {} Created /etc/paygress/docker-compose.yml", "✓".green());

            // Start ngx_l402
            let start_compose = format!("cd /etc/paygress && {}docker compose up -d", sudo);
            run_ssh_command(&args, &start_compose)?;
            println!(
                "  {} ngx_l402 started — sweeping both Nostr-DM and HTTP ecash to Lightning ⚡",
                "✓".green()
            );
        }
    } else {
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
    }
    println!();

    // Step 9: Start Service
    println!("{}", "Step 9: Starting Provider Service".blue().bold());
    println!("{}", "─".repeat(50));

    if args.dry_run {
        println!("  Would run: systemctl enable --now paygress-provider");
    } else {
        if use_lxd {
            let start_cmd = format!(
                "{}systemctl enable paygress-provider && {}systemctl restart paygress-provider",
                sudo, sudo
            );
            run_ssh_command(&args, &start_cmd)?;
            println!("  {} Service started successfully!", "✓".green());
        } else {
            // Don't actually start yet since config needs token
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
    }
    println!();

    // Summary
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
        println!("  {} Next Steps:", "📋".to_string());
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

    // Clean up the temporary NOPASSWD sudoers rule added at bootstrap start
    if !is_root && !args.dry_run {
        let _ = run_ssh_command(&args, "sudo rm -f /etc/sudoers.d/paygress-bootstrap");
        println!("  {} Temporary sudo rule removed", "✓".green());
    }

    // Close the SSH ControlMaster connection
    if !args.dry_run {
        close_ssh_master(&args);
    }

    Ok(())
}

fn control_path(host: &str, port: u16) -> String {
    format!("/tmp/paygress-ssh-{}-{}", host, port)
}

fn base_ssh_args(args: &BootstrapArgs) -> Vec<String> {
    let cp = control_path(&args.host, args.port);
    let mut v = vec![
        "-o".to_string(),
        "StrictHostKeyChecking=no".to_string(),
        "-o".to_string(),
        "ControlMaster=auto".to_string(),
        "-o".to_string(),
        format!("ControlPath={}", cp),
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

/// Open a persistent ControlMaster connection (authenticates once).
fn open_ssh_master(args: &BootstrapArgs) -> Result<()> {
    let cp = control_path(&args.host, args.port);
    // If a socket already exists, nothing to do
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
    // Use sshpass when password is provided to avoid repeated password prompts
    let (program, final_args) = if let Some(ref password) = args.password {
        let mut sshpass_args = vec!["-p".to_string(), password.clone(), "ssh".to_string()];
        sshpass_args.extend(ssh_args);
        ("sshpass".to_string(), sshpass_args)
    } else {
        ("ssh".to_string(), ssh_args)
    };

    let status = Command::new(&program)
        .args(&final_args)
        .status()
        .context(format!(
            "Failed to open SSH master connection. {}",
            if program == "sshpass" {
                "Is sshpass installed? (apt-get install sshpass / brew install sshpass)"
            } else {
                ""
            }
        ))?;
    if !status.success() {
        return Err(anyhow::anyhow!("SSH master connection failed"));
    }
    Ok(())
}

/// Close the ControlMaster connection.
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

    // Use sshpass when password is provided to avoid repeated password prompts
    let (program, final_args) = if let Some(ref password) = args.password {
        let mut sshpass_args = vec!["-p".to_string(), password.clone(), "ssh".to_string()];
        sshpass_args.extend(ssh_args);
        ("sshpass".to_string(), sshpass_args)
    } else {
        ("ssh".to_string(), ssh_args)
    };

    let status = Command::new(&program)
        .args(&final_args)
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()
        .context(format!(
            "Failed to execute {} command. {}",
            program,
            if program == "sshpass" {
                "Is sshpass installed? (apt-get install sshpass / brew install sshpass)"
            } else {
                ""
            }
        ))?;

    Ok(status.success())
}

fn run_ssh_command_output(args: &BootstrapArgs, cmd: &str) -> Result<String> {
    let mut ssh_args = base_ssh_args(args);
    ssh_args.push(format!("{}@{}", args.user, args.host));
    ssh_args.push(cmd.to_string());

    // Use sshpass when password is provided to avoid repeated password prompts
    let (program, final_args) = if let Some(ref password) = args.password {
        let mut sshpass_args = vec!["-p".to_string(), password.clone(), "ssh".to_string()];
        sshpass_args.extend(ssh_args);
        ("sshpass".to_string(), sshpass_args)
    } else {
        ("ssh".to_string(), ssh_args)
    };

    let output = Command::new(&program)
        .args(&final_args)
        .output()
        .context(format!(
            "Failed to execute {} command. {}",
            program,
            if program == "sshpass" {
                "Is sshpass installed? (apt-get install sshpass / brew install sshpass)"
            } else {
                ""
            }
        ))?;

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
