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

    /// Provider display name
    #[arg(long)]
    pub name: String,

    /// Location description (e.g., "US-East", "Germany")
    #[arg(long)]
    pub location: Option<String>,

    /// Nostr private key (nsec format, auto-generated if not provided)
    #[arg(long)]
    pub nostr_key: Option<String>,

    /// Whitelisted Cashu mints (comma-separated)
    #[arg(long, default_value = "https://mint.minibits.cash")]
    pub mints: String,

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
    println!("Name:   {}", args.name.yellow());
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
                .map_err(|e| anyhow::anyhow!("Failed to get secret key: {}", e))?
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
  "whitelisted_mints": ["{}"],
  "heartbeat_interval_secs": 60,
  "minimum_duration_seconds": 60
}}"#,
        backend_type,
        storage,
        proxmox_template,
        bridge,
        nostr_key,
        args.name,
        args.location
            .as_ref()
            .map(|l| format!("\"{}\"", l))
            .unwrap_or("null".to_string()),
        args.host, // <--- Added arg
        args.mints
    );

    if args.dry_run {
        println!("  Would create /etc/paygress/provider-config.json");
    } else {
        let create_config = if is_root {
            format!(
                "mkdir -p /etc/paygress && cat > /etc/paygress/provider-config.json << 'EOF'\n{}\nEOF",
                config
            )
        } else {
            format!(
                "{}mkdir -p /etc/paygress && echo '{}' | {}tee /etc/paygress/provider-config.json > /dev/null",
                sudo, config.replace("'", "'\\''"), sudo
            )
        };
        run_ssh_command(&args, &create_config)?;
        println!(
            "  {} Created /etc/paygress/provider-config.json",
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

    // Step 8: Start Service
    println!("{}", "Step 8: Starting Provider Service".blue().bold());
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
    println!("  Provider Name: {}", args.name.yellow());
    println!("  Server:        {}", args.host.cyan());

    if !use_lxd {
        println!("  Proxmox UI:    https://{}:8006", args.host);
        println!();
        println!("  {} Next Steps:", "📋".to_string());
        println!("    1. SSH into {} and get your API token", args.host);
        println!("    2. Update the config with the token secret");
        println!("    3. Start the service: systemctl start paygress-provider");
    } else {
        println!("  Backend:       LXD (Native)");
        println!("  Status:        Running 🟢");
    }

    println!();
    println!("  Users can discover you with:");
    println!("    {} market list", "paygress-cli".cyan());
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
