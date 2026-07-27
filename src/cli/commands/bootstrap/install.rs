// Getting the paygress-cli binary onto the remote host.

use anyhow::{Context, Result};
use colored::Colorize;
use std::io::Write;
use std::process::Command;

use super::ssh::{control_path, run_ssh_command};
use super::{step_banner, BootstrapArgs};

pub(super) fn step_install_cli(args: &BootstrapArgs) -> Result<()> {
    step_banner("Step 4: Installing paygress-cli");

    if args.dry_run {
        if args.local_binary.is_some() {
            println!("  Would scp local binary to remote and install to /usr/local/bin/");
        } else {
            println!("  Would run: cargo install paygress-cli");
        }
    } else if let Some(ref bin_path) = args.local_binary {
        install_from_local_binary(args, bin_path)?;
    } else {
        install_from_crates_io(args)?;
    }

    if !args.dry_run && args.tunnel {
        print!("  Installing WireGuard for tunnel support... ");
        std::io::stdout().flush()?;
        run_ssh_command(
            args,
            &format!(
                "export DEBIAN_FRONTEND=noninteractive && {}apt-get install -y wireguard wireguard-tools",
                args.sudo()
            ),
        )?;
        println!("{}", "OK".green());
    }
    println!();
    Ok(())
}

fn install_from_local_binary(args: &BootstrapArgs, bin_path: &str) -> Result<()> {
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

    let sudo = args.sudo();

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

fn install_from_crates_io(args: &BootstrapArgs) -> Result<()> {
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
        args.sudo()
    );

    print!("  Installing paygress-cli from crates.io (this may take a few minutes)... ");
    std::io::stdout().flush()?;
    if !run_ssh_command(args, &install_cmd)? {
        return Err(anyhow::anyhow!("Failed to install paygress-cli"));
    }
    println!("{}", "OK".green());
    Ok(())
}
