// List command - Discover providers and their offers
//
// Unified command that works in both modes:
//   - Nostr mode (default): discovers providers via Nostr relays
//   - HTTP mode (--server): queries a specific Paygress server

use anyhow::Result;
use clap::{Args, Subcommand};
use colored::Colorize;
use indicatif::{ProgressBar, ProgressStyle};

use super::identity::parse_relays;
use crate::api::PaygressClient;
use paygress::discovery::DiscoveryClient;
use paygress::nostr::ProviderFilter;

#[derive(Args)]
pub struct ListArgs {
    #[command(subcommand)]
    pub action: Option<ListAction>,

    /// Query a specific HTTP server instead of Nostr
    #[arg(long)]
    pub server: Option<String>,

    /// Filter by capability (lxc, vm)
    #[arg(long)]
    pub capability: Option<String>,

    /// Sort by (price, uptime, capacity, jobs)
    #[arg(long, default_value = "price")]
    pub sort: String,

    /// Only show online providers
    #[arg(long)]
    pub online_only: bool,

    /// Custom Nostr relays (comma-separated)
    #[arg(long)]
    pub relays: Option<String>,
}

#[derive(Subcommand)]
pub enum ListAction {
    /// Show detailed info for a specific provider
    Info(InfoArgs),
}

#[derive(Args)]
pub struct InfoArgs {
    /// Provider npub
    pub provider: String,

    /// Custom Nostr relays (comma-separated)
    #[arg(long)]
    pub relays: Option<String>,
}

pub async fn execute(args: ListArgs, verbose: bool) -> Result<()> {
    // If a subcommand (info) was given, dispatch to it
    if let Some(action) = args.action {
        return match action {
            ListAction::Info(info_args) => execute_info(info_args, verbose).await,
        };
    }

    // If --server is provided, use HTTP mode
    if let Some(ref server) = args.server {
        return execute_http_list(server, verbose).await;
    }

    // Default: Nostr discovery mode
    execute_nostr_list(args, verbose).await
}

async fn execute_nostr_list(args: ListArgs, verbose: bool) -> Result<()> {
    println!("{}", "Discovering Providers...".blue().bold());
    println!();

    let relays = parse_relays(args.relays);

    if verbose {
        println!("  Connecting to {} relays...", relays.len());
    }

    let client = DiscoveryClient::new(relays).await?;

    let filter = ProviderFilter {
        capability: args.capability,
        min_uptime: None,
        min_memory_mb: None,
        min_cpu: None,
    };

    let mut providers = client.list_providers(Some(filter)).await?;

    if args.online_only {
        providers.retain(|p| p.is_online);
    }

    DiscoveryClient::sort_providers(&mut providers, &args.sort);

    if providers.is_empty() {
        println!("{}", "No providers found matching your criteria.".yellow());
        println!();
        println!("Try:");
        println!("  - Removing filters");
        println!("  - Checking different relays with --relays");
        return Ok(());
    }

    println!("Found {} providers:\n", providers.len().to_string().green());
    println!("{}", DiscoveryClient::format_provider_table(&providers));

    println!();
    println!("To see details: {} list info <npub>", "paygress-cli".cyan());
    println!(
        "To spawn:       {} spawn --provider <npub> --token <cashu-token>",
        "paygress-cli".cyan()
    );

    Ok(())
}

async fn execute_http_list(server: &str, verbose: bool) -> Result<()> {
    if verbose {
        println!("{} Fetching offers from {}...", "->".blue(), server);
    }

    let spinner = ProgressBar::new_spinner();
    spinner.set_style(
        ProgressStyle::default_spinner()
            .template("{spinner:.blue} {msg}")
            .unwrap(),
    );
    spinner.set_message("Fetching available offers...");
    spinner.enable_steady_tick(std::time::Duration::from_millis(100));

    let client = PaygressClient::new(server);
    let response = client.get_offers().await?;
    spinner.finish_and_clear();

    if !response.success {
        let error_msg = response
            .error
            .unwrap_or_else(|| "Unknown error".to_string());
        return Err(anyhow::anyhow!("Failed to get offers: {}", error_msg));
    }

    println!("{}", "Available Pod Tiers".bold());
    println!();

    if let Some(offers) = response.offers {
        if offers.is_empty() {
            println!("{}", "  No offers available".dimmed());
        } else {
            println!(
                "  {:<12} {:<20} {:<10} {:<10} {:>15}",
                "ID".bold().underline(),
                "Name".bold().underline(),
                "CPU".bold().underline(),
                "RAM".bold().underline(),
                "Rate".bold().underline()
            );
            println!();

            for offer in offers {
                let rate_display = format!("{} msats/sec", offer.rate_msats_per_sec);
                let cpu_display = format!("{} cores", offer.cpu_millicores / 1000);
                let ram_display = if offer.memory_mb >= 1024 {
                    format!("{} GB", offer.memory_mb / 1024)
                } else {
                    format!("{} MB", offer.memory_mb)
                };

                println!(
                    "  {:<12} {:<20} {:<10} {:<10} {:>15}",
                    offer.id.cyan(),
                    offer.name,
                    cpu_display,
                    ram_display,
                    rate_display.yellow()
                );

                if !offer.description.is_empty() {
                    println!("  {}", format!("  {}", offer.description).dimmed());
                }
            }
        }
    }

    println!();

    if let Some(mints) = response.mint_urls {
        println!("{}", "Accepted Mints".bold());
        for mint in mints {
            println!("  - {}", mint.cyan());
        }
        println!();
    }

    println!(
        "{}",
        "Tip: Use 'paygress-cli spawn --server <URL> --tier <ID> --token <CASHU_TOKEN>' to spawn"
            .dimmed()
    );

    Ok(())
}

async fn execute_info(args: InfoArgs, _verbose: bool) -> Result<()> {
    println!("{}", "Provider Details".blue().bold());
    println!();

    let relays = parse_relays(args.relays);
    let client = DiscoveryClient::new(relays).await?;

    match client.get_provider(&args.provider).await? {
        Some(provider) => {
            println!("{}", DiscoveryClient::format_provider_details(&provider));

            println!();
            println!("To spawn on this provider:");
            println!("  {} spawn \\", "paygress-cli".cyan());
            println!("    --provider {} \\", args.provider);
            println!("    --tier basic \\");
            println!("    --token <your-cashu-token> \\");
            println!("    --ssh-pass <password>");
        }
        None => {
            println!("{}", "Provider not found.".red());
            println!();
            println!("Make sure the NPUB is correct and the provider is online.");
        }
    }

    Ok(())
}
