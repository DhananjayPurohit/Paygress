use anyhow::Result;
use clap::{Args, Subcommand};
use colored::*;
use std::io::{self, Write};
use std::process::Command;

#[derive(Args)]
pub struct SystemArgs {
    #[command(subcommand)]
    pub action: SystemAction,
}

#[derive(Subcommand)]
pub enum SystemAction {
    /// Reset the system to a clean state (uninstall/cleanup)
    Reset(ResetArgs),
}

#[derive(Args)]
pub struct ResetArgs {
    /// Target server IP for remote reset (runs locally if not provided)
    #[arg(long)]
    pub host: Option<String>,

    /// SSH user for remote reset
    #[arg(long, default_value = "root")]
    pub user: String,

    /// SSH port for remote reset
    #[arg(long, default_value = "22")]
    pub port: u16,

    /// Skip confirmation prompts
    #[arg(long, short = 'y')]
    pub yes: bool,

    /// Uninstall the compute backend (LXD/Proxmox)
    #[arg(long)]
    pub uninstall_backend: bool,
}

pub async fn execute(args: SystemArgs, verbose: bool) -> Result<()> {
    match args.action {
        SystemAction::Reset(reset_args) => execute_reset(reset_args, verbose).await,
    }
}

async fn execute_reset(args: ResetArgs, verbose: bool) -> Result<()> {
    if let Some(ref host) = args.host {
        return execute_remote_reset(
            host,
            &args.user,
            args.port,
            args.uninstall_backend,
            args.yes,
            verbose,
        )
        .await;
    }

    println!("{}", "⚠️  SYSTEM RESET (LOCAL) ⚠️".red().bold());
    println!(
        "{}",
        "This will permanently remove Paygress services and configurations from THIS machine."
            .red()
    );
    if args.uninstall_backend {
        println!(
            "{}",
            "WARNING: This will also attempt to UNINSTALL your compute backend (LXD/Proxmox)."
                .red()
                .bold()
        );
    }
    println!();

    if !args.yes {
        print!("Are you sure you want to proceed? [y/N] ");
        io::stdout().flush()?;
        let mut input = String::new();
        io::stdin().read_line(&mut input)?;
        if !input.trim().eq_ignore_ascii_case("y") {
            println!("Reset aborted.");
            return Ok(());
        }
    }

    // 1. Stop and disable service
    print!("  Stopping paygress-provider service... ");
    io::stdout().flush()?;
    let _ = Command::new("systemctl")
        .args(["stop", "paygress-provider"])
        .output();
    let _ = Command::new("systemctl")
        .args(["disable", "paygress-provider"])
        .output();
    println!("{}", "DONE".green());

    // 2. Remove systemd unit
    print!("  Removing systemd unit... ");
    io::stdout().flush()?;
    let _ = Command::new("rm")
        .args(["-f", "/etc/systemd/system/paygress-provider.service"])
        .output();
    let _ = Command::new("systemctl").args(["daemon-reload"]).output();
    println!("{}", "DONE".green());

    // 3. Remove configurations
    print!("  Removing /etc/paygress... ");
    io::stdout().flush()?;
    let _ = Command::new("rm").args(["-rf", "/etc/paygress"]).output();
    println!("{}", "DONE".green());

    // 4. Uninstall Backend if requested
    if args.uninstall_backend {
        println!("{}", "  Uninstalling compute backend...".yellow());

        print!("    Removing LXD (snap)... ");
        io::stdout().flush()?;
        let output = Command::new("snap")
            .args(["remove", "lxd", "--purge"])
            .output();
        if output.is_ok() {
            println!("{}", "DONE".green());
        } else {
            println!("{}", "SKIPPED (not via snap)".yellow());
        }

        print!("    Removing LXC (apt)... ");
        io::stdout().flush()?;
        let _ = Command::new("apt-get")
            .args(["remove", "--purge", "-y", "lxc", "lxcfs"])
            .output();
        let _ = Command::new("apt-get").args(["autoremove", "-y"]).output();
        println!("{}", "DONE".green());

        println!(
            "    {} Manual Proxmox cleanup may be required if using Proxmox VE.",
            "Note:".yellow()
        );
    }

    println!();
    println!("{}", "━".repeat(50).green());
    println!("{}", "Reset Complete!".green().bold());
    println!("Paygress has been uninstalled from this machine.");
    println!("{}", "━".repeat(50).green());

    Ok(())
}

async fn execute_remote_reset(
    host: &str,
    user: &str,
    port: u16,
    uninstall_backend: bool,
    yes: bool,
    _verbose: bool,
) -> Result<()> {
    println!("{}", "Remote System Reset".bold());
    println!("  Host: {}@{}:{}", user, host, port);
    println!();

    if !yes {
        print!("Are you sure you want to reset the remote server? [y/N] ");
        io::stdout().flush()?;
        let mut input = String::new();
        io::stdin().read_line(&mut input)?;
        if !input.trim().eq_ignore_ascii_case("y") {
            println!("Reset aborted.");
            return Ok(());
        }
    }

    let ssh_target = format!("{}@{}", user, host);
    let port_str = port.to_string();

    let mut reset_script = String::from(
        "systemctl stop paygress-provider 2>/dev/null; \
         systemctl disable paygress-provider 2>/dev/null; \
         rm -f /etc/systemd/system/paygress-provider.service; \
         systemctl daemon-reload; \
         rm -rf /etc/paygress; \
         echo 'Paygress service and config removed.'",
    );

    if uninstall_backend {
        reset_script.push_str(
            "; snap remove lxd --purge 2>/dev/null; \
             apt-get remove --purge -y lxc lxcfs 2>/dev/null; \
             apt-get autoremove -y 2>/dev/null; \
             echo 'Backend cleanup attempted.'",
        );
    }

    let output = Command::new("ssh")
        .args([
            "-p",
            &port_str,
            "-o",
            "StrictHostKeyChecking=no",
            &ssh_target,
            &reset_script,
        ])
        .output()?;

    if output.status.success() {
        println!("{}", String::from_utf8_lossy(&output.stdout));
        println!("{}", "Remote Reset Complete!".green().bold());
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(anyhow::anyhow!("Remote reset failed: {}", stderr));
    }

    Ok(())
}
