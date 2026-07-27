// `paygress-cli provider` — machine-operator commands for setting up
// and running a Paygress provider.

mod service;
mod setup;
mod setup_multi;
mod tunnel;

use anyhow::Result;
use clap::{Args, Subcommand};
use colored::Colorize;
use paygress::provider::BackendType;

use service::{ConfigArgs, StartArgs};
use setup::SetupArgs;
use setup_multi::SetupMultiArgs;
use tunnel::TunnelArgs;

const CONFIG_PATH: &str = "/etc/paygress/provider-config.json";

#[derive(Args)]
pub struct ProviderArgs {
    #[command(subcommand)]
    pub action: ProviderAction,
}

#[derive(Subcommand)]
pub enum ProviderAction {
    /// Initial setup - configure Proxmox connection and provider settings
    Setup(SetupArgs),

    /// Scaffold N independent provider configs on the SAME host
    SetupMulti(SetupMultiArgs),

    /// Start the provider service (heartbeats + request handler)
    Start(StartArgs),

    /// Stop the provider service
    Stop,

    /// Show provider status and configuration
    Status,

    /// Edit configuration
    Config(ConfigArgs),

    /// Setup WireGuard VPN tunnel for providers behind NAT
    Tunnel(TunnelArgs),
}

pub async fn execute(args: ProviderArgs, verbose: bool) -> Result<()> {
    match args.action {
        ProviderAction::Setup(a) => setup::execute_setup(a, verbose).await,
        ProviderAction::SetupMulti(a) => setup_multi::execute_setup_multi(a, verbose).await,
        ProviderAction::Start(a) => service::execute_start(a, verbose).await,
        ProviderAction::Stop => service::execute_stop(verbose).await,
        ProviderAction::Status => service::execute_status(verbose).await,
        ProviderAction::Config(a) => service::execute_config(a, verbose).await,
        ProviderAction::Tunnel(a) => tunnel::execute_tunnel(a, verbose).await,
    }
}

/// Section separator shared by every provider subcommand.
fn rule() {
    println!("{}", "━".repeat(50).blue());
}

/// clap value-parser for `--backend`.
fn parse_backend(s: &str) -> std::result::Result<BackendType, String> {
    match s {
        "proxmox" => Ok(BackendType::Proxmox),
        "lxd" => Ok(BackendType::LXD),
        "docker" => Ok(BackendType::Docker),
        "kvm" => Ok(BackendType::Kvm),
        other => Err(format!(
            "unknown backend `{}` (expected one of: proxmox, lxd, docker, kvm)",
            other
        )),
    }
}

/// Best-effort public IP lookup; `None` when the service is unreachable.
async fn detect_public_ip() -> Option<String> {
    let resp = reqwest::get("https://api.ipify.org").await.ok()?;
    let ip = resp.text().await.ok()?.trim().to_string();
    (!ip.is_empty()).then_some(ip)
}
