// `paygress-cli list` — discover providers via Nostr, or query one
// Paygress HTTP server directly with `--server`.

use anyhow::Result;
use clap::{Args, Subcommand};
use colored::Colorize;

use crate::api::PaygressClient;
use crate::util::{parse_isolation_level, parse_relays, spinner};
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

    /// Minimum isolation tier: shared-kernel, dedicated-host, or
    /// attested-research-tier. Stricter tiers also match.
    #[arg(long, value_parser = parse_isolation_level)]
    pub isolation_level: Option<paygress::nostr::IsolationLevel>,

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
    /// Provider ID
    pub provider: String,

    /// Custom Nostr relays (comma-separated)
    #[arg(long)]
    pub relays: Option<String>,
}

pub async fn execute(args: ListArgs, verbose: bool) -> Result<()> {
    if let Some(ListAction::Info(info_args)) = args.action {
        return execute_info(info_args).await;
    }
    if let Some(ref server) = args.server {
        return execute_http_list(server, verbose).await;
    }
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
        isolation_level: args.isolation_level,
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
    println!("To see details: {} list info <id>", "paygress-cli".cyan());
    println!(
        "To spawn:       {} spawn --provider <id> --token <cashu-token>",
        "paygress-cli".cyan()
    );

    Ok(())
}

async fn execute_http_list(server: &str, verbose: bool) -> Result<()> {
    if verbose {
        println!("{} Fetching offers from {}...", "->".blue(), server);
    }

    let spinner = spinner("Fetching available offers...");
    let client = PaygressClient::new(server);
    let response = client.get_offers().await?;
    spinner.finish_and_clear();

    if !response.success {
        let error_msg = response.error.as_deref().unwrap_or("Unknown error");
        return Err(anyhow::anyhow!("Failed to get offers: {}", error_msg));
    }

    println!("{}", "Available Pod Tiers".bold());
    println!();

    match response.offers.as_deref() {
        None => {}
        Some([]) => println!("{}", "  No offers available".dimmed()),
        Some(offers) => print_offer_table(offers),
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

fn print_offer_table(offers: &[crate::api::PodOffer]) {
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

async fn execute_info(args: InfoArgs) -> Result<()> {
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
