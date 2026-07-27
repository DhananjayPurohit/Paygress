// Compute-backend provisioning: LXD on Ubuntu, Proxmox VE on Debian.

use anyhow::Result;
use colored::Colorize;
use std::io::Write;

use super::ssh::{run_ssh_command, run_ssh_command_output};
use super::{step_banner, BootstrapArgs};

/// Detect the remote OS and install the matching compute backend.
/// Returns true when the LXD path was taken.
pub(super) fn step_install_backend(args: &BootstrapArgs) -> Result<bool> {
    step_banner("Step 2: Checking OS & Installing Backend");

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
        install_lxd(args)?;
    } else if !args.skip_proxmox {
        install_proxmox(args, &os_id)?;
    } else {
        println!("  Skipping Proxmox installation (--skip-proxmox)");
    }
    println!();
    Ok(use_lxd)
}

fn install_lxd(args: &BootstrapArgs) -> Result<()> {
    let sudo = args.sudo();
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

fn install_proxmox(args: &BootstrapArgs, os_id: &str) -> Result<()> {
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

    let sudo = args.sudo();
    let cmd = if sudo.is_empty() {
        PROXMOX_INSTALL_SCRIPT.to_string()
    } else {
        format!(
            "sudo bash -c '{}'",
            PROXMOX_INSTALL_SCRIPT.replace('\'', "'\\''")
        )
    };
    run_ssh_command(args, &cmd)?;

    println!("  {} Proxmox VE installed!", "✓".green());
    Ok(())
}

pub(super) fn step_api_token(args: &BootstrapArgs, use_lxd: bool, verbose: bool) -> Result<()> {
    step_banner("Step 3: Creating Proxmox API Token");

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
                args.sudo(),
                TOKEN_NAME
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

const PROXMOX_INSTALL_SCRIPT: &str = r#"
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
"#;
