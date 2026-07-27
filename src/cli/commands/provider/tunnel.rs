// `provider tunnel` — pay a VPN service for a WireGuard peer config and
// bring the interface up, so a provider behind NAT gets a routable IP.

use anyhow::Result;
use clap::Args;
use colored::Colorize;
use std::process::Command;

use super::{rule, CONFIG_PATH};
use paygress::provider::{load_config, save_config};

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

pub(super) async fn execute_tunnel(args: TunnelArgs, _verbose: bool) -> Result<()> {
    println!("{}", "WireGuard Tunnel Setup".blue().bold());
    rule();
    println!();

    let need_sudo = !is_root();
    let sudo: &[&str] = if need_sudo { &["sudo"] } else { &[] };

    let wg_conf_path = format!("/etc/wireguard/{}.conf", args.interface);

    // /etc/wireguard is typically 0700, so probe via sudo when not root.
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

        // Still refresh the provider config from what's on disk.
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

    ensure_wireguard_installed(sudo)?;
    let wg_config = fetch_vpn_config(&args).await?;
    write_wg_config(&wg_config, &wg_conf_path, need_sudo)?;

    let (public_ip, port_start, port_end) = parse_wg_config(&wg_config)
        .ok_or_else(|| anyhow::anyhow!("Could not extract tunnel IP from WireGuard config"))?;

    println!("  {} Tunnel public IP: {}", "V".green(), public_ip.cyan());
    if let (Some(ps), Some(pe)) = (port_start, port_end) {
        println!("  {} Port range: {}-{}", "V".green(), ps, pe);
    }

    bring_interface_up(sudo, &args.interface)?;

    let unit = format!("wg-quick@{}", args.interface);
    let _ = elevated(sudo, &["systemctl", "enable", &unit]).output();
    println!("  {} Enabled on boot", "V".green());

    update_provider_tunnel_config(&args.interface, &public_ip, port_start, port_end)?;

    println!();
    rule();
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

fn is_root() -> bool {
    Command::new("id")
        .arg("-u")
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim() == "0")
        .unwrap_or(false)
}

/// Build `argv` prefixed with `sudo` when the caller isn't root.
fn elevated(sudo: &[&str], argv: &[&str]) -> Command {
    let mut full: Vec<&str> = sudo.to_vec();
    full.extend_from_slice(argv);
    let mut cmd = Command::new(full.remove(0));
    cmd.args(&full);
    cmd
}

fn ensure_wireguard_installed(sudo: &[&str]) -> Result<()> {
    print!("  Checking WireGuard installation... ");
    let installed = Command::new("which").arg("wg-quick").output();
    if matches!(&installed, Ok(o) if o.status.success()) {
        println!("{}", "OK".green());
        return Ok(());
    }

    println!("{}", "not found, installing...".yellow());
    let install = elevated(
        sudo,
        &["apt-get", "install", "-y", "wireguard", "wireguard-tools"],
    )
    .env("DEBIAN_FRONTEND", "noninteractive")
    .output();
    if !matches!(&install, Ok(o) if o.status.success()) {
        anyhow::bail!(
            "Failed to install WireGuard. Install manually: sudo apt install wireguard wireguard-tools"
        );
    }
    println!("  {} WireGuard installed", "V".green());
    Ok(())
}

async fn fetch_vpn_config(args: &TunnelArgs) -> Result<String> {
    print!("  Requesting VPN config from {}... ", args.vpn_url);
    let response = reqwest::Client::new()
        .get(&args.vpn_url)
        .header("Authorization", format!("Cashu {}", args.token))
        .header(
            "User-Agent",
            format!("Paygress-CLI/{}", env!("CARGO_PKG_VERSION")),
        )
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

    if !wg_config.contains("[Interface]") {
        println!(
            "  {} Received invalid config (no [Interface] section)",
            "X".red()
        );
        anyhow::bail!("Invalid WireGuard config received from VPN service");
    }
    println!("  {} Config validated", "V".green());
    Ok(wg_config)
}

fn write_wg_config(wg_config: &str, wg_conf_path: &str, need_sudo: bool) -> Result<()> {
    if need_sudo {
        Command::new("sudo")
            .args(["mkdir", "-p", "/etc/wireguard"])
            .spawn()?
            .wait()?;

        let mut tee = Command::new("sudo")
            .args(["tee", wg_conf_path])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::null())
            .spawn()?;
        if let Some(ref mut stdin) = tee.stdin {
            use std::io::Write;
            stdin.write_all(wg_config.as_bytes())?;
        }
        tee.wait()?;

        Command::new("sudo")
            .args(["chmod", "600", wg_conf_path])
            .output()?;
    } else {
        std::fs::create_dir_all("/etc/wireguard")?;
        std::fs::write(wg_conf_path, wg_config)?;
        Command::new("chmod").args(["600", wg_conf_path]).output()?;
    }
    println!("  {} Saved to {}", "V".green(), wg_conf_path);
    Ok(())
}

fn bring_interface_up(sudo: &[&str], interface: &str) -> Result<()> {
    print!("  Starting WireGuard interface {}... ", interface);
    let output = elevated(sudo, &["wg-quick", "up", interface]).output()?;

    if output.status.success() {
        println!("{}", "UP".green());
        return Ok(());
    }

    let stderr = String::from_utf8_lossy(&output.stderr);
    if stderr.contains("already exists") {
        println!("{}", "ALREADY UP".yellow());
        return Ok(());
    }
    println!("{}", "FAILED".red());
    println!("  {}", stderr.trim());
    Err(anyhow::anyhow!("Failed to start WireGuard interface"))
}

/// Extract (public_ip, port_start, port_end) from a WireGuard config.
fn parse_wg_config(config: &str) -> Option<(String, Option<u16>, Option<u16>)> {
    // "Endpoint = 1.2.3.4:51820"
    let public_ip = config
        .lines()
        .find(|l| l.trim().starts_with("Endpoint"))
        .and_then(|l| l.split('=').nth(1))
        .map(|v| v.trim().split(':').next().unwrap_or("").to_string())
        .filter(|s| !s.is_empty())?;

    // "# Public Ports: 1.2.3.4:11000-11999"
    let (port_start, port_end) = config
        .lines()
        .find(|l| l.contains("Public Ports:") || l.contains("Port Range:"))
        .and_then(|l| {
            let re_part = l.split(':').next_back()?;
            let range_str = re_part.trim().split(':').next_back()?.trim();
            let mut parts = range_str.split('-');
            let start: u16 = parts.next()?.trim().parse().ok()?;
            let end: u16 = parts.next()?.trim().parse().ok()?;
            Some((Some(start), Some(end)))
        })
        .unwrap_or((None, None));

    Some((public_ip, port_start, port_end))
}

fn update_provider_tunnel_config(
    interface: &str,
    public_ip: &str,
    port_start: Option<u16>,
    port_end: Option<u16>,
) -> Result<()> {
    let Ok(mut config) = load_config(CONFIG_PATH) else {
        println!(
            "  {} No provider config found at {}. Run 'provider setup' first.",
            "⚠".yellow(),
            CONFIG_PATH
        );
        println!("  Tunnel is active but provider config not updated.");
        return Ok(());
    };

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
    Ok(())
}
